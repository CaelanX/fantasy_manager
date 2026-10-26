"""Goalie start planner (analysis.goalie_plan: fm goalies --week, the matchup page)."""
from datetime import date, timedelta

import pytest

from fantasy_manager.analysis.goalie_plan import (BACKUP_REPEAT_P, RESTED_STARTER_P, goalie_week, start_points,
                                                  team_gf_factors)
from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.providers.nhl import games_per_day
from fantasy_manager.scoring import PointsScoring
from fantasy_manager.valuation.schedule import B2B_SECOND_NIGHT_P, night_start_probs
from fantasy_manager.valuation.valuate import PlayerValue

MON = date(2026, 10, 26)
WEIGHTS = {"W": 2.0, "GA": -1.5, "SV": 0.2, "SO": 2.0}
RATES = {"W": 0.5, "L": 0.4, "OTL": 0.1, "GA": 2.8, "SV": 25.0, "SA": 27.8, "SO": 0.05, "GS": 1.0}


def goalie(cid: str, team: str) -> Player:
    return Player(cid=cid, name=f"{cid.title()} Goalie", name_norm=cid, ids={}, team=team, positions=["G"])


def skater(cid: str, team: str, goals: float) -> Player:
    return Player(cid=cid, name=f"{cid.title()} Skater", name_norm=cid, ids={}, team=team, positions=["C"],
                  lines={"season": StatLine(split="season", gp=10, stats={"G": goals})})


def pv(p: Player, share: float, fpg: float | None = None) -> PlayerValue:
    f = PointsScoring(WEIGHTS).value(RATES) if fpg is None else fpg
    return PlayerValue(player=p, fpg=f, fpg_season=f * share, fpg_week=f * share, vorp=0.0, vorp_week=0.0,
                       start_share=share, rates=dict(RATES))


def make_ctx(mine, fas=(), schedule=None, bench=(), opponents=None, moves=(None, 0)):
    sched = schedule or {"EDM": [MON + timedelta(days=k) for k in (1, 2, 4)],     # Tue, Wed (B2B), Fri
                         "CGY": [MON + timedelta(days=k) for k in (1, 3, 5)],
                         "VAN": [MON + timedelta(days=k) for k in range(7)]}
    slots = [RosterSlot(slot="G", player=p, starting=True) for p in mine]
    slots += [RosterSlot(slot="BN", player=p, starting=False) for p in bench]
    ctx = LeagueContext(provider="t", league_id="1", season=2027, name="T",
                        scoring=ScoringConfig(kind="points", weights=WEIGHTS),
                        roster_shape={"G": 2, "BN": 1},
                        teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True, slots=slots)],
                        free_agents=list(fas), matchup_period=1, as_of=MON, schedule=sched,
                        games_per_day=games_per_day(sched), season_start=date(2026, 10, 7), lineup_lock="daily",
                        period_start=MON, period_end=MON + timedelta(days=6),
                        opponents=opponents or {})
    ctx.moves_limit_per_period, ctx.moves_used_this_period = moves
    return ctx


def no_odds(_day):
    return {}


def by_day(row):
    return {g.day: g for g in row.games}


def test_back_to_back_second_night_goes_to_the_backup():
    starter, backup = goalie("one", "EDM"), goalie("two", "EDM")
    ctx = make_ctx([starter, backup])
    gw = goalie_week(ctx, {"one": pv(starter, 0.7), "two": pv(backup, 0.3)}, odds=no_odds, streams=False)
    one, two = by_day(gw.row("one")), by_day(gw.row("two"))
    tue, wed = MON + timedelta(days=1), MON + timedelta(days=2)
    p1, p2 = night_start_probs(0.7)
    assert one[tue].start_prob == pytest.approx(p1, abs=1e-3) and one[wed].start_prob == pytest.approx(p2, abs=1e-3)
    assert one[wed].second_night and one[wed].start_source == "B2B 2nd night"
    assert two[wed].start_prob == pytest.approx(1 - B2B_SECOND_NIGHT_P, abs=1e-3)
    assert two[wed].start_prob > one[wed].start_prob
    # the plan keeps both in the two G slots, ordered by expected points
    wed_plan = next(d for d in gw.plan if d.day == wed)
    assert wed_plan.start == ["two", "one"]


def test_confirmed_start_overrides_the_share_and_conditions_tomorrow():
    sched = {"EDM": [MON, MON + timedelta(days=1)]}                 # Mon-Tue back-to-back
    starter, backup = goalie("one", "EDM"), goalie("two", "EDM")
    starter.confirmed_start, starter.start_source = False, "Daily Faceoff: Confirmed"
    backup.confirmed_start, backup.start_source = True, "Daily Faceoff: Confirmed"
    ctx = make_ctx([starter, backup], schedule=sched)
    gw = goalie_week(ctx, {"one": pv(starter, 0.7), "two": pv(backup, 0.3)}, odds=no_odds, streams=False)
    one, two = by_day(gw.row("one")), by_day(gw.row("two"))
    tue = MON + timedelta(days=1)
    assert one[MON].start_prob == 0.0 and one[MON].start_source == "confirmed"
    assert two[MON].start_prob == 1.0
    # the #1 rested tonight -> he likely starts tomorrow; the backup who started repeats rarely
    assert one[tue].start_prob == pytest.approx(RESTED_STARTER_P)
    assert two[tue].start_prob == pytest.approx(BACKUP_REPEAT_P)


def test_odds_drive_win_and_goals_against():
    g = goalie("one", "EDM")
    ctx = make_ctx([g], opponents={"EDM": {MON + timedelta(days=1): "@BOS"}})
    tue = MON + timedelta(days=1)
    odds = {"EDM": {"win_prob": 0.65, "opp_implied_total": 2.4, "implied_total": 3.6},
            "BOS": {"win_prob": 0.35, "opp_implied_total": 3.6, "implied_total": 2.4}}
    gw = goalie_week(ctx, {"one": pv(g, 0.7)}, odds=lambda d: odds if d == tue else {}, streams=False)
    game = by_day(gw.row("one"))[tue]
    assert game.matchup_source == "odds" and game.win_prob == 0.65 and game.opp_goals == 2.4
    assert game.opponent == "@BOS"
    neutral = PointsScoring(WEIGHTS).value(RATES)
    assert game.pts_if_starts > neutral                         # favourite vs a weak attack
    expected = start_points(PointsScoring(WEIGHTS), pv(g, 0.7), 0.65, 2.4 / 3.0)
    assert game.pts_if_starts == pytest.approx(expected, abs=0.01)


def test_without_odds_falls_back_to_opponent_goals_per_game_then_neutral():
    g = goalie("one", "EDM")
    sched = {"EDM": [MON - timedelta(days=k) for k in range(1, 6)] + [MON + timedelta(days=1)],
             "BOS": [MON - timedelta(days=k) for k in range(1, 6)],
             "CGY": [MON - timedelta(days=k) for k in range(1, 6)]}
    shooters = [skater("b1", "BOS", 30.0), skater("c1", "CGY", 10.0), skater("e1", "EDM", 20.0)]
    ctx = make_ctx([g], fas=shooters, schedule=sched, opponents={"EDM": {MON + timedelta(days=1): "BOS"}})
    f = team_gf_factors(ctx)
    assert f["BOS"][0] > 1.0 > f["CGY"][0]
    gw = goalie_week(ctx, {"one": pv(g, 0.7)}, odds=no_odds, streams=False)
    game = gw.row("one").games[0]
    assert game.matchup_source == "goals per game" and game.win_prob is None
    assert game.pts_if_starts < PointsScoring(WEIGHTS).value(RATES)          # BOS scores the most
    assert any("goals per game" in n for n in gw.notes)
    # no history at all: neutral, his own per-start value
    ctx2 = make_ctx([g])
    game2 = goalie_week(ctx2, {"one": pv(g, 0.7)}, odds=no_odds, streams=False).row("one").games[0]
    assert game2.matchup_source == "neutral"
    assert game2.pts_if_starts == pytest.approx(PointsScoring(WEIGHTS).value(RATES), abs=0.01)


def test_open_goalie_nights_and_stream_suggestion_respect_the_move_budget():
    mine1, mine2, weak = goalie("one", "EDM"), goalie("two", "EDM"), goalie("weak", "CGY")
    fa = goalie("fa", "VAN")
    vals = {"one": pv(mine1, 0.6), "two": pv(mine2, 0.4), "weak": pv(weak, 0.2, fpg=0.5), "fa": pv(fa, 0.9)}
    ctx = make_ctx([mine1, mine2], fas=[fa], bench=[weak])
    gw = goalie_week(ctx, vals, odds=no_odds)
    assert any(d.open_slots for d in gw.plan)                    # VAN plays nights EDM doesn't
    assert gw.free_agents and gw.free_agents[0].cid == "fa"
    assert gw.streams and gw.streams[0].add[0].cid == "fa"
    assert gw.streams[0].rank_in_kind == 1 and gw.streams[0].kind_total == len(gw.streams)
    spent = make_ctx([mine1, mine2], fas=[fa], bench=[weak], moves=(2, 2))
    assert goalie_week(spent, vals, odds=no_odds).streams == []


def test_out_goalie_has_no_games_and_categories_keep_fpg():
    g = goalie("one", "EDM")
    g.status = "out"
    ctx = make_ctx([g])
    gw = goalie_week(ctx, {"one": pv(g, 0.7)}, odds=no_odds, streams=False)
    assert gw.row("one").games == [] and all(d.start == [] for d in gw.plan)
    assert start_points(None, pv(g, 0.7, fpg=3.0), 0.9, 0.5) == 3.0


def test_matchup_page_goalie_section(monkeypatch):
    from types import SimpleNamespace

    from fantasy_manager.analysis import goalie_plan
    from fantasy_manager.web.views import goalie_section

    monkeypatch.setattr(goalie_plan, "_odds_fn", lambda data_dir: no_odds)
    starter, backup = goalie("one", "EDM"), goalie("two", "EDM")
    res = SimpleNamespace(ctx=make_ctx([starter, backup]), values={"one": pv(starter, 0.7), "two": pv(backup, 0.3)},
                          memo={})
    sec = goalie_section(res)
    assert sec["g_slots"] == 2 and len(sec["days"]) == 7
    tue = next(d for d in sec["days"] if d["day"] == MON + timedelta(days=1))
    assert [g["name"] for g in tue["start"]] == ["One Goalie", "Two Goalie"] and tue["open"] == 0
    assert sec["total"] > 0
