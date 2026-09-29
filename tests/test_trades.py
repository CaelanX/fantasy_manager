from datetime import date

import pytest

from fantasy_manager.models import (FantasyTeam, LeagueContext, Player, Reason, Recommendation,
                                    RosterSlot, ScoringConfig)
from fantasy_manager.recommend.advise import advise, group_by_kind, normalize_scores
from fantasy_manager.recommend.trades import (_greedy_lineup_value, evaluate_trade, fairness,
                                              recommend_trades)
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
    top = recs[0]
    assert top.kind == "trade" and top.counterparty == "Team A"
    assert [p.cid for p in top.drop] == ["m_c2"]
    assert [p.cid for p in top.add][0] in ("a_d1", "a_d2")
    codes = [r.code for r in top.reasons]
    assert codes[:5] == ["VALUE_IN", "VALUE_OUT", "FAIR_PCT", "DELTA_ME", "DELTA_THEM"]
    assert "THEIR_NEED" in codes       # a C is Team A's weakest slot
    reasons = {r.code: r for r in top.reasons}
    assert reasons["DELTA_ME"].value > 0.5 and reasons["FAIR_PCT"].value <= 0.12
    them_eff = reasons["DELTA_THEM"].value + reasons["THEIR_NEED"].value
    assert top.score == pytest.approx(reasons["DELTA_ME"].value + 0.5 * min(them_eff, 1.0))


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


def test_wants_bonus_applies():
    ctx, values = build()
    base = evaluate_trade(ctx, values, give=["m_d1"], get=["b_d2"], lineup_fn=False)
    wanted = evaluate_trade(ctx, values, give=["m_d1"], get=["b_d2"], wants={"B": {"D"}},
                            lineup_fn=False)
    assert wanted.need_bonus >= base.need_bonus and wanted.need_bonus == 1.0


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


def test_contend_rejects_trade_that_costs_this_season():
    from fantasy_manager.recommend.trades import WIN_NOW_TOLERANCE, _to_rec, dynasty_scale

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
    recs = recommend_trades(ctx, values, dynasty_values=dyn, lineup_fn=False, future_only=fut)
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
