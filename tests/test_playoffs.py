"""Roster-aware fantasy-playoff planner (analysis/playoffs)."""
from datetime import date, timedelta
from types import SimpleNamespace

import pytest

from fantasy_manager.analysis import playoffs as po_mod
from fantasy_manager.analysis.playoffs import parse_deadline, playoff_plan, trade_deadline
from fantasy_manager.analysis.schedule_grid import Period, PeriodCalendar, playoff_weeks
from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig
from fantasy_manager.providers.nhl import games_per_day

D = date
P1 = Period(number=1, start=D(2027, 3, 1), end=D(2027, 3, 7))
P2 = Period(number=2, start=D(2027, 3, 8), end=D(2027, 3, 14), playoffs=True)
P3 = Period(number=3, start=D(2027, 3, 15), end=D(2027, 3, 21), playoffs=True)
CAL = PeriodCalendar(periods=[P1, P2, P3], playoff_start=2, source="test periods")


def _days(start, offsets):
    return [start + timedelta(days=k) for k in offsets]


# Playoff days: Mar 8-21 (14 days). Teams' playoff games:
SCHEDULE = {
    "AAA": _days(D(2027, 3, 8), [0, 2, 4, 7, 9, 11]),          # 6: Mon/Wed/Fri x2
    "BBB": _days(D(2027, 3, 8), [1, 3, 8, 10]),                # 4: Tue/Thu x2 (never with AAA)
    "CCC": _days(D(2027, 3, 8), [0, 1, 2, 3, 4, 7, 8, 9]),     # 8
    "DDD": _days(D(2027, 3, 8), [0, 7]),                       # 2: weak playoff schedule
}


def _p(cid, team, pos=("C",), status="healthy"):
    return Player(cid=cid, name=cid.upper(), name_norm=cid, ids={}, team=team, positions=list(pos), status=status)


def _v(fpg, share=None, ret=None):
    return SimpleNamespace(fpg=fpg, start_share=share, return_date=ret)


def make_ctx(mine, theirs=(), fas=(), shape=None, lock="daily"):
    teams = [FantasyTeam(team_id="1", name="me", owner_is_me=True,
                         slots=[RosterSlot(slot="BN", player=p, starting=False) for p in mine]),
             FantasyTeam(team_id="2", name="them", owner_is_me=False,
                         slots=[RosterSlot(slot="BN", player=p, starting=False) for p in theirs])]
    sched = {t: list(ds) for t, ds in SCHEDULE.items()}
    return LeagueContext(provider="test", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights={"G": 1.0}),
                         roster_shape=shape or {"C": 1, "BN": 4}, teams=teams, free_agents=list(fas),
                         matchup_period=1, as_of=D(2026, 10, 26), schedule=sched,
                         games_per_day=games_per_day(sched), season_start=D(2026, 10, 1), lineup_lock=lock)


def plan_for(ctx, values, **kw):
    return playoff_plan(ctx, values, weeks=playoff_weeks(ctx, calendar=CAL), **kw)


def test_playoff_periods_map_to_dates_and_games():
    ctx = make_ctx([_p("a", "AAA")])
    pl = plan_for(ctx, {"a": _v(2.0)})
    assert [p.number for p in pl.periods] == [2, 3] and pl.days == 14
    a = pl.hold[0]
    assert a.games == [3, 3] and a.total == 6
    assert pl.avg_team_games == pytest.approx((6 + 4 + 8 + 2) / 4)
    assert a.vs_avg == pytest.approx(1.0)


def test_daily_lineup_lets_two_players_share_one_slot():
    ctx = make_ctx([_p("a", "AAA"), _p("b", "BBB")])
    pl = plan_for(ctx, {"a": _v(2.0), "b": _v(1.0)})
    rows = {r.cid: r for r in pl.hold}
    assert rows["a"].starts == 6 and rows["b"].starts == 4      # they never play the same night
    assert pl.my_points == pytest.approx(6 * 2.0 + 4 * 1.0)
    assert rows["a"].points == pytest.approx(12.0) and rows["b"].points == pytest.approx(4.0)


def test_acquire_ranks_by_what_he_adds_to_my_lineup():
    # one C slot held by AAA (6 games); a BBB free agent fills the other nights in full, a CCC one
    # collides with AAA on 5 of his 8 nights
    mine = [_p("a", "AAA")]
    fas = [_p("b", "BBB"), _p("c", "CCC")]
    vals = {"a": _v(2.0), "b": _v(1.5), "c": _v(1.5)}
    pl = plan_for(make_ctx(mine, fas=fas), vals)
    assert [r.cid for r in pl.acquire_fa] == ["b", "c"]
    by = {r.cid: r for r in pl.acquire_fa}
    assert by["b"].points == pytest.approx(4 * 1.5)
    assert by["c"].points == pytest.approx(3 * 1.5)             # only CCC's 3 nights without AAA
    assert by["c"].naive > by["b"].naive                         # naive games x FPG would overrate him


def test_trade_targets_carry_their_owner_and_rank():
    pl = plan_for(make_ctx([_p("a", "AAA")], theirs=[_p("t", "BBB")]), {"a": _v(2.0), "t": _v(3.0)})
    assert [(r.cid, r.owner) for r in pl.acquire_trade] == [("t", "them")]
    assert pl.rank == 1 and pl.n_teams == 2                      # 12 pts vs their 12 -> not behind


def test_sell_flags_weak_playoff_schedule():
    pl = plan_for(make_ctx([_p("a", "AAA"), _p("d", "DDD")]), {"a": _v(2.0), "d": _v(3.0)})
    assert [r.cid for r in pl.sell] == ["d"]
    assert pl.sell[0].vs_avg == pytest.approx(-3.0) and pl.sell[0].cost == pytest.approx(9.0)


def test_injuries_count_from_return_date_and_ltir_is_out():
    after = D(2027, 3, 16)          # back for period 3 only: Mar 16, 18 (AAA)
    ctx = make_ctx([_p("a", "AAA", status="ir"), _p("l", "BBB", status="ltir")])
    pl = plan_for(ctx, {"a": _v(2.0, ret=after), "l": _v(5.0)})
    rows = {r.cid: r for r in pl.hold}
    assert rows["a"].starts == 2 and rows["l"].starts == 0
    # an injured player without a return date is assumed back by the playoffs
    pl2 = plan_for(make_ctx([_p("a", "AAA", status="out")]), {"a": _v(2.0)})
    assert pl2.hold[0].starts == 6


def test_goalie_value_uses_start_share():
    ctx = make_ctx([_p("g", "AAA", pos=("G",))], shape={"G": 1, "BN": 2})
    pl = plan_for(ctx, {"g": _v(5.0, share=0.6)})
    assert pl.hold[0].per_game == pytest.approx(3.0)
    assert pl.my_points == pytest.approx(6 * 3.0)
    assert [g.group for g in pl.groups] == ["G"]


def test_weekly_lock_sets_one_lineup_per_period():
    # weekly lock: the C slot holds one player per period -> AAA (3 x 2.0) beats BBB (2 x 1.0)
    ctx = make_ctx([_p("a", "AAA"), _p("b", "BBB")], lock="weekly")
    pl = plan_for(ctx, {"a": _v(2.0), "b": _v(1.0)})
    assert pl.lock == "weekly"
    assert pl.my_points == pytest.approx(12.0)
    assert {r.cid: r.starts for r in pl.hold} == {"a": 6, "b": 0}


@pytest.mark.parametrize("text,expected", [
    ("Thu Mar 4, 2027 12:00PM EST", D(2027, 3, 4)),
    ("March 4, 2027", D(2027, 3, 4)),
    ("2027-03-04", D(2027, 3, 4)),
    ("03/04/2027", D(2027, 3, 4)),
    ("Not Used", None),
    (None, None),
])
def test_parse_deadline(text, expected):
    assert parse_deadline(text) == expected


def test_trade_deadline_sources():
    ctx = make_ctx([_p("a", "AAA")])
    ctx.provider = "fantrax"
    rules = SimpleNamespace(get=lambda label: "Not Used" if label == po_mod.DEADLINE_LABEL else None)
    assert trade_deadline(ctx, SimpleNamespace(rules=rules)) == (None, "Fantrax Rules page: Not Used")
    rules = SimpleNamespace(get=lambda label: "Fri Feb 26, 2027" if label == po_mod.DEADLINE_LABEL else None)
    assert trade_deadline(ctx, SimpleNamespace(rules=rules)) == (D(2027, 2, 26), "Fantrax Rules page")
    ctx.provider = "espn"
    ms = 1803960000000                  # 2027-03-01 (local)
    league = SimpleNamespace(settings=SimpleNamespace(trade_deadline=ms))
    d, src = trade_deadline(ctx, SimpleNamespace(_league=lambda: league))
    assert src == "ESPN league settings" and d is not None and abs((d - D(2027, 3, 1)).days) <= 1
    pl = playoff_plan(ctx, {"a": _v(2.0)}, SimpleNamespace(_league=lambda: league),
                      weeks=playoff_weeks(ctx, calendar=CAL))
    assert pl.days_to_deadline == (pl.deadline - ctx.as_of).days


def test_playoffs_command_and_schedule_page(monkeypatch, tmp_path):
    import json

    from typer.testing import CliRunner

    from fantasy_manager import cli

    from .test_schedule_web_cli import _patch, _result

    _patch(monkeypatch, tmp_path)
    runner = CliRunner()
    res = runner.invoke(cli.app, ["playoffs"])
    assert res.exit_code == 0, res.output
    assert "projected playoff pts" in res.output and "Trade deadline" in res.output
    res = runner.invoke(cli.app, ["--json", "playoffs"])
    data = json.loads(res.output)
    assert data["my_team"] == "Mine" and data["periods"] and data["hold"]
    assert data["meta"]["provider"] == "espn"

    pytest.importorskip("fastapi")
    from fastapi.testclient import TestClient

    from fantasy_manager.web.app import create_app

    h = TestClient(create_app(lambda league: _result(league))).get("/schedule?league=espn").text
    assert "projected playoff pts" in h and "Hold" in h


def test_no_playoff_periods_is_empty_not_an_error():
    ctx = make_ctx([_p("a", "AAA")])
    cal = PeriodCalendar(periods=[P1], playoff_start=None)
    pl = playoff_plan(ctx, {"a": _v(2.0)}, weeks=playoff_weeks(ctx, calendar=cal))
    assert pl.hold == [] and pl.periods == []
    assert any("No fantasy-playoff periods" in n for n in pl.notes)
