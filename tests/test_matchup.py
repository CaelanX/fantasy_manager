"""H2H matchup preview (analysis/matchup): apportioning, Monte Carlo win probability, advice and
the provider score readers (ESPN / Fantrax) with fakes."""
from datetime import date
from types import SimpleNamespace

import pytest

from fantasy_manager.analysis import matchup as mu
from fantasy_manager.analysis.schedule_grid import Period, PeriodCalendar
from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.valuation.valuate import PlayerValue

from .test_schedule_grid import GPD, SCHEDULE

D = date
TODAY = D(2026, 10, 8)          # Thursday of the Oct 5-11 week


def _p(cid, team, pos=("C",), status="healthy"):
    return Player(cid=cid, name=cid.upper(), name_norm=cid, ids={}, team=team, positions=list(pos), status=status)


def _slots(pairs):
    return [RosterSlot(slot=s, player=p, starting=s not in ("BN", "IR")) for s, p in pairs]


def make_league(provider="espn"):
    mine = [("C", _p("m1", "AAA")), ("C", _p("m2", "BBB")), ("D", _p("m3", "CCC", ("D",))),
            ("G", _p("mg", "AAA", ("G",))), ("BN", _p("mb", "CCC"))]
    t1 = _p("t1", "AAA")
    t1.lines["last15"] = StatLine(split="last15", gp=5, stats={"G": 10.0})
    theirs = [("C", t1), ("C", _p("t2", "BBB")), ("D", _p("t3", "BBB", ("D",))), ("G", _p("tg", "BBB", ("G",))),
              ("IR", _p("tir", "AAA", status="ir"))]
    ctx = LeagueContext(
        provider=provider, league_id="1", season=2027, name="T",
        scoring=ScoringConfig(kind="points", weights={"G": 1.0}),
        roster_shape={"C": 2, "D": 1, "G": 1, "BN": 2, "IR": 1},
        teams=[FantasyTeam(team_id="1", name="Mine", owner_is_me=True, slots=_slots(mine)),
               FantasyTeam(team_id="2", name="Theirs", owner_is_me=False, slots=_slots(theirs))],
        free_agents=[_p("fa1", "AAA"), _p("fa2", "BBB", ("D",))], matchup_period=1, as_of=TODAY,
        schedule={t: list(ds) for t, ds in SCHEDULE.items()}, games_per_day=dict(GPD), season_start=D(2026, 10, 5))
    fpg = {"m1": 2.0, "m2": 1.5, "m3": 1.0, "mg": 3.0, "mb": 0.5, "t1": 1.0, "t2": 2.0, "t3": 1.2, "tg": 2.5,
           "tir": 4.0, "fa1": 1.8, "fa2": 1.1}
    values = {}
    for p in ctx.all_players():
        g7 = len([d for d in ctx.schedule.get(p.team, []) if TODAY <= d <= D(2026, 10, 14)])
        values[p.cid] = PlayerValue(player=p, fpg=fpg[p.cid], fpg_season=fpg[p.cid], fpg_week=fpg[p.cid], vorp=0.0,
                                    proj_week=fpg[p.cid] * g7, games_next7=g7, offnight_next7=0)
    return ctx, values


CAL = PeriodCalendar(periods=[Period(number=1, start=D(2026, 10, 5), end=D(2026, 10, 11)),
                              Period(number=2, start=D(2026, 10, 12), end=D(2026, 10, 18), playoffs=True)],
                     playoff_start=2, source="test")


def _scores(mine, theirs):
    return mu.ScoreInfo(period=1, start=D(2026, 10, 5), end=D(2026, 10, 11), opponent_id="2", my_points=mine,
                        their_points=theirs, source="fake")


def test_apportion_by_games():
    pv = SimpleNamespace(proj_week=8.0, games_next7=4, fpg=2.0)
    assert mu.apportion(pv, 2) == pytest.approx(4.0)
    assert mu.apportion(pv, 0) == 0.0 and mu.apportion(None, 3) == 0.0
    no_proj = SimpleNamespace(proj_week=None, games_next7=None, fpg=1.5, start_share=None, offnight_next7=None)
    assert mu.apportion(no_proj, 3) == pytest.approx(4.5)
    goalie = SimpleNamespace(proj_week=None, games_next7=None, fpg=3.0, start_share=0.6, offnight_next7=None)
    assert mu.apportion(goalie, 2) == pytest.approx(3.6)
    assert mu.game_sd(4.0, False) == pytest.approx(2.6) and mu.game_sd(4.0, True) == pytest.approx(5.0)


def test_projection_counts_remaining_games_only():
    ctx, values = make_league()
    m = mu.current_matchup(ctx, None, values, today=TODAY, scores=_scores(10.0, 12.0), calendar=CAL, seed=1)
    assert m.opponent_team == "Theirs" and m.period == 1 and m.days_left == 4 and m.from_day == TODAY
    mine = {x.cid: x for x in m.my_players}
    # Thu-Sun: AAA plays Thu + Sat, BBB Sat + Sun, CCC Thu + Fri
    assert mine["m1"].games_left == 2 and mine["m1"].proj_remaining == pytest.approx(
        values["m1"].proj_week * 2 / values["m1"].games_next7)
    assert mine["m3"].games_left == 2
    assert "mb" not in mine                          # bench player not in the optimal lineup (m1/m2 better)
    assert m.my_projected_remaining == pytest.approx(sum(x.proj_remaining for x in m.my_players))
    theirs = {x.cid for x in m.their_players}
    assert "tir" not in theirs                       # IR players never start
    assert m.my_points_so_far == 10.0 and m.my_projected_total == pytest.approx(10.0 + m.my_projected_remaining)
    assert set(m.gap_by_position) == {"F", "D", "G"}
    assert m.gap_by_position["G"].mine == pytest.approx(mine["mg"].proj_remaining)
    js = m.to_json()
    assert js["gap_by_position"]["F"]["gap"] == pytest.approx(m.gap_by_position["F"].gap)
    assert js["margin"] == pytest.approx(m.margin)


def test_win_probability_monotonic_in_points_banked():
    mine = [(10.0, 4.0), (6.0, 3.0)]
    theirs = [(12.0, 4.5), (5.0, 2.0)]
    probs = [mu.win_probability(x, 20.0, mine, theirs, draws=2000, seed=7) for x in range(0, 41, 4)]
    assert probs == sorted(probs)
    assert probs[0] < 0.1 and probs[-1] > 0.9
    # symmetric: about a coin flip
    assert mu.win_probability(0, 0, [(10.0, 3.0)], [(10.0, 3.0)], seed=3) == pytest.approx(0.5, abs=0.05)
    # no variance left: deterministic
    assert mu.win_probability(5, 4, [], []) == 1.0 and mu.win_probability(4, 5, [], []) == 0.0
    assert mu.win_probability(4, 4, [(0.0, 0.0)], []) == 0.5


def test_win_probability_monotonic_through_current_matchup():
    ctx, values = make_league()
    ps = [mu.current_matchup(ctx, None, values, today=TODAY, scores=_scores(x, 30.0), calendar=CAL, seed=11,
                             streaming=False).win_probability for x in (0.0, 20.0, 30.0, 40.0, 60.0)]
    assert ps == sorted(ps) and ps[0] < ps[-1]


def test_advice_chase_protect_even():
    ctx, values = make_league()
    behind = mu.current_matchup(ctx, None, values, today=TODAY, scores=_scores(0.0, 60.0), calendar=CAL, seed=1)
    assert behind.stance == "chase" and behind.advice[0].startswith("Chase")
    assert "stream" in behind.advice[0] and "FA1" in behind.advice[0]     # best add for the days left
    ahead = mu.current_matchup(ctx, None, values, today=TODAY, scores=_scores(60.0, 0.0), calendar=CAL, seed=1)
    assert ahead.stance == "protect" and ahead.advice[0].startswith("Protect")
    assert mu.stance_for(0.5) == "even" and mu.stance_for(None) is None
    assert any(a.startswith("Their unavailable players: TIR") for a in behind.advice)


def test_key_players_injured_hot_top():
    ctx, values = make_league()
    m = mu.current_matchup(ctx, None, values, today=TODAY, scores=_scores(0.0, 0.0), calendar=CAL, seed=1)
    tags = {k.cid: k.tag for k in m.key_players_theirs}
    assert tags["tir"] == "injured"
    assert tags["t1"] == "hot"                       # 2.0 G/GP over the last 15 vs 1.0 FPG model
    hot = next(k for k in m.key_players_theirs if k.cid == "t1")
    assert hot.recent_fpg == pytest.approx(2.0)
    assert "top" in tags.values()


def test_no_scores_is_projection_only():
    ctx, values = make_league()
    m = mu.current_matchup(ctx, None, values, today=TODAY, calendar=CAL)
    assert m.opponent_team is None and m.win_probability is None
    assert m.period == 1 and m.my_projected_remaining > 0
    assert any("no provider handle" in w for w in m.warnings)
    assert m.advice[0].startswith("No opponent found")


def test_lineups_locked():
    ctx, _ = make_league(provider="fantrax")
    assert mu.lineups_locked(ctx, SimpleNamespace(rules=None)) is True
    daily = SimpleNamespace(rules=SimpleNamespace(get=lambda label: "Daily"))
    weekly = SimpleNamespace(rules=SimpleNamespace(get=lambda label: "Weekly"))
    assert mu.lineups_locked(ctx, daily) is False and mu.lineups_locked(ctx, weekly) is True
    espn_ctx, _ = make_league()
    assert mu.lineups_locked(espn_ctx, None) is False


def test_locked_lineup_uses_current_starters_and_no_streaming():
    ctx, values = make_league(provider="fantrax")
    weekly = SimpleNamespace(rules=SimpleNamespace(get=lambda label: "Weekly"))
    m = mu.current_matchup(ctx, weekly, values, today=TODAY, scores=_scores(0.0, 60.0), calendar=CAL, seed=1)
    assert m.lineup_basis.startswith("current lineup")
    assert {x.cid for x in m.my_players} == {"m1", "m2", "m3", "mg"}
    assert "Your lineup is set" in m.advice[0] and "FA1" not in m.advice[0]
    assert m.advice[-1].startswith("Lineups lock")


FX_SCHEDULE = {"tableList": [
    {"caption": "Scoring Period 1", "subCaption": "(Mon Oct 5, 2026 - Sun Oct 11, 2026)", "rows": [
        {"cells": [{"content": "Theirs", "teamId": "2"}, {"content": "41.5"},
                   {"content": "Mine", "teamId": "1"}, {"content": "1,012.25"}]},
        {"cells": [{"content": "X", "teamId": "3"}, {"content": "0"}, {"content": "Y", "teamId": "4"},
                   {"content": "0"}]}]},
    {"caption": "Scoring Period 2", "subCaption": "(Mon Oct 12, 2026 - Sun Oct 18, 2026)", "rows": [
        {"cells": [{"content": "Mine", "teamId": "1"}, {"content": "0"}, {"content": "X", "teamId": "3"},
                   {"content": "0"}]}]},
]}


def test_parse_fantrax_schedule():
    tables = mu.parse_fantrax_schedule(FX_SCHEDULE)
    assert [t["number"] for t in tables] == [1, 2]
    assert tables[0]["start"] == D(2026, 10, 5) and tables[0]["end"] == D(2026, 10, 11)
    assert tables[0]["games"][0] == ("2", 41.5, "1", 1012.25)


def test_fantrax_scores_with_fake_client():
    ctx, values = make_league(provider="fantrax")
    client = SimpleNamespace(call=lambda *calls: [FX_SCHEDULE])
    prov = SimpleNamespace(_get_client=lambda: client, rules=SimpleNamespace(get=lambda label: "Daily"))
    s = mu.fantrax_scores(ctx, prov, CAL, TODAY)
    assert (s.period, s.opponent_id, s.my_points, s.their_points) == (1, "2", 1012.25, 41.5)
    m = mu.current_matchup(ctx, prov, values, today=TODAY, calendar=CAL, seed=2)
    assert m.opponent_team == "Theirs" and m.source == "Fantrax schedule" and m.lineup_basis == "optimal week lineup"
    assert m.win_probability == pytest.approx(1.0)


class FakeEspnRequest:
    def league_get(self, params=None, **kw):
        assert params == {"view": "mMatchupScore"}
        return {"schedule": [
            {"matchupPeriodId": 1, "playoffTierType": "NONE",
             "home": {"teamId": 2, "totalPoints": 20.0, "totalPointsLive": 22.5},
             "away": {"teamId": 1, "totalPoints": 30.0, "totalPointsLive": None}},
            {"matchupPeriodId": 2, "home": {"teamId": 1}, "away": {"teamId": 3}}]}


def test_espn_scores_with_fake_league():
    ctx, values = make_league()
    league = SimpleNamespace(currentMatchupPeriod=1, espn_request=FakeEspnRequest())
    prov = SimpleNamespace(_league_cached=lambda: league)
    s = mu.espn_scores(ctx, prov, CAL)
    assert (s.period, s.opponent_id, s.my_points, s.their_points) == (1, "2", 30.0, 22.5)
    assert (s.start, s.end) == (D(2026, 10, 5), D(2026, 10, 11))
    m = mu.current_matchup(ctx, prov, values, today=TODAY, calendar=CAL, seed=3)
    assert m.opponent_team == "Theirs" and m.my_points_so_far == 30.0 and m.source == "ESPN scoreboard"


def test_provider_failure_becomes_warning():
    ctx, values = make_league()

    def boom():
        raise RuntimeError("espn down")

    m = mu.current_matchup(ctx, SimpleNamespace(_league_cached=boom), values, today=TODAY, calendar=CAL)
    assert m.opponent_team is None and any("espn down" in w for w in m.warnings)
