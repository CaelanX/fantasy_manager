import random
from datetime import date

import pytest

from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig
from fantasy_manager.recommend.lineup import optimal_lineup, recommend_lineup, starting_slots
from fantasy_manager.valuation.replacement import eligible_slots
from fantasy_manager.valuation.valuate import PlayerValue


def P(cid, pos, status="healthy"):
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team="EDM", positions=pos, status=status)


def V(p, val, proj=None, games=None):
    return PlayerValue(player=p, fpg=val, fpg_season=val, fpg_week=val, vorp=0.0, proj_week=proj,
                       games_next7=games)


def team(slotted):
    return FantasyTeam(team_id="1", name="me", owner_is_me=True,
                       slots=[RosterSlot(slot=s, player=p, starting=s not in ("BN", "IR")) for s, p in slotted])


def ctx_for(t, shape):
    return LeagueContext(provider="t", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape=shape,
                         teams=[t], free_agents=[], matchup_period=1, as_of=date(2026, 10, 7))


def test_starting_slots_expand_and_skip_bench():
    assert starting_slots({"G": 2, "C": 1, "BN": 4, "IR": 1, "UTIL": 1, "F": 1}) == ["C", "F", "UTIL", "G", "G"]


def test_known_optimum_beats_greedy():
    # greedy (best player into his first eligible slot) puts A at C and strands B -> 5 + 1 = 6
    a, b, c = P("A", ["C", "LW"]), P("B", ["C"]), P("C", ["LW"])
    t = team([("C", a), ("LW", c), ("BN", b)])
    vals = {"A": V(a, 5), "B": V(b, 4), "C": V(c, 1)}
    assign, total = optimal_lineup(t, vals, {"C": 1, "LW": 1, "BN": 1}, "season")
    assert total == pytest.approx(9.0)
    assert assign == {0: "B", 1: "A"}


def test_eligibility_rules():
    g1, g2 = P("g1", ["G"]), P("g2", ["G"])
    c, d, lw = P("c", ["C", "F"]), P("d", ["D"]), P("lw", ["LW"])
    ir_star = P("irstar", ["C"], status="healthy")
    t = team([("G", g2), ("C", c), ("F", lw), ("UTIL", d), ("BN", g1), ("IR", ir_star)])
    vals = {"g1": V(g1, 9), "g2": V(g2, 1), "c": V(c, 3), "d": V(d, 2), "lw": V(lw, 4), "irstar": V(ir_star, 50)}
    shape = {"C": 1, "F": 1, "UTIL": 1, "G": 1, "BN": 1, "IR": 1}
    assign, total = optimal_lineup(t, vals, shape, "season")
    slots = starting_slots(shape)
    got = {slots[i]: cid for i, cid in assign.items()}
    assert got == {"C": "c", "F": "lw", "UTIL": "d", "G": "g1"}   # g1 never in UTIL; IR player excluded
    assert total == pytest.approx(3 + 4 + 2 + 9)


def brute_force(players, vals, slots):
    """Try every injective partial assignment of players to slots (recursive)."""
    def rec(k, used):
        if k == len(slots):
            return 0.0
        best = rec(k + 1, used)                       # leave slot k empty
        for p in players:
            if p.cid not in used and slots[k] in eligible_slots(p.positions, [slots[k]]):
                best = max(best, vals[p.cid].fpg_season + rec(k + 1, used | {p.cid}))
        return best
    return rec(0, frozenset())


@pytest.mark.parametrize("seed", range(6))
def test_matches_brute_force(seed):
    rng = random.Random(seed)
    pos_pool = [["C"], ["LW"], ["RW"], ["C", "LW"], ["LW", "RW"], ["D"], ["G"]]
    players = [P(f"p{i}", rng.choice(pos_pool)) for i in range(8)]
    vals = {p.cid: V(p, round(rng.uniform(0, 5), 2)) for p in players}
    shape = {"C": 1, "LW": 1, "F": 1, "D": 1, "UTIL": 1, "G": 1}
    t = team([("BN", p) for p in players])
    _, total = optimal_lineup(t, vals, shape, "season")
    assert total == pytest.approx(brute_force(players, vals, starting_slots(shape)))


def test_week_horizon_prefers_projected_points():
    a, b = P("a", ["C"]), P("b", ["C"])
    t = team([("C", a), ("BN", b)])
    # a is better per game, b plays twice as often this week
    vals = {"a": V(a, 3.0, proj=6.0, games=2), "b": V(b, 2.0, proj=8.0, games=4)}
    assert optimal_lineup(t, vals, {"C": 1, "BN": 1}, "week")[0] == {0: "b"}
    assert optimal_lineup(t, vals, {"C": 1, "BN": 1}, "season")[0] == {0: "a"}


def test_ties_keep_current_starter():
    a, b = P("a", ["C"]), P("b", ["C"])
    t = team([("BN", a), ("C", b)])
    vals = {"a": V(a, 2.0), "b": V(b, 2.0)}
    assert optimal_lineup(t, vals, {"C": 1, "BN": 1}, "season")[0] == {0: "b"}


def test_recommend_lineup_swaps_and_thresholds():
    s1, s2, b1, b2 = P("s1", ["C"]), P("s2", ["D"]), P("b1", ["C"]), P("b2", ["D"])
    t = team([("C", s1), ("D", s2), ("BN", b1), ("BN", b2)])
    shape = {"C": 1, "D": 1, "BN": 2}
    vals = {"s1": V(s1, 1, proj=4.0, games=2), "b1": V(b1, 1, proj=8.0, games=4),   # +4.0 -> recommend
            "s2": V(s2, 1, proj=10.0, games=3), "b2": V(b2, 1, proj=10.4, games=4)}  # +0.4 < 0.8 (8%)
    recs = recommend_lineup(ctx_for(t, shape), vals)
    assert [r.title for r in recs] == ["Start b1 over s1"]
    codes = [x.code for x in recs[0].reasons]
    assert codes[0] == "LINEUP_GAIN" and "GAMES_NEXT7" in codes and "PROJ_WEEK" in codes
    assert recs[0].score == pytest.approx(4.0)


def test_recommend_lineup_flags_injured_starter_and_healthy_ir():
    hurt, bench, ir_ok = P("hurt", ["C"], status="out"), P("bench", ["LW"]), P("irok", ["C"])
    t = team([("C", hurt), ("BN", bench), ("IR", ir_ok)])
    shape = {"C": 1, "BN": 1, "IR": 1}
    vals = {"hurt": V(hurt, 0.0), "bench": V(bench, 2.0), "irok": V(ir_ok, 3.0)}
    titles = [r.title for r in recommend_lineup(ctx_for(t, shape), vals)]
    assert any(t.startswith("Bench hurt") for t in titles)
    assert "Activate irok from IR (healthy)" in titles
    # the injured starter is paired with a valid replacement when one exists
    repl = P("repl", ["C"])
    t2 = team([("C", hurt), ("BN", repl)])
    vals2 = {"hurt": V(hurt, 0.0), "repl": V(repl, 2.0)}
    recs = recommend_lineup(ctx_for(t2, {"C": 1, "BN": 1}), vals2)
    assert [r.title for r in recs] == ["Start repl over hurt (out)"]
    assert any(x.code == "STATUS" for x in recs[0].reasons)


def test_solver_is_fast_on_full_roster():
    import time
    pos_pool = [["C"], ["LW"], ["RW"], ["C", "LW"], ["LW", "RW"], ["D"], ["D"], ["G"]]
    rng = random.Random(1)
    players = [P(f"p{i}", rng.choice(pos_pool)) for i in range(25)]
    vals = {p.cid: V(p, rng.uniform(0, 5)) for p in players}
    shape = {"C": 2, "LW": 2, "RW": 2, "F": 1, "D": 4, "UTIL": 1, "G": 2}
    t0 = time.perf_counter()
    assign, _ = optimal_lineup(team([("BN", p) for p in players]), vals, shape, "season")
    assert time.perf_counter() - t0 < 2.0
    assert len(set(assign.values())) == len(assign)


def test_fantrax_lineup_recs_are_weekly():
    from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig
    from fantasy_manager.recommend.lineup import WEEKLY_LOCK_TEXT, recommend_lineup
    from fantasy_manager.valuation.valuate import PlayerValue

    def p(cid):
        return Player(cid=cid, name=cid, name_norm=cid, ids={}, team="EDM", positions=["C"])

    a, b = p("a"), p("b")
    vals = {x.cid: PlayerValue(player=x, fpg=f, fpg_season=f, fpg_week=f, vorp=f)
            for x, f in ((a, 1.0), (b, 3.0))}
    t = FantasyTeam(team_id="1", name="me", owner_is_me=True,
                    slots=[RosterSlot(slot="C", player=a, starting=True),
                           RosterSlot(slot="BN", player=b, starting=False)])
    for provider, weekly in (("fantrax", True), ("espn", False)):
        ctx = LeagueContext(provider=provider, league_id="1", season=2027, name="T",
                            scoring=ScoringConfig(kind="points"), roster_shape={"C": 1, "BN": 1},
                            teams=[t], free_agents=[], matchup_period=1, as_of=date(2026, 10, 1))
        recs = recommend_lineup(ctx, vals)
        assert recs and all((WEEKLY_LOCK_TEXT in r.title) == weekly for r in recs)
        assert all(("LINEUP_LOCK" in {x.code for x in r.reasons}) == weekly for r in recs)


def test_lineup_predicted_gain_is_week_points():
    s1, b1 = P("s1", ["C"]), P("b1", ["C"])
    t = team([("C", s1), ("BN", b1)])
    vals = {"s1": V(s1, 1, proj=4.0, games=2), "b1": V(b1, 1, proj=8.0, games=4)}
    r = recommend_lineup(ctx_for(t, {"C": 1, "BN": 1}), vals)[0]
    assert r.predicted_gain == pytest.approx(4.0)
    assert (r.gain_units, r.horizon_days) == ("week_pts", 7)
    assert r.strength is not None and 6.0 < r.strength < 9.0      # 3 pts -> 6, 6 pts -> 9
    no_sched = {"s1": V(s1, 1.0), "b1": V(b1, 2.0)}
    r2 = recommend_lineup(ctx_for(t, {"C": 1, "BN": 1}), no_sched)[0]
    assert r2.gain_units == "season_fpg" and r2.predicted_gain == pytest.approx(1.0)


# -- swaps come from the slot diff -----------------------------------------------------------

def N(name, pos, status="healthy"):
    cid = name.split()[-1].lower()
    return Player(cid=cid, name=name, name_norm=cid, ids={}, team="EDM", positions=pos, status=status)


def _swaps(recs):
    return [r for r in recs if any(x.code == "LINEUP_GAIN" for x in r.reasons)]


def _espn_like():
    """The live ESPN bug: suspended G Hellebuyck, IR F Marchand, weak UTIL Pinto; bench C Frost,
    C/LW Jenner and G Annunen. The old code paired "Frost over Hellebuyck" (C for a G)."""
    hel, mar, pin = N("Connor Hellebuyck", ["G"], "suspended"), N("Brad Marchand", ["LW", "RW", "F"], "ir"), \
        N("Shane Pinto", ["C", "RW", "F"])
    dra, bus = N("Leon Draisaitl", ["C", "F"]), N("Brandon Bussi", ["G"])
    fro, jen, ann = N("Morgan Frost", ["C", "F"]), N("Boone Jenner", ["C", "LW", "F"]), N("Justus Annunen", ["G"])
    t = team([("F", dra), ("F", mar), ("UTIL", pin), ("G", bus), ("G", hel),
              ("BN", fro), ("BN", jen), ("BN", ann)])
    vals = {"hellebuyck": V(hel, 0, proj=0.0), "marchand": V(mar, 0, proj=0.0), "pinto": V(pin, 1, proj=1.65),
            "draisaitl": V(dra, 3, proj=7.86), "bussi": V(bus, 1, proj=3.46), "frost": V(fro, 1, proj=4.33),
            "jenner": V(jen, 1, proj=3.65), "annunen": V(ann, 1, proj=1.10)}
    return t, vals, {"F": 2, "UTIL": 1, "G": 2, "BN": 3}


def test_suspended_goalie_is_replaced_by_a_goalie():
    t, vals, shape = _espn_like()
    recs = recommend_lineup(ctx_for(t, shape), vals)
    titles = [r.title for r in recs]
    assert titles == ["Start Morgan Frost over Brad Marchand (ir)", "Start Boone Jenner over Shane Pinto",
                      "Start Justus Annunen over Connor Hellebuyck (suspended)"]
    ann = recs[2]
    assert [p.name for p in ann.add] == ["Justus Annunen"] and [p.name for p in ann.drop] == ["Connor Hellebuyck"]
    assert ann.predicted_gain == pytest.approx(1.10) and ann.gain_units == "week_pts" and ann.horizon_days == 7
    assert ann.strength is not None and ann.subjects == []
    assert not any(t.startswith("Bench Connor Hellebuyck") for t in titles)   # covered by the swap


def test_swap_gains_add_up_to_optimal_minus_current():
    from fantasy_manager.recommend.lineup import current_total
    t, vals, shape = _espn_like()
    recs = recommend_lineup(ctx_for(t, shape), vals)
    _, opt = optimal_lineup(t, vals, shape)
    assert sum(r.predicted_gain for r in _swaps(recs)) == pytest.approx(opt - current_total(t, vals))
    assert opt - current_total(t, vals) == pytest.approx(4.33 + 2.0 + 1.10, abs=0.01)


def test_chain_move_is_one_recommendation():
    # Z (C, out) sits at C, Y (C) at UTIL; bench X is LW only, so he can only take UTIL and
    # Y must slide over to C: one rec describing the whole chain, gain v(X) - v(Z).
    z, y, x = N("Zed Out", ["C"], "out"), N("Yan Mover", ["C"]), N("Xav Bench", ["LW"])
    t = team([("C", z), ("UTIL", y), ("BN", x)])
    vals = {"out": V(z, 0, proj=0.0), "mover": V(y, 1, proj=5.0), "bench": V(x, 1, proj=3.0)}
    recs = _swaps(recommend_lineup(ctx_for(t, {"C": 1, "UTIL": 1, "BN": 1}), vals))
    assert [r.title for r in recs] == ["Start Xav Bench at UTIL for Yan Mover; move Yan Mover to C over Zed Out (out)"]
    r = recs[0]
    assert [p.cid for p in r.add] == ["bench"] and [p.cid for p in r.drop] == ["out"]
    assert [p.cid for p in r.subjects] == ["mover"]
    assert r.predicted_gain == pytest.approx(3.0) and "LINEUP_MOVE" in {x.code for x in r.reasons}


def test_empty_slot_titles():
    g1, g2 = N("Goalie One", ["G"]), N("Goalie Two", ["G"])
    t = team([("G", g1), ("BN", g2)])                       # second G slot is empty
    vals = {"one": V(g1, 1, proj=3.0), "two": V(g2, 1, proj=2.5)}
    recs = recommend_lineup(ctx_for(t, {"G": 2, "BN": 1}), vals)
    assert [r.title for r in recs] == ["Start Goalie Two at G (empty slot)"]
    assert recs[0].drop == [] and recs[0].predicted_gain == pytest.approx(2.5)
    # chain ending in an empty slot
    y, x = N("Yan Mover", ["C"]), N("Xav Bench", ["LW"])
    t2 = team([("UTIL", y), ("BN", x)])
    vals2 = {"mover": V(y, 1, proj=5.0), "bench": V(x, 1, proj=3.0)}
    recs2 = recommend_lineup(ctx_for(t2, {"C": 1, "UTIL": 1, "BN": 1}), vals2)
    assert [r.title for r in recs2] == ["Start Xav Bench at UTIL for Yan Mover; move Yan Mover to C (empty slot)"]
    assert recs2[0].predicted_gain == pytest.approx(3.0)


def test_chain_titles_keep_fantrax_weekly_lock():
    from fantasy_manager.recommend.lineup import WEEKLY_LOCK_TEXT
    z, y, x = N("Zed Out", ["C"], "out"), N("Yan Mover", ["C"]), N("Xav Bench", ["LW"])
    t = team([("C", z), ("UTIL", y), ("BN", x)])
    vals = {"out": V(z, 0, proj=0.0), "mover": V(y, 1, proj=5.0), "bench": V(x, 1, proj=3.0)}
    ctx = ctx_for(t, {"C": 1, "UTIL": 1, "BN": 1}).model_copy(update={"provider": "fantrax"})
    r = recommend_lineup(ctx, vals)[0]
    assert r.title.endswith(WEEKLY_LOCK_TEXT) and r.title.startswith("Start Xav Bench at UTIL")
    assert "LINEUP_LOCK" in {x.code for x in r.reasons}


@pytest.mark.parametrize("seed", range(40))
def test_goalies_and_skaters_never_mismatched_and_gains_add_up(seed, monkeypatch):
    from fantasy_manager.recommend import lineup as L
    monkeypatch.setattr(L, "MIN_GAIN_ABS", 0.0)
    monkeypatch.setattr(L, "MIN_GAIN_REL", 0.0)
    rng = random.Random(seed)
    pos_pool = [["C"], ["LW"], ["RW"], ["C", "LW"], ["LW", "RW"], ["C", "RW"], ["D"], ["G"], ["G"]]
    statuses = ["healthy"] * 6 + ["out", "suspended", "ir"]
    players = [P(f"p{i}", rng.choice(pos_pool), rng.choice(statuses)) for i in range(14)]
    shape = {"C": 1, "LW": 1, "RW": 1, "F": 1, "D": 2, "UTIL": 1, "G": 2, "BN": 8}
    slots = starting_slots(shape)
    rng.shuffle(players)
    slotted, used = [], set()
    for s in slots:                                    # a random valid (not optimal) lineup
        if rng.random() < 0.15:
            continue                                   # leave some slots empty
        for p in players:
            if p.cid not in used and s in eligible_slots(p.positions, [s]):
                slotted.append((s, p))
                used.add(p.cid)
                break
    slotted += [("BN", p) for p in players if p.cid not in used]
    t = team(slotted)
    vals = {p.cid: V(p, 0.0 if p.status != "healthy" else round(rng.uniform(0, 5), 2),
                     proj=0.0 if p.status != "healthy" else round(rng.uniform(0, 12), 2)) for p in players}
    recs = recommend_lineup(ctx_for(t, shape), vals)
    for r in _swaps(recs):
        group = r.add + r.drop + r.subjects
        assert len({p.is_goalie for p in group}) <= 1, r.title
    _, opt = optimal_lineup(t, vals, shape)
    assert sum(r.predicted_gain for r in _swaps(recs)) == pytest.approx(opt - L.current_total(t, vals), abs=1e-4)
