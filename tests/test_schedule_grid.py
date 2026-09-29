"""Season schedule grid, week matrix, streaming planner and league calendars (analysis/schedule_grid)."""
from datetime import date, timedelta
from types import SimpleNamespace

import pytest

from fantasy_manager.analysis import schedule_grid as sg
from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig

D = date


def _p(cid, team, pos=("C",), status="healthy"):
    return Player(cid=cid, name=cid.upper(), name_norm=cid, ids={}, team=team, positions=list(pos), status=status)


# Week 1 = Mon 2026-10-05 .. Sun 10-11; the season ends Mon 10-12.
SCHEDULE = {
    "AAA": [D(2026, 10, 5), D(2026, 10, 6), D(2026, 10, 8), D(2026, 10, 10), D(2026, 10, 12)],
    "BBB": [D(2026, 10, 6), D(2026, 10, 10), D(2026, 10, 11), D(2026, 10, 12)],
    "CCC": [D(2026, 10, 7), D(2026, 10, 8), D(2026, 10, 9)],
}
GPD = {D(2026, 10, 5): 3, D(2026, 10, 6): 12, D(2026, 10, 7): 9, D(2026, 10, 8): 4, D(2026, 10, 9): 10,
       D(2026, 10, 10): 13, D(2026, 10, 11): 5, D(2026, 10, 12): 5}


def make_ctx(provider="test", free_agents=None, as_of=D(2026, 10, 5)):
    opps = {t: {d: ("@" if i % 2 else "") + "XXX" for i, d in enumerate(ds)} for t, ds in SCHEDULE.items()}
    return LeagueContext(
        provider=provider, league_id="1", season=2027, name="T",
        scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape={"C": 1},
        teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True,
                           slots=[RosterSlot(slot="C", player=_p("mine", "AAA"), starting=True)])],
        free_agents=free_agents or [], matchup_period=1, as_of=as_of,
        schedule={t: list(ds) for t, ds in SCHEDULE.items()}, games_per_day=dict(GPD), opponents=opps,
        season_start=D(2026, 10, 5))


def test_window_counts_games_offnights_b2b():
    g, off, b2b = sg.window_counts(SCHEDULE["AAA"], GPD, D(2026, 10, 5), D(2026, 10, 11))
    assert (g, off, b2b) == (4, 2, 1)          # Mon (3 games) and Thu (4) are off-nights; Mon-Tue b2b
    assert sg.window_counts(SCHEDULE["CCC"], GPD, D(2026, 10, 5), D(2026, 10, 11)) == (3, 1, 2)
    # a back-to-back spanning two weeks counts in the week of its second game
    assert sg.window_counts(SCHEDULE["BBB"], GPD, D(2026, 10, 12), D(2026, 10, 18)) == (1, 1, 1)


def test_season_grid_weeks_and_totals():
    grid = sg.season_grid(make_ctx())
    assert grid.weeks == [D(2026, 10, 5), D(2026, 10, 12)]
    assert grid.week_ends == [D(2026, 10, 11), D(2026, 10, 12)]
    a, b, c = (grid.team(t) for t in ("AAA", "BBB", "CCC"))
    assert (a.games, a.offnights, a.b2b) == ([4, 1], [2, 1], [1, 0])
    assert (b.games, b.offnights, b.b2b) == ([3, 1], [1, 1], [1, 1])
    assert (c.games, c.offnights, c.b2b) == ([3, 0], [1, 0], [2, 0])
    assert a.total_games == 5 and c.total_b2b == 2
    assert grid.league_games == [56, 5]
    assert grid.offnight_days == [3, 1]       # Oct 5, 8, 11 | Oct 12
    assert grid.week_index(D(2026, 10, 9)) == 0 and grid.week_index(D(2027, 1, 1)) is None
    assert sg.team_week_counts(make_ctx()).model_dump() == grid.model_dump()
    # a sub-range clips the weeks
    part = sg.season_grid(make_ctx(), D(2026, 10, 8), D(2026, 10, 9))
    assert part.team("CCC").games == [2] and part.team("AAA").games == [1]


def test_empty_schedule_grid():
    ctx = make_ctx()
    ctx.schedule, ctx.games_per_day = {}, {}
    assert sg.season_grid(ctx).teams == []


def test_week_rows_matrix():
    view = sg.week_rows(make_ctx(), D(2026, 10, 8))        # any day -> that week's Monday
    assert view.start == D(2026, 10, 5) and view.end == D(2026, 10, 11) and len(view.days) == 7
    assert view.off_days == [True, False, False, True, False, False, True]
    assert view.league_games[0] == 3
    # most games first; ties by off-nights, then fewer back-to-backs
    assert [r.team for r in view.rows] == ["AAA", "BBB", "CCC"]
    aaa = view.rows[0]
    assert aaa.games == 4 and aaa.offnights == 2 and aaa.b2b == 1
    assert aaa.cells[0].opp == "XXX" and aaa.cells[0].off and not aaa.cells[0].b2b
    assert aaa.cells[1].b2b and aaa.cells[2] is None
    assert sg.default_week(make_ctx(as_of=D(2026, 10, 9))) == D(2026, 10, 5)
    pre = make_ctx(as_of=D(2026, 9, 20))                       # preseason -> opening week
    assert sg.default_week(pre) == D(2026, 10, 5)


def _pv(fpg, share=None):
    return SimpleNamespace(fpg=fpg, proj_week=None, games_next7=None, offnight_next7=None, start_share=share)


def test_streaming_targets_rank_by_games_and_offnights():
    fas = [_p("fa1", "AAA"), _p("fa2", "BBB", ("LW",)), _p("fa3", "CCC", ("D",)), _p("fa4", "AAA", ("G",)),
           _p("hurt", "AAA", status="out"), _p("nogames", "ZZZ")]
    values = {"fa1": _pv(2.0), "fa2": _pv(2.5), "fa3": _pv(1.0), "fa4": _pv(4.0, share=0.5),
              "hurt": _pv(9.0), "nogames": _pv(9.0)}
    ctx = make_ctx(free_agents=fas)
    bonus = sg.offnight_bonus()
    plan = sg.streaming_targets(ctx, values, D(2026, 10, 5), today=D(2026, 10, 5))
    f = plan.by_slot["F"]
    assert [t.cid for t in f] == ["fa1", "fa2"]              # injured / unscheduled skipped
    assert f[0].games == 4 and f[0].offnights == 2
    assert f[0].proj == pytest.approx(2.0 * 4 * (1 + bonus * 2))
    assert f[1].proj == pytest.approx(2.5 * 3 * (1 + bonus * 1))
    assert plan.by_slot["D"][0].cid == "fa3"
    g = plan.by_slot["G"][0]
    assert g.per_game == pytest.approx(2.0) and g.proj == pytest.approx(2.0 * 4 * (1 + bonus * 2))
    assert [r.team for r in plan.teams] == ["AAA", "BBB", "CCC"]
    assert plan.teams[0].free_agents == 2                      # healthy FAs on AAA: fa1, fa4 (not hurt)
    # later in the week only the remaining games count: BBB (Sat + Sun off-night) beats AAA (Sat)
    late = sg.streaming_targets(ctx, values, D(2026, 10, 5), today=D(2026, 10, 9))
    assert late.from_day == D(2026, 10, 9)
    assert [t.cid for t in late.by_slot["F"]] == ["fa2", "fa1"]
    assert late.by_slot["F"][0].games == 2
    assert [r.team for r in late.teams][0] == "BBB"
    assert sg.streaming_targets(ctx, values, D(2026, 10, 5), slots=("F",), limit=1).by_slot == {
        "F": [plan.by_slot["F"][0]]}


def test_per_game_value_from_week_projection():
    pv = SimpleNamespace(fpg=3.0, proj_week=12.0 * (1 + sg.offnight_bonus()), games_next7=4, offnight_next7=1)
    assert sg.per_game_value(pv) == pytest.approx(3.0)
    assert sg.per_game_value(None) is None


def test_parse_fantrax_periods():
    assert sg.parse_fantrax_period("(Mar 22/27 - Mar 28/27)") == (D(2027, 3, 22), D(2027, 3, 28))
    assert sg.parse_fantrax_period("Full Season") is None
    ps = sg.fantrax_periods([{"name": "Full Season", "value": 9999}, {"name": "(Oct 12/26 - Oct 12/26)", "value": 2},
                             {"name": "(Oct 5/26 - Oct 11/26)", "value": 1}])
    assert [(p.number, p.start, p.end) for p in ps] == [(1, D(2026, 10, 5), D(2026, 10, 11)),
                                                        (2, D(2026, 10, 12), D(2026, 10, 12))]


class FakeRules:
    def __init__(self, start):
        self.start = start

    def get_int(self, label):
        return self.start if label == sg.FANTRAX_PLAYOFF_LABEL else None

    def get(self, label):
        return None


class FakeFxClient:
    def call(self, *calls):
        assert calls[0][0] == "getTeamRosterInfo"
        return [{"displayedLists": {"scoringPeriodList": [
            {"name": "Full Season", "value": 9999}, {"name": "(Oct 5/26 - Oct 11/26)", "value": 1},
            {"name": "(Oct 12/26 - Oct 12/26)", "value": 2}]}}]


class FakeFantrax:
    def __init__(self, start=2):
        self.rules = FakeRules(start)

    def _get_client(self):
        return FakeFxClient()


def test_playoff_weeks_fantrax_rules():
    ctx = make_ctx(provider="fantrax")
    po = sg.playoff_weeks(ctx, FakeFantrax(start=2))
    assert po.playoff_start == 2 and [p.number for p in po.periods] == [2]
    assert po.source == "Fantrax scoring periods"
    assert [(t.team, t.total) for t in po.teams] == [("AAA", 1), ("BBB", 1), ("CCC", 0)]
    assert po.teams[1].total_b2b == 1 and po.avg_total == pytest.approx(2 / 3)
    cal = sg.league_calendar(ctx, FakeFantrax(start=1))
    assert cal.current(D(2026, 10, 12)).number == 2 and cal.current(D(2026, 9, 1)).number == 1
    assert all(p.playoffs for p in cal.periods)


def test_playoff_weeks_fantrax_default_and_env(monkeypatch):
    prov = FakeFantrax(start=None)
    assert sg._fantrax_playoff_start(prov) == (25, "default for this league (period 25)")
    monkeypatch.setenv(sg.PLAYOFF_ENV, "2")
    assert sg._fantrax_playoff_start(prov)[0] == 2


def test_playoff_weeks_espn_settings():
    league = SimpleNamespace(settings=SimpleNamespace(matchup_periods={"1": [1], "2": [2]}, reg_season_count=1),
                             current_week=1, finalScoringPeriod=8)
    prov = SimpleNamespace(_league_cached=lambda: league)
    ctx = make_ctx(provider="espn")
    cal = sg.league_calendar(ctx, prov)
    assert cal.source == "ESPN matchup periods" and cal.playoff_start == 2
    assert [(p.number, p.start, p.end, p.playoffs) for p in cal.periods] == [
        (1, D(2026, 10, 5), D(2026, 10, 11), False), (2, D(2026, 10, 12), D(2026, 10, 12), True)]
    po = sg.playoff_weeks(ctx, prov)
    assert [p.number for p in po.periods] == [2] and po.teams[0].total == 1


def test_calendar_failure_falls_back_to_weeks():
    def boom():
        raise RuntimeError("down")

    ctx = make_ctx(provider="espn")
    cal = sg.league_calendar(ctx, SimpleNamespace(_league_cached=boom))
    assert cal.source.startswith("NHL calendar weeks") and "down" in cal.notes[0]
    assert len(cal.periods) == 2 and cal.playoff_start == 1   # last 3 weeks -> everything here


def test_calendar_weeks_merges_break_and_opening_week():
    start = D(2026, 9, 29)                       # Tuesday opener -> short first week
    days = [start + timedelta(days=i) for i in range(34)]          # .. Sun Nov 1
    gpd = {d: 10 for d in days}
    for i in range(7):                            # Mon Oct 12 - Sun Oct 18: all-star break
        gpd[D(2026, 10, 12) + timedelta(days=i)] = 1
    ctx = make_ctx()
    ctx.schedule, ctx.games_per_day = {"AAA": days}, gpd
    assert sg.calendar_weeks(ctx) == [(D(2026, 9, 29), D(2026, 10, 4)), (D(2026, 10, 5), D(2026, 10, 11)),
                                      (D(2026, 10, 12), D(2026, 10, 25)), (D(2026, 10, 26), D(2026, 11, 1))]
    # still one too many periods: the short opening week joins week 2
    assert sg.calendar_weeks(ctx, n_periods=3) == [(D(2026, 9, 29), D(2026, 10, 11)),
                                                   (D(2026, 10, 12), D(2026, 10, 25)),
                                                   (D(2026, 10, 26), D(2026, 11, 1))]
