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


# -- gain units (GAIN_WEEK / GAIN_SEASON) and exploit trades -----------------------------------

from datetime import timedelta  # noqa: E402

from fantasy_manager.recommend.base import (DEFAULT_GAMES_PER_WEEK, SEASON_GAMES, GameRate,  # noqa: E402
                                            game_rate, gain_units_text, lineup_players, season_bounds)
from fantasy_manager.recommend.trades import (PRESSURE_PTS, exploit_opportunities, relieves,  # noqa: E402
                                              team_pressures)


def test_game_rate_from_my_starters_schedule():
    ctx, values = build()
    as_of = ctx.as_of                                     # Thu Oct 1 2026
    # EDM (every starter's team) plays every other day for 10 weeks from Oct 1; MTL daily (not mine)
    ctx.schedule = {"EDM": [as_of + timedelta(days=2 * i) for i in range(35)],
                    "MTL": [as_of + timedelta(days=i) for i in range(70)]}
    first, last = season_bounds(ctx)
    assert first == as_of and last == as_of + timedelta(days=69)
    starters = lineup_players(ctx.my_team)
    assert {p.cid for p in starters} == {"m_c1", "m_lw", "m_d1", "m_d2", "m_g"}   # bench / IR excluded
    rate = game_rate(ctx)
    assert rate.source == "schedule" and rate.remaining == pytest.approx(35.0)
    assert rate.per_week == pytest.approx(35.0 / 10.0)                            # 70 days = 10 weeks
    # half the season played: only the remaining games count
    ctx.as_of = as_of + timedelta(days=35)
    assert game_rate(ctx).remaining == pytest.approx(17.0)
    # a starter whose NHL team is not on the schedule is ignored; nobody on it -> the defaults
    for p in starters:
        p.team = "XXX"
    assert game_rate(ctx).source == "default"


def test_game_rate_defaults_without_a_schedule():
    ctx, values = build()
    ctx.as_of = date(2026, 9, 29)                         # preseason: the whole season is left
    rate = game_rate(ctx)
    assert rate == GameRate(DEFAULT_GAMES_PER_WEEK, float(SEASON_GAMES), "default")
    ctx.season_start = date(2026, 10, 7)
    ctx.as_of = date(2027, 1, 11)                         # 96 of 192 days played (Oct 7 .. Apr 16)
    rate = game_rate(ctx)
    assert rate.per_week == DEFAULT_GAMES_PER_WEEK
    assert rate.remaining == pytest.approx(82 * 96 / 192)
    ctx.as_of = date(2027, 5, 1)                          # regular season over
    assert game_rate(ctx) == GameRate(0.0, 0.0, "season over")


def test_gain_units_text_and_reasons():
    rate = GameRate(3.5, 83.0, "schedule")
    assert gain_units_text(0.66, rate) == "+0.66/g · +2.3/wk · +55/season"
    ctx, values = build()
    ctx.as_of = date(2026, 9, 29)
    r = recommend_trades(ctx, values, lineup_fn=False)[0]
    reasons = {x.code: x for x in r.reasons}
    d = reasons["DELTA_ME"].value
    assert reasons["GAIN_WEEK"].value == pytest.approx(d * DEFAULT_GAMES_PER_WEEK)
    assert reasons["GAIN_SEASON"].value == pytest.approx(d * 82)
    assert reasons["GAIN_WEEK"].text.startswith(f"{d * 3.4:+.1f} pts/week")
    assert reasons["GAIN_SEASON"].text.startswith(f"{d * 82:+.0f} pts rest of season")
    assert f"({d * 3.4:+.1f} pts/week, {d * 82:+.0f} rest of season)" in reasons["MY_EDGE"].text
    codes = [x.code for x in r.reasons]
    assert codes[:2] == ["MY_EDGE", "MARKET_VIEW"]         # reader-facing order unchanged


def test_cli_trade_gain_text():
    from fantasy_manager.cli import trade_gain_text

    r = Recommendation(kind="trade", score=1.0, title="t", reasons=[
        Reason(code="DELTA_ME", text="", value=0.66), Reason(code="GAIN_WEEK", text="", value=2.31),
        Reason(code="GAIN_SEASON", text="", value=54.8)])
    assert trade_gain_text(r) == "+0.66/g · +2.3/wk · +55/season"
    assert trade_gain_text(Recommendation(kind="trade", score=1.0, title="t", reasons=[
        Reason(code="DELTA_ME", text="", value=-0.4)])) == "-0.40/g"


def exploit_ctx():
    """Me: surplus C on the bench, a weak goalie. X: 3 goalies at the G limit 3, one of them on IR,
    and a weak C. Y: a small, healthy roster (no pressure)."""
    shape = {"C": 1, "LW": 1, "D": 2, "G": 1, "BN": 2, "IR": 1}
    spec = {"me": [("C", "m_c1", ["C"], 3.0), ("BN", "m_c2", ["C"], 2.8), ("LW", "m_lw", ["LW"], 2.0),
                   ("D", "m_d1", ["D"], 1.5), ("D", "m_d2", ["D"], 1.4), ("G", "m_g", ["G"], 1.0),
                   ("BN", "m_lw2", ["LW"], 0.3)],
            "X": [("C", "x_c", ["C"], 1.2), ("LW", "x_lw", ["LW"], 1.9), ("D", "x_d1", ["D"], 2.0),
                  ("D", "x_d2", ["D"], 1.8), ("G", "x_g1", ["G"], 2.6), ("BN", "x_g2", ["G"], 2.4),
                  ("IR", "x_g3", ["G"], 2.5)],
            "Y": [("C", "y_c", ["C"], 2.0), ("LW", "y_lw", ["LW"], 2.0), ("D", "y_d1", ["D"], 2.0),
                  ("D", "y_d2", ["D"], 2.0), ("G", "y_g", ["G"], 2.0), ("BN", "y_g2", ["G"], 1.5)]}
    values, teams = {}, []
    for tid, rows in spec.items():
        entries = []
        for slot, cid, pos, f in rows:
            p = pl(cid, pos, status="ir" if cid == "x_g3" else "healthy")
            values[cid] = pv(p, f)
            entries.append((slot, p))
        teams.append(team(tid, f"Team {tid}", tid == "me", entries))
    ctx = LeagueContext(provider="test", league_id="1", season=2027, name="X",
                        scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape=shape,
                        teams=teams, free_agents=[], matchup_period=1, as_of=date(2026, 10, 1),
                        position_limits={"G": 3})
    return ctx, values


def _team(ctx, tid):
    return next(t for t in ctx.teams if t.team_id == tid)


def test_pressure_position_cap_with_an_injured_player():
    ctx, values = exploit_ctx()
    X, Y = _team(ctx, "X"), _team(ctx, "Y")
    prs = team_pressures(ctx, values, X)
    cap = [p for p in prs if p.kind == "cap"]
    assert len(cap) == 1 and cap[0].position == "G"
    assert cap[0].text.startswith("3 goalies at the G limit 3 with x_g3 injured")
    assert {p.cid for p in cap[0].pool} == {"x_g1", "x_g2", "x_g3"}
    by = {p.cid: p for t in ctx.teams for p in t.players}
    assert relieves(cap[0], [by["m_c2"]], [by["x_g2"]], ctx, values)          # a goalie leaves X
    assert not relieves(cap[0], [by["m_g"]], [by["x_g2"]], ctx, values)       # goalie for goalie: still full
    assert not relieves(cap[0], [by["m_c2"]], [by["x_c"]], ctx, values)
    assert team_pressures(ctx, values, Y) == []                               # 2 goalies, nobody hurt
    by["x_g3"].status = "healthy"                                             # full but healthy: no need
    assert not [p for p in team_pressures(ctx, values, X) if p.kind == "cap"]
    ctx.position_limits = {"G": 2}                                            # over the limit
    assert [p.text for p in team_pressures(ctx, values, X) if p.kind == "cap"] == ["3 goalies, over the G limit 2"]


def test_pressure_ir_logjam():
    ctx, values = exploit_ctx()
    ctx.position_limits = {}
    X = _team(ctx, "X")
    by = {p.cid: p for p in X.players}
    assert not [p for p in team_pressures(ctx, values, X) if p.kind == "ir"]  # 1 injured, 1 IR slot
    by["x_d2"].status = "out"
    ir = [p for p in team_pressures(ctx, values, X) if p.kind == "ir"]
    assert len(ir) == 1 and ir[0].text == "2 injured players (x_d2 + x_g3) for 1 IR slot"
    mine = {p.cid: p for p in ctx.my_team.players}
    assert relieves(ir[0], [mine["m_lw2"]], [by["x_d2"]], ctx, values)
    assert not relieves(ir[0], [mine["m_lw2"]], [by["x_d1"]], ctx, values)


def test_pressure_goalie_shortage_needs_games_this_week():
    ctx, values = exploit_ctx()
    ctx.position_limits = {}
    Y = _team(ctx, "Y")
    by = {p.cid: p for p in Y.players}
    by["y_g2"].status = "ir"                                                  # one healthy goalie left
    assert not [p for p in team_pressures(ctx, values, Y) if p.kind == "goalies"]   # no schedule: unknown
    ctx.schedule = {"EDM": [ctx.as_of + timedelta(days=30)]}                  # no games this week
    assert not [p for p in team_pressures(ctx, values, Y) if p.kind == "goalies"]
    ctx.schedule = {"EDM": [ctx.as_of + timedelta(days=2)]}
    g = [p for p in team_pressures(ctx, values, Y) if p.kind == "goalies"]
    assert len(g) == 1 and g[0].text == "only 1 healthy goalie with games this week"
    mine = {p.cid: p for p in ctx.my_team.players}
    assert relieves(g[0], [mine["m_g"]], [by["y_c"]], ctx, values)
    assert not relieves(g[0], [mine["m_g"]], [by["y_g"]], ctx, values)       # swaps their only healthy one
    values["m_g"].games_next7 = 0                                            # my goalie does not play
    assert not relieves(g[0], [mine["m_g"]], [by["y_c"]], ctx, values)


def test_pressure_weak_slot_below_minus_one():
    ctx, values = exploit_ctx()
    ctx.position_limits = {}
    Y = _team(ctx, "Y")
    w = [p for p in team_pressures(ctx, values, Y, weakest=("D", -1.4), median_weak={"D": 0.3})
         if p.kind == "weak"]
    assert len(w) == 1 and w[0].text == "a weak D slot (starter VORP -1.40, league median +0.30)"
    assert not [p for p in team_pressures(ctx, values, Y, weakest=("D", -0.9)) if p.kind == "weak"]
    empty = [p for p in team_pressures(ctx, values, Y, weakest=("D", -math.inf)) if p.kind == "weak"]
    assert empty[0].text == "an empty D slot"
    mine = {p.cid: p for p in ctx.my_team.players}
    assert relieves(w[0], [mine["m_d2"]], [], ctx, values)                   # D, VORP +0.4 > -1.4
    assert not relieves(w[0], [mine["m_c2"]], [], ctx, values)               # not a D


def test_exploit_takes_the_surplus_goalie_and_respects_legality(monkeypatch):
    patch_market(monkeypatch, {"m_c2": 60.0, "x_g2": 64.0, "x_g1": 70.0, "x_g3": 64.0})
    ctx, values = exploit_ctx()
    recs = exploit_opportunities(ctx, values, lineup_fn=False)
    assert recs and all(r.kind == "trade" for r in recs)
    assert {r.counterparty for r in recs} == {"Team X"}                      # Y is under no pressure
    top = recs[0]
    codes = [x.code for x in top.reasons]
    assert codes[:3] == ["MY_EDGE", "MARKET_VIEW", "EXPLOIT"] and {"GAIN_WEEK", "GAIN_SEASON"} <= set(codes)
    ex = next(x for x in top.reasons if x.code == "EXPLOIT")
    assert ex.text.startswith("Exploit: Team X has 3 goalies at the G limit 3") and ex.value == PRESSURE_PTS
    assert any(p.is_goalie for p in top.add) and not any(p.is_goalie for p in top.drop)
    p_acc = next(x for x in top.reasons if x.code == "P_ACCEPT")
    assert "+8 relieves their roster pressure" in p_acc.text and p_acc.value >= 0.25
    # the same deal without the pressure bonus: exactly PRESSURE_PTS less in their view
    plain = evaluate_trade(ctx, values, give=[p.cid for p in top.drop], get=[p.cid for p in top.add],
                           lineup_fn=False)
    mv = next(x for x in top.reasons if x.code == "MARKET_VIEW").value
    assert mv == pytest.approx(plain.perceived + PRESSURE_PTS)
    for r in recs:                                                           # both rosters stay legal
        mine = [p for p in ctx.my_team.players if p.cid not in {x.cid for x in r.drop}] + list(r.add)
        assert sum(p.is_goalie for p in mine) <= 3
        assert next(x for x in r.reasons if x.code == "MY_EDGE").value > 0.3
    # limit caps the list; at my own G limit, taking a goalie forces me to drop mine (still legal)
    assert len(exploit_opportunities(ctx, values, lineup_fn=False, limit=1)) == 1
    ctx.position_limits = {"G": 1}                                           # I hold 1 G: at my limit
    capped = exploit_opportunities(ctx, values, lineup_fn=False, limit=20)
    assert capped
    for r in capped:
        if any(p.is_goalie for p in r.add) and not any(p.is_goalie for p in r.drop):
            texts = {x.code: x.text for x in r.reasons}
            assert "you drop m_g" in texts["ROSTER_CONSEQUENCE"] and "POSITION_CAP" in texts


def test_exploit_skips_fa_pickups_without_moves_left(monkeypatch):
    patch_market(monkeypatch, {})
    ctx, values = exploit_ctx()
    fa = pl("fa_lw", ["LW"])
    values["fa_lw"] = pv(fa, 1.9)
    ctx.free_agents = [fa]

    def fills(recs):
        return [r for r in recs if any(x.code == "FA_FILL" for x in r.reasons)]

    assert fills(exploit_opportunities(ctx, values, lineup_fn=False))        # 2-for-1 + my FA pickup
    ctx.moves_limit_per_period, ctx.moves_used_this_period = 2, 1
    r = fills(exploit_opportunities(ctx, values, lineup_fn=False))[0]
    budget = next(x for x in r.reasons if x.code == "MOVE_BUDGET")
    assert budget.text == "The free-agent pickup uses a move (1 of 2 moves left this week)" and budget.value == 1
    ctx.moves_used_this_period = 2                                           # 0 moves left
    none_left = exploit_opportunities(ctx, values, lineup_fn=False)
    assert none_left and not fills(none_left)                                 # a straight deal instead


def test_web_trade_info_gain_units_and_exploit():
    from fantasy_manager.web import views

    r = Recommendation(kind="trade", score=1.0, title="t", reasons=[
        Reason(code="MY_EDGE", text="", value=0.66), Reason(code="P_ACCEPT", text="", value=0.5),
        Reason(code="DELTA_ME", text="", value=0.66), Reason(code="GAIN_WEEK", text="", value=2.31),
        Reason(code="GAIN_SEASON", text="", value=54.8),
        Reason(code="EXPLOIT", text="Exploit: Team X has 3 goalies", value=8.0)])
    info = views.trade_info(r)
    assert info["units_text"] == "+2.3 pts/week · +55 pts rest of season"
    assert info["gain_text"] == "+0.66 pts/game · +2.3 pts/week · +55 pts rest of season"
    assert info["exploit"] == "Exploit: Team X has 3 goalies"


# -- regression: ΔMe / ΔThem = optimal(after) - optimal(before), season FPG ---------------------
# Live Fantrax dynasty deal (balanced): "Trade Nylander + Misa to Dee for Vejmelka" showed ΔMe
# +2.35 (an FA pickup that would start anyway was credited to the trade) and ΔThem -4.31 (Dee
# cut Hofer, the backup goalie it has to start after giving up Vejmelka, leaving a G slot empty).

import random  # noqa: E402

FX_SHAPE = {"F": 5, "D": 3, "G": 2, "BN": 6, "IR": 2}


def gv(p, season, share):
    """Goalie: ``fpg_season`` is already start-share adjusted (fpg x share)."""
    fpg = season / share
    return PlayerValue(player=p, fpg=fpg, fpg_season=season, fpg_week=season, vorp=season - REPL,
                       vorp_week=season - REPL, start_share=share,
                       horizon_values={"season": season, "week": season})


def fx_ctx(fas=(("kantserov", ["F"], 4.75), ("fa_d", ["D"], 3.0), ("fa_g", ["G"], 2.5))):
    me = [("F", "nylander", ["F"], 5.15), ("F", "panarin", ["F"], 4.89), ("F", "tippett", ["F"], 4.37),
          ("F", "eichel", ["F"], 5.41), ("F", "holloway", ["F"], 4.69), ("D", "hughes", ["D"], 4.62),
          ("D", "jones", ["D"], 3.43), ("D", "sanderson", ["D"], 4.18), ("G", "blackwood", ["G"], (3.40, 0.51)),
          ("G", "saros", ["G"], (4.41, 0.62)), ("BN", "byfield", ["F"], 3.86), ("BN", "stenberg", ["F"], 3.44),
          ("BN", "frondell", ["F"], 4.26), ("BN", "lafreniere", ["F"], 3.78), ("BN", "misa", ["F"], 3.40),
          ("BN", "snuggerud", ["F"], 4.08), ("IR", "jarvis", ["F"], 4.5)]
    dee = [("F", "johnston", ["F"], 5.10), ("F", "martone", ["F"], 4.89), ("F", "pastrnak", ["F"], 6.06),
           ("F", "scheifele", ["F"], 5.01), ("F", "keller", ["F"], 4.81), ("D", "fox", ["D"], 4.39),
           ("D", "lacombe", ["D"], 3.58), ("D", "werenski", ["D"], 5.12), ("G", "greaves", ["G"], (4.06, 0.55)),
           ("G", "vejmelka", ["G"], (4.64, 0.76)), ("BN", "rakell", ["F"], 4.45), ("BN", "eklund", ["F"], 3.76),
           ("BN", "miller", ["F"], 4.70), ("BN", "raymond", ["F"], 4.39), ("BN", "thomas", ["F"], 4.39),
           ("BN", "hofer", ["G"], (3.77, 0.54))]
    values, teams = {}, []
    for tid, rows in (("me", me), ("dee", dee)):
        entries = []
        for slot, cid, pos, f in rows:
            p = pl(cid, pos, "ir" if slot == "IR" else "healthy")
            values[cid] = gv(p, *f) if isinstance(f, tuple) else pv(p, f)
            entries.append((slot, p))
        teams.append(team(tid, "Dee_snuts69" if tid == "dee" else "Me", tid == "me", entries))
    fa_players = []
    for cid, pos, f in fas:
        p = pl(cid, pos)
        values[cid] = pv(p, f)
        fa_players.append(p)
    ctx = LeagueContext(provider="test", league_id="1", season=2027, name="FX",
                        scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape=FX_SHAPE,
                        teams=teams, free_agents=fa_players, matchup_period=1, as_of=date(2026, 10, 1))
    ctx.dynasty, ctx.dynasty_mode = True, "balanced"
    # dynasty value: Hofer is Dee's least valuable asset (the old drop rule cut him), Misa a prospect
    dyn = {cid: values[cid].fpg_season for cid in values}
    dyn.update({"hofer": 0.5, "eklund": 1.0, "misa": 8.0, "stenberg": 6.0})
    return ctx, values, dyn


@pytest.mark.parametrize("lineup_fn", [None, False])      # optimal_lineup / built-in greedy
def test_fantrax_nylander_misa_for_vejmelka_is_a_small_gain(lineup_fn):
    ctx, values, dyn = fx_ctx()
    ev = evaluate_trade(ctx, values, give=["nylander", "misa"], get=["vejmelka"], dynasty_values=dyn,
                        lineup_fn=lineup_fn)
    # G: Vejmelka 4.64 for Blackwood 3.40 (+1.24); F: Nylander 5.15 out, Tippett 4.37 keeps his F
    # slot because Kantserov (4.75) is a pickup I can make without the trade too (-0.78)
    assert 0.1 <= ev.delta_me <= 0.7
    assert ev.delta_me == pytest.approx(1.24 - 0.78)
    assert ev.my_fill is not None and ev.my_fill.cid == "kantserov"
    # Dee: Vejmelka 4.64 -> Hofer 3.77 in goal (-0.87), Nylander 5.15 over Keller 4.81 (+0.34);
    # Dee cuts a bench skater, never Hofer (the goalie it must now start)
    assert -1.5 <= ev.delta_them <= -0.3
    assert ev.delta_them == pytest.approx(-0.87 + 0.34)
    assert [p.cid for p in ev.their_drops] == ["eklund"]
    dm = next(x for x in _to_rec(ev, dyn, "dynasty", "greedy").reasons if x.code == "DELTA_ME")
    assert dm.value == pytest.approx(ev.delta_me) and "same pickup of kantserov" in dm.text


@pytest.mark.parametrize("lineup_fn", [None, False])
def test_an_fa_who_would_start_anyway_is_not_a_trade_gain(lineup_fn):
    # an FA valued far above my starters (Imama: 6.27 from one game) is the pickup, but the
    # trade is credited only with what it adds on top of making that pickup without it
    ctx, values, dyn = fx_ctx(fas=(("imama", ["F"], 6.27), ("kantserov", ["F"], 4.75), ("fa_g", ["G"], 2.5)))
    ev = evaluate_trade(ctx, values, give=["nylander", "misa"], get=["vejmelka"], dynasty_values=dyn,
                        lineup_fn=lineup_fn)
    assert ev.my_fill.cid == "imama"
    assert 0.1 <= ev.delta_me <= 0.7 and ev.delta_me == pytest.approx(1.24 - 0.78)


def test_fa_fill_takes_an_open_starting_slot_and_counts_when_he_starts():
    # I give both my goalies for a skater: the fill is the best FA goalie (an open G slot the
    # roster cannot fill), not the best FA by FPG, and his starts count (nobody else can start)
    ctx, values, dyn = fx_ctx()
    ev = evaluate_trade(ctx, values, give=["blackwood", "saros"], get=["pastrnak"], dynasty_values=dyn,
                        lineup_fn=False)
    assert ev.my_fill.cid == "fa_g"
    # G: 3.40 + 4.41 -> 2.50 + empty; F: Pastrnak 6.06 over Tippett 4.37
    assert ev.delta_me == pytest.approx(2.50 - 3.40 - 4.41 + 6.06 - 4.37)
    # a 2-for-1 with no starting need picks up depth at the outgoing players' position
    ctx, values, dyn = fx_ctx(fas=(("fa_d", ["D"], 3.0), ("fa_f", ["F"], 1.0), ("fa_g", ["G"], 1.0)))
    ev = evaluate_trade(ctx, values, give=["byfield", "stenberg"], get=["eklund"], dynasty_values=dyn,
                        lineup_fn=False)
    assert ev.my_fill.cid == "fa_f" and ev.delta_me == pytest.approx(0.0)


def test_trading_equal_players_changes_nothing():
    ctx, values, dyn = fx_ctx()
    for mine, theirs in (("eichel", "scheifele"), ("saros", "greaves"), ("frondell", "raymond")):
        values[theirs] = values[theirs].model_copy(update={
            "fpg": values[mine].fpg, "fpg_season": values[mine].fpg_season, "fpg_week": values[mine].fpg_week,
            "start_share": values[mine].start_share})
        for fn in (None, False):
            ev = evaluate_trade(ctx, values, give=[mine], get=[theirs], dynasty_values=dyn, lineup_fn=fn)
            assert ev.delta_me == pytest.approx(0.0, abs=1e-9)
            assert ev.delta_them == pytest.approx(0.0, abs=1e-9)


def _random_league(rng, bench):
    shape = {"F": 3, "D": 2, "G": 1, "BN": bench}
    kinds = ["F"] * 3 + ["D"] * 2 + ["G"]

    def roster(tag):
        pos = kinds + [rng.choice("FFDG") for _ in range(bench)]
        return [(("BN" if i >= len(kinds) else pos[i]), pl(f"{tag}{i}", [pos[i]])) for i in range(len(pos))]

    values = {}
    rosters = {"me": roster("m"), "o": roster("o")}
    for entries in rosters.values():
        for _, p in entries:
            values[p.cid] = pv(p, round(rng.uniform(1.0, 6.0), 2))
    fas = [pl(f"fa{i}", [rng.choice("FDG")]) for i in range(5)]
    for p in fas:
        values[p.cid] = pv(p, round(rng.uniform(0.5, 5.0), 2))
    ctx = LeagueContext(provider="test", league_id="1", season=2027, name="R",
                        scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape=shape,
                        teams=[team("me", "Me", True, rosters["me"]), team("o", "O", False, rosters["o"])],
                        free_agents=fas, matchup_period=1, as_of=date(2026, 10, 1))
    return ctx, values


@pytest.mark.parametrize("seed", range(6))
def test_delta_me_never_exceeds_what_the_trade_brings(seed):
    """ΔMe <= Σ incoming FPG - Σ outgoing FPG + best FA FPG when every rostered player starts;
    with bench depth, Σ outgoing FPG becomes the lineup value the outgoing players carry
    (optimal(before) - optimal(before without them)), as a bench player can replace them."""
    rng = random.Random(seed)
    for bench in (0, 3):
        ctx, values = _random_league(rng, bench)
        me, them = ctx.my_team, next(t for t in ctx.teams if not t.owner_is_me)
        best_fa = max(values[p.cid].fpg_season for p in ctx.free_agents)
        mine, theirs = [p.cid for p in me.players], [p.cid for p in them.players]
        shape = ctx.roster_shape
        for _ in range(12):
            n_give, n_get = rng.choice([(1, 1), (2, 1), (1, 2)])
            give, get = rng.sample(mine, n_give), rng.sample(theirs, n_get)
            s_in = sum(values[c].fpg_season for c in get)
            s_out = sum(values[c].fpg_season for c in give)
            before = _greedy_lineup_value(me.players, values, shape)
            carried = before - _greedy_lineup_value([p for p in me.players if p.cid not in give], values, shape)
            assert carried <= s_out + 1e-9
            for fn in (None, False):
                ev = evaluate_trade(ctx, values, give=give, get=get, team_id="o", lineup_fn=fn)
                assert ev.delta_me <= s_in - carried + best_fa + 1e-9
                if bench == 0:
                    assert ev.delta_me <= s_in - s_out + best_fa + 1e-9
