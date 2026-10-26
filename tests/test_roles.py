"""Role changes for established skaters and goalie job news (valuation.roles via valuate_league)."""
import json
from datetime import date, datetime, timedelta, timezone

import pytest

from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.providers import rookie_enrich
from fantasy_manager.providers.lines_enrich import LineSnapshotStore, usual_roles
from fantasy_manager.providers.news_roles import RoleSignal, classify_text
from fantasy_manager.scoring import PointsScoring
from fantasy_manager.valuation.roles import (DEPTH_PENALTY, ROLE_BOOST, ROLE_MINOR_BOOST, ROLE_SEASON_WEIGHT,
                                             SCRATCH_MULT, START_NEWS_WEIGHT)
from fantasy_manager.valuation.valuate import valuate_league

D = date(2026, 10, 20)
SKATER_W = {"G": 3.0, "A": 2.0, "SOG": 0.5, "HIT": 0.5}
GOALIE_W = {"W": 4.0, "SV": 0.2}


@pytest.fixture(autouse=True)
def _clean_registry():
    rookie_enrich.clear_registry()
    yield
    rookie_enrich.clear_registry()


def vet(cid, team="EDM", **kw):
    """An established forward: three full prior seasons (not unproven)."""
    stats = {"G": 20.0, "A": 30.0, "SOG": 200.0, "HIT": 50.0}
    lines = {s: StatLine(split=s, gp=80, stats=stats) for s in ("prior", "prior2", "prior3")}
    return Player(cid=cid, name=f"{cid.title()} Vet", name_norm=cid, ids={}, team=team, positions=["C", "F"],
                  career_gp=600, lines=lines, **kw)


def goalie(cid, gs, team="EDM"):
    stats = {"GS": gs, "W": gs * 0.5, "SV": gs * 25.0, "GP": max(gs, 1)}
    return Player(cid=cid, name=f"{cid.title()} Goalie", name_norm=cid, ids={}, team=team, positions=["G"],
                  lines={"prior": StatLine(split="prior", gp=max(int(gs), 1), stats=stats)})


def ctx_of(players, weights=SKATER_W):
    slots = [RosterSlot(slot="BN", player=p, starting=False) for p in players]
    return LeagueContext(provider="t", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights=weights),
                         roster_shape={"C": 2, "G": 2, "BN": 6},
                         teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True, slots=slots)],
                         free_agents=[], matchup_period=1, as_of=D)


def signal(kind, direction, days_ago=1, conf=0.8, quote="news"):
    return RoleSignal(player_name=None, kind=kind, direction=direction, confidence=conf, quote=quote,
                      published=datetime.combine(D - timedelta(days=days_ago), datetime.min.time(), timezone.utc),
                      source="rotowire")


def value(players, cid, weights=SKATER_W):
    return valuate_league(ctx_of(players, weights), PointsScoring(weights))[cid]


def codes(pv):
    return {r.code: r for r in pv.reasons}


def test_veteran_promoted_to_pp1_gets_the_change_this_week_and_half_for_the_season():
    base = value([vet("a")], "a")
    pv = value([vet("a", line="f2", pp_unit="pp1", usual_line="f2", usual_pp_unit="pp2")], "a")
    r = ROLE_BOOST / ROLE_MINOR_BOOST
    assert pv.fpg == pytest.approx(base.fpg * r, rel=0.02)     # offense only scales: HIT is untouched
    season_r = 1 + (r - 1) * ROLE_SEASON_WEIGHT
    assert pv.fpg_season / pv.fpg == pytest.approx(season_r / r, rel=1e-3)
    c = codes(pv)
    assert "ROLE_BOOST" in c and "usually F2, PP2, now F2, PP1" in c["ROLE_BOOST"].text
    assert "ROLE_DFO" in c


def test_veteran_already_on_pp1_gets_nothing():
    base = value([vet("a")], "a")
    pv = value([vet("a", line="f1", pp_unit="pp1", usual_line="f1", usual_pp_unit="pp1")], "a")
    assert pv.fpg == pytest.approx(base.fpg)
    assert not {"ROLE_BOOST", "ROLE_DEPTH"} & set(codes(pv))


def test_veteran_demoted_to_fourth_line_and_off_the_pp():
    teammate = vet("t", line="f1", pp_unit="pp1", usual_line="f1", usual_pp_unit="pp1")   # EDM's PP units listed
    base = value([vet("a"), teammate], "a")
    pv = value([vet("a", line="f4", pp_unit=None, usual_line="f2", usual_pp_unit="pp2"), teammate], "a")
    assert pv.fpg < base.fpg
    assert codes(pv)["ROLE_DEPTH"].value == pytest.approx(DEPTH_PENALTY / ROLE_MINOR_BOOST, rel=1e-3)
    # without PP units listed for his team, the missing PP unit is not read as a demotion off the PP
    alone = value([vet("a", line="f4", pp_unit=None, usual_line="f2", usual_pp_unit="pp2")], "a")
    assert alone.fpg == pytest.approx(base.fpg)


def test_without_snapshots_pp_share_is_the_usual_role():
    # 70% of the team's PP time = a usual PP1; Daily Faceoff now has him on PP1: no change
    p = vet("a", line="f1", pp_unit="pp1", pp_share=0.7)
    assert not {"ROLE_BOOST", "ROLE_DEPTH"} & set(codes(value([p], "a")))
    # 5% of PP time, now PP1: a promotion (line part unknown -> unchanged)
    q = vet("b", line="f1", pp_unit="pp1", pp_share=0.05)
    assert codes(value([q], "b"))["ROLE_BOOST"].value == pytest.approx(ROLE_BOOST, rel=1e-3)


def test_news_fills_in_when_daily_faceoff_does_not_list_him():
    p = vet("a", usual_line="f2", usual_pp_unit="pp2")
    rookie_enrich.register_signals("a", [signal("pp1", 1, quote="moved to the top power-play unit")])
    c = codes(value([p], "a"))
    assert "ROLE_NEWS" in c and "top power-play unit" in c["ROLE_NEWS"].text
    assert c["ROLE_BOOST"].value == pytest.approx(ROLE_BOOST / ROLE_MINOR_BOOST, rel=1e-3)


def test_stale_news_is_ignored():
    p = vet("a", usual_line="f2", usual_pp_unit="pp2")
    rookie_enrich.register_signals("a", [signal("pp1", 1, days_ago=15)])
    assert not {"ROLE_BOOST", "ROLE_NEWS"} & set(codes(value([p], "a")))


def test_scratched_veteran_loses_week_value_only():
    teammate = vet("t", line="f1", usual_line="f1")          # Daily Faceoff lists EDM's lineup
    base = value([vet("a"), teammate], "a")
    pv = value([vet("a", line_change="scratched"), teammate], "a")
    assert pv.fpg_season == pytest.approx(base.fpg_season)
    assert pv.fpg_week == pytest.approx(base.fpg_week * SCRATCH_MULT)
    assert "ROLE_SCRATCH" in codes(pv)


def test_rookies_keep_the_rookie_model_only():
    # unproven (no prior seasons), on the top line: the rookie model's x1.15, no role-change step
    p = Player(cid="r", name="Rook Ie", name_norm="rook", ids={}, team="EDM", positions=["C", "F"], career_gp=10,
               draft_overall=5, line="f1", usual_line="f3",
               lines={"projected": StatLine(split="projected", gp=70, stats={"G": 15.0, "A": 20.0, "SOG": 150.0})})
    c = codes(value([p], "r"))
    assert "ROOKIE_BLEND" in c or "ROLE_DFO" in c
    assert "Role change" not in (c.get("ROLE_BOOST").text if "ROLE_BOOST" in c else "")


def test_goalie_named_starter_raises_share_and_lost_job_lowers_it():
    base = value([goalie("g", 41)], "g", GOALIE_W)
    rookie_enrich.register_signals("g", [signal("goalie_role", 1, quote="has won the starting job")])
    up = value([goalie("g", 41)], "g", GOALIE_W)
    assert up.start_share > base.start_share
    expect = base.start_share + START_NEWS_WEIGHT * 0.8 * (0.70 - base.start_share)
    assert up.start_share == pytest.approx(expect, rel=1e-3)
    assert "START_NEWS" in codes(up)
    rookie_enrich.register_signals("g", [signal("goalie_role", -1, quote="lost the starting job")])
    assert value([goalie("g", 41)], "g", GOALIE_W).start_share < base.start_share


def test_goalie_news_never_overrides_ledger_starts():
    ctx = ctx_of([goalie("g", 41)], GOALIE_W)
    ctx.goalie_actual_starts = {"g": (8, 10)}
    rookie_enrich.register_signals("g", [signal("goalie_role", -1)])
    pv = valuate_league(ctx, PointsScoring(GOALIE_W))["g"]
    assert "START_ACTUAL" in codes(pv) and "START_NEWS" not in codes(pv)


@pytest.mark.parametrize("text,expected", [
    ("Vasilevskiy has won the starting job outright.", ("goalie_role", 1)),
    ("Jarry lost his net to Nedeljkovic last week.", ("goalie_role", -1)),
    ("The Bruins will split the starts between Swayman and Korpisalo.", ("goalie_split", 1)),
    ("Coach called it a 1A/1B tandem.", ("goalie_split", 1)),
])
def test_goalie_job_rules(text, expected):
    assert expected in {(f["kind"], f["direction"]) for f in classify_text(text)}


def test_usual_roles_from_snapshots(tmp_path):
    store = LineSnapshotStore(tmp_path)
    for i, (line, pp) in enumerate([("f2", "pp2"), ("f2", "pp2"), ("f1", "pp1")]):
        store.save(D - timedelta(days=3 - i), {"players": {"x": {"nhl_id": 7, "line": line, "pp_unit": pp}}})
    store.save(D, {"players": {"x": {"nhl_id": 7, "line": "f4", "pp_unit": None}}})   # today: not "usual"
    assert usual_roles(store, D) == {7: ("f2", "pp2")}
    assert json.loads(store.path(D).read_text())["players"]["x"]["line"] == "f4"
