from datetime import date

from fantasy_manager.models import (FantasyTeam, LeagueContext, Player, Reason, Recommendation, RosterSlot,
                                    ScoringConfig, StatLine)
from fantasy_manager.recommend.market import (DISAGREE_GAP, MarketPool, market_check_reason, market_disagrees,
                                              trade_market_check)
from fantasy_manager.recommend.trades import recommend_trades
from fantasy_manager.recommend.waivers import recommend_waivers
from fantasy_manager.valuation.valuate import PlayerValue

from .test_trades import build


def pl(cid, team="EDM"):
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team=team, positions=["C"])


def pv(p, fpg, reasons=()):
    return PlayerValue(player=p, fpg=fpg, fpg_season=fpg, fpg_week=fpg, vorp=fpg - 1.0, vorp_week=fpg - 1.0,
                       horizon_values={"season": fpg, "week": fpg, "vorp_season": fpg - 1.0,
                                       "vorp_week": fpg - 1.0}, reasons=list(reasons))


def pool_ctx(dynasty=False):
    """12 rostered players: ADP 1..12 and % rostered 100..45 agree with our FPG 3.0..1.9."""
    rows, values = [], {}
    for i in range(12):
        p = pl(f"p{i}")
        p.adp, p.pct_owned = float(i + 1), 100.0 - 5 * i
        values[p.cid] = pv(p, 3.0 - 0.1 * i)
        rows.append(p)
    slots = [RosterSlot(slot="C", player=p, starting=True) for p in rows]
    ctx = LeagueContext(provider="test", league_id="1", season=2027, name="M", dynasty=dynasty,
                        scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape={"C": 12},
                        teams=[FantasyTeam(team_id="me", name="Me", owner_is_me=True, slots=slots)],
                        free_agents=[], matchup_period=1, as_of=date(2026, 10, 1))
    return ctx, values, {p.cid: p for p in rows}


def test_consensus_uses_only_independent_signals():
    ctx, values, P = pool_ctx(dynasty=True)
    dyn = {cid: 20.0 - i for i, cid in enumerate(P)}
    pool = MarketPool(ctx, values, dyn)
    assert "dyn" in pool.value_and_sources(P["p0"])[1]          # trade acceptance still uses it
    assert pool.consensus(P["p0"])[1] == ("adp", "owned")       # the consensus check does not
    bare = pl("bare")                                            # no ADP / % rostered: no consensus
    assert pool.consensus(bare) is None and market_check_reason(bare, pool, "add") is None


def test_market_check_agrees_and_disagrees_with_drivers():
    ctx, values, P = pool_ctx()
    pool = MarketPool(ctx, values)
    agree = market_check_reason(P["p0"], pool, "add")
    assert agree.code == "MARKET_CHECK" and "market agrees" in agree.text and abs(agree.value) < DISAGREE_GAP
    # the market's last player, whom our model ranks first because of a PP1 role signal
    values["p11"] = pv(P["p11"], 3.5, [Reason(code="ROLE_DFO", text="PP1"), Reason(code="FPG_BLEND", text="")])
    pool = MarketPool(ctx, values)
    r = market_check_reason(P["p11"], pool, "add")
    assert r.value >= DISAGREE_GAP and r.baseline < 10
    assert "model disagrees with market, rates him higher (line / PP role)" in r.text


def test_market_disagrees_flags():
    rec = Recommendation(kind="waiver", score=1.0, title="x", reasons=[])
    assert market_disagrees(rec) is None
    rec.reasons = [Reason(code="MARKET_CHECK", text="", value=5.0)]
    assert market_disagrees(rec) is False
    rec.reasons.append(Reason(code="MARKET_CHECK", text="", value=-30.0))
    assert market_disagrees(rec) is True
    # MARKET_VIEW is the acceptance input (it includes need / block bonuses): alone it flags nothing
    trade = Recommendation(kind="trade", score=1.0, title="t", reasons=[
        Reason(code="MARKET_VIEW", text="", value=17.5), Reason(code="MARKET_CHECK", text="", value=2.0)])
    assert market_disagrees(trade) is False
    deal = trade_market_check(17.5, 0.99, ["x_d: line / PP role"])    # market value handed over: 17.5 worth pts
    assert deal.text.startswith("Model disagrees with market: you overpay by market value (+18 worth pts")
    assert "worth it only if you trust the model's edge of +0.99 (drivers: x_d: line / PP role)" in deal.text
    trade.reasons.append(deal)
    assert market_disagrees(trade) is True                       # clearly in their favour by market value
    assert "projection only" in trade_market_check(17.5, 0.99).text
    assert trade_market_check(2.0, 0.99) is None and trade_market_check(17.5, -0.1) is None


def mk(cid, goals_pg, team="EDM"):
    lines = {"projected": StatLine(split="projected", gp=80, stats={"G": goals_pg * 80, "GP": 80})}
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team=team, positions=["C"], lines=lines)


def test_waivers_skip_unsigned_free_agents_and_carry_market_checks():
    unsigned, signed, mine = mk("unsigned", 2.0, team=None), mk("signed", 1.0), mk("mine", 0.2)
    ctx = LeagueContext(provider="test", league_id="1", season=2027, name="T",
                        scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape={"C": 1, "IR": 1},
                        teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True,
                                           slots=[RosterSlot(slot="C", player=mine, starting=True)])],
                        free_agents=[unsigned, signed], matchup_period=1, as_of=date(2026, 10, 1))
    values = {p.cid: pv(p, f) for p, f in ((unsigned, 3.0), (signed, 2.0), (mine, 0.2))}
    recs = recommend_waivers(ctx, values)
    assert [r.add[0].cid for r in recs] == ["signed"]
    # one rostered player is too few for a market pool: no MARKET_CHECK, nothing breaks
    assert not [x for x in recs[0].reasons if x.code == "MARKET_CHECK"]


def test_waiver_pickup_checks_add_and_drop_against_market():
    ctx, values, P = pool_ctx()
    fa = pl("fa")
    fa.adp, fa.pct_owned = 150.0, 2.0             # the market barely rosters him
    values["fa"] = pv(fa, 3.5, [Reason(code="ROOKIE_BLEND", text="")])
    ctx.free_agents = [fa]
    recs = recommend_waivers(ctx, values)
    assert recs and recs[0].add[0].cid == "fa" and recs[0].drop[0].cid == "p11"
    checks = {x.text.split(")")[0]: x for x in recs[0].reasons if x.code == "MARKET_CHECK"}
    assert set(checks) == {"Market check (add", "Market check (drop"}
    assert "rates him higher (rookie model)" in checks["Market check (add"].text
    assert market_disagrees(recs[0]) is True


def test_trades_carry_market_checks_for_every_player_moved():
    ctx, values = build()
    for t in ctx.teams:
        for p in t.players:
            p.pct_owned = 20.0 * values[p.cid].fpg_season     # the market agrees with our model
    recs = recommend_trades(ctx, values, lineup_fn=False, limit=5)
    assert recs
    for r in recs:
        checks = [x for x in r.reasons if x.code == "MARKET_CHECK" and x.value is not None]
        assert len(checks) == len(r.add) + len(r.drop)
        assert [x.code for x in r.reasons][:2] == ["MY_EDGE", "MARKET_VIEW"]   # appended, order kept


def test_market_info_badge():
    from fantasy_manager.web.views import market_info

    rec = Recommendation(kind="waiver", score=1.0, title="x", reasons=[])
    assert market_info(rec) is None
    rec.reasons = [Reason(code="MARKET_CHECK", text="Market check (add) A: ...", value=-40.0)]
    info = market_info(rec)
    assert info["disagrees"] is True and info["label"] == "model disagrees with market"
    assert info["title"] == "Market check (add) A: ..."
