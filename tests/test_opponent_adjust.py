"""Per-game skater opponent multipliers (valuation.schedule: betting-market implied team totals,
else opponent goals against) and their use in the daily plan (recommend.lineup.day_value)."""
from datetime import date

import pytest

from fantasy_manager.analysis.goalie_plan import team_ga_factors
from fantasy_manager.models import FantasyTeam, LeagueContext, Player, Reason, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.recommend.lineup import day_value, recommend_lineup
from fantasy_manager.valuation import schedule as sch
from fantasy_manager.valuation.valuate import PlayerValue

MON, TUE, WED = date(2026, 11, 2), date(2026, 11, 3), date(2026, 11, 4)


def P(cid, team, pos=("C",)):
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team=team, positions=list(pos))


def V(p, per_game, games=2):
    return PlayerValue(player=p, fpg=per_game, fpg_season=per_game, fpg_week=per_game, vorp=0.0,
                       proj_week=per_game * games, games_next7=games, offnight_next7=0,
                       start_share=1.0 if p.is_goalie else None)


def make_ctx(players, extra=(), as_of=TUE):
    """EDM plays Tue vs BOS and Wed @ CGY; BOS plays Tue @ EDM; CGY Wed vs EDM. One C slot."""
    t = FantasyTeam(team_id="1", name="me", owner_is_me=True,
                    slots=[RosterSlot(slot=s, player=p, starting=s != "BN") for s, p in players])
    sched = {"EDM": [TUE, WED], "BOS": [TUE], "CGY": [WED]}
    opps = {"EDM": {TUE: "BOS", WED: "@CGY"}, "BOS": {TUE: "@EDM"}, "CGY": {WED: "EDM"}}
    return LeagueContext(provider="espn", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape={"C": 1, "BN": 1},
                         teams=[t], free_agents=list(extra), matchup_period=1, as_of=as_of,
                         period_end=date(2026, 11, 8), schedule=sched, opponents=opps)


def odds_fn(table):
    return lambda d: table.get(d, {})


def test_multiplier_formula_and_clamps():
    assert sch.opponent_multiplier(None, 0.7) == 1.0 and sch.opponent_multiplier(0.0, 0.7) == 1.0
    assert sch.opponent_multiplier(1.2, 0.7) == pytest.approx(1.2 ** 0.7)
    assert sch.opponent_multiplier(0.9, 0.5) == pytest.approx(0.9 ** 0.5)
    assert sch.opponent_multiplier(3.0, 1.0) == sch.OPP_MULT_MAX == 1.25
    assert sch.opponent_multiplier(0.3, 1.0) == sch.OPP_MULT_MIN == 0.80


def test_implied_ratio_shrinks_the_days_average_toward_the_league():
    # one game on the night: its two teams' average alone would cancel the market's total
    r = sch.implied_ratio(3.6, [3.6, 3.4])
    avg = (3.6 + 3.4 + sch.IMPLIED_AVG_K * sch.LEAGUE_IMPLIED_TOTAL) / (2 + sch.IMPLIED_AVG_K)
    assert r == pytest.approx(3.6 / avg) and r > 3.6 / 3.5
    assert sch.implied_ratio(None, [3.0]) is None and sch.implied_ratio(0.0, [3.0]) is None
    assert sch.implied_ratio(sch.LEAGUE_IMPLIED_TOTAL, []) == pytest.approx(1.0)


def test_odds_first_then_goals_against_then_neutral():
    ctx = make_ctx([("C", P("a", "EDM"))])
    odds = odds_fn({TUE: {"EDM": {"implied_total": 3.8}, "BOS": {"implied_total": 2.6}}})
    adj = sch.OpponentAdjust(ctx, odds=odds, ga={"CGY": (1.2, 20)}, enabled=True)
    tue = adj.game("EDM", TUE)
    assert tue.source == "odds" and tue.opponent == "BOS"
    assert tue.mult == pytest.approx(sch.opponent_multiplier(sch.implied_ratio(3.8, [3.8, 2.6]), 0.7))
    assert adj.game("BOS", TUE).mult < 1.0 < tue.mult
    wed = adj.game("EDM", WED)                       # no odds Wed: CGY allows 1.2x the average
    assert wed.source == "goals against" and wed.mult == pytest.approx(1.2 ** 0.5)
    assert "x1.10" in wed.text("a") and "@CGY" in wed.text("a")
    assert adj.game("CGY", WED).source == "neutral" and adj.game("CGY", WED).mult == 1.0   # EDM: no GA data
    assert adj.game(None, TUE).mult == 1.0
    off = sch.OpponentAdjust(ctx, odds=odds, ga={"CGY": (1.2, 20)}, enabled=False)
    assert off.game("EDM", TUE).mult == 1.0 and off.game("EDM", WED).mult == 1.0


def test_env_switch_turns_it_off(monkeypatch):
    ctx = make_ctx([("C", P("a", "EDM"))])
    monkeypatch.setenv("FM_OPP_ADJUST", "0")
    assert not sch.OpponentAdjust(ctx, odds=odds_fn({}), ga={"BOS": (1.5, 30)}).enabled


def test_team_ga_factors_from_goalie_lines_with_coverage():
    g1, g2, g3 = P("g1", "BOS", ("G",)), P("g2", "CGY", ("G",)), P("g3", "EDM", ("G",))
    sched = {t: [date(2026, 10, d) for d in range(5, 25)] for t in ("BOS", "CGY", "EDM")}   # 20 games before Nov
    g1.lines["season"] = StatLine(split="season", gp=20, stats={"GA": 80.0})      # 4.0 per game
    g2.lines["season"] = StatLine(split="season", gp=20, stats={"GA": 40.0})      # 2.0 per game
    g3.lines["season"] = StatLine(split="season", gp=5, stats={"GA": 10.0})       # covers 5 of 20: left out
    ctx = make_ctx([("C", P("a", "EDM"))], extra=[g1, g2, g3]).model_copy(update={"schedule": sched})
    f = team_ga_factors(ctx)
    assert set(f) == {"BOS", "CGY"}
    k = 10.0
    assert f["BOS"][0] == pytest.approx((20 * 4.0 / 3.0 + k) / (20 + k))
    assert f["CGY"][0] == pytest.approx((20 * 2.0 / 3.0 + k) / (20 + k))
    assert f["BOS"][1] == 20


def test_day_value_scales_skaters_per_game_but_never_goalies():
    a, g = P("a", "EDM"), P("g", "EDM", ("G",))
    ctx = make_ctx([("C", a), ("BN", g)])
    vals = {"a": V(a, 2.0), "g": V(g, 3.0)}
    sch.opponent_adjust(ctx, odds=odds_fn({TUE: {"EDM": {"implied_total": 3.8}, "BOS": {"implied_total": 2.6}}}),
                        ga={"CGY": (0.9, 20)})
    m_tue = sch.opponent_multiplier(sch.implied_ratio(3.8, [3.8, 2.6]), sch.OPP_BETA_ODDS)
    assert day_value(ctx, a, vals["a"], TUE) == pytest.approx(2.0 * m_tue)
    assert day_value(ctx, a, vals["a"], WED) == pytest.approx(2.0 * 0.9 ** sch.OPP_BETA_GA)
    assert day_value(ctx, g, vals["g"], TUE) == pytest.approx(3.0)          # goalies: own matchup model
    # season FPG and the week projection are untouched
    assert vals["a"].fpg == 2.0 and vals["a"].proj_week == 4.0
    # a week projection that already carries the factor (OPP_ADJ_WEEK reason) is divided back out
    carried = vals["a"].model_copy(update={"reasons": [Reason(code=sch.OPP_WEEK_REASON, text="x", value=1.1)],
                                           "proj_week": 4.0 * 1.1})
    assert day_value(ctx, a, carried, TUE) == pytest.approx(2.0 * m_tue)


def test_no_odds_no_data_is_neutral():
    a = P("a", "EDM")
    ctx = make_ctx([("C", a)])
    sch.opponent_adjust(ctx, odds=odds_fn({}), ga={})
    assert day_value(ctx, a, V(a, 2.0), TUE) == pytest.approx(2.0)


def test_opp_week_factor_is_the_mean_over_the_window():
    a, g = P("a", "EDM"), P("g", "EDM", ("G",))
    ctx = make_ctx([("C", a)])
    sch.opponent_adjust(ctx, odds=odds_fn({}), ga={"BOS": (1.21, 20), "CGY": (0.81, 20)})
    w = sch.WeekWindow(start=TUE, days=7)
    assert sch.opp_week_factor(ctx, a, w) == pytest.approx((1.1 + 0.9) / 2)
    assert sch.opp_week_factor(ctx, g, w) == 1.0
    assert sch.opp_week_factor(ctx, a, sch.WeekWindow(start=date(2026, 12, 1), days=7)) == 1.0


def test_opp_adj_reason_on_todays_lineup_rec():
    # a (EDM) is idle today, b (BOS) plays a soft matchup: the start rec says why b's value moved
    a, b = P("a", "CGY"), P("b", "BOS")
    ctx = make_ctx([("C", a), ("BN", b)], as_of=TUE)
    sch.opponent_adjust(ctx, odds=odds_fn({TUE: {"BOS": {"implied_total": 3.9}, "EDM": {"implied_total": 2.7}}}),
                        ga={})
    r = recommend_lineup(ctx, {"a": V(a, 2.0, 1), "b": V(b, 2.0, 1)})[0]
    opp = next(x for x in r.reasons if x.code == "OPP_ADJ")
    assert opp.value > 1.0 and opp.baseline == 1.0
    assert "b: today @EDM - market implies 3.90 goals for his team" in opp.text
    assert r.predicted_gain == pytest.approx(2.0 * opp.value)


def test_valuate_week_projection_carries_the_opponent_factor_once():
    from fantasy_manager.scoring import PointsScoring
    from fantasy_manager.valuation.valuate import valuate_league
    a = P("a", "EDM")
    a.lines["season"] = StatLine(split="season", gp=20, stats={"G": 10.0})
    ctx = make_ctx([("C", a)])
    sch.opponent_adjust(ctx, odds=odds_fn({}), ga={"BOS": (1.21, 20), "CGY": (1.21, 20)})
    pv = valuate_league(ctx, PointsScoring({"G": 1.0}))["a"]
    r = next(r for r in pv.reasons if r.code == sch.OPP_WEEK_REASON)
    assert r.value == pytest.approx(1.1)
    neutral = ctx.model_copy()
    sch.opponent_adjust(neutral, odds=odds_fn({}), ga={})              # same league, neutral opponents
    plain = valuate_league(neutral, PointsScoring({"G": 1.0}))["a"]
    assert all(r.code != sch.OPP_WEEK_REASON for r in plain.reasons)
    assert pv.proj_week == pytest.approx(plain.proj_week * 1.1)
    assert day_value(ctx, a, pv, TUE) == pytest.approx(day_value(ctx, a, plain, TUE))   # not counted twice
