"""Ledger schema, archive ingest (v1 + v2), episode rolling, realized pulls and the CLI."""
import json
import shutil
from datetime import date
from pathlib import Path
from types import SimpleNamespace

import pytest
from rich.console import Console
from typer.testing import CliRunner

from fantasy_manager import cli_backtest, cli_harness
from fantasy_manager.backtest.data import WindowLine
from fantasy_manager.harness import TABLES, Ledger, ingest_archive, pull_lineups, pull_realized, rec_key
from fantasy_manager.harness.ingest import group_days
from fantasy_manager.models import (FantasyTeam, LeagueContext, LineupDay, Player, RosterSlot, ScoringConfig)
from fantasy_manager.providers.nhl import NhlClient

FIX = Path(__file__).parent / "fixtures" / "harness"
V1_RECS = FIX / "recs-espn-2026-09-28.json"


def _archive(tmp_path):
    d = tmp_path / "archive"
    d.mkdir(exist_ok=True)
    return d


def _v1_projections(tmp_path, day="2026-09-28"):
    """A tiny v1 projections file for the fixture's IR target (subject lookup by name)."""
    snap = {"version": 1, "provider": "espn", "as_of": day, "scoring": {"kind": "points"},
            "players": [{"cid": "espn:3852", "name": "Brad Marchand", "nhl_id": 8473419, "team": "FLA",
                         "positions": ["LW"], "status": "ir", "pct_owned": 80.0, "projected": None,
                         "fm": {"fpg": 1.9, "rates": {"G": 0.3}}}]}
    (_archive(tmp_path) / f"projections-espn-{day}.json").write_text(json.dumps(snap), encoding="utf-8")


def test_schema_has_every_table(tmp_path):
    with Ledger(tmp_path) as led:
        names = {r["name"] for r in led.query("SELECT name FROM sqlite_master WHERE type='table'")}
        assert set(TABLES) <= names
        assert led.path == tmp_path / "harness.db"
    Ledger(tmp_path).close()   # re-opening an existing db is fine


def test_rec_key_is_order_insensitive_and_distinguishes_subjects():
    a = rec_key("espn", "waiver", ["x", "y"], ["d"], None)
    assert a == rec_key("espn", "waiver", ["y", "x"], ["d"], "")
    assert a != rec_key("fantrax", "waiver", ["x", "y"], ["d"])
    assert rec_key("espn", "injury", [], [], subjects=["h1"]) != rec_key("espn", "injury", [], [], subjects=["h2"])
    assert rec_key("espn", "trade", ["x"], ["m"], "Opp A") != rec_key("espn", "trade", ["x"], ["m"], "Opp B")


def test_ingest_real_v1_recs_file(tmp_path):
    shutil.copy(V1_RECS, _archive(tmp_path) / V1_RECS.name)
    _v1_projections(tmp_path)
    with Ledger(tmp_path) as led:
        stats = ingest_archive(led, tmp_path)
        assert stats["files_ingested"] == 2 and stats["errors"] == []
        kinds = {r["kind"]: r["n"] for r in led.query("SELECT kind, COUNT(*) n FROM recs GROUP BY kind")}
        assert kinds == {"injury": 1, "lineup": 3, "waiver": 1, "trade": 3}
        rows = led.query("SELECT * FROM recs WHERE kind='trade'")
        assert all(r["archive_version"] == 1 and r["predicted_gain"] is None for r in rows)
        assert {r["counterparty"] for r in rows} == {"Opponent B", "Opponent D"}
        # the v1 IR target is recovered from the title and that day's projections
        subj = led.query("SELECT r.kind, p.cid FROM rec_players p JOIN recs r USING (league, as_of, rec_key)"
                         " WHERE p.side='subject' ORDER BY r.kind")
        assert subj == [{"kind": "injury", "cid": "espn:3852"}, {"kind": "waiver", "cid": "espn:3852"}]
        assert led.count("rec_episodes") == 8
        assert led.count("projections") == 1
        before = led.query("SELECT * FROM rec_episodes ORDER BY episode_id")
        again = ingest_archive(led, tmp_path)
        assert again["files_ingested"] == 0                    # unchanged files are skipped
        forced = ingest_archive(led, tmp_path, force=True)
        assert forced["files_ingested"] == 2
        after = led.query("SELECT * FROM rec_episodes ORDER BY episode_id")
        strip = lambda rows: [{k: v for k, v in r.items() if k != "updated_at"} for r in rows]  # noqa: E731
        assert strip(after) == strip(before) and led.count("recs") == 8


def _write(tmp_path, day, recs):
    (_archive(tmp_path) / f"recs-espn-{day}.json").write_text(json.dumps(
        {"version": 2, "provider": "espn", "as_of": day, "recommendations": recs}), encoding="utf-8")


def _rec(title="Add a, drop d", add=("espn:a",), drop=("espn:d",)):
    ref = lambda c: {"cid": c, "name": c}  # noqa: E731
    return {"kind": "waiver", "score": 1.0, "title": title, "add": [ref(c) for c in add],
            "drop": [ref(c) for c in drop], "subjects": [], "counterparty": None, "predicted_gain": 0.8,
            "gain_units": "season_fpg", "horizon_days": None, "strength": 5.6, "reasons": []}


def test_episode_dedup_across_three_days_and_gap(tmp_path):
    for day in ("2026-10-01", "2026-10-02", "2026-10-03", "2026-10-08"):
        _write(tmp_path, day, [_rec()])
    _write(tmp_path, "2026-10-02", [_rec(), _rec("Add b, drop d", add=("espn:b",))])
    with Ledger(tmp_path) as led:
        ingest_archive(led, tmp_path)
        eps = led.query("SELECT first_seen, last_seen, n_days, predicted_gain, strength, title FROM rec_episodes"
                        " ORDER BY title, first_seen")
        assert [(e["first_seen"], e["last_seen"], e["n_days"]) for e in eps] == [
            ("2026-10-01", "2026-10-03", 3), ("2026-10-08", "2026-10-08", 1), ("2026-10-02", "2026-10-02", 1)]
        assert eps[0]["predicted_gain"] == 0.8 and eps[0]["strength"] == 5.6
        # re-archiving a day that drops the rec splits the episode, and the stale one is removed
        _write(tmp_path, "2026-10-02", [_rec("Add b, drop d", add=("espn:b",))])
        ingest_archive(led, tmp_path)
        rows = led.query("SELECT first_seen, n_days FROM rec_episodes WHERE title='Add a, drop d' ORDER BY 1")
        assert [(r["first_seen"], r["n_days"]) for r in rows] == [("2026-10-01", 2), ("2026-10-08", 1)]


def test_group_days_gap_rule():
    d = lambda n: date(2026, 10, n)  # noqa: E731
    assert group_days([d(1), d(3), d(6), d(7)]) == [[d(1), d(3)], [d(6), d(7)]]


def test_pull_realized_no_games_is_a_clean_noop(tmp_path):
    calls = []

    def fetch(url, params):
        calls.append(url)
        return {"data": [], "total": 0}
    with Ledger(tmp_path) as led:
        assert pull_realized(led, NhlClient(fetch_json=fetch), date(2026, 9, 27)) == 0
        assert led.count("realized_daily") == 0
        assert led.query("SELECT game_date, n_players FROM realized_pulls") == [
            {"game_date": "2026-09-27", "n_players": 0}]
        assert calls and all("gameDate" not in u for u in calls)     # the date filter travels in params


def test_pull_realized_stores_raw_stats(tmp_path, monkeypatch):
    from fantasy_manager.backtest import data as D
    seen = {}

    def fake_window(client, season, start, end, faceoffs=False):
        seen.update(season=season, start=start, end=end, faceoffs=faceoffs)
        return {8478402: WindowLine(1, {"G": 2.0, "SOG": 5.0}), 99: WindowLine(0, {})}
    monkeypatch.setattr(D, "fetch_window", fake_window)
    with Ledger(tmp_path) as led:
        assert pull_realized(led, object(), date(2026, 10, 10)) == 1
        assert pull_realized(led, object(), date(2026, 10, 10)) == 1        # idempotent upsert
        row = led.query("SELECT * FROM realized_daily")[0]
        assert (row["nhl_id"], row["game_date"], row["gp"]) == (8478402, "2026-10-10", 1)
        assert json.loads(row["stats_json"]) == {"G": 2.0, "SOG": 5.0}
    assert seen == {"season": 20262027, "start": date(2026, 10, 10), "end": date(2026, 10, 10), "faceoffs": True}


def _ctx():
    p = Player(cid="fantrax:p", name="P", name_norm="p", ids={}, team="EDM", positions=["C"])
    q = Player(cid="fantrax:q", name="Q", name_norm="q", ids={}, team="EDM", positions=["C"])
    teams = [FantasyTeam(team_id="t1", name="me", owner_is_me=True,
                         slots=[RosterSlot(slot="C", player=p, starting=True)]),
             FantasyTeam(team_id="t2", name="them", owner_is_me=False,
                         slots=[RosterSlot(slot="BN", player=q, starting=False)])]
    return LeagueContext(provider="fantrax", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points"), roster_shape={"C": 1}, teams=teams, free_agents=[],
                         matchup_period=None, as_of=date(2026, 10, 12))


def test_pull_lineups_from_snapshot_provider(tmp_path):
    ctx = _ctx()

    class Snap:
        def lineup_snapshot(self, day):
            return [LineupDay(team_id=t.team_id, date=day, cid=s.player.cid, slot=s.slot, starting=s.starting)
                    for t in ctx.teams for s in t.slots]
    with Ledger(tmp_path) as led:
        assert pull_lineups(led, Snap(), "fantrax", date(2026, 10, 12), ctx) == 2
        rows = led.query("SELECT team_id, cid, starting, is_me FROM lineup_days ORDER BY team_id")
        assert rows == [{"team_id": "t1", "cid": "fantrax:p", "starting": 1, "is_me": 1},
                        {"team_id": "t2", "cid": "fantrax:q", "starting": 0, "is_me": 0}]


# --------------------------------------------------------------------------- CLI

@pytest.fixture
def cli_env(tmp_path, monkeypatch):
    monkeypatch.setattr(cli_harness, "_settings", lambda: SimpleNamespace(fm_data_dir=tmp_path, fm_offline=True))
    monkeypatch.setattr(cli_harness, "console", Console(width=200))
    return tmp_path


def test_cli_rebuild_status_and_ledger(cli_env):
    shutil.copy(V1_RECS, _archive(cli_env) / V1_RECS.name)
    runner = CliRunner()
    res = runner.invoke(cli_harness.harness_app, ["rebuild"])
    assert res.exit_code == 0, res.output
    assert "8 episodes" in res.output
    res = runner.invoke(cli_harness.harness_app, ["status"])
    assert res.exit_code == 0, res.output
    assert "Recommendation episodes" in res.output and "NHL results: 0 day(s) pulled" in res.output
    res = runner.invoke(cli_harness.harness_app, ["ledger", "--kind", "trade", "--json"])
    data = json.loads(res.output)
    assert len(data["episodes"]) == 3 and {e["status"] for e in data["episodes"]} <= {"open", "expired"}
    res = runner.invoke(cli_harness.harness_app, ["ledger"])
    assert res.exit_code == 0 and "Opponent B" in res.output


def test_cli_daily_is_idempotent_and_handles_no_games(cli_env, monkeypatch):
    from fantasy_manager.harness import realized as R
    ctx = _ctx()

    class Prov:
        warnings: list = []

        def activity(self, since=None):
            return []

        def lineup_snapshot(self, day):
            return [LineupDay(team_id="t1", date=day, cid="fantrax:p", slot="C", starting=True)]
    monkeypatch.setattr(cli_backtest, "_load_league_full", lambda lg, s, c, with_recs: (ctx, {}, [], [], Prov()))
    monkeypatch.setattr(R, "pull_realized", lambda ledger, client, day: 0)
    runner = CliRunner()
    for _ in range(2):
        res = runner.invoke(cli_harness.harness_app, ["daily", "--league", "fantrax"])
        assert res.exit_code == 0, res.output
        assert "no NHL regular-season games" in res.output
    with Ledger(cli_env) as led:
        assert led.count("lineup_days") == 1 and led.count("runs") == 2
        assert led.count("projections") == 2                  # both rostered players, archived once
        assert led.count("projections", "WHERE inputs_json IS NOT NULL AND fpg IS NULL") == 2
    assert list((cli_env / "archive").glob("recs-fantrax-*.json"))
