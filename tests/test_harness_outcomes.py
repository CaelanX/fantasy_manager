"""harness.outcomes: realized gains per episode / move and window on synthetic ledgers."""
import json
from datetime import date, timedelta

import pytest

from fantasy_manager.harness import Ledger
from fantasy_manager.harness.outcomes import (GAMES_PER_DAY, grade_outcomes, predicted_pts, scoring_config,
                                              season_end, store_scoring)
from fantasy_manager.models import ScoringConfig

D = date
SC = ScoringConfig(kind="points", weights={"G": 3.0, "A": 2.0, "SOG": 0.5, "W": 4.0, "SV": 0.2, "GA": -1.0})


@pytest.fixture
def led(tmp_path):
    with Ledger(tmp_path) as L:
        store_scoring(L, "espn", SC)
        store_scoring(L, "fantrax", SC)
        yield L


def days(start, n):
    return [start + timedelta(days=i) for i in range(n)]


def pull(L, dates, stats=None):
    """Mark ``dates`` pulled and store ``stats`` {nhl_id: {date: stats}}."""
    rows = [{"nhl_id": nid, "game_date": d.isoformat(), "gp": 1, "stats_json": json.dumps(st)}
            for nid, by in (stats or {}).items() for d, st in by.items()]
    L.upsert("realized_daily", rows, ("nhl_id", "game_date"))
    L.upsert("realized_pulls", [{"game_date": d.isoformat(), "n_players": 1} for d in dates], ("game_date",))


NHL = {"a": 1, "d": 2, "s": 3, "b": 4, "c": 5, "r1": 11, "r2": 12}


def episode(L, eid, kind, first, last=None, adds=(), drops=(), status="expired", acted_on=None, gain=None,
            units=None, horizon=None, league="espn", match=None, strength=None, title=None):
    L.upsert("rec_episodes", [{"episode_id": eid, "league": league, "rec_key": eid, "kind": kind,
                               "title": title or eid, "first_seen": first.isoformat(),
                               "last_seen": (last or first).isoformat(), "n_days": 1, "predicted_gain": gain,
                               "gain_units": units, "horizon_days": horizon, "strength": strength,
                               "status": status, "acted_on": acted_on.isoformat() if acted_on else None,
                               "match_json": json.dumps(match) if match else None}], ("episode_id",))
    L.upsert("rec_players", [{"league": league, "as_of": first.isoformat(), "rec_key": eid, "side": side, "cid": c,
                              "name": c, "nhl_id": NHL.get(c)}
                             for side, cs in (("add", adds), ("drop", drops)) for c in cs],
             ("league", "as_of", "rec_key", "side", "cid"))


def proj(L, day, cid, league="espn", **kw):
    row = {"league": league, "as_of": day.isoformat(), "cid": cid, "name": cid, "nhl_id": NHL.get(cid),
           "positions": "C", "fpg": 2.0}
    if "inputs" in kw:
        kw["inputs_json"] = json.dumps(kw.pop("inputs"))
    row.update(kw)
    L.upsert("projections", [row], ("league", "as_of", "cid"))


def rows(L, **where):
    out = L.query("SELECT * FROM outcomes ORDER BY episode_id, decision_id, basis, window")
    for r in out:
        r["detail"] = json.loads(r["detail_json"])
    return [r for r in out if all(r[k] == v for k, v in where.items())]


def one(L, **where):
    got = rows(L, **where)
    assert len(got) == 1, got
    return got[0]


def test_waiver_window_delta_with_known_stats(led):
    start = D(2026, 10, 5)
    episode(led, "w", "waiver", start, adds=["a"], drops=["d"], gain=0.5, units="season_fpg")
    proj(led, start, "a", games_next7=3)
    pull(led, days(D(2026, 10, 6), 40), {
        1: {D(2026, 10, 6): {"G": 1}, D(2026, 10, 10): {"A": 1}, D(2026, 10, 20): {"G": 1, "SOG": 2}},
        2: {D(2026, 10, 7): {"SOG": 2}}})
    s = grade_outcomes(led, "espn", D(2026, 11, 10))
    assert s.scoring == "meta" and s.graded == 3
    w7 = one(led, window="7d")
    assert (w7["realized_gain"], w7["hit"], w7["complete"], w7["partial"]) == (4.0, 1, 1, 0)
    assert w7["origin"] == "ignored" and w7["basis"] == "first_seen" and w7["kind"] == "waiver"
    assert (w7["window_start"], w7["window_end"]) == ("2026-10-06", "2026-10-12")
    assert w7["predicted_pts"] == pytest.approx(0.5 * 3 / 7 * 7)       # season_fpg x the add's games / day
    assert w7["detail"]["pts_in"] == 5.0 and w7["detail"]["pts_out"] == 1.0
    assert one(led, window="28d")["realized_gain"] == 8.0            # 3 + 2 + 4 - 1
    ros = one(led, window="ros")
    assert ros["complete"] == 0 and ros["window_end"] == season_end(start).isoformat()


def test_incomplete_window_is_stored_partial_and_regraded(led):
    start = D(2026, 10, 5)
    episode(led, "w", "waiver", start, adds=["a"], drops=["d"])
    pull(led, [d for d in days(D(2026, 10, 6), 28) if d != D(2026, 10, 9)], {1: {D(2026, 10, 6): {"G": 1}}})
    grade_outcomes(led, "espn", D(2026, 11, 10))
    assert one(led, window="7d")["complete"] == 0                       # 10-09 never pulled
    assert one(led, window="28d")["complete"] == 0
    # mid-window: graded so far, partial
    grade_outcomes(led, "espn", D(2026, 10, 9))
    w7 = one(led, window="7d")
    assert w7["complete"] == 0 and w7["partial"] == 1 and w7["detail"]["graded_through"] == "2026-10-08"
    assert w7["realized_gain"] == 3.0
    assert rows(led, window="ros")                                     # started: stored
    pull(led, [D(2026, 10, 9)])
    grade_outcomes(led, "espn", D(2026, 11, 10))
    assert one(led, window="7d")["complete"] == 1 and one(led, window="28d")["complete"] == 1


def test_window_not_started_is_not_stored(led):
    episode(led, "w", "waiver", D(2026, 10, 5), adds=["a"])
    assert grade_outcomes(led, "espn", D(2026, 10, 6)).graded == 0      # first day is 10-06, not over yet


def test_grading_is_idempotent_and_drops_vanished_subjects(led):
    episode(led, "w", "waiver", D(2026, 10, 5), adds=["a"], drops=["d"])
    pull(led, days(D(2026, 10, 6), 28), {1: {D(2026, 10, 6): {"G": 1}}})
    grade_outcomes(led, "espn", D(2026, 11, 10))
    strip = lambda rs: [{k: v for k, v in r.items() if k != "graded_at"} for r in rs]  # noqa: E731
    before = strip(rows(led))
    grade_outcomes(led, "espn", D(2026, 11, 10))
    assert strip(rows(led)) == before and led.count("outcomes") == 3
    led.execute("DELETE FROM rec_episodes")
    grade_outcomes(led, "espn", D(2026, 11, 10))
    assert led.count("outcomes") == 0


def test_espn_lineup_delta_only_on_shared_game_days(led):
    start = D(2026, 10, 5)
    episode(led, "l", "lineup", start, adds=["a"], drops=["d"], gain=6.0, units="week_pts", horizon=7)
    pull(led, days(start, 7), {1: {start: {"G": 1}, D(2026, 10, 6): {"G": 2}, D(2026, 10, 7): {"A": 1}},
                               2: {start: {"A": 1}, D(2026, 10, 7): {"SOG": 2}, D(2026, 10, 8): {"G": 1}}})
    grade_outcomes(led, "espn", D(2026, 10, 20))
    r = one(led, kind="lineup")
    assert r["window"] == "7d" and r["window_start"] == "2026-10-05" and r["window_end"] == "2026-10-11"
    assert r["detail"]["shared_days"] == 2
    assert r["realized_gain"] == pytest.approx((3 - 2) + (2 - 1))      # 10-06 / 10-08: only one side played
    assert r["predicted_pts"] == 6.0 and r["complete"] == 1 and r["hit"] == 1
    assert not rows(led, window="28d")                                 # lineup recs: one weekly window


def test_lineup_without_shared_days_has_no_gain(led):
    start = D(2026, 10, 5)
    episode(led, "l", "lineup", start, adds=["a"], drops=["d"])
    pull(led, days(start, 7), {1: {start: {"G": 1}}, 2: {D(2026, 10, 6): {"G": 1}}})
    grade_outcomes(led, "espn", D(2026, 10, 20))
    r = one(led, kind="lineup")
    assert r["realized_gain"] is None and r["hit"] is None and r["detail"]["shared_days"] == 0


def test_fantrax_lineup_graded_over_the_locked_week(led):
    wed = D(2026, 10, 7)
    episode(led, "l", "lineup", wed, adds=["a"], drops=["d"], league="fantrax", gain=3.0, units="week_pts")
    pull(led, days(D(2026, 10, 6), 14), {1: {D(2026, 10, 10): {"G": 5}, D(2026, 10, 13): {"G": 1}},
                                         2: {D(2026, 10, 15): {"A": 1}, D(2026, 10, 19): {"G": 3}}})
    grade_outcomes(led, "fantrax", D(2026, 10, 25))
    r = one(led, kind="lineup")
    assert (r["window_start"], r["window_end"]) == ("2026-10-12", "2026-10-18")    # Monday lock to Sunday
    assert r["realized_gain"] == pytest.approx(3.0 - 2.0)             # 10-10 (pre-lock) and 10-19 excluded
    assert r["detail"]["lock"] is True and r["complete"] == 1


def test_flags_are_graded_directionally_against_l15(led):
    start = D(2026, 10, 5)
    hot = {"last15": {"gp": 5, "stats": {"G": 5}}}                     # L15 FPG 3.0
    proj(led, start, "s", inputs=hot)
    proj(led, start, "b", inputs=hot)
    episode(led, "sh", "sell_high", start, drops=["s"], gain=-0.8, units="season_fpg", horizon=28)
    episode(led, "bl", "buy_low", start, adds=["b"], gain=0.5, units="season_fpg", horizon=28)
    window = days(D(2026, 10, 6), 28)
    pull(led, window, {3: {d: {"G": 1} if i % 2 == 0 else {"SOG": 1} for i, d in enumerate(window[:10])},
                       4: {d: {"G": 1, "A": 1} for d in window[:10]}})
    grade_outcomes(led, "espn", D(2026, 11, 10))
    sh = one(led, episode_id="sh")
    assert sh["window"] == "28d" and sh["gain_units"] == "season_fpg"
    assert sh["detail"]["l15_fpg"] == 3.0 and sh["detail"]["gp"] == 10
    assert sh["realized_gain"] == pytest.approx((5 * 3 + 5 * 0.5) / 10 - 3.0) and sh["hit"] == 1   # cooled off
    bl = one(led, episode_id="bl")
    assert bl["realized_gain"] == pytest.approx(5.0 - 3.0) and bl["hit"] == 1                     # bounced up
    assert bl["predicted_pts"] is None


def test_buy_low_miss_and_no_games_is_ungraded(led):
    start = D(2026, 10, 5)
    proj(led, start, "b", inputs={"last15": {"gp": 5, "stats": {"A": 5}}})    # 2.0 FPG
    proj(led, start, "s", inputs={"last15": {"gp": 5, "stats": {"A": 5}}})
    episode(led, "bl", "buy_low", start, adds=["b"])
    episode(led, "sh", "sell_high", start, drops=["s"])
    pull(led, days(D(2026, 10, 6), 28), {4: {D(2026, 10, 6): {"SOG": 2}}})
    grade_outcomes(led, "espn", D(2026, 11, 10))
    assert one(led, episode_id="bl")["hit"] == 0
    sh = one(led, episode_id="sh")
    assert sh["realized_gain"] is None and sh["hit"] is None           # 0 GP in the window


def test_two_for_one_trade_is_adjusted_for_the_replacement(led):
    start = D(2026, 10, 5)
    proj(led, start, "r1", vorp=0.05)
    proj(led, start, "r2", vorp=-0.1)
    proj(led, start, "a", vorp=1.5)
    episode(led, "t", "trade", start, adds=["a", "b"], drops=["c"], status="followed", acted_on=start,
            gain=0.4, units="lineup_fpg", match={"got": ["a", "b"], "gave": ["c"]})
    d1 = D(2026, 10, 6)
    pull(led, days(d1, 28), {1: {d1: {"G": 2}}, 4: {d1: {"A": 1}}, 5: {d1: {"G": 1}},
                             11: {d1: {"A": 1}}, 12: {d1: {"SOG": 2}}})
    grade_outcomes(led, "espn", D(2026, 11, 10))
    r = one(led, episode_id="t", basis="first_seen", window="7d")
    repl = (2.0 + 1.0) / 2                       # the players closest to replacement level (|vorp| ~ 0)
    assert r["detail"]["replacement_pts"] == repl
    assert r["realized_gain"] == pytest.approx(6 + 2 - 3 - repl)
    assert r["predicted_pts"] == pytest.approx(0.4 * GAMES_PER_DAY * 7, abs=1e-4)
    acted = one(led, episode_id="t", basis="acted_on", window="7d")
    assert acted["origin"] == "followed" and acted["realized_gain"] == r["realized_gain"]
    assert one(led, episode_id="t", window="ros", basis="first_seen")["detail"]["label"] == "partial"


def test_injury_rec_is_the_waiver_adds_gain_and_ir_only_is_skipped(led):
    start = D(2026, 10, 5)
    episode(led, "i", "injury", start, adds=["a"], gain=4.0, units="week_pts", horizon=7)
    episode(led, "ir", "injury", start + timedelta(days=1))          # IR move only: nothing to grade
    pull(led, days(D(2026, 10, 6), 28), {1: {D(2026, 10, 7): {"G": 1}}})
    grade_outcomes(led, "espn", D(2026, 11, 10))
    r = one(led, episode_id="i", window="7d")
    assert r["realized_gain"] == 3.0 and r["predicted_pts"] == 4.0
    assert not rows(led, episode_id="ir")


def test_user_only_move_has_the_models_view_and_alternative(led):
    day = D(2026, 10, 8)
    proj(led, D(2026, 10, 5), "b", fpg=1.2, games_next7=4)
    proj(led, D(2026, 10, 5), "c", fpg=1.5)
    episode(led, "w", "waiver", D(2026, 10, 6), last=D(2026, 10, 9), adds=["a"], drops=["c"], gain=0.9,
            units="season_fpg", strength=7)
    episode(led, "w2", "waiver", D(2026, 10, 7), adds=["d"], drops=["c"], gain=0.2, units="season_fpg")
    led.upsert("decisions", [{"decision_id": "tx:espn:g1", "league": "espn", "day": day.isoformat(),
                              "origin": "user_only", "kind": "waiver", "adds_json": '["b"]',
                              "drops_json": '["c"]'}], ("decision_id",))
    pull(led, days(D(2026, 10, 7), 40), {4: {D(2026, 10, 9): {"A": 1}}, 1: {D(2026, 10, 10): {"G": 2}},
                                         5: {D(2026, 10, 12): {"SOG": 2}}})
    grade_outcomes(led, "espn", D(2026, 11, 20))
    r = one(led, decision_id="tx:espn:g1", window="7d")
    assert r["origin"] == "user_only" and r["basis"] == "decision" and r["episode_id"] is None
    assert r["realized_gain"] == pytest.approx(2.0 - 1.0)
    assert r["detail"]["model_fpg_delta"] == pytest.approx(1.2 - 1.5)   # the model disliked my move
    assert r["predicted_gain"] == pytest.approx(-0.3) and r["predicted_pts"] == pytest.approx(-1.2, abs=1e-4)
    alt = r["detail"]["alt"]
    assert alt["episode_id"] == "w" and alt["gain"] == pytest.approx(6.0 - 1.0)   # same window as my move


def test_scoring_from_archive_header_then_meta(tmp_path):
    arch = tmp_path / "archive"
    arch.mkdir()
    (arch / "projections-fantrax-2026-09-28.json").write_text(json.dumps(
        {"version": 2, "scoring": {"kind": "points", "weights": {"G": 4.0}, "goalie_weights": {"G": 20.0}},
         "players": []}), encoding="utf-8")
    with Ledger(tmp_path) as L:
        cfg, src = scoring_config(L, "fantrax")
        assert src == "archive" and cfg.weights == {"G": 4.0} and cfg.goalie_weights == {"G": 20.0}
        assert scoring_config(L, "fantrax")[1] == "meta"
        assert scoring_config(L, "espn")[1] == "preset"
        assert scoring_config(L, "yahoo") == (None, "none")
        assert grade_outcomes(L, "yahoo", D(2026, 11, 1)).note


def test_predicted_pts_units():
    assert predicted_pts(7.0, "week_pts", 28) == 28.0
    assert predicted_pts(0.5, "season_fpg", 7, 3 / 7) == pytest.approx(1.5)
    assert predicted_pts(None, "week_pts", 7) is None and predicted_pts(1.0, None, 7) is None


def test_old_ledger_gets_the_new_outcome_columns(tmp_path):
    import sqlite3
    db = sqlite3.connect(tmp_path / "harness.db")
    db.execute("CREATE TABLE outcomes (outcome_id TEXT PRIMARY KEY, league TEXT, episode_id TEXT, decision_id TEXT,"
               " window TEXT NOT NULL, basis TEXT, predicted_gain REAL, realized_gain REAL, gain_units TEXT,"
               " hit INTEGER, partial INTEGER, graded_at TEXT, detail_json TEXT)")
    db.commit()
    db.close()
    with Ledger(tmp_path) as L:
        cols = {r["name"] for r in L.query("PRAGMA table_info(outcomes)")}
        assert {"kind", "origin", "complete", "window_start", "window_end", "predicted_pts"} <= cols
        assert L.query("SELECT value FROM meta WHERE key='schema_version'")[0]["value"] == "2"


def test_daily_with_grade_stores_scoring_and_grades(tmp_path, monkeypatch):
    from types import SimpleNamespace

    from rich.console import Console
    from typer.testing import CliRunner

    from fantasy_manager import cli_backtest, cli_harness
    from fantasy_manager.harness import realized as R
    from tests.test_harness_ledger import _ctx

    monkeypatch.setattr(cli_harness, "_settings", lambda: SimpleNamespace(fm_data_dir=tmp_path, fm_offline=True))
    monkeypatch.setattr(cli_harness, "console", Console(width=200))
    ctx = _ctx()
    ctx.scoring = SC

    class Prov:
        warnings: list = []

        def activity(self, since=None):
            return []

        def lineup_snapshot(self, day):
            return []
    monkeypatch.setattr(cli_backtest, "_load_league_full", lambda lg, s, c, with_recs: (ctx, {}, [], [], Prov()))
    monkeypatch.setattr(R, "pull_realized", lambda ledger, client, day: 0)
    res = CliRunner().invoke(cli_harness.harness_app, ["daily", "--league", "fantrax", "--grade", "--json"])
    assert res.exit_code == 0, res.output
    out = json.loads(res.output)
    assert out["grade"][0]["league"] == "fantrax" and out["grade"][0]["outcomes"]["scoring"] == "meta"
    with Ledger(tmp_path) as L:
        cfg, src = scoring_config(L, "fantrax")
        assert src == "meta" and cfg.weights == SC.weights
        assert L.count("metric_snapshots", "WHERE league='fantrax'") > 0


def test_lineup_filling_an_empty_slot_counts_his_points(led):
    start = D(2026, 10, 5)
    episode(led, "l", "lineup", start, adds=["a"], gain=4.0, units="week_pts")
    episode(led, "b", "lineup", start, drops=["d"])                      # "Bench X (out)"
    pull(led, days(start, 7), {1: {start: {"G": 1}, D(2026, 10, 8): {"A": 1}}})
    grade_outcomes(led, "espn", D(2026, 10, 20))
    r = one(led, episode_id="l")
    assert r["realized_gain"] == 5.0 and r["detail"]["shared_days"] == 2 and r["hit"] == 1
    assert one(led, episode_id="b")["realized_gain"] is None


def test_no_games_is_not_a_miss_and_unpulled_windows_wait(led):
    start = D(2026, 9, 28)                                                # preseason: no NHL games
    episode(led, "w", "waiver", start, adds=["a"], drops=["d"])
    episode(led, "l", "lineup", start, adds=["a"], drops=["d"], league="fantrax")
    assert grade_outcomes(led, "espn", D(2026, 10, 1)).graded == 0      # nothing pulled yet: not stored
    pull(led, days(D(2026, 9, 28), 8))
    grade_outcomes(led, "espn", D(2026, 10, 10))
    grade_outcomes(led, "fantrax", D(2026, 10, 10))
    w7 = one(led, episode_id="w", window="7d")
    assert w7["complete"] == 1 and w7["realized_gain"] is None and w7["hit"] is None
    lu = one(led, episode_id="l")
    assert lu["window_start"] == "2026-09-28" and lu["realized_gain"] is None and lu["hit"] is None
