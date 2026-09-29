from datetime import date

import pytest

from fantasy_manager.models import (FantasyTeam, LeagueContext, Player, Reason, Recommendation, StatLine,
                                    RosterSlot, ScoringConfig)
from fantasy_manager.recommend.advise import advise, group_by_kind, normalize_scores
from fantasy_manager.recommend.trades import (BLOCK_PTS, _greedy_lineup_value, _to_rec, evaluate_trade,
                                              fairness, recommend_trades)
from fantasy_manager.valuation.valuate import PlayerValue

REPL = 1.0
SHAPE = {"C": 1, "LW": 1, "D": 2, "G": 1, "BN": 2, "IR": 1}   # 7 active spots


def pl(cid, pos, status="healthy"):
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team="EDM", positions=pos, status=status)


def pv(p, fpg):
    return PlayerValue(player=p, fpg=fpg, fpg_season=fpg, fpg_week=fpg, vorp=fpg - REPL,
                       vorp_week=fpg - REPL,
                       horizon_values={"season": fpg, "week": fpg, "vorp_season": fpg - REPL,
                                       "vorp_week": fpg - REPL})


def team(tid, name, mine, entries):
    return FantasyTeam(team_id=tid, name=name, owner_is_me=mine,
                       slots=[RosterSlot(slot=s, player=p, starting=s not in ("BN", "IR"))
                              for s, p in entries])


def build():
    """Me: C-rich, D-poor. Opponent A: D-rich, C-poor. Opponent B: balanced/thin."""
    spec = {
        "me": [("C", "m_c1", ["C"], 3.0), ("BN", "m_c2", ["C"], 2.8), ("LW", "m_lw", ["LW"], 2.0),
               ("D", "m_d1", ["D"], 1.0), ("D", "m_d2", ["D"], 0.4), ("G", "m_g", ["G"], 2.5),
               ("BN", "m_lw2", ["LW"], 0.3)],
        "A": [("C", "a_c", ["C"], 1.2), ("LW", "a_lw", ["LW"], 1.9), ("D", "a_d1", ["D"], 2.9),
              ("D", "a_d2", ["D"], 2.7), ("BN", "a_d3", ["D"], 1.0), ("G", "a_g", ["G"], 2.4)],
        "B": [("C", "b_c", ["C"], 1.8), ("LW", "b_lw", ["LW"], 1.7), ("D", "b_d1", ["D"], 1.6),
              ("D", "b_d2", ["D"], 1.3), ("G", "b_g", ["G"], 2.0), ("BN", "b_star", ["C", "LW"], 4.5)],
    }
    values, teams = {}, []
    for tid, rows in spec.items():
        entries = []
        for slot, cid, pos, f in rows:
            p = pl(cid, pos)
            values[cid] = pv(p, f)
            entries.append((slot, p))
        teams.append(team(tid, f"Team {tid}", tid == "me", entries))
    fas = []
    for cid, pos, f in [("fa_c", ["C"], 0.9), ("fa_d", ["D"], 0.8), ("fa_g", ["G"], 1.0)]:
        p = pl(cid, pos)
        values[cid] = pv(p, f)
        fas.append(p)
    ctx = LeagueContext(provider="test", league_id="1", season=2027, name="T",
                        scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape=SHAPE,
                        teams=teams, free_agents=fas, matchup_period=1, as_of=date(2026, 10, 1))
    return ctx, values


def key(r):
    return frozenset(p.cid for p in r.drop), frozenset(p.cid for p in r.add)


def test_greedy_lineup_value_respects_slots():
    ps = [pl("c", ["C"]), pl("lw", ["LW"]), pl("rw", ["RW"]), pl("d", ["D"]), pl("g", ["G"]),
          pl("g2", ["G"])]
    vals = {p.cid: pv(p, f) for p, f in zip(ps, [3.0, 2.0, 1.5, 1.0, 2.0, 5.0])}
    shape = {"C": 1, "F": 1, "UTIL": 1, "G": 1, "BN": 3, "IR": 1}
    # C->c, F->lw, UTIL->rw (goalie g2 is never UTIL), G->g2
    assert _greedy_lineup_value(ps, vals, shape) == pytest.approx(3.0 + 2.0 + 1.5 + 5.0)
    assert _greedy_lineup_value([ps[5]], vals, {"UTIL": 2, "BN": 1}) == 0.0


def test_fairness_bands():
    assert fairness([1.0], [1.12]) == (True, pytest.approx(0.12 / 1.12))
    assert fairness([1.0], [1.2])[0] is False
    assert fairness([2.0], [1.0, 1.0])[0] is True
    assert fairness([2.0], [1.0, 1.0], repl=0.5)[0] is False   # 2 / (2 - 0.5) = 1.33
    assert fairness([1.7], [1.0, 1.0], repl=0.5)[0] is True    # 1.7 / 1.5 = 1.13


@pytest.mark.parametrize("lineup_fn", [None, False])  # auto (optimal_lineup if present) / greedy
def test_positional_need_swap_is_proposed(lineup_fn):
    ctx, values = build()
    recs = recommend_trades(ctx, values, lineup_fn=lineup_fn)
    assert recs, "expected at least one proposal"
    swaps = [r for r in recs if r.counterparty == "Team A" and [p.cid for p in r.drop] == ["m_c2"]]
    assert swaps and [p.cid for p in swaps[0].add][0] in ("a_d1", "a_d2")
    top = swaps[0]
    codes = [r.code for r in top.reasons]
    assert codes[:3] == ["MY_EDGE", "MARKET_VIEW", "THEIR_NEED"]      # a C is Team A's weakest slot
    assert {"P_ACCEPT", "ROSTER_CONSEQUENCE", "DELTA_ME", "DELTA_THEM", "FAIR_PCT"} <= set(codes)
    reasons = {r.code: r for r in top.reasons}
    assert reasons["MY_EDGE"].value > 0.3 and reasons["P_ACCEPT"].value >= 0.25
    assert reasons["MY_EDGE"].text.startswith("You gain +")
    assert "by market value" in reasons["MARKET_VIEW"].text and "acceptance ~" in reasons["MARKET_VIEW"].text
    assert top.score == pytest.approx(reasons["MY_EDGE"].value * reasons["P_ACCEPT"].value)
    assert [r.score for r in recs] == sorted((r.score for r in recs), reverse=True)     # ranked by EV


def test_unfair_three_for_one_rejected():
    ctx, values = build()
    ev = evaluate_trade(ctx, values, give=["m_d1", "m_d2", "m_lw2"], get=["b_star"], lineup_fn=False)
    assert ev.team.team_id == "B"
    assert not ev.fair and not ev.accepted
    recs = recommend_trades(ctx, values, lineup_fn=False, limit=50, max_per_team=50)
    assert all(len(r.drop) + len(r.add) <= 3 and min(len(r.drop), len(r.add)) == 1 for r in recs)
    assert not any("b_star" in {p.cid for p in r.add} and len(r.drop) == 3 for r in recs)


def test_proposals_are_deduped():
    ctx, values = build()
    recs = recommend_trades(ctx, values, lineup_fn=False, limit=100, max_per_team=100)
    keys = [key(r) for r in recs]
    assert len(keys) == len(set(keys))
    for give, get in keys:                       # never both a trade and its mirror
        assert (get, give) not in set(keys)
    singles = {key(r): r.score for r in recs if len(r.drop) == 1 and len(r.add) == 1}
    for r in recs:                               # no 2-for-1 that only pads a better 1-for-1
        if len(r.drop) + len(r.add) == 3:
            cores = [(frozenset({g.cid}), frozenset({a.cid})) for g in r.drop for a in r.add]
            assert all(singles.get(c, float("-inf")) < r.score for c in cores)


def test_max_per_team_and_limit():
    ctx, values = build()
    recs = recommend_trades(ctx, values, lineup_fn=False, max_per_team=1, limit=10)
    teams = [r.counterparty for r in recs]
    assert len(teams) == len(set(teams))
    assert len(recommend_trades(ctx, values, lineup_fn=False, limit=1)) <= 1


def test_receiving_two_needs_a_drop():
    ctx, values = build()
    ev = evaluate_trade(ctx, values, give=["m_c2"], get=["a_d2", "a_d3"], lineup_fn=False)
    assert [p.cid for p in ev.my_drops] == ["m_lw2"]      # full roster: lowest-value non-incoming
    assert ev.their_drops == []


def test_dynasty_values_drive_fairness():
    ctx, values = build()
    dyn = {cid: 1.0 for cid in values}
    dyn["m_c2"], dyn["a_d1"] = 10.0, 1.0                 # a_d1 is old: no longer fair
    ev = evaluate_trade(ctx, values, give=["m_c2"], get=["a_d1"], dynasty_values=dyn, lineup_fn=False)
    assert not ev.fair


def test_trade_block_bonus_applies():
    ctx, values = build()
    base = evaluate_trade(ctx, values, give=["m_d1"], get=["b_d2"], lineup_fn=False)
    wanted = evaluate_trade(ctx, values, give=["m_d1"], get=["b_d2"], wants={"B": {"D"}},
                            lineup_fn=False)
    assert base.block_text is None and "trade-block wants (D)" in wanted.block_text
    assert wanted.perceived == pytest.approx(base.perceived + BLOCK_PTS) and wanted.p > base.p
    offered = evaluate_trade(ctx, values, give=["m_d1"], get=["b_d2"], offered={"B": {"b_d2"}}, lineup_fn=False)
    assert "b_d2 is on Team B's trade block" in offered.block_text
    codes = {r.code for r in _to_rec(offered, {}, "VORP", "greedy").reasons}
    assert "TRADE_BLOCK" in codes


# -- advise ---------------------------------------------------------------------------

def rec(kind, score):
    return Recommendation(kind=kind, score=score, title=f"{kind} {score}")


def test_normalize_scores_rank_based_per_kind():
    out = normalize_scores([rec("trade", 5.0), rec("trade", 1.0), rec("waiver", 0.1),
                            rec("sell_high", 3.0), rec("buy_low", 9.0)])
    by = {(r.kind, r.title): r.score for r in out}
    assert by[("trade", "trade 5.0")] == 10 and by[("trade", "trade 1.0")] == 5
    assert by[("waiver", "waiver 0.1")] == 10
    assert by[("buy_low", "buy_low 9.0")] == 10 and by[("sell_high", "sell_high 3.0")] == 5
    assert all(r.reasons[-1].code == "RAW_SCORE" for r in out)


def test_advise_merges_and_orders_by_priority():
    ctx, values = build()
    recs = advise(ctx, values, include=("trades", "waivers", "flags", "nonexistent"))
    assert recs and all(0 < r.score <= 10 for r in recs)
    assert recs[0].score == 10
    tops = [r.kind for r in recs if r.score == 10]
    order = ["injury", "lineup", "waiver", "trade", "sell_high", "buy_low"]
    assert tops == sorted(tops, key=order.index)
    groups = group_by_kind(recs)
    assert "trade" in groups and list(groups) == sorted(groups, key=order.index)


def test_advise_survives_failing_engine(monkeypatch):
    import fantasy_manager.recommend.trades as tr

    def boom(*a, **k):
        raise RuntimeError("x")
    monkeypatch.setattr(tr, "recommend_trades", boom)
    ctx, values = build()
    assert advise(ctx, values, include=("trades",)) == []


def test_ir_status_player_moves_to_ir_before_any_drop():
    ctx, values = build()
    me = ctx.my_team
    hurt = next(s.player for s in me.slots if s.player.cid == "m_lw")
    hurt.status = "ir"                                   # IR status in an active slot, IR slot free
    ev = evaluate_trade(ctx, values, give=["m_c2"], get=["a_d2", "a_d3"], lineup_fn=False)
    assert [p.cid for p in ev.my_ir] == ["m_lw"] and ev.my_drops == []
    from fantasy_manager.recommend.trades import _to_rec

    reasons = _to_rec(ev, {}, "VORP", "greedy").reasons
    assert any(r.code == "IR_MOVE" and "move m_lw to IR" in r.text for r in reasons)
    assert "ROSTER_DROP" not in {r.code for r in reasons}


def test_no_ir_slot_means_drop():
    ctx, values = build()
    ctx.roster_shape = {k: v for k, v in SHAPE.items() if k != "IR"}
    next(s.player for s in ctx.my_team.slots if s.player.cid == "m_lw").status = "ir"
    ev = evaluate_trade(ctx, values, give=["m_c2"], get=["a_d2", "a_d3"], lineup_fn=False)
    assert ev.my_ir == [] and len(ev.my_drops) == 1


def test_dynasty_drop_uses_dynasty_value():
    ctx, values = build()
    dyn = {cid: 5.0 for cid in values}
    dyn["m_lw2"] = 9.0                                   # lowest FPG but a prized prospect
    dyn["m_d2"] = 0.5                                    # lowest dynasty value
    dyn["m_c2"], dyn["a_d2"], dyn["a_d3"] = 8.0, 4.0, 4.0
    ev = evaluate_trade(ctx, values, give=["m_c2"], get=["a_d2", "a_d3"], dynasty_values=dyn,
                        lineup_fn=False)
    assert [p.cid for p in ev.my_drops] == ["m_d2"]


def test_dynasty_drop_skips_players_without_stats():
    ctx, values = build()
    values["m_lw2"].reasons.append(Reason(code="NO_DATA", text="no stats"))
    dyn = {cid: 5.0 for cid in values}
    dyn["m_lw2"], dyn["m_d2"] = 0.0, 1.0
    dyn["m_c2"], dyn["a_d2"], dyn["a_d3"] = 8.0, 4.0, 4.0
    ev = evaluate_trade(ctx, values, give=["m_c2"], get=["a_d2", "a_d3"], dynasty_values=dyn,
                        lineup_fn=False)
    assert [p.cid for p in ev.my_drops] == ["m_d2"]


# -- dynasty modes ------------------------------------------------------------------------

def dynasty_ctx(mode):
    ctx, values = build()
    ctx.dynasty, ctx.dynasty_mode = True, mode
    dyn = {cid: 1.0 for cid in values}
    dyn["m_lw"], dyn["a_d3"] = 10.0, 11.3              # fair (11.5% gap), +1.3 dynasty for me
    return ctx, values, dyn


def test_contend_rejects_trade_that_costs_this_season(monkeypatch):
    from fantasy_manager.recommend.trades import WIN_NOW_TOLERANCE, _to_rec, dynasty_scale

    patch_market(monkeypatch, {})                   # the market calls every deal even: test the mode rule
    ctx, values, dyn = dynasty_ctx("contend")
    ev = evaluate_trade(ctx, values, give=["m_lw"], get=["a_d3"], dynasty_values=dyn, lineup_fn=False)
    # LW starter 2.0 -> 0.3 backup, D 0.4 -> 1.0: this season's lineup loses 1.1 FPG
    assert ev.mode == "contend" and ev.fair and ev.delta_me == pytest.approx(-1.1)
    assert ev.delta_dyn == pytest.approx(1.3)
    assert ev.delta_dyn_n == pytest.approx(1.3 / dynasty_scale(ctx))
    assert dynasty_scale(ctx) == pytest.approx(1.0 + 0.65 + 0.45 + 0.8 ** 3 * 0.8)
    assert ev.win_now_cost and not ev.accepted and ev.accepted_future
    assert ev.delta_me < -WIN_NOW_TOLERANCE
    codes = {r.code for r in _to_rec(ev, dyn, "dynasty", "greedy").reasons}
    assert {"WIN_NOW_COST", "DYNASTY_DELTA"} <= codes

    fut = []
    recs = recommend_trades(ctx, values, dynasty_values=dyn, lineup_fn=False, future_only=fut, limit=50)
    assert all((_reason := next(x for x in r.reasons if x.code == "DELTA_ME")).value >= -WIN_NOW_TOLERANCE
               for r in recs)
    assert (frozenset({"m_lw"}), frozenset({"a_d3"})) in {key(r) for r in fut}
    assert all("WIN_NOW_COST" in {x.code for x in r.reasons} for r in fut)
    assert (frozenset({"m_lw"}), frozenset({"a_d3"})) not in {key(r) for r in recs}


def test_mode_scoring_weights():
    for mode, me_w, dyn_w in (("contend", 1.0, 0.5), ("balanced", 1.0, 1.0), ("rebuild", 0.5, 1.0)):
        ctx, values, dyn = dynasty_ctx(mode)
        ev = evaluate_trade(ctx, values, give=["m_lw"], get=["a_d3"], dynasty_values=dyn, lineup_fn=False)
        assert ev.core == pytest.approx(me_w * ev.delta_me + dyn_w * ev.delta_dyn_n), mode
        assert not ev.win_now_cost or mode == "contend"
    # non-dynasty leagues keep the plain lineup-gain rule
    ctx, values, dyn = dynasty_ctx("contend")
    ctx.dynasty = False
    ev = evaluate_trade(ctx, values, give=["m_lw"], get=["a_d3"], dynasty_values=dyn, lineup_fn=False)
    assert ev.mode is None and ev.core == pytest.approx(ev.delta_me)


def test_trade_never_forces_dropping_a_protected_prospect():
    ctx, values = build()
    ctx.dynasty = True
    kid = next(s.player for s in ctx.my_team.slots if s.player.cid == "m_lw2")
    kid.birth_date, kid.draft_overall = date(2007, 9, 30), 2      # 19-year-old #2 pick, lowest value
    dyn = {cid: 7.0 for cid in values}
    dyn["m_lw2"] = 6.0                                           # incoming 8.0 < 1.5 x 6.0
    dyn["m_c2"], dyn["a_d2"], dyn["a_d3"] = 8.0, 4.0, 4.0
    ev = evaluate_trade(ctx, values, give=["m_c2"], get=["a_d2", "a_d3"], dynasty_values=dyn, lineup_fn=False)
    assert "m_lw2" not in {p.cid for p in ev.my_drops}          # someone else goes instead
    assert ev.blocked is None
    # when the prospect is the only possible drop and incoming value < 1.5x, the trade is blocked
    for s in ctx.my_team.slots:
        if s.player is not None and s.player.cid not in ("m_lw2", "m_c2"):
            s.player.birth_date, s.player.draft_overall = date(2006, 1, 1), 10
    ev = evaluate_trade(ctx, values, give=["m_c2"], get=["a_d2", "a_d3"], dynasty_values=dyn, lineup_fn=False)
    assert ev.blocked and not ev.accepted


def test_trade_predicted_gain_is_lineup_delta_me():
    ctx, values = build()
    recs = recommend_trades(ctx, values, lineup_fn=False)
    assert recs
    for r in recs:
        dme = next(x.value for x in r.reasons if x.code == "DELTA_ME")
        assert r.predicted_gain == pytest.approx(dme)
        assert r.gain_units == "lineup_fpg" and r.horizon_days is None
        assert r.strength is not None and 0 <= r.strength <= 10
    assert [r.rank_in_kind for r in recs] == list(range(1, len(recs) + 1))
    assert {r.kind_total for r in recs} == {len(recs)}


def test_advise_sets_rank_fields_and_keeps_strength():
    recs = normalize_scores([rec("waiver", 3.0), rec("waiver", 1.0), rec("trade", 2.0),
                             rec("sell_high", 1.0), rec("buy_low", 2.0)])
    by = {(r.kind, r.rank_in_kind, r.kind_total) for r in recs}
    assert ("waiver", 1, 2) in by and ("waiver", 2, 2) in by and ("trade", 1, 1) in by
    assert ("buy_low", 1, 2) in by and ("sell_high", 2, 2) in by    # flags share one group
    assert all(r.strength is not None for r in recs)


def test_two_for_one_over_a_position_cap_drops_a_same_position_player():
    ctx, values = build()
    ctx.position_limits = {"D": 3}                        # I have 2 D and would receive 2 more
    ev = evaluate_trade(ctx, values, give=["m_c2"], get=["a_d2", "a_d3"], lineup_fn=False)
    assert [p.cid for p in ev.my_drops] == ["m_d2"]      # not m_lw2 (lowest overall): must be a D
    assert ev.my_cap == "D limit 3: would roster 4 defensemen" and ev.blocked is None
    from fantasy_manager.recommend.trades import _to_rec

    reasons = _to_rec(ev, {}, "VORP", "greedy").reasons
    cap = next(r for r in reasons if r.code == "POSITION_CAP")
    assert "D limit 3" in cap.text and "drop" in cap.text
    assert any(r.code == "ROSTER_DROP" and "m_d2" in r.text for r in reasons)


def test_position_cap_without_a_same_position_drop_blocks_the_trade():
    ctx, values = build()
    ctx.position_limits = {"C": 1}                        # Team A has one C and would get two
    ev = evaluate_trade(ctx, values, give=["m_c1", "m_c2"], get=["a_d1"], lineup_fn=False)
    assert ev.blocked and "C limit 1" in ev.blocked and not ev.accepted
    assert ev.their_cap and [p.cid for p in ev.their_drops] == ["a_c"]


def test_position_cap_under_the_limit_changes_nothing():
    ctx, values = build()
    ctx.position_limits = {"D": 4, "G": 2}
    ev = evaluate_trade(ctx, values, give=["m_c2"], get=["a_d2", "a_d3"], lineup_fn=False)
    assert [p.cid for p in ev.my_drops] == ["m_lw2"] and ev.my_cap is None


# -- market view, acceptance and EV ranking --------------------------------------------------

import json  # noqa: E402
import math  # noqa: E402
from types import SimpleNamespace  # noqa: E402

import fantasy_manager.recommend.trades as tr  # noqa: E402
from fantasy_manager.recommend.base import roster_legal_after  # noqa: E402
from fantasy_manager.recommend.trades import (DEFAULT_ACCEPT, DEPTH_WEIGHT, NEED_PTS,  # noqa: E402
                                              ROSTER_SPOT_PENALTY, AcceptParams, MarketPool,
                                              calibrate_acceptance, market_value, package_value, p_accept,
                                              percentile, team_standing)


def market_ctx():
    """12 rostered players (ADP 1..12, % rostered 100..45) + one data-less player + one FA."""
    rows = []
    values = {}
    for i in range(12):
        p = pl(f"p{i}", ["C"])
        p.adp, p.pct_owned = float(i + 1), 100.0 - 5 * i
        values[p.cid] = pv(p, 3.0 - 0.1 * i)
        rows.append(p)
    bare = pl("bare", ["C"])                     # no ADP / % rostered: falls back to our FPG
    values["bare"] = pv(bare, 2.95)
    fa = pl("fa", ["C"])
    fa.adp, fa.pct_owned = 40.0, 3.0
    values["fa"] = pv(fa, 0.5)
    teams = [team("me", "Me", True, [("C", p) for p in rows[:6]] + [("BN", bare)]),
             team("o", "Other", False, [("C", p) for p in rows[6:]])]
    ctx = LeagueContext(provider="test", league_id="1", season=2027, name="M",
                        scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape={"C": 1, "BN": 8},
                        teams=teams, free_agents=[fa], matchup_period=1, as_of=date(2026, 10, 1))
    return ctx, values, {p.cid: p for p in rows + [bare, fa]}


def test_percentile_is_mid_rank():
    ref = [1.0, 2.0, 2.0, 3.0]
    assert percentile(3.0, ref) == pytest.approx(87.5) and percentile(2.0, ref) == pytest.approx(50.0)
    assert percentile(0.0, ref) == 0.0 and percentile(9.0, ref) == 100.0 and percentile(1.0, []) == 50.0


def test_market_value_percentiles_weights_and_fallbacks():
    ctx, values, P = market_ctx()
    pool = MarketPool(ctx, values)
    top, last = market_value(P["p0"], pool), market_value(P["p11"], pool)
    # 12 rostered players report ADP / % rostered: the best sits at 11.5 / 12 of the way up
    assert top == pytest.approx(100 * 11.5 / 12) and last == pytest.approx(100 * 0.5 / 12)
    vals = [market_value(P[f"p{i}"], pool) for i in range(12)]
    assert vals == sorted(vals, reverse=True)
    assert pool.value_and_sources(P["p3"])[1] == ("adp", "owned")
    assert pool.value(P["fa"]) == 0.0              # free agents are placed against the rostered pool
    # ADP weighs twice as much as % rostered
    P["p0"].pct_owned = 0.0
    pool = MarketPool(ctx, values)
    comps = pool.components(P["p0"])
    assert pool.value(P["p0"]) == pytest.approx((2 * comps["adp"] + comps["owned"]) / 3)
    # no provider data: the percentile of our season FPG within the rostered pool (13 values)
    assert pool.value_and_sources(P["bare"]) == (pytest.approx(100 * 11.5 / 13), ("fpg",))
    # a component reported by fewer than MARKET_MIN_REF rostered players is ignored
    for i in range(5, 12):
        P[f"p{i}"].adp = None
    pool = MarketPool(ctx, values)
    assert "adp" not in pool.ref and pool.value_and_sources(P["p0"])[1] == ("owned",)


def test_market_value_uses_fantrax_projected_fpts():
    ctx, values, P = market_ctx()
    for i in range(12):
        p = P[f"p{i}"]
        p.adp = None
        p.lines["projected"] = StatLine(split="projected", gp=80, stats={"FPTS": 80 * (3.0 - 0.2 * i), "GP": 80})
    P["p11"].lines["projected"].stats["FPTS"] = 80 * 9.0        # Fantrax loves him, % rostered does not
    pool = MarketPool(ctx, values)
    assert pool.value_and_sources(P["p11"])[1] == ("owned", "proj")
    assert pool.value(P["p11"]) > pool.value(P["p10"])


def test_market_worth_is_convex_so_depth_does_not_buy_a_star():
    assert tr.market_worth(100.0) == 100.0 and tr.market_worth(50.0) == pytest.approx(12.5)
    assert tr.market_worth(-5.0) == 0.0 and tr.market_worth(120.0) == 100.0
    # a deep pool: the #2 player (99.5th pct) vs a #42 (86th) + a #150 (62nd)
    star = tr.market_worth(99.5)
    two = package_value([tr.market_worth(86.0), tr.market_worth(62.0)])
    assert star - two > 20                                            # clearly not a fair offer
    # mid pool, 10 percentile points stay roughly 10 worth points
    assert 7.0 < tr.market_worth(60.0) - tr.market_worth(50.0) < 11.0


def test_package_value_discounts_depth():
    assert package_value([80.0]) == 80.0
    assert package_value([60.0, 80.0]) == pytest.approx(80.0 + DEPTH_WEIGHT * 60.0)
    assert package_value([]) == 0.0


def test_logistic_anchors_and_inverse():
    a = DEFAULT_ACCEPT
    assert a.p(0) == pytest.approx(0.5) and a.p(10) == pytest.approx(0.75) and a.p(-10) == pytest.approx(0.25)
    assert a.p(60) == pytest.approx(a.ceiling) and a.p(-1e6) >= 0.0
    assert a.perceived_for(0.25) == pytest.approx(-10.0) and a.perceived_for(0.75) == pytest.approx(10.0)


class StubMarket:
    def __init__(self, m):
        self.m = m

    def value(self, p):
        return self.m.get(p.cid, 0.0)

    def sources(self, ps):
        return ["adp", "owned"]


def deal(ctx, give, get, **kw):
    by = {p.cid: p for t in ctx.teams for p in t.players}
    base = dict(give=[by[c] for c in give], get=[by[c] for c in get], their_drops=[], their_ir=[],
                their_fill=None, need_text=None, block_text=None, delta_them=0.0)
    base.update(kw)
    return SimpleNamespace(**base)


def test_p_accept_logistic_and_modifiers(monkeypatch):
    monkeypatch.setattr(tr, "MARKET_CONVEXITY", 1.0)       # worth = percentile: test the curve itself
    ctx, values = build()
    A = next(t for t in ctx.teams if t.team_id == "A")
    even = p_accept(deal(ctx, ["m_c2"], ["a_d2"]), A, ctx, StubMarket({"m_c2": 60, "a_d2": 60}))
    assert even.perceived == 0 and even.p == pytest.approx(0.5) and even.legal
    theirs = p_accept(deal(ctx, ["m_c2"], ["a_d2"]), A, ctx, StubMarket({"m_c2": 70, "a_d2": 60}))
    mine = p_accept(deal(ctx, ["m_c2"], ["a_d2"]), A, ctx, StubMarket({"m_c2": 50, "a_d2": 60}))
    assert theirs.p == pytest.approx(0.75) and mine.p == pytest.approx(0.25)
    m = StubMarket({"m_c2": 60, "a_d2": 60, "a_c": 5})
    bonus = p_accept(deal(ctx, ["m_c2"], ["a_d2"], need_text="fills C", block_text="block"), A, ctx, m)
    assert bonus.perceived == pytest.approx(NEED_PTS + tr.BLOCK_PTS) and bonus.p > 0.5
    by = {p.cid: p for p in A.players}
    drop = p_accept(deal(ctx, ["m_c2"], ["a_d2"], their_drops=[by["a_c"]]), A, ctx, m)
    assert drop.perceived == pytest.approx(-ROSTER_SPOT_PENALTY) and any("drop a_c" in n for n in drop.notes)
    # an illegal roster for them (C limit 1: they already have a_c) -> 0
    ctx.position_limits = {"C": 1}
    ill = p_accept(deal(ctx, ["m_c2"], ["a_d2"]), A, ctx, m)
    assert ill.p == 0.0 and not ill.legal and "C limit 1" in ill.legal_note
    assert roster_legal_after(A, ctx, add=deal(ctx, ["m_c2"], []).give)[0] is False


def test_p_accept_dynasty_standing_modifiers(monkeypatch):
    monkeypatch.setattr(tr, "MARKET_CONVEXITY", 1.0)
    ctx, values = build()
    ctx.dynasty = True
    records = {"A": (10, 0, 0), "me": (5, 5, 0), "B": (0, 10, 0)}
    for t in ctx.teams:
        t.record = records[t.team_id]
    assert team_standing(ctx) == {"A": "contender", "me": "contender", "B": "rebuilder"}
    A = next(t for t in ctx.teams if t.team_id == "A")
    B = next(t for t in ctx.teams if t.team_id == "B")
    m = StubMarket({"m_c2": 60, "a_d2": 60, "b_d2": 60})
    no_help = p_accept(deal(ctx, ["m_c2"], ["a_d2"], delta_them=-0.1), A, ctx, m)
    helps = p_accept(deal(ctx, ["m_c2"], ["a_d2"], delta_them=0.4), A, ctx, m)
    assert no_help.p == pytest.approx(0.5 * tr.CONTENDER_FUTURE_MULT) and helps.p == pytest.approx(0.5)
    by = {p.cid: p for t in ctx.teams for p in t.players}
    by["m_c2"].birth_date, by["b_d2"].birth_date = date(1991, 1, 1), date(2004, 1, 1)
    older = p_accept(deal(ctx, ["m_c2"], ["b_d2"]), B, ctx, m)
    assert older.p == pytest.approx(0.5 * tr.REBUILDER_VETERAN_MULT)
    ctx.dynasty = False                              # redraft leagues ignore standings
    assert p_accept(deal(ctx, ["m_c2"], ["a_d2"], delta_them=-0.1), A, ctx, m).p == pytest.approx(0.5)
    for t in ctx.teams:
        t.record = (0, 0, 0)
    assert team_standing(ctx) == {}                   # preseason: nobody is a contender yet


def ev_ctx():
    """Me: surplus C (m_c2, benched), weak LW (1.0) and very weak D (0.5). X: an LW that is a
    small upgrade for me and a D that is a big one."""
    shape = {"C": 1, "LW": 1, "D": 1, "BN": 1}
    spec = {"me": [("C", "m_c", ["C"], 2.0), ("LW", "m_lw", ["LW"], 1.0), ("D", "m_d", ["D"], 0.5),
                   ("BN", "m_c2", ["C"], 1.9)],
            "X": [("C", "x_c", ["C"], 1.0), ("LW", "x_lw", ["LW"], 1.6), ("D", "x_d", ["D"], 2.5),
                  ("BN", "x_bn", ["D"], 0.2)]}
    values, teams = {}, []
    for tid, rows in spec.items():
        entries = []
        for slot, cid, pos, f in rows:
            p = pl(cid, pos)
            values[cid] = pv(p, f)
            entries.append((slot, p))
        teams.append(team(tid, f"Team {tid}", tid == "me", entries))
    ctx = LeagueContext(provider="test", league_id="1", season=2027, name="E",
                        scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape=shape,
                        teams=teams, free_agents=[], matchup_period=1, as_of=date(2026, 10, 1))
    return ctx, values


def patch_market(monkeypatch, m):
    """Fixed market values; worth = percentile so the numbers below read directly."""
    monkeypatch.setattr(tr, "MARKET_CONVEXITY", 1.0)
    monkeypatch.setattr(tr.MarketPool, "value_and_sources", lambda self, p: (m.get(p.cid, 50.0), ("adp",)))


def test_ev_ranking_prefers_a_likely_small_win_over_an_unlikely_big_one(monkeypatch):
    ctx, values = ev_ctx()
    # m_c2 fills X's weakest slot (C, +5). Small deal: +25 for them by market -> p capped at 0.9.
    # Big deal: -14.8 + 5 = -9.8 -> p ~0.255, barely plausible.
    patch_market(monkeypatch, {"m_c2": 50.0, "x_lw": 30.0, "x_d": 64.8})
    small = evaluate_trade(ctx, values, give=["m_c2"], get=["x_lw"], lineup_fn=False)
    big = evaluate_trade(ctx, values, give=["m_c2"], get=["x_d"], lineup_fn=False)
    assert small.delta_me == pytest.approx(0.6) and big.delta_me == pytest.approx(2.0)
    assert small.p == pytest.approx(0.9) and 0.25 <= big.p < 0.26
    assert small.accepted and big.accepted
    assert small.ev > big.ev and big.delta_me > small.delta_me
    recs = recommend_trades(ctx, values, lineup_fn=False, limit=20, max_per_team=20, max_per_given=20,
                            sweet_spot=[])
    titles = [r.title for r in recs]
    i_small, i_big = titles.index("Trade m_c2 to Team X for x_lw"), titles.index("Trade m_c2 to Team X for x_d")
    assert i_small < i_big and recs[i_big].predicted_gain > recs[i_small].predicted_gain
    # below the plausibility floor the big deal is not proposed at all
    patch_market(monkeypatch, {"m_c2": 50.0, "x_lw": 30.0, "x_d": 66.0})
    assert not evaluate_trade(ctx, values, give=["m_c2"], get=["x_d"], lineup_fn=False).accepted


def test_min_gain_is_point_three(monkeypatch):
    ctx, values = ev_ctx()
    patch_market(monkeypatch, {"m_c2": 50.0, "x_lw": 50.0})
    values["x_lw"] = pv(values["x_lw"].player, 1.35)          # +0.35 for me: enough now
    assert evaluate_trade(ctx, values, give=["m_c2"], get=["x_lw"], lineup_fn=False).accepted
    values["x_lw"] = pv(values["x_lw"].player, 1.25)          # +0.25: not enough
    assert not evaluate_trade(ctx, values, give=["m_c2"], get=["x_lw"], lineup_fn=False).accepted


def test_sweet_spot_detection(monkeypatch):
    ctx, values = ev_ctx()
    patch_market(monkeypatch, {"m_c2": 50.0, "x_lw": 58.0, "x_d": 90.0})   # -8 + 5 need = -3
    ev = evaluate_trade(ctx, values, give=["m_c2"], get=["x_lw"], lineup_fn=False)
    assert ev.perceived == pytest.approx(-3.0) and ev.sweet_spot
    assert ev.sweet_spot_score == pytest.approx(0.6 - tr.MARKET_PTS_TO_FPG * 3.0)
    sweet: list = []
    recs = recommend_trades(ctx, values, lineup_fn=False, sweet_spot=sweet)
    assert "Trade m_c2 to Team X for x_lw" in [r.title for r in sweet]
    s = {x.code: x for x in next(r for r in sweet if r.title == "Trade m_c2 to Team X for x_lw").reasons}
    assert s["SWEET_SPOT"].value == pytest.approx(ev.sweet_spot_score)
    assert "market calls it fair" in s["SWEET_SPOT"].text
    assert "SWEET_SPOT" in {x.code for x in recs[0].reasons}
    # the market says it clearly favours them: a fine proposal, but not a model-vs-market sweet spot
    patch_market(monkeypatch, {"m_c2": 50.0, "x_lw": 40.0, "x_d": 90.0})
    ev = evaluate_trade(ctx, values, give=["m_c2"], get=["x_lw"], lineup_fn=False)
    assert ev.accepted and not ev.sweet_spot
    # our model gain below 0.4: not a sweet spot either
    patch_market(monkeypatch, {"m_c2": 50.0, "x_lw": 55.0, "x_d": 90.0})
    values["x_lw"] = pv(values["x_lw"].player, 1.35)
    ev = evaluate_trade(ctx, values, give=["m_c2"], get=["x_lw"], lineup_fn=False)
    assert ev.accepted and not ev.sweet_spot


def test_diversity_caps_per_team_and_per_player_given():
    ctx, values = build()
    loose = recommend_trades(ctx, values, lineup_fn=False, limit=50, max_per_team=50, max_per_given=50,
                             sweet_spot=[])
    capped = recommend_trades(ctx, values, lineup_fn=False, limit=50, sweet_spot=[])

    def counts(recs):
        teams, given, got = {}, {}, {}
        for r in recs:
            teams[r.counterparty] = teams.get(r.counterparty, 0) + 1
            for p in r.drop:
                given[p.cid] = given.get(p.cid, 0) + 1
            for p in r.add:
                got[p.cid] = got.get(p.cid, 0) + 1
        return teams, given, got
    lt, lg, _ = counts(loose)
    assert max(lt.values()) > 3 or max(lg.values()) > 2            # the caps bind on this league
    ct, cg, cr = counts(capped)
    assert max(ct.values()) <= 3 and max(cg.values()) <= tr.MAX_PER_GIVEN and max(cr.values()) <= 2
    assert [r.score for r in capped] == sorted((r.score for r in capped), reverse=True)


def test_sweet_spots_get_reserved_slots_without_a_collector():
    """advise / the web Moves page only see the returned list: sweet-spot deals keep a place."""
    ctx, values = build()
    sweet: list = []
    plain = recommend_trades(ctx, values, lineup_fn=False, limit=4, sweet_spot=sweet)
    reserved = recommend_trades(ctx, values, lineup_fn=False, limit=4)
    assert sweet and len(plain) <= 4 and len(reserved) <= 4
    assert sweet[0].title in {r.title for r in reserved}
    assert [r.score for r in reserved] == sorted((r.score for r in reserved), reverse=True)


def test_roster_legality_is_enforced_for_both_sides():
    ctx, values = build()
    ctx.roster_shape = {**SHAPE, "BN": 3}          # room for 8 by slots ...
    ctx.max_roster_size = 7                        # ... but the league caps rosters at 7
    ev = evaluate_trade(ctx, values, give=["m_c2"], get=["a_d2", "a_d3"], lineup_fn=False)
    assert ev.my_drops == [] and ev.my_illegal and "roster limit 7" in ev.my_illegal and not ev.accepted
    ctx.max_roster_size = 6                        # Team A holds 6 and would receive 2 for 1
    ev = evaluate_trade(ctx, values, give=["m_c1", "m_c2"], get=["a_d1"], lineup_fn=False)
    assert ev.my_illegal is None and not ev.acceptance.legal and ev.p == 0.0 and not ev.accepted
    recs = recommend_trades(ctx, values, lineup_fn=False, limit=50, max_per_team=50, max_per_given=50)
    assert not any(r.counterparty == "Team A" and len(r.drop) == 2 for r in recs)


def test_future_only_keeps_win_now_cost_with_acceptance(monkeypatch):
    patch_market(monkeypatch, {})
    ctx, values, dyn = dynasty_ctx("contend")
    fut: list = []
    recommend_trades(ctx, values, dynasty_values=dyn, lineup_fn=False, future_only=fut, limit=50)
    r = next(r for r in fut if key(r) == (frozenset({"m_lw"}), frozenset({"a_d3"})))
    codes = {x.code for x in r.reasons}
    assert {"WIN_NOW_COST", "DYNASTY_DELTA", "P_ACCEPT", "MARKET_VIEW"} <= codes
    p = next(x.value for x in r.reasons if x.code == "P_ACCEPT")
    assert r.score == pytest.approx(r.predicted_gain * p) and r.gain_units == "dynasty"


class FakeLedger:
    def __init__(self, obs):
        self.obs = obs

    def query(self, sql, args=()):
        if "rec_episodes" in sql:
            return [{"league": "L", "rec_key": f"k{i}", "status": "followed" if y else "proposed"}
                    for i, (_, y) in enumerate(self.obs)]
        i = int(args[1][1:])
        return [{"reasons_json": json.dumps([{"code": "MARKET_VIEW", "text": "", "value": self.obs[i][0]}])}]


def test_calibrate_acceptance_keeps_the_prior_until_twenty_proposals():
    few = calibrate_acceptance(FakeLedger([(0.0, 1)] * 5))
    assert (few.x0, few.k, few.n, few.source) == (DEFAULT_ACCEPT.x0, DEFAULT_ACCEPT.k, 5, "prior")
    assert calibrate_acceptance(object()).n == 0                  # unreadable ledger: the prior
    # managers in this league are harder to convince: true midpoint +8 points
    true = AcceptParams(x0=8.0, k=DEFAULT_ACCEPT.k, ceiling=1.0)
    obs = []
    for i in range(241):
        x = -30.0 + 0.25 * i
        u = (i * 0.6180339887) % 1.0
        obs.append((x, 1 if u < true.p(x) else 0))
    fit = calibrate_acceptance(FakeLedger(obs))
    assert fit.n == 241 and fit.source.startswith("fit on") and fit.x0 > 4.0 and fit.k > 0
    assert math.isfinite(fit.p(0.0)) and fit.p(0.0) < 0.5


def test_digest_trade_lines_show_the_reader_reasons():
    from fantasy_manager.report.digest import TRADE_REASONS, _md_rec

    ctx, values = build()
    r = recommend_trades(ctx, values, lineup_fn=False)[0]
    lines = [ln for ln in _md_rec(r) if ln.startswith("- ")]
    shown = [x.text for x in r.reasons if x.code in TRADE_REASONS]
    assert lines == [f"- {t}" for t in shown] and lines[0].startswith("- You gain +")
    assert not any("My lineup" in ln for ln in lines)          # DELTA_ME detail stays in the app


def test_market_ranks_per_game_projections_within_goalies_and_skaters():
    skaters, goalies, values = [], [], {}
    for i in range(10):
        s, g = pl(f"s{i}", ["C"]), pl(f"g{i}", ["G"])
        s.lines["projected"] = StatLine(split="projected", gp=80, stats={"FPTS": 80 * (1.0 + 0.1 * i), "GP": 80})
        g.lines["projected"] = StatLine(split="projected", gp=60, stats={"FPTS": 60 * (3.0 + 0.1 * i), "GP": 60})
        skaters.append(s)
        goalies.append(g)
    ctx = LeagueContext(provider="test", league_id="1", season=2027, name="G",
                        scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape={"C": 1, "G": 1, "BN": 20},
                        teams=[team("me", "Me", True, [("BN", p) for p in skaters]),
                               team("o", "O", False, [("BN", p) for p in goalies])],
                        free_agents=[], matchup_period=1, as_of=date(2026, 10, 1))
    pool = MarketPool(ctx, values)
    # the best skater projection is as valuable as the best goalie projection, not below every goalie
    assert pool.value(skaters[-1]) == pytest.approx(pool.value(goalies[-1])) == pytest.approx(95.0)
    assert pool.value(skaters[0]) == pytest.approx(5.0)


def test_dynasty_leagues_price_dynasty_value_into_the_market():
    ctx, values, P = market_ctx()
    dyn = {cid: 10.0 - i for i, cid in enumerate(P)}
    assert "dyn" not in MarketPool(ctx, values, dyn).value_and_sources(P["p0"])[1]    # redraft: ignored
    ctx.dynasty = True
    pool = MarketPool(ctx, values, dyn)
    assert pool.value_and_sources(P["p0"])[1] == ("adp", "dyn", "owned")
    assert "dynasty value" in tr.MARKET_SOURCE_LABEL.values()


def test_dynasty_delta_charges_each_extra_roster_spot():
    ctx, values, dyn = dynasty_ctx("balanced")
    ctx.roster_shape = {**SHAPE, "BN": 3}           # room to take two for one without a drop
    V, _, repl = tr.trade_values(ctx, values, dyn)
    ev = evaluate_trade(ctx, values, give=["m_c2"], get=["a_d2", "a_d3"], dynasty_values=dyn, lineup_fn=False)
    assert ev.my_drops == [] and ev.my_fill is None
    assert ev.delta_dyn == pytest.approx(V["a_d2"] + V["a_d3"] - V["m_c2"] - repl)
    one = evaluate_trade(ctx, values, give=["m_lw"], get=["a_d3"], dynasty_values=dyn, lineup_fn=False)
    assert one.delta_dyn == pytest.approx(V["a_d3"] - V["m_lw"])                  # 1-for-1: no spot cost
