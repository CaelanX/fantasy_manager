"""`evaluate_offer` / `fm trade-eval`: judging one specific trade offer."""
import json

import pytest

from fantasy_manager.models import Player
from fantasy_manager.recommend.trades import (EVAL_ACCEPT_EDGE, _TradeEngine, evaluate_offer, evaluate_trade,
                                              find_pick, find_players, find_team, offer_summary, pick_assets)

from . import test_picks as tp
from . import test_trades as tt


# -- evaluator -------------------------------------------------------------------------------

def test_good_offer_is_accept_and_uses_the_proposal_scoring():
    ctx, values = tt.build()
    o = evaluate_offer(ctx, values, give=["m_c2"], get=["a_d1"], lineup_fn=False)
    plain = evaluate_trade(ctx, values, give=["m_c2"], get=["a_d1"], lineup_fn=False)
    assert o.ev.team.team_id == "A"                       # inferred from the --get player
    assert o.ev.delta_me == pytest.approx(plain.delta_me) and o.ev.delta_me > EVAL_ACCEPT_EDGE
    assert o.ev.delta_them == pytest.approx(plain.delta_them)
    assert o.verdict == "accept" and o.why[0].startswith("You gain")
    codes = [r.code for r in o.rec.reasons]
    assert codes[:2] == ["MY_EDGE", "MARKET_VIEW"] and {"DELTA_THEM", "FAIR_PCT", "GAIN_WEEK"} <= set(codes)
    s = offer_summary(o)
    assert s["verdict"] == "accept" and s["team"] == "Team A"
    assert s["my_gain"]["fpg"] == pytest.approx(round(plain.delta_me, 3))
    assert s["their_gain"]["fpg"] == pytest.approx(round(plain.delta_them, 3))
    assert s["dynasty_delta"] is None and s["units"] == "VORP"
    assert s["fairness"]["market_label"].startswith("Looks") and 0 <= s["fairness"]["p_accept"] <= 0.9
    json.dumps(s)                                         # plain data


def test_losing_offer_is_decline():
    ctx, values = tt.build()
    o = evaluate_offer(ctx, values, give=["m_c1", "m_c2"], get=["a_d3"], lineup_fn=False)   # both Cs for a bench D
    assert o.verdict == "decline" and o.ev.my_edge < 0 and "You lose" in o.why[0]


def test_near_even_offer_is_counter():
    ctx, values = tt.build()
    o = evaluate_offer(ctx, values, give=["m_lw"], get=["a_lw"], lineup_fn=False)    # 2.0 LW for 1.9 LW
    assert o.verdict == "counter" and -0.3 <= o.ev.my_edge < EVAL_ACCEPT_EDGE


def test_explicit_team_and_wrong_owner():
    ctx, values = tt.build()
    with pytest.raises(LookupError, match="Team B's roster"):
        evaluate_offer(ctx, values, give=["m_c2"], get=["a_d1"], team_id="B", lineup_fn=False)
    with pytest.raises(LookupError, match="single team"):
        evaluate_offer(ctx, values, give=["m_c2"], get=["a_d1", "b_c"], lineup_fn=False)
    with pytest.raises(LookupError, match="your roster"):
        evaluate_offer(ctx, values, give=["a_c"], get=["b_c"], lineup_fn=False)
    with pytest.raises(ValueError):
        evaluate_offer(ctx, values, give=[], get=[], lineup_fn=False)


def test_picks_count_at_net_for_me_and_need_a_dynasty_league():
    ctx, values, dyn = tp.build()
    picks = pick_assets(ctx, values, dyn)
    theirs = sorted(picks["strong"], key=lambda a: -a.value)[0]
    me = ctx.my_team.players[5].cid
    them = ctx.teams[0].players[2].cid
    base = evaluate_offer(ctx, values, give=[me], get=[them], dynasty_values=dyn, lineup_fn=False)
    o = evaluate_offer(ctx, values, give=[me], get=[them], dynasty_values=dyn, get_picks=[theirs.key],
                       lineup_fn=False)
    assert o.ev.get_picks and o.ev.delta_dyn == pytest.approx(base.ev.delta_dyn + theirs.net)
    assert o.ev.perceived < base.ev.perceived                   # they give more by market value
    assert any(r.code == "PICK_IN" for r in o.rec.reasons)
    assert any(r.code == "MARKET_CHECK" and "pick" in r.text for r in o.rec.reasons)
    s = offer_summary(o)
    assert s["get_picks"][0]["key"] == theirs.key and s["dynasty_delta"] is not None and s["mode"] == "balanced"
    # the engine's own pick sweetener uses the same pick arithmetic
    eng = _TradeEngine(ctx, values, dyn, lineup_fn=False)
    ev = eng.evaluate("strong", [ctx.my_team.players[5]], [ctx.teams[0].players[2]], screen=False)
    assert eng.with_picks(ev, get_picks=[theirs]).delta_dyn == pytest.approx(o.ev.delta_dyn)
    # only the holder can send a pick
    with pytest.raises(LookupError, match="do not hold"):
        evaluate_offer(ctx, values, give=[me], get=[them], dynasty_values=dyn, give_picks=[theirs.key],
                       lineup_fn=False)
    # picks only, the counterparty from the pick holder
    only = evaluate_offer(ctx, values, give=[me], get=[], dynasty_values=dyn, get_picks=[theirs.key],
                          lineup_fn=False)
    assert only.ev.team.team_id == "strong"
    ctx2, values2 = tt.build()
    with pytest.raises(ValueError, match="dynasty"):
        evaluate_offer(ctx2, values2, give=["m_c2"], get=["a_d1"], get_picks=["pick:2027:1:A"], lineup_fn=False)


def test_counter_suggests_their_cheapest_fixing_pick(monkeypatch):
    ctx, values, dyn = tp.build()
    picks = pick_assets(ctx, values, dyn)
    big = max(a.net for a in picks["strong"])
    if big <= 0:
        pytest.skip("fixture picks carry no net value")
    import fantasy_manager.recommend.trades as tr
    real = tr._verdict

    def verdict(ev):                       # counter without a pick, accept once any pick with net > 0 is added
        if ev.get_picks and ev.delta_dyn_n > 0:
            return "accept", ["ok"]
        return ("counter", ["close"]) if not ev.get_picks else real(ev)
    monkeypatch.setattr(tr, "_verdict", verdict)
    monkeypatch.setattr(tr, "MIN_P_ACCEPT", 0.0)
    o = evaluate_offer(ctx, values, give=[ctx.my_team.players[5].cid], get=[ctx.teams[0].players[2].cid],
                       dynasty_values=dyn, lineup_fn=False)
    assert o.verdict == "counter"
    cheapest = min((a for a in picks["strong"] if a.net > 0), key=lambda a: a.value)
    assert o.counter == f"Ask Team strong to add their {cheapest.short('strong')} pick"


# -- resolving typed names -------------------------------------------------------------------

def _named(cid, name, team="EDM"):
    return Player(cid=cid, name=name, name_norm=name.lower(), ids={}, team=team, positions=["C"])


POOL = [_named("1", "Connor McDavid"), _named("2", "Leon Draisaitl"), _named("3", "Jack Hughes", "NJ"),
        _named("4", "Quinn Hughes", "VAN"), _named("5", "Tim Stützle", "OTT")]


@pytest.mark.parametrize("q,cid", [("Connor McDavid", "1"), ("mcdavid", "1"), ("c mcdavid", "1"),
                                   ("Conor McDavid", "1"), ("J Hughes", "3"), ("tim stutzle", "5"), ("2", "2")])
def test_find_players(q, cid):
    assert [p.cid for p in find_players([q], POOL)] == [cid]


def test_find_players_errors():
    with pytest.raises(LookupError, match="ambiguous"):
        find_players(["hughes"], POOL)
    with pytest.raises(LookupError, match="no player"):
        find_players(["Wayne Gretzky"], POOL)


def test_find_team():
    ctx, _ = tt.build()
    assert find_team(ctx, "A").team_id == "A"
    assert find_team(ctx, "team b").team_id == "B"
    with pytest.raises(LookupError):
        find_team(ctx, "Team")             # every team matches


def test_find_pick():
    ctx, values, dyn = tp.build(picks={
        "mid": {2027: [{"round": 1, "original_owner": "mid"}, {"round": 1, "original_owner": "weak"}]},
        "strong": {2027: [{"round": 1, "original_owner": "strong"}]},
        "weak": {2027: []}})
    all_picks = [a for ps in pick_assets(ctx, values, dyn).values() for a in ps]
    own = find_pick(all_picks, "2027 R1", "mid")
    assert own.original == "mid" and own.holder == "mid"            # own pick wins the tie
    assert find_pick(all_picks, "2027 1 (from Team weak)", "mid").original == "weak"
    assert find_pick(all_picks, "2027 round 1 weak", "mid").original == "weak"
    assert find_pick(all_picks, own.key, "mid") is own
    with pytest.raises(LookupError, match="held"):
        find_pick(all_picks, "2030 R1", "mid")
    with pytest.raises(LookupError, match="bad pick"):
        find_pick(all_picks, "first rounder", "mid")
