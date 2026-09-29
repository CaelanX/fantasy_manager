"""harness.metrics: statistics, trust labels, weekly snapshots and the headline on synthetic ledgers."""
import json
from datetime import date, timedelta

import pytest

from fantasy_manager.harness import Ledger
from fantasy_manager.harness.metrics import (bootstrap_skill, calibration_bins, calibration_trust, compute_week,
                                             grade_week, headline, hit_trust, pool_of, projection_trust,
                                             skill_score, status_report, weekly_snapshots, wilson)
from fantasy_manager.harness.outcomes import store_scoring
from fantasy_manager.models import ScoringConfig

D = date
SC = ScoringConfig(kind="points", weights={"G": 3.0, "A": 2.0, "SOG": 0.5, "W": 4.0, "SV": 0.2, "GA": -1.0})
WEEK = D(2026, 11, 9)


@pytest.fixture
def led(tmp_path):
    with Ledger(tmp_path) as L:
        store_scoring(L, "espn", SC)
        yield L


# --------------------------------------------------------------------------- statistics

@pytest.mark.parametrize("k,n,lo,hi", [
    (0, 0, None, None),
    (5, 10, 0.2366, 0.7634),
    (20, 20, 0.8389, 1.0),
    (0, 20, 0.0, 0.1611),
    (30, 50, 0.4618, 0.7239),
])
def test_wilson_interval(k, n, lo, hi):
    a, b = wilson(k, n)
    if lo is None:
        assert (a, b) == (None, None)
    else:
        assert a == pytest.approx(lo, abs=1e-4) and b == pytest.approx(hi, abs=1e-4)


@pytest.mark.parametrize("n,label", [(0, "hidden"), (19, "hidden"), (20, "provisional"), (49, "provisional"),
                                     (50, "reliable"), (500, "reliable")])
def test_hit_rate_trust(n, label):
    assert hit_trust(n) == label


@pytest.mark.parametrize("n,weeks,pool,week,label", [
    (149, 9, "F", D(2027, 2, 1), "hidden"),
    (150, 1, "F", D(2026, 10, 26), "provisional"),
    (399, 8, "D", D(2026, 12, 7), "provisional"),
    (400, 3, "F", D(2026, 12, 7), "provisional"),          # enough windows, too few weeks
    (400, 4, "F", D(2026, 12, 7), "reliable"),
    (900, 10, "G", D(2026, 12, 28), "provisional"),        # goalies capped before January
    (400, 4, "G", D(2027, 1, 4), "reliable"),
    (100, 10, "G", D(2027, 1, 4), "hidden"),
])
def test_projection_trust(n, weeks, pool, week, label):
    assert projection_trust(n, weeks, pool, week) == label


@pytest.mark.parametrize("n,label", [(3, "hidden"), (99, "hidden"), (100, "reliable")])
def test_calibration_trust(n, label):
    assert calibration_trust(n) == label


def test_calibration_bins_terciles_then_deciles():
    assert calibration_bins([1.0, 2.0], [0.0, 1.0]) == []
    pred = [9.0, 1.0, 5.0, 2.0, 8.0, 4.0, 3.0, 7.0, 6.0]
    bins = calibration_bins(pred, [p * 2 for p in pred])
    assert [b["n"] for b in bins] == [3, 3, 3]
    assert [b["mean_pred"] for b in bins] == [2.0, 5.0, 8.0] and [b["mean_real"] for b in bins] == [4.0, 10.0, 16.0]
    assert bins[0]["pred_lo"] == 1.0 and bins[-1]["pred_hi"] == 9.0
    many = calibration_bins([float(i) for i in range(100)], [0.0] * 100)
    assert len(many) == 10 and all(b["n"] == 10 for b in many)
    assert calibration_bins([float(i) for i in range(99)], [1.0] * 99)[0]["n"] == 33


def test_skill_score_and_player_clustered_bootstrap():
    # four players, two windows each; fm errors are half the to-date errors for every player
    pairs = [(p, 0.5 * e, e) for p, e in [(1, 1.0), (1, 2.0), (2, 0.4), (2, 0.6), (3, 3.0), (3, 1.0), (4, 2.0),
                                           (4, 2.0)]]
    assert skill_score([a for _, a, _ in pairs], [b for _, _, b in pairs]) == pytest.approx(0.5)
    assert bootstrap_skill(pairs) == (pytest.approx(0.5), pytest.approx(0.5))   # every resample: 0.5
    mixed = [(1, 0.5, 1.0), (1, 0.5, 1.0), (2, 1.0, 1.0), (3, 0.2, 1.0), (4, 1.5, 1.0), (5, 0.3, 1.0)]
    lo, hi = bootstrap_skill(mixed, n_boot=200, seed=7)
    point = skill_score([a for _, a, _ in mixed], [b for _, _, b in mixed])
    assert lo < point < hi and (lo, hi) == bootstrap_skill(mixed, n_boot=200, seed=7)   # seeded: deterministic
    assert bootstrap_skill([(1, 0.5, 1.0), (1, 0.4, 1.0)]) == (None, None)             # one cluster: no CI
    assert skill_score([], []) is None


def test_pool_and_weekly_snapshot_helpers():
    assert [pool_of(p) for p in ("C,F", "D", "G", "F,D", "C,F,G", "LW,RW,F", None)] == \
        ["F", "D", "G", "F", "F", "F", "F"]
    snaps = weekly_snapshots(["2026-10-07", "2026-10-06", "2026-10-12", "2026-10-14", "2026-10-21"])
    assert snaps == [(D(2026, 10, 5), D(2026, 10, 6)), (D(2026, 10, 12), D(2026, 10, 12)),
                     (D(2026, 10, 19), D(2026, 10, 21))]


# --------------------------------------------------------------------------- projection accuracy

def _pull(L, dates, stats):
    L.upsert("realized_daily", [{"nhl_id": nid, "game_date": d.isoformat(), "gp": 1, "stats_json": json.dumps(st)}
                                for nid, by in stats.items() for d, st in by.items()], ("nhl_id", "game_date"))
    L.upsert("realized_pulls", [{"game_date": d.isoformat(), "n_players": 1} for d in dates], ("game_date",))


def _projection_ledger(L):
    """Snapshot Monday 2026-10-05: three skaters and a goalie, 14 games each over the next 28 days."""
    snap = D(2026, 10, 5)
    players = [  # cid, nhl, pos, fpg, fpg_week, proj_week, stats per game, season-to-date line, projection
        ("f1", 1, "C,F", 3.0, 3.0, 9.0, {"G": 1}, {"gp": 2, "stats": {"G": 2}}, {"gp": 80, "stats": {"G": 40}}),
        ("f2", 2, "LW,F", 1.0, 1.0, 4.0, {"A": 1}, {"gp": 2, "stats": {"A": 2}}, None),
        ("d1", 3, "D", 2.0, 2.0, 6.0, {"SOG": 2}, None, {"gp": 80, "stats": {"SOG": 80}}),
        ("g1", 4, "G", 5.0, 5.0, 10.0, {"W": 1, "SV": 5}, {"gp": 1, "stats": {"W": 1}}, None),
    ]
    rows, stats = [], {}
    for cid, nid, pos, fpg, fw, pw, per_game, season, projection in players:
        inputs = {"season": season, "projection": projection, "history": {"rates": {"G": 0.5}, "gp": 82}}
        rows.append({"league": "espn", "as_of": snap.isoformat(), "cid": cid, "name": cid, "nhl_id": nid,
                     "positions": pos, "fpg": fpg, "fpg_week": fw, "proj_week": pw,
                     "inputs_json": json.dumps(inputs)})
        # a game every other day from 10-06: 4 in the first 7 days, 14 in the 28-day window
        stats[nid] = {snap + timedelta(days=i): per_game for i in range(1, 29, 2)}
    L.upsert("projections", rows, ("league", "as_of", "cid"))
    _pull(L, [snap + timedelta(days=i) for i in range(1, 40)], stats)
    return snap


def test_projection_accuracy_against_realized_and_baselines(led):
    _projection_ledger(led)
    rows, info = compute_week(led, "espn", WEEK)
    assert info["matured_28d"] == 1 and info["matured_7d"] == 1
    by = {(r["metric"], r["pool"]): r for r in rows}
    f = by[("proj_fpg_mae", "F")]
    d = json.loads(f["detail_json"])
    # realized FPG: f1 3.0 (pred 3.0), f2 2.0 (pred 1.0) -> MAE 0.5
    assert f["n"] == 2 and f["value"] == pytest.approx(0.5) and f["trust"] == "hidden"
    assert d["need"] == 148 and d["weeks"] == 1 and d["per_week"] == [{"week": "2026-10-05", "n": 2, "mae": 0.5}]
    base = d["baselines"]
    assert base["to_date"]["n"] == 2 and base["to_date"]["mae"] == pytest.approx(0.0)   # to-date was spot on
    assert base["provider"]["n"] == 1 and base["provider"]["mae"] == pytest.approx(1.5)  # f1: 40 G / 80 GP
    assert base["preseason"]["mae"] == pytest.approx(0.5) and base["last_season"]["n"] == 2
    sk = by[("proj_fpg_skill", "F")]
    assert sk["n"] == 2 and sk["value"] is None                         # MAE_to_date = 0: undefined
    g = by[("proj_fpg_mae", "G")]
    assert g["n"] == 1 and g["value"] == pytest.approx(0.0)             # 4 + 1.0 = 5.0 per game
    wk = by[("proj_week_mae", "F")]
    wd = json.loads(wk["detail_json"])
    # 7 days, 4 games: f1 real 12 vs pred 9 (3 games), f2 real 8 vs 4
    assert wk["n"] == 2 and wk["value"] == pytest.approx((3 + 4) / 2)
    assert wd["rate_mae"] == pytest.approx((0 + 4) / 2) and wd["avail_mae"] == pytest.approx((3 + 0) / 2)


def test_projection_windows_mature_by_week(led):
    _projection_ledger(led)
    _, info = compute_week(led, "espn", D(2026, 10, 12))
    assert info["matured_7d"] == 0                                      # 7-day window runs to Monday 10-12
    _, info = compute_week(led, "espn", D(2026, 10, 19))
    assert info["matured_7d"] == 1 and info["matured_28d"] == 0         # 28-day window runs to 11-02
    _, info = compute_week(led, "espn", D(2026, 10, 7))                 # mid-week date = its Monday 10-05
    assert info["week"] == "2026-10-05" and info["matured_7d"] == 0
    led.execute("DELETE FROM realized_pulls WHERE game_date='2026-10-20'")
    _, info = compute_week(led, "espn", WEEK)
    assert info["matured_28d"] == 0 and info["unpulled"] == 1          # a missing day: not matured


# --------------------------------------------------------------------------- recommendation quality

def _outcome(L, oid, kind="waiver", origin="followed", hit=1, gain=2.0, pred=1.5, window="28d", units="season_fpg",
             complete=1, end="2026-11-01", detail=None, basis="first_seen", decision=None):
    L.upsert("outcomes", [{"outcome_id": oid, "league": "espn", "episode_id": None if decision else f"e{oid}",
                           "decision_id": decision, "window": window, "basis": basis, "predicted_gain": 0.5,
                           "realized_gain": gain, "gain_units": units, "hit": hit, "partial": 1 - complete,
                           "kind": kind, "origin": origin, "complete": complete, "window_start": "2026-10-05",
                           "window_end": end, "predicted_pts": pred,
                           "detail_json": json.dumps(detail or {"title": f"t{oid}", "day": "2026-10-04"})}],
             ("outcome_id",))


def test_hit_rates_trust_and_trades_never_aggregated(led):
    for i in range(25):
        _outcome(led, f"f{i}", hit=int(i < 15), gain=1.0 if i < 15 else -1.0)
    for i in range(5):
        _outcome(led, f"i{i}", origin="ignored", hit=0, gain=-0.5)
    _outcome(led, "late", end="2026-11-20")                                 # window ends after the week
    _outcome(led, "part", complete=0)                                       # incomplete window
    _outcome(led, "w7", window="7d")                                        # not the primary window
    _outcome(led, "acted", basis="acted_on")                                # secondary basis
    _outcome(led, "tr", kind="trade", window="ros", complete=0, units="lineup_fpg")
    rows, _ = compute_week(led, "espn", WEEK)
    hits = {r["pool"]: r for r in rows if r["metric"] == "hit_rate"}
    fol = hits["waiver:followed"]
    assert fol["n"] == 25 and fol["value"] == pytest.approx(0.6) and fol["trust"] == "provisional"
    assert (fol["ci_lo"], fol["ci_hi"]) == (pytest.approx(wilson(15, 25)[0], abs=1e-4),
                                            pytest.approx(wilson(15, 25)[1], abs=1e-4))
    assert json.loads(fol["detail_json"])["mean_gain"] == pytest.approx((15 - 10) / 25)
    ign = hits["waiver:ignored"]
    assert ign["n"] == 5 and ign["trust"] == "hidden" and json.loads(ign["detail_json"])["need"] == 15
    assert hits["lineup:followed"]["n"] == 0 and hits["lineup:followed"]["value"] is None
    assert not any(k.startswith("trade") for k in hits)
    cases = next(r for r in rows if r["metric"] == "trade_cases")
    assert cases["trust"] == "cases" and cases["value"] is None
    assert json.loads(cases["detail_json"])["cases"][0]["label"] == "partial"
    cal = next(r for r in rows if r["metric"] == "calibration")
    cd = json.loads(cal["detail_json"])
    assert cal["n"] == 30 and cal["trust"] == "hidden" and cd["binning"] == "tercile" and len(cd["bins"]) == 3
    assert cal["value"] == pytest.approx((15 - 10 - 2.5) / (30 * 1.5), abs=1e-4)


def test_primary_window_follows_the_units(led):
    for i in range(3):
        _outcome(led, f"wk{i}", window="7d", units="week_pts")
        _outcome(led, f"wk28{i}", window="28d", units="week_pts")
        _outcome(led, f"lu{i}", kind="lineup", window="7d", units="week_pts")
    rows, _ = compute_week(led, "espn", WEEK)
    hits = {r["pool"]: r for r in rows if r["metric"] == "hit_rate"}
    assert json.loads(hits["waiver:followed"]["detail_json"])["windows"] == ["7d"]
    assert hits["waiver:followed"]["n"] == 3 and hits["lineup:followed"]["n"] == 3


def test_counterfactual_my_moves_vs_the_models(led):
    for i in range(4):
        detail = {"alt": {"episode_id": "e", "gain": 3.0} if i < 3 else None, "model_fpg_delta": 0.2 if i else -0.1}
        _outcome(led, f"u{i}", origin="user_only", gain=1.0 + i, decision=f"tx:{i}", basis="decision",
                 detail=detail, pred=None)
    rows, _ = compute_week(led, "espn", WEEK)
    cf = next(r for r in rows if r["metric"] == "counterfactual")
    d = json.loads(cf["detail_json"])
    assert cf["n"] == 3 and d["moves"] == 4 and cf["trust"] == "hidden"
    assert d["my_mean"] == pytest.approx(2.0) and d["model_mean"] == pytest.approx(3.0)
    assert cf["value"] == pytest.approx(-1.0) and d["model_better"] == pytest.approx(2 / 3)
    assert d["model_agreed"] == pytest.approx(3 / 4) and d["my_mean_all"] == pytest.approx(2.5)
    hits = {r["pool"]: r for r in rows if r["metric"] == "hit_rate"}
    assert hits["waiver:user_only"]["n"] == 4


# --------------------------------------------------------------------------- the bar

def test_headline_is_none_until_trustworthy(led):
    assert headline(led, "espn") is None                                   # nothing graded
    for i in range(19):
        _outcome(led, f"f{i}", hit=int(i % 2 == 0))
    grade_week(led, "espn", WEEK)
    assert headline(led, "espn") is None                                   # 19 < 20: hidden
    rep = status_report(led)["leagues"]["espn"]
    nj = {(x["metric"], x["pool"]): x for x in rep["not_judgeable"]}
    assert nj[("hit_rate", "waiver:followed")] == {"metric": "hit_rate", "pool": "waiver:followed", "n": 19, "need": 1}
    assert nj[("proj_fpg_mae", "F")]["need"] == 150
    _outcome(led, "f19", hit=1)
    info = grade_week(led, "espn", WEEK)
    assert info["trust"]["provisional"] == 1
    line = headline(led, "espn")
    assert line.startswith("Model: waiver recs followed hit 55%") and "provisional, n=20" in line


def test_headline_reports_projection_skill(led):
    rows = [{"snapshot_id": "s1", "week": "2026-12-07", "league": "espn", "metric": "proj_fpg_skill", "pool": "F",
             "value": 0.12, "n": 420, "ci_lo": 0.05, "ci_hi": 0.18, "trust": "reliable", "detail_json": "{}"},
            {"snapshot_id": "s2", "week": "2026-12-07", "league": "espn", "metric": "proj_fpg_skill", "pool": "G",
             "value": 0.3, "n": 20, "ci_lo": None, "ci_hi": None, "trust": "hidden", "detail_json": "{}"}]
    led.upsert("metric_snapshots", rows, ("snapshot_id",))
    assert headline(led, "espn") == ("Model: forwards projections beat season-to-date by 12%, 95% CI 5% to 18% "
                                     "(reliable, n=420)")


def test_grade_week_replaces_its_rows_and_status_report_shape(led):
    first = grade_week(led, "espn", WEEK)
    n = led.count("metric_snapshots")
    assert first["rows"] == n and n > 0
    grade_week(led, "espn", WEEK + timedelta(days=2))                      # same week: replaced, not added
    assert led.count("metric_snapshots") == n
    rep = status_report(led)
    assert rep["params"]["version"] == "packaged" and rep["params"]["hash"]
    L = rep["leagues"]["espn"]
    assert L["graded"] and L["week"] == WEEK.isoformat() and L["headline"] is None
    assert {r["metric"] for r in L["projection"]} == {"proj_fpg_mae", "proj_fpg_skill", "proj_week_mae"}
    assert L["calibration"]["trust"] == "hidden" and L["counterfactual"]["n"] == 0 and L["trades"] == []
    json.dumps(rep)                                                        # JSON-serialisable for the web


def test_status_before_any_grading_lists_what_is_needed(led):
    led.upsert("rec_episodes", [{"episode_id": "e", "league": "fantrax", "rec_key": "k", "kind": "waiver",
                                 "first_seen": "2026-10-05", "last_seen": "2026-10-05", "n_days": 1}],
               ("episode_id",))
    L = status_report(led)["leagues"]["fantrax"]
    assert not L["graded"] and L["week"] is None
    assert {"metric": "calibration", "pool": "all", "n": 0, "need": 100} in L["not_judgeable"]


# --------------------------------------------------------------------------- CLI

def test_cli_grade_and_status_report_insufficient_data(tmp_path, monkeypatch):
    from types import SimpleNamespace

    from rich.console import Console
    from typer.testing import CliRunner

    from fantasy_manager import cli_harness

    monkeypatch.setattr(cli_harness, "_settings", lambda: SimpleNamespace(fm_data_dir=tmp_path, fm_offline=True))
    monkeypatch.setattr(cli_harness, "console", Console(width=200))
    with Ledger(tmp_path) as L:
        store_scoring(L, "espn", SC)
        L.upsert("rec_episodes", [{"episode_id": "e", "league": "espn", "rec_key": "k", "kind": "waiver",
                                   "first_seen": "2026-10-05", "last_seen": "2026-10-05", "n_days": 1}],
                 ("episode_id",))
    runner = CliRunner()
    res = runner.invoke(cli_harness.harness_app, ["grade", "--week", "2026-10-14", "--league", "espn", "--json"])
    assert res.exit_code == 0, res.output
    data = json.loads(res.output)
    assert data["week"] == "2026-10-12" and data["leagues"][0]["week"]["rows"] > 0
    res = runner.invoke(cli_harness.harness_app, ["grade", "--league", "espn"])
    assert res.exit_code == 0 and "bar for week" in res.output
    res = runner.invoke(cli_harness.harness_app, ["status", "--league", "espn"])
    assert res.exit_code == 0, res.output
    out = res.output
    assert "Params: packaged" in out and "Projection accuracy" in out and "Not judgeable yet" in out
    assert "hit_rate waiver:followed (n=0, need 20 more)" in out and "Digest line: hidden" in out
    res = runner.invoke(cli_harness.harness_app, ["status", "--json"])
    bar = json.loads(res.output)["bar"]
    assert bar["leagues"]["espn"]["graded"] and bar["leagues"]["espn"]["headline"] is None



def test_per_week_baselines_for_the_sparklines(led):
    _projection_ledger(led)
    rows, _ = compute_week(led, "espn", WEEK)
    f = next(r for r in rows if (r["metric"], r["pool"]) == ("proj_fpg_mae", "F"))
    (pw,) = json.loads(f["detail_json"])["per_week_base"]
    assert pw["week"] == "2026-10-05" and pw["n_to_date"] == 2
    assert pw["base_mae"]["to_date"] == pytest.approx(0.0) and pw["base_mae"]["provider"] == pytest.approx(1.5)
    assert pw["skill"]["fm"] is None                                        # MAE_to_date = 0: undefined
    from fantasy_manager.harness.health import series

    grade_week(led, "espn", WEEK)
    s = series(led, "espn")
    assert s["mae"]["F"]["x"] == ["2026-10-05"] and s["mae"]["F"]["lines"]["fm"] == [0.5]
    assert s["mae"]["F"]["lines"]["provider"] == [1.5] and s["mae"]["F"]["trust"] == "hidden"
    assert s["skill"]["F"]["lines"]["to_date"] == [0.0] and s["hit"] == {}
