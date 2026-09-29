from datetime import date, timedelta

import pytest

from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.providers.nhl import games_per_day
from fantasy_manager.scoring import PointsScoring
from fantasy_manager.valuation.schedule import (games_in_next, off_night_games, proj_week, schedule_factor,
                                                start_share, week_window)
from fantasy_manager.valuation.valuate import valuate_league

D = date(2026, 10, 7)


def days(*offsets, start=D):
    return [start + timedelta(days=o) for o in offsets]


def test_games_in_next_window_is_half_open():
    dates = days(0, 2, 6, 7)
    assert games_in_next(dates, D) == 3                  # day 7 is outside [D, D+7)
    assert games_in_next(dates, D, days=8) == 4
    assert games_in_next(dates, D - timedelta(days=10)) == 0


def test_off_night_games():
    per_day = {D: 3, D + timedelta(days=2): 12, D + timedelta(days=6): 7}
    assert off_night_games(days(0, 2, 6), per_day, D) == 2
    assert off_night_games(days(0, 2, 6), per_day, D, threshold=4) == 1


def test_proj_week_formula():
    assert proj_week(2.0, 1.0, 4, 0) == pytest.approx(8.0)
    assert proj_week(2.0, 0.75, 4, 2) == pytest.approx(2.0 * 0.75 * 4 * 1.10)


def goalie(gs, gp):
    return Player(cid="g", name="g", name_norm="g", ids={}, team="EDM", positions=["G"],
                  lines={"season": StatLine(split="season", gp=gp, stats={"GS": gs, "GP": gp})})


def test_start_share_shrinks_toward_half():
    assert start_share(goalie(0, 0)) == pytest.approx(0.5)
    assert start_share(goalie(10, 10)) == pytest.approx((10 + 5) / 20)
    assert start_share(goalie(60, 60)) == pytest.approx(65 / 70)
    skater = Player(cid="s", name="s", name_norm="s", ids={}, team="EDM", positions=["C"])
    assert start_share(skater) == 1.0


def test_week_window_preseason_uses_season_start():
    sched = {"EDM": days(0, 2, 4), "CGY": days(1, 2)}
    gpd = games_per_day(sched)
    w = week_window(date(2026, 9, 20), D, sched, gpd)
    assert w.preseason and w.start == D
    assert w.avg_team_games == pytest.approx(2.5)
    # in season: window starts at as_of
    w2 = week_window(D + timedelta(days=1), D, sched, gpd)
    assert not w2.preseason and w2.start == D + timedelta(days=1)
    # the day before opening night already has games in its window -> no fallback
    w3 = week_window(D - timedelta(days=1), D, sched, gpd)
    assert not w3.preseason and w3.start == D - timedelta(days=1)
    assert week_window(D, D, {}, {}) is None


def test_schedule_factor_unknown_team():
    sched = {"EDM": days(0)}
    w = week_window(D, D, sched, games_per_day(sched))
    p = Player(cid="x", name="x", name_norm="x", ids={}, team=None, positions=["C"])
    assert schedule_factor(p, sched, {}, w) is None
    p.team = "EDM"
    assert schedule_factor(p, sched, {}, w).games == 1


def mk(cid, team, pos, g_pg, status="healthy", gs=None):
    stats = {"G": g_pg * 80, "GP": 80}
    if gs is not None:
        stats["GS"] = gs
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team=team, positions=pos, status=status,
                  lines={"prior": StatLine(split="prior", gp=80, stats=stats)})


def ctx_with_schedule(as_of, players):
    sched = {"EDM": days(0, 1, 3, 5), "CGY": days(0, 4), "TOR": days(1, 3)}
    slots = [RosterSlot(slot="BN", player=p, starting=False) for p in players]
    return LeagueContext(provider="t", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights={"G": 1.0}),
                         roster_shape={"C": 1, "G": 1, "BN": 3},
                         teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True, slots=slots)],
                         free_agents=[], matchup_period=1, as_of=as_of, schedule=sched,
                         games_per_day=games_per_day(sched), season_start=D)


def test_valuation_uses_schedule_and_preseason_reason():
    edm, cgy = mk("edm", "EDM", ["C"], 1.0), mk("cgy", "CGY", ["C"], 1.0)
    ctx = ctx_with_schedule(date(2026, 9, 28), [edm, cgy])
    vals = valuate_league(ctx, PointsScoring({"G": 1.0}))
    fpg = vals["edm"].fpg
    # every game day is on an off-night (< 8 league games)
    assert vals["edm"].games_next7 == 4 and vals["edm"].offnight_next7 == 4
    assert vals["edm"].proj_week == pytest.approx(fpg * 4 * 1.2)
    assert vals["cgy"].proj_week == pytest.approx(fpg * 2 * 1.1)
    avg = (4 + 2 + 2) / 3
    assert vals["edm"].fpg_week == pytest.approx(vals["edm"].proj_week / avg)
    assert vals["edm"].fpg_week > vals["cgy"].fpg_week
    codes = {r.code: r for r in vals["edm"].reasons}
    assert "preseason" in codes["GAMES_NEXT7"].text and "PROJ_WEEK" in codes
    assert vals["edm"].horizon_values["proj_week"] == pytest.approx(vals["edm"].proj_week)


def test_valuation_goalie_start_share_and_injury():
    g = mk("g", "EDM", ["G"], 0.5, gs=40)
    hurt = mk("hurt", "EDM", ["C"], 1.0, status="out")
    ctx = ctx_with_schedule(D, [g, hurt])
    vals = valuate_league(ctx, PointsScoring({"G": 1.0}))
    share = (40 + 5) / (82 + 10)   # GS over a full 82-game team season
    assert vals["g"].start_share == pytest.approx(share)
    assert vals["g"].proj_week == pytest.approx(vals["g"].fpg * 4 * 1.2 * share)
    assert vals["hurt"].proj_week == 0.0 and vals["hurt"].fpg_week == 0.0
    assert "preseason" not in {r.code: r for r in vals["g"].reasons}["GAMES_NEXT7"].text


def test_no_schedule_keeps_old_semantics():
    p = mk("p", "EDM", ["C"], 1.0, status="dtd")
    ctx = ctx_with_schedule(D, [p]).model_copy(update={"schedule": {}, "games_per_day": {}})
    v = valuate_league(ctx, PointsScoring({"G": 1.0}))["p"]
    assert v.proj_week is None and v.fpg_week == pytest.approx(v.fpg * 0.75)
    assert v.lineup_value("week") == pytest.approx(v.fpg_week)
