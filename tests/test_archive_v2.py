import json
from datetime import date
from pathlib import Path

import pytest

from fantasy_manager.backtest import archive as A
from fantasy_manager.backtest import data as D
from fantasy_manager.models import (FantasyTeam, LeagueContext, Player, Reason, Recommendation, RosterSlot,
                                    ScoringConfig, StatLine)
from fantasy_manager.scoring import PointsScoring
from fantasy_manager.valuation import params as VP
from fantasy_manager.valuation.valuate import valuate_league

FIX = Path(__file__).parent / "fixtures" / "harness"


def _line(split, gp, g):
    return StatLine(split=split, gp=gp, stats={"G": g * gp, "A": 0.0, "GP": gp})


def _ctx(as_of=date(2026, 10, 20)):
    star = Player(cid="espn:1", name="Connor Test", name_norm="connor test", ids={"espn": "1", "nhl": "8478402"},
                  team="EDM", positions=["C"], pct_owned=99.0, birth_date=date(1997, 1, 13), status="dtd",
                  status_note="Upper body",
                  lines={"projected": _line("projected", 80, 0.6), "prior": _line("prior", 82, 0.55),
                         "prior2": _line("prior2", 70, 0.5), "season": _line("season", 6, 0.8),
                         "last7": _line("last7", 3, 1.0), "last15": _line("last15", 6, 0.8)})
    fa = Player(cid="espn:2", name="Free Agent", name_norm="free agent", ids={"espn": "2"}, team="SJS",
                positions=["D"], lines={"projected": _line("projected", 70, 0.1)})
    team = FantasyTeam(team_id="1", name="me", owner_is_me=True, slots=[RosterSlot(slot="C", player=star,
                                                                                   starting=True)])
    return LeagueContext(provider="espn", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights={"G": 2.0}), roster_shape={"C": 1, "D": 1},
                         teams=[team], free_agents=[fa], matchup_period=1, as_of=as_of)


def test_v2_projection_archive_round_trip(tmp_path):
    ctx = _ctx()
    values = valuate_league(ctx, PointsScoring({"G": 2.0}))
    path, status = A.archive_projections(ctx, tmp_path, values)
    assert status == "created"
    snap = A.load_snapshot(path)
    assert snap["version"] == A.ARCHIVE_VERSION == 2
    assert snap["params_hash"] == VP.params_hash() and len(snap["code_hash"]) == 40
    rec = {r["cid"]: r for r in snap["players"]}["espn:1"]
    inp = rec["inputs"]
    assert inp["season"] == {"gp": 6, "stats": {"G": pytest.approx(4.8), "GP": 6}}      # zero stats dropped
    assert inp["last7"]["gp"] == 3 and inp["last15"]["gp"] == 6 and "last30" not in inp
    assert inp["history"]["gp"] == 152 and inp["history"]["rates"]["G"] > 0
    assert inp["projection"]["gp"] == 80
    assert inp["status"] == "dtd" and inp["status_note"] == "Upper body"
    assert inp["birth_date"] == "1997-01-13" and inp["age"] == pytest.approx(29.7, abs=0.1)
    assert inp["pct_owned"] == 99.0 and inp["positions"] == ["C"]
    assert set(inp) >= {"games_next7", "offnight_next7", "start_share"}
    assert rec["fm"]["fpg"] == pytest.approx(values["espn:1"].fpg, abs=1e-4)
    assert rec["fm"]["fpg_season"] == pytest.approx(values["espn:1"].fpg_season, abs=1e-4)
    # same content -> unchanged (dedupe per day), different day -> a new file
    assert A.archive_projections(ctx, tmp_path, values)[1] == "unchanged"
    assert A.archive_projections(_ctx(date(2026, 10, 21)), tmp_path, values)[1] == "created"


def test_v2_recommendation_records_carry_gain_fields(tmp_path):
    ctx = _ctx()
    rec = Recommendation(kind="waiver", score=0.7, title="Add Free Agent", add=ctx.free_agents,
                         subjects=[ctx.teams[0].players[0]], predicted_gain=0.95, gain_units="season_fpg",
                         strength=6.2, reasons=[Reason(code="VORP_DELTA", text="x", value=0.95)])
    path, _ = A.archive_recommendations([rec], "espn", tmp_path, as_of=date(2026, 10, 20), league_id="1")
    snap = A.load_snapshot(path)
    r = snap["recommendations"][0]
    assert snap["version"] == 2 and snap["code_hash"] and snap["params_hash"]
    assert (r["predicted_gain"], r["gain_units"], r["horizon_days"], r["strength"]) == (0.95, "season_fpg", None, 6.2)
    assert r["subjects"][0]["cid"] == "espn:1"


def test_v1_files_still_load_and_score(tmp_path):
    v1 = A.load_snapshot(FIX / "recs-espn-2026-09-28.json")
    assert v1["version"] == 1 and v1["code_hash"] is None
    assert all(r["predicted_gain"] is None and r["subjects"] == [] for r in v1["recommendations"])
    # a v1 projections file (no inputs, fm with fpg + rates only) still scores
    (tmp_path / "archive").mkdir()
    snap = {"version": 1, "provider": "espn", "as_of": "2026-09-28", "scoring": {"kind": "points", "weights": {"G": 2.0}},
            "players": [{"cid": "espn:1", "name": "Connor Test", "nhl_id": 8478402, "projected":
                         {"split": "projected", "gp": 80, "stats": {"G": 40.0, "GP": 80}},
                         "fm": {"fpg": 1.0, "rates": {"G": 0.5}}}]}
    (tmp_path / "archive" / "projections-espn-2026-09-28.json").write_text(json.dumps(snap))
    loaded = A.load_snapshot(tmp_path / "archive" / "projections-espn-2026-09-28.json")
    assert loaded["players"][0]["inputs"] is None
    actuals = [D.PlayerSeason(8478402, 20262027, "Connor Test", "F", "C", 80, {"G": 40.0})]
    out = A.score_archive(actuals, tmp_path, providers=("espn",))
    assert out["espn"]["pools"]["skaters"]["fm"]["n"] == 1


def test_code_hash_tracks_model_sources(tmp_path):
    for d in A.CODE_HASH_DIRS:
        (tmp_path / d).mkdir()
        (tmp_path / d / "a.py").write_text("x = 1\n")
    h1 = A.code_hash(tmp_path)
    (tmp_path / "valuation" / "a.py").write_bytes(b"x = 1\r\n")   # line endings do not matter
    assert A.code_hash(tmp_path) == h1
    (tmp_path / "recommend" / "a.py").write_text("x = 2\n")
    assert A.code_hash(tmp_path) != h1
    assert A.code_hash() == A.code_hash()


def test_params_hash_is_stable_and_content_sensitive():
    assert VP.params_hash({"a": 1, "b": 2}) == VP.params_hash({"b": 2, "a": 1})
    assert VP.params_hash({"a": 1}) != VP.params_hash({"a": 2})
    assert len(VP.params_hash()) == 40
