"""Keeper leagues (ctx.keepers_per_team = K) on the consumer side: trades, waivers, hold-or-cut and
prospect protection use the roster-level change (recommend.base.dynasty_change: season part +
keeper_total after - before), not a sum of per-player dynasty values. DynastyValue maps are built
by hand so these tests do not depend on valuation.dynasty.apply_dynasty."""
from datetime import date

import pytest

from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig
from fantasy_manager.recommend import trades as tr
from fantasy_manager.recommend.base import (dynasty_change, dynasty_delta, keeper_k, keepers_text,
                                            protected_reason, swap_values)
from fantasy_manager.recommend.injuries import recommend_injuries
from fantasy_manager.recommend.waivers import recommend_waivers
from fantasy_manager.scoring import PointsScoring
from fantasy_manager.valuation.dynasty import DynastyValue
from fantasy_manager.valuation.valuate import PlayerValue, valuate_league

from .test_waivers import league, mk

AS_OF = date(2026, 10, 1)
SHAPE = {"C": 1, "LW": 1, "D": 2, "G": 1, "BN": 3, "IR": 1}     # 8 active spots


def pl(cid, pos, age=None, pick=None):
    born = date(AS_OF.year - age, 1, 1) if age is not None else None
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team="EDM", positions=pos, status="healthy",
                  birth_date=born, draft_overall=pick)


def pv(p, fpg):
    return PlayerValue(player=p, fpg=fpg, fpg_season=fpg, fpg_week=fpg, vorp=fpg - 1.0, vorp_week=fpg - 1.0,
                       horizon_values={"season": fpg, "week": fpg, "vorp_season": fpg - 1.0,
                                       "vorp_week": fpg - 1.0})


def dv(p, season, surplus, keeper, keep_all=False, age=27.0):
    """Hand-built DynastyValue. Keeper league: value = season + surplus for a keeper, else season.
    Keep-everyone (keep_all): value = season + surplus and everyone is a keeper."""
    if keep_all:
        keeper = True
    return DynastyValue(player=p, value=season + (surplus if keeper else 0.0), age=age, age_mult=1.0, upside=0.0,
                        horizon_years=3, season_part=season, future=surplus, keeper_surplus=surplus, keeper=keeper)


# -- base helpers on synthetic rosters --------------------------------------------------------

def _ctx(k):
    ctx = LeagueContext(provider="t", league_id="1", season=2027, name="T",
                        scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape=SHAPE, teams=[],
                        free_agents=[], matchup_period=1, as_of=AS_OF)
    ctx.dynasty, ctx.keepers_per_team = True, k
    return ctx


def test_dynasty_change_counts_only_the_top_k_keepers():
    ctx = _ctx(2)
    a, b, c, new, star = (pl(x, ["C"]) for x in ("a", "b", "c", "new", "star"))
    dyn = {"a": dv(a, 1.0, 5.0, True), "b": dv(b, 1.0, 3.0, True), "c": dv(c, 1.0, 1.0, False),
           "new": dv(new, 0.5, 2.0, False), "star": dv(star, 0.8, 4.0, False)}
    roster = [a, b, c]
    # a 4th young player who would be my (K+1)-th keeper adds only his season part
    ch = dynasty_change(ctx, dyn, roster, roster + [new])
    assert ch.total == pytest.approx(0.5) and ch.keepers == pytest.approx(0.0) and ch.kept_in == ()
    assert keepers_text(ch) == "keepers unchanged"
    # one who displaces the K-th keeper adds his surplus less the displaced one's
    ch = dynasty_change(ctx, dyn, roster, roster + [star])
    assert ch.keepers == pytest.approx(4.0 - 3.0) and ch.total == pytest.approx(0.8 + 1.0)
    assert [p.cid for p in ch.kept_in] == ["star"] and [p.cid for p in ch.kept_out] == ["b"]
    assert keepers_text(ch) == "star replaces b as a keeper"
    # losing a keeper costs his surplus less the next best's (and his season part)
    assert dynasty_delta(ctx, dyn, roster, [a, c]) == pytest.approx(-1.0 - (3.0 - 1.0))
    # dropping a non-keeper costs only his season part
    assert dynasty_delta(ctx, dyn, roster, [a, b]) == pytest.approx(-1.0)
    # swap values: add - drop is the exact change
    add_v, drop_v = swap_values(ctx, dyn, roster, star, c)
    assert drop_v == pytest.approx(1.0) and add_v == pytest.approx(0.8 + 1.0)
    assert add_v - drop_v == pytest.approx(dynasty_delta(ctx, dyn, roster, [a, b, star]))


def test_keep_everyone_and_float_maps_stay_additive():
    a, b, new = pl("a", ["C"]), pl("b", ["C"]), pl("new", ["C"])
    dyn = {"a": dv(a, 1.0, 5.0, True), "b": dv(b, 1.0, 3.0, True), "new": dv(new, 0.5, 2.0, False)}
    everyone = _ctx(None)
    assert keeper_k(everyone, dyn) is None
    assert dynasty_delta(everyone, dyn, [a, b], [a, new]) == pytest.approx(0.5 - 4.0)   # V(new) - V(b)
    floats = {"a": 6.0, "b": 4.0, "new": 2.5}
    keepers = _ctx(2)
    assert keeper_k(keepers, floats) is None             # no keeper fields: plain additive values
    assert dynasty_delta(keepers, floats, [a, b], [a, new]) == pytest.approx(2.5 - 4.0)
    assert swap_values(keepers, floats, [a, b], new, b) == (2.5, 4.0)


# -- trades -------------------------------------------------------------------------------------

def trade_league(k, mode="balanced", keep_all=False, kid_keeper=False):
    """Me: keepers m_c (surplus 5) and m_lw (3); m_d1 (1) is the next best; m_kid a 19-year-old #2
    pick (surplus 0.8, a non-keeper unless ``kid_keeper``). Team A: a_kid (surplus 4) and a_young (2)
    are its keepers; a_lw is a veteran with no surplus."""
    spec = {
        "me": [("C", "m_c", ["C"], 3.0, 5.0), ("LW", "m_lw", ["LW"], 2.0, 3.0), ("D", "m_d1", ["D"], 1.5, 1.0),
               ("D", "m_d2", ["D"], 1.0, 0.0), ("G", "m_g", ["G"], 2.5, 0.5), ("BN", "m_bn1", ["C"], 0.6, 0.0),
               ("BN", "m_bn2", ["LW"], 0.5, 0.0), ("BN", "m_kid", ["LW"], 0.3, 6.0 if kid_keeper else 0.8)],
        "A": [("C", "a_c", ["C"], 1.2, 0.0), ("LW", "a_lw", ["LW"], 2.2, 0.0), ("D", "a_young", ["D"], 1.4, 2.0),
              ("D", "a_kid", ["D"], 1.2, 4.0), ("BN", "a_d", ["D"], 1.0, 0.0), ("G", "a_g", ["G"], 2.4, 0.0)],
    }
    values, dyn, teams = {}, {}, []
    for tid, rows in spec.items():
        ranked = sorted((r for r in rows if r[4] > 0), key=lambda r: -r[4])
        keepers = {r[1] for r in ranked[:k or 0]}
        slots = []
        for slot, cid, pos, f, s in rows:
            kid = cid == "m_kid"
            p = pl(cid, pos, age=19 if kid else None, pick=2 if kid else None)
            values[cid] = pv(p, f)
            dyn[cid] = dv(p, f, s, cid in keepers, keep_all=keep_all, age=19.0 if kid else 27.0)
            slots.append(RosterSlot(slot=slot, player=p, starting=slot not in ("BN", "IR")))
        teams.append(FantasyTeam(team_id=tid, name=f"Team {tid}", owner_is_me=tid == "me", slots=slots))
    fas = []
    for cid, pos, f in (("fa_c", ["C"], 0.9), ("fa_d", ["D"], 0.8), ("fa_g", ["G"], 1.0)):
        p = pl(cid, pos)
        values[cid] = pv(p, f)
        dyn[cid] = dv(p, f, 0.0, False, keep_all=keep_all)
        fas.append(p)
    ctx = LeagueContext(provider="t", league_id="1", season=2027, name="T",
                        scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape=dict(SHAPE),
                        teams=teams, free_agents=fas, matchup_period=1, as_of=AS_OF)
    ctx.dynasty, ctx.dynasty_mode, ctx.keepers_per_team = True, mode, k
    return ctx, values, dyn


def _ev(ctx, values, dyn, give, get):
    return tr.evaluate_trade(ctx, values, give=give, get=get, dynasty_values=dyn, lineup_fn=False)


def test_trade_for_a_would_be_k_plus_1_keeper_adds_only_his_season_part():
    ctx, values, dyn = trade_league(2)
    ev = _ev(ctx, values, dyn, ["m_d2"], ["a_young"])
    assert ev.my_drops == [] and ev.my_fill is None
    assert ev.keeper_change.keepers == pytest.approx(0.0)
    assert ev.delta_dyn == pytest.approx(1.4 - 1.0)                # season parts only
    assert ev.delta_dyn_n == pytest.approx(ev.delta_dyn / tr.dynasty_scale(ctx))
    # a keep-everyone dynasty adds his whole value (unchanged behaviour)
    ctx2, values2, dyn2 = trade_league(None, keep_all=True)
    ev2 = _ev(ctx2, values2, dyn2, ["m_d2"], ["a_young"])
    assert ev2.keeper_change is None
    assert ev2.delta_dyn == pytest.approx(dyn2["a_young"].value - dyn2["m_d2"].value) == pytest.approx(3.4 - 1.0)


def test_trade_for_a_player_who_displaces_my_kth_keeper():
    ctx, values, dyn = trade_league(2)
    ev = _ev(ctx, values, dyn, ["m_d2"], ["a_kid"])
    assert ev.keeper_change.keepers == pytest.approx(4.0 - 3.0)      # his surplus - the displaced keeper's
    assert ev.delta_dyn == pytest.approx((1.2 - 1.0) + (4.0 - 3.0))
    assert [p.cid for p in ev.keeper_change.kept_in] == ["a_kid"]
    assert [p.cid for p in ev.keeper_change.kept_out] == ["m_lw"]
    text = next(x for x in tr._to_rec(ev, tr.trade_values(ctx, values, dyn)[0], "dynasty", "greedy").reasons
                if x.code == "DYNASTY_DELTA").text
    assert text.startswith("Keepers: top-2 long-term +1.00 (a_kid replaces m_lw as a keeper); this season +0.20")
    assert "balanced edge = 1 x lineup + 1 x dynasty" in text


def test_trading_away_a_keeper_loses_his_surplus_over_the_next_best():
    ctx, values, dyn = trade_league(2)
    ev = _ev(ctx, values, dyn, ["m_lw"], ["a_lw"])
    assert ev.keeper_change.keepers == pytest.approx(-(3.0 - 1.0))   # m_d1 (surplus 1) becomes the keeper
    assert ev.delta_dyn == pytest.approx((2.2 - 2.0) - 2.0)
    assert [p.cid for p in ev.keeper_change.kept_out] == ["m_lw"]
    assert [p.cid for p in ev.keeper_change.kept_in] == ["m_d1"]


def test_keeper_roster_spot_costs_this_season_only():
    ctx, values, dyn = trade_league(2)
    ctx.roster_shape = {**SHAPE, "BN": 4}                 # room to take two for one without a drop
    _, _, repl = tr.trade_values(ctx, values, dyn)
    everyone = ctx.model_copy(update={"keepers_per_team": None})
    _, _, repl_all = tr.trade_values(everyone, values, dyn)
    assert repl > 0
    assert repl == pytest.approx(repl_all / tr.dynasty_scale(ctx) * tr.season_weight(ctx))
    ev = _ev(ctx, values, dyn, ["m_d2"], ["a_young", "a_d"])
    assert ev.my_drops == [] and ev.my_fill is None
    assert ev.spot_cost == pytest.approx(repl)
    assert ev.delta_dyn == pytest.approx((1.4 + 1.0 - 1.0) - repl)


def test_non_keeper_prospect_can_be_dropped_but_a_keeper_prospect_stays_protected():
    # keep-everyone: the 19-year-old #2 pick is protected, so the 2-for-1 cuts someone else
    ctx, values, dyn = trade_league(None, keep_all=True)
    eng = tr._TradeEngine(ctx, values, dyn, lineup_fn=False)
    assert "m_kid" in eng.protected
    ev = _ev(ctx, values, dyn, ["m_d2"], ["a_young", "a_d"])
    assert "m_kid" not in {p.cid for p in ev.my_drops}
    # keeper league: he is not one of my top-2 keepers, so he has no future value to protect
    ctx, values, dyn = trade_league(2)
    assert protected_reason(ctx.my_team.players[-1], ctx, dyn) is None
    assert "m_kid" not in tr._TradeEngine(ctx, values, dyn, lineup_fn=False).protected
    ev = _ev(ctx, values, dyn, ["m_d2"], ["a_young", "a_d"])
    assert [p.cid for p in ev.my_drops] == ["m_kid"] and ev.blocked is None
    assert ev.delta_dyn == pytest.approx(1.4 + 1.0 - 1.0 - 0.3)      # his drop costs only his season part
    # a keeper prospect stays protected
    ctx, values, dyn = trade_league(2, kid_keeper=True)
    assert dyn["m_kid"].keeper
    why = protected_reason(ctx.my_team.players[-1], ctx, dyn)
    assert why is not None and why.endswith("a top-2 keeper")
    ev = _ev(ctx, values, dyn, ["m_d2"], ["a_young", "a_d"])
    assert "m_kid" not in {p.cid for p in ev.my_drops}


def test_keeper_league_with_plain_float_values_stays_additive():
    ctx, values, dyn = trade_league(2)
    floats = {cid: d.value for cid, d in dyn.items()}
    ev = _ev(ctx, values, floats, ["m_d2"], ["a_young"])
    assert ev.keeper_change is None
    assert ev.delta_dyn == pytest.approx(floats["a_young"] - floats["m_d2"])


# -- waivers and hold-or-cut ------------------------------------------------------------------

def waiver_league(k, keep_all=False, kid_keeper=False):
    """My C star (keeper) and a bench C prospect (20, #3 pick); a better free-agent C."""
    fa = mk("fa_c", ["C"], 1.0, gp=20)
    star = mk("star", ["C"], 1.2)
    kid = mk("kid", ["C"], 0.3)
    kid.birth_date, kid.draft_overall = date(2006, 1, 1), 3
    ctx = league([fa], [("C", star), ("BN", kid)])
    ctx.dynasty, ctx.keepers_per_team = True, k
    values = valuate_league(ctx, PointsScoring({"G": 1.0}))
    sp = {c: values[c].fpg_season for c in ("fa_c", "star", "kid")}
    dyn = {"star": dv(star, sp["star"], 6.0, True, keep_all),
           "kid": dv(kid, sp["kid"], 2.0, kid_keeper, keep_all, age=20.0),
           "fa_c": dv(fa, sp["fa_c"], 0.0, False, keep_all)}
    return ctx, values, dyn, sp


def test_waiver_drop_of_a_non_keeper_costs_only_his_season_part():
    ctx, values, dyn, sp = waiver_league(1)
    [r] = recommend_waivers(ctx, values, dynasty_values=dyn)
    assert [p.cid for p in r.add] == ["fa_c"] and [p.cid for p in r.drop] == ["kid"]
    reasons = {x.code: x for x in r.reasons}
    assert reasons["DYNASTY_DROP"].baseline == pytest.approx(sp["kid"])          # his season part only
    assert reasons["DYNASTY_GAIN"].value == pytest.approx(sp["fa_c"] - sp["kid"])
    assert "top-1 keepers" in reasons["DYNASTY_DROP"].text
    # keep-everyone: the same prospect is protected and worth his whole value: no pickup
    ctx, values, dyn, _ = waiver_league(None, keep_all=True)
    debug = []
    assert recommend_waivers(ctx, values, dynasty_values=dyn, debug=debug) == []
    assert any(d["drop"] == "kid" and d["reason"].code == "PROSPECT_PROTECTED" for d in debug)


def test_waiver_keeper_prospect_stays_protected():
    ctx, values, dyn, _ = waiver_league(2, kid_keeper=True)
    debug = []
    assert recommend_waivers(ctx, values, dynasty_values=dyn, debug=debug) == []
    hit = [d for d in debug if d["drop"] == "kid" and d["reason"].code == "PROSPECT_PROTECTED"]
    assert hit and "a top-2 keeper" in hit[0]["reason"].text


def test_hold_or_cut_judges_an_injured_non_keeper_prospect_on_this_season():
    star = mk("star", ["C"], 1.2)
    kid = mk("kid", ["C"], 0.5, status="ir")
    kid.status_note = "Knee: out (est. return 2027-03-01)"
    kid.birth_date, kid.draft_overall = date(2006, 1, 1), 3
    fa = mk("fa", ["C"], 0.6)
    slots = [RosterSlot(slot="C", player=star, starting=True), RosterSlot(slot="BN", player=kid, starting=False)]
    ctx = LeagueContext(provider="t", league_id="1", season=2027, name="T",
                        scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape={"C": 1, "BN": 1},
                        teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True, slots=slots)],
                        free_agents=[fa], matchup_period=1, as_of=date(2026, 10, 7))
    ctx.dynasty = True
    vals = valuate_league(ctx, PointsScoring({"G": 1.0}))
    sp = {c: vals[c].fpg_season for c in ("star", "kid", "fa")}

    def verdict(k, keep_all):
        ctx.keepers_per_team = k
        dyn = {"star": dv(star, sp["star"], 6.0, True, keep_all),
               "kid": dv(kid, sp["kid"], 2.0, False, keep_all, age=20.0),
               "fa": dv(fa, sp["fa"], 0.0, False, keep_all)}
        return [r for r in recommend_injuries(ctx, vals, {}, dynasty_values=dyn) if r.title.startswith(("Hold", "Cut"))]

    [held] = verdict(None, True)                 # keep-everyone: a protected prospect is never cut
    assert held.title.startswith("Hold kid") and "protected dynasty asset" in held.title
    [cut] = verdict(1, False)                    # keeper league: not a keeper, so this season decides
    assert cut.title.startswith("Cut kid, add fa")
    cost = next(x for x in cut.reasons if x.code == "DYNASTY_COST")
    assert cost.baseline == pytest.approx(sp["kid"]) and "top-1 keepers" in cost.text


# -- outlook ------------------------------------------------------------------------------------

def test_outlook_future_counts_only_each_teams_keepers():
    from types import SimpleNamespace

    from fantasy_manager.analysis.outlook import build_outlook, future_values

    from .test_outlook import _dyn_league

    ctx, values, dyn = _dyn_league(my_record=(8, 0, 0), their_record=(0, 8, 0))
    surplus = {"kid": 3.0, "riser": 4.0}
    dyn = {cid: SimpleNamespace(**vars(d), season_part=values[cid].fpg_season, keeper_surplus=surplus.get(cid, 0.0),
                                keeper=cid in surplus) for cid, d in dyn.items()}
    ctx.keepers_per_team = 1
    o = build_outlook(ctx, values, dyn, sims=50)
    kid = next(p for p in ctx.my_team.players if p.cid == "kid")
    assert o.me.future == pytest.approx(future_values(kid, ctx, values, dyn))   # next season: kid only
    assert any("each team's top 1 keeper (everyone else" in n for n in o.notes)
    assert o.recommended == "contend"    # a non-starting keeper is sold on his keeper surplus
    assert [a.player.cid for a in o.sell] == ["kid"] and "long-term value 3.0, one of your keepers" in o.sell[0].why
