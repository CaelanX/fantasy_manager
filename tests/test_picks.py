from datetime import date

import pytest

import fantasy_manager.recommend.trades as tr
from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig
from fantasy_manager.recommend.market import market_disagrees
from fantasy_manager.recommend.trades import Acceptance, _TradeEngine, _to_rec, p_accept
from fantasy_manager.valuation.picks import (DRAFT_CLASS_DISCOUNT, expected_slots, holdings, slot_curve,
                                             value_picks)
from fantasy_manager.valuation.valuate import PlayerValue

SHAPE = {"C": 1, "LW": 1, "D": 2, "G": 1, "BN": 2, "IR": 1}


def pl(cid, pos, career_gp=None):
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team="EDM", positions=pos, status="healthy",
                  career_gp=career_gp)


def pv(p, fpg):
    return PlayerValue(player=p, fpg=fpg, fpg_season=fpg, fpg_week=fpg, vorp=fpg - 1.0, vorp_week=fpg - 1.0,
                       horizon_values={"season": fpg, "week": fpg, "vorp_season": fpg - 1.0,
                                       "vorp_week": fpg - 1.0})


def build(picks=None, record=None):
    """Three teams: 'strong' (high FPG / dynasty), 'mid', 'weak'. Every player is unproven (no prior
    seasons), so all of them anchor the slot curve."""
    spec = {"strong": 3.0, "mid": 2.0, "weak": 1.0}
    values, teams, dyn = {}, [], {}
    for tid, f in spec.items():
        entries = []
        for i, (slot, pos) in enumerate([("C", ["C"]), ("LW", ["LW"]), ("D", ["D"]), ("D", ["D"]),
                                         ("G", ["G"]), ("BN", ["C"])]):
            p = pl(f"{tid}_{i}", pos, career_gp=10)
            values[p.cid] = pv(p, f + 0.1 * i)
            dyn[p.cid] = 10.0 * f + i
            entries.append((slot, p))
        teams.append(FantasyTeam(team_id=tid, name=f"Team {tid}", owner_is_me=tid == "mid",
                                 slots=[RosterSlot(slot=s, player=p, starting=s not in ("BN", "IR"))
                                        for s, p in entries],
                                 record=(record or {}).get(tid)))
    if picks is None:
        picks = {tid: {2027: [{"round": r, "original_owner": tid} for r in (1, 2)],
                       2028: [{"round": r, "original_owner": tid} for r in (1, 2)]} for tid in spec}
    ctx = LeagueContext(provider="test", league_id="1", season=2027, name="T",
                        scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape=SHAPE,
                        teams=teams, free_agents=[], matchup_period=1, as_of=date(2026, 11, 1),
                        season_start=date(2026, 10, 7), dynasty=True, dynasty_mode="balanced",
                        draft_picks=picks)
    return ctx, values, dyn


def _pick(picks, holder, year, rnd, orig=None):
    return next(a for a in picks if a.holder == holder and a.year == year and a.round == rnd
                and a.original == (orig or holder))


def test_weakest_team_picks_first_and_its_pick_is_worth_more():
    ctx, values, dyn = build()
    slots = expected_slots(ctx, values, dyn, 0)
    assert slots["weak"] < slots["mid"] < slots["strong"]
    picks = value_picks(ctx, values, dyn)
    assert _pick(picks, "weak", 2027, 1).value > _pick(picks, "strong", 2027, 1).value
    assert _pick(picks, "weak", 2027, 1).value > _pick(picks, "weak", 2027, 2).value   # round 1 > round 2


def test_future_picks_are_discounted_and_pulled_to_the_middle():
    ctx, values, dyn = build()
    now, later = expected_slots(ctx, values, dyn, 0), expected_slots(ctx, values, dyn, 1)
    assert abs(later["weak"] - 2.0) < abs(now["weak"] - 2.0)       # toward the middle slot (2 of 3)
    picks = value_picks(ctx, values, dyn)
    assert _pick(picks, "weak", 2028, 1).value < _pick(picks, "weak", 2027, 1).value


def test_slot_curve_is_league_prospects_discounted():
    ctx, values, dyn = build()
    curve = slot_curve(ctx, dyn)
    assert curve[0] == pytest.approx(DRAFT_CLASS_DISCOUNT * max(dyn.values()))
    assert curve == sorted(curve, reverse=True)


def test_record_moves_next_draft_order_late_in_the_season():
    # strongest roster but a losing record most of the way through the season
    ctx, values, dyn = build(record={"strong": (2, 18, 0), "mid": (10, 10, 0), "weak": (18, 2, 0)})
    ctx.as_of = date(2027, 4, 1)
    slots = expected_slots(ctx, values, dyn, 0)
    assert slots["strong"] < slots["weak"]


def test_unreported_teams_hold_their_own_picks_minus_known_ones():
    ctx, values, dyn = build(picks={"mid": {2027: [{"round": 1, "original_owner": "mid"},
                                                    {"round": 1, "original_owner": "weak"}]}})
    rows = holdings(ctx)
    assert ("mid", 2027, 1, "weak", False) in rows
    assert ("strong", 2027, 1, "strong", True) in rows
    assert not any(h == "weak" and o == "weak" for h, _, _, o, _ in rows)   # weak's R1 is known to be mine


def test_net_is_value_over_the_roster_spot(monkeypatch):
    import fantasy_manager.valuation.picks as pk

    ctx, values, dyn = build()
    monkeypatch.setattr(pk, "roster_spot_value", lambda ctx, values: 24.0)
    for a in value_picks(ctx, values, dyn):
        assert a.net == pytest.approx(max(0.0, a.value - 24.0 * 0.8 ** a.years_ahead))
    assert any(a.net > 0.0 for a in value_picks(ctx, values, dyn))      # early picks still add value
    assert any(a.net == 0.0 for a in value_picks(ctx, values, dyn))     # late picks add nothing


def test_no_picks_outside_dynasty():
    ctx, values, dyn = build()
    ctx.dynasty = False
    assert value_picks(ctx, values, dyn) == []


def test_p_accept_counts_a_pick_sweetener():
    ctx, values, dyn = build()
    eng = _TradeEngine(ctx, values, dyn, lineup_fn=False)
    ev = eng.evaluate("strong", [ctx.my_team.players[0]], [ctx.teams[0].players[0]], screen=False)
    mine = sorted(eng.picks["mid"], key=lambda a: -a.value)[0]
    ev.give_picks = [mine]
    with_pick = p_accept(ev, ev.team, ctx, eng.market, standing={})
    ev.give_picks = []
    without = p_accept(ev, ev.team, ctx, eng.market, standing={})
    assert with_pick.perceived > without.perceived


def _fake_accept(perceived_base):
    """An acceptance that reads only the picks: each received pick costs them 10 worth points."""
    def accept(ev):
        perceived = perceived_base - 10.0 * len(ev.get_picks) + 10.0 * len(ev.give_picks)
        p = tr.DEFAULT_ACCEPT.p(perceived)
        return Acceptance(gets=0.0, gives=0.0, perceived=perceived, p_raw=p, p=p)
    return accept


def test_overpay_asks_for_their_pick_back_and_rec_shows_it(monkeypatch):
    ctx, values, dyn = build()
    eng = _TradeEngine(ctx, values, dyn, lineup_fn=False)
    ev = eng.evaluate("strong", [ctx.my_team.players[5]], [ctx.teams[0].players[2]], screen=False)
    eng._accept = _fake_accept(25.0)          # clearly in their favour
    ev.acceptance = eng._accept(ev)
    ev.delta_me, ev.delta_dyn, ev.delta_dyn_n = 1.0, 0.0, 0.0
    var = eng.with_pick(ev)
    assert var is not None and var.get_picks and not var.give_picks
    assert var.delta_dyn == pytest.approx(var.get_picks[0].net)
    assert var.ev > ev.ev
    rec = _to_rec(var, eng.V, eng.units, "greedy", eng.market)
    assert "their 2027 R" in rec.title or "their 2028 R" in rec.title
    assert any(r.code == "PICK_IN" for r in rec.reasons)
    pick_checks = [r for r in rec.reasons if r.code == "MARKET_CHECK" and "pick" in r.text]
    assert pick_checks and all("model-only" in r.text and r.value is None for r in pick_checks)
    # a model-only pick check never counts as a disagreement on its own
    rec.reasons = [r for r in rec.reasons if r.code != "MARKET_CHECK" or "pick" in r.text]
    rec.reasons = [r for r in rec.reasons if r.code != "MARKET_VIEW"]
    assert market_disagrees(rec) is False


def test_refused_deal_is_rescued_with_my_cheapest_pick():
    ctx, values, dyn = build()
    eng = _TradeEngine(ctx, values, dyn, lineup_fn=False)
    ev = eng.evaluate("strong", [ctx.my_team.players[5]], [ctx.teams[0].players[2]], screen=False)
    eng._accept = _fake_accept(-15.0)         # they would refuse
    ev.acceptance = eng._accept(ev)
    ev.delta_me, ev.delta_dyn, ev.delta_dyn_n = 10.0, 0.0, 0.0   # worth more to me than any pick
    assert not ev.accepted
    var = eng.with_pick(ev)
    assert var is not None and var.give_picks and var.accepted
    assert var.give_picks[0].net == min(a.net for a in eng.picks["mid"])
    rec = _to_rec(var, eng.V, eng.units, "greedy", eng.market)
    assert "your 20" in rec.title and any(r.code == "PICK_OUT" for r in rec.reasons)
