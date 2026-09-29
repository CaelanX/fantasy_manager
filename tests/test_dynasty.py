from datetime import date

import pytest

from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.valuation.dynasty import (DISCOUNT, MAX_GROWTH, PRODUCTION_CURVES, DynastyValue,
                                               apply_dynasty, dynasty_rank, growth_ratio, horizon_value,
                                               market_values, mode_weights, pedigree_multiplier,
                                               production_curve, trajectory_factor)
from fantasy_manager.valuation.valuate import PlayerValue

AS_OF = date(2026, 10, 1)


def born(age_years: float) -> date:
    return date(AS_OF.year - int(age_years), AS_OF.month, AS_OF.day)


def player(cid, positions, birth=None, gp=40, stats=None, **kw):
    lines = {"season": StatLine(split="season", gp=gp, stats=dict(stats or {}, GP=gp))} if gp else {}
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team="BOS", positions=positions,
                  birth_date=birth, lines=lines, **kw)


def pv(p, fpg):
    return PlayerValue(player=p, fpg=fpg, fpg_season=fpg, fpg_week=fpg, vorp=0.0)


def ctx_for(players, horizon=3, mode="balanced", fas=()):
    slots = [RosterSlot(slot="BN", player=p, starting=False) for p in players]
    return LeagueContext(provider="fantrax", league_id="x", season=2027, name="t",
                         scoring=ScoringConfig(kind="points"), roster_shape={}, as_of=AS_OF,
                         teams=[FantasyTeam(team_id="1", name="a", owner_is_me=True, slots=slots)],
                         free_agents=list(fas), matchup_period=None, dynasty=True,
                         keeper_horizon_years=horizon, dynasty_mode=mode)


# -- production curves ---------------------------------------------------------------

# F and D: fitted from NHL history (valuation/fitted_params.json); G: hand-set.
@pytest.mark.parametrize("group,age,level", [
    ("F", 18, 0.762), ("F", 19, 0.762), ("F", 20, (0.762 + 0.897) / 2), ("F", 21, 0.897), ("F", 24, 0.99),
    ("F", 25, 1.00), ("F", 26, (1.0 + 0.969) / 2), ("F", 29, 0.897), ("F", 33, 0.697), ("F", 35, 0.576),
    ("F", 40, 0.459), ("F", 16, 0.762),
    ("D", 20, (0.848 + 0.946) / 2), ("D", 23, 1.00), ("D", 29, 0.943), ("D", 33, 0.831), ("D", 35, 0.746),
    ("G", 22, 0.75), ("G", 24, 0.85), ("G", 30, 1.00), ("G", 34, 0.85), ("G", 36, 0.70), ("G", 40, 0.70)])
def test_production_curve_anchors(group, age, level):
    assert production_curve(group, age) == pytest.approx(level)


def test_production_curve_unknown():
    assert production_curve("F", None) == 1.0
    assert production_curve("X", 21) == pytest.approx(0.897)  # unknown group -> forward curve
    assert growth_ratio("F", None, 2) == 1.0


@pytest.mark.parametrize("group", ["F", "D", "G"])
def test_curves_rise_to_peak_then_decline(group):
    pts = PRODUCTION_CURVES[group]
    peak = [a for a, m in pts if m == 1.0]
    lo, hi = min(peak), max(peak)
    ages = [x / 4 for x in range(16 * 4, 42 * 4)]
    vals = [(a, production_curve(group, a)) for a in ages]
    pre = [m for a, m in vals if a <= lo]
    post = [m for a, m in vals if a >= hi]
    assert all(x <= y + 1e-12 for x, y in zip(pre, pre[1:]))      # growth into the peak
    assert all(x >= y - 1e-12 for x, y in zip(post, post[1:]))    # decline after it
    assert all(m == pytest.approx(1.0) for a, m in vals if lo <= a <= hi)


def test_fitted_curves_peak_at_25_forwards_23_defense():
    assert max(PRODUCTION_CURVES["F"], key=lambda pt: pt[1])[0] == 25
    assert max(PRODUCTION_CURVES["D"], key=lambda pt: pt[1])[0] == 23
    # defense arrives earlier and declines slower than forwards
    assert production_curve("D", 21) > production_curve("F", 21)
    assert production_curve("D", 33) > production_curve("F", 33)


def test_production_curves_come_from_the_packaged_fit():
    from fantasy_manager.valuation import params
    assert PRODUCTION_CURVES["F"] == params.dynasty_age_curves()["F"]
    assert PRODUCTION_CURVES["D"] == params.dynasty_age_curves()["D"]
    assert params.dynasty_age_curves({}) == params.FALLBACK_DYNASTY_CURVES   # no file: fallback


def test_growth_ratio_and_cap():
    assert growth_ratio("F", 19, 2) == pytest.approx(0.897 / 0.762)
    assert trajectory_factor("F", 19, 2) == pytest.approx(0.897 / 0.762)
    assert trajectory_factor("F", 17, 8, 1.3) == pytest.approx(1.3 / 0.762)   # below the cap
    assert growth_ratio("F", 17, 8) * 2.0 > MAX_GROWTH
    assert trajectory_factor("F", 17, 8, 2.0) == pytest.approx(MAX_GROWTH)   # capped


def test_nineteen_year_old_forward_trajectory_grows():
    kid = player("kid", ["C", "F"], born(19))
    out = apply_dynasty({"kid": pv(kid, 3.0)}, ctx_for([kid]))["kid"]
    # value of the age-21 season (year 2) exceeds the age-19 season (year 0) per game
    assert 3.0 * growth_ratio("F", 19, 2) > 3.0
    assert out.upside > 0 and out.value > horizon_value(3.0, "F", 26, 3)
    assert out.age_mult == pytest.approx(0.762, abs=1e-3)


# -- modes --------------------------------------------------------------------------------

def test_mode_weights():
    assert mode_weights("contend", 3) == ([1.0, 0.65, 0.45], 0.8)
    assert mode_weights("balanced", 3) == ([1.0, 0.8, 0.64], 1.5)
    assert mode_weights("rebuild", 3) == ([0.7, 0.9, 0.9], 2.0)
    assert mode_weights(None, 3) == mode_weights("contend", 3)       # default mode
    w, _ = mode_weights("balanced", 5)
    assert w == pytest.approx([1.0, 0.8, 0.64, 0.512, 0.4096])       # geometric extension
    w, _ = mode_weights("rebuild", 4)
    assert w[3] == pytest.approx(0.9 * DISCOUNT)                     # flat tail extends at 0.8
    assert mode_weights("contend", 1)[0] == [1.0]


def test_balanced_formula_prime_age():
    vet = player("vet", ["C", "F"], born(25))
    out = apply_dynasty({"vet": pv(vet, 3.0)}, ctx_for([vet]))["vet"]
    g = [growth_ratio("F", out.age, y) for y in (1, 2, 3)]            # past the peak at 25: < 1
    exp = 3.0 * (1.0 + 0.8 * g[0] + 0.64 * g[1]) + 0.8 ** 3 * 3.0 * g[2] * 1.5
    assert all(x < 1.0 for x in g)
    assert out.value == pytest.approx(exp, abs=0.01)
    assert out.upside < 0 and out.mode == "balanced"
    horizon = next(r for r in out.reasons if r.code == "DYNASTY_HORIZON")
    assert horizon.value == pytest.approx(exp, abs=0.01) and horizon.baseline == 3.0


def test_contend_values_now_rebuild_values_later():
    star = player("star", ["C", "F"], born(29))
    kid = player("kid", ["C", "F"], born(19), draft_overall=5, draft_round=1, career_gp=0)
    vals = {"star": pv(star, 4.2), "kid": pv(kid, 3.0)}
    contend = apply_dynasty(vals, ctx_for([star, kid], mode="contend"))
    rebuild = apply_dynasty(vals, ctx_for([star, kid], mode="rebuild"))
    assert contend["star"].value > contend["kid"].value
    assert rebuild["kid"].value > rebuild["star"].value
    # the mode argument overrides the context
    assert apply_dynasty(vals, ctx_for([star, kid], mode="contend"), mode="rebuild")["kid"].value == \
        pytest.approx(rebuild["kid"].value)


def test_young_vs_old_forward():
    young = player("young", ["C", "F"], born(22))
    old = player("old", ["LW", "F"], born(33))
    out = apply_dynasty({"young": pv(young, 3.0), "old": pv(old, 3.0)}, ctx_for([young, old]))
    assert out["young"].value > out["old"].value
    assert out["old"].upside < 0                                   # decline below a flat trajectory
    assert {"AGE_CURVE", "DYNASTY_HORIZON"} <= {r.code for r in out["young"].reasons}


# -- pedigree ------------------------------------------------------------------------------

def test_pedigree_tiers_and_fade():
    def mult(age, **kw):
        return pedigree_multiplier(player("p", ["C"], **kw), age)[0]
    assert mult(19, draft_overall=2, draft_round=1, career_gp=0) == pytest.approx(1.30)
    assert mult(19, draft_overall=8, draft_round=1, career_gp=0) == pytest.approx(1.20)
    assert mult(19, draft_overall=20, draft_round=1, career_gp=0) == pytest.approx(1.10)
    assert mult(19, draft_overall=44, draft_round=2, career_gp=0) == pytest.approx(1.03)
    assert mult(19, draft_overall=120, draft_round=4, career_gp=0) == 1.0
    assert mult(19) == 1.0                                          # undrafted / unknown
    # fades linearly between 23 and 25 ...
    assert mult(23, draft_overall=1, draft_round=1, career_gp=0) == pytest.approx(1.30)
    assert mult(24, draft_overall=1, draft_round=1, career_gp=0) == pytest.approx(1.15)
    assert mult(25, draft_overall=1, draft_round=1, career_gp=0) == 1.0
    assert mult(26, draft_overall=1, draft_round=1, career_gp=0) == 1.0
    # ... and with career NHL games
    assert mult(20, draft_overall=1, draft_round=1, career_gp=100) == pytest.approx(1.15)
    assert mult(20, draft_overall=1, draft_round=1, career_gp=250) == 1.0
    assert mult(None, draft_overall=1, draft_round=1) == 1.0


def test_pedigree_only_lifts_future_years():
    a = player("a", ["C", "F"], born(20), draft_overall=1, draft_round=1, career_gp=0)
    b = player("b", ["C", "F"], born(20))
    out = apply_dynasty({"a": pv(a, 3.0), "b": pv(b, 3.0)}, ctx_for([a, b], horizon=1))
    later = 0.8 * 3.0 * growth_ratio("F", 20, 1) * 1.5            # terminal term only (H=1)
    assert out["b"].value == pytest.approx(3.0 + later)
    assert out["a"].value == pytest.approx(3.0 + 1.3 * later)      # this season unchanged
    ped = next(r for r in out["a"].reasons if r.code == "PEDIGREE")
    assert ped.value == pytest.approx(1.3) and "#1 overall" in ped.text


def test_stenberg_beats_olivier_by_a_wide_margin():
    """#2 overall 2026 pick, 19, Fantrax 3.17 FPG projection vs a 29-year-old enforcer at 4.13."""
    stenberg = player("stenberg", ["C", "F"], date(2007, 9, 30), gp=0, draft_overall=2, draft_round=1,
                      draft_year=2026, career_gp=0, pct_owned=88.0)
    stenberg.lines["projected"] = StatLine(split="projected", gp=52, stats={"GP": 52, "FPTS": 164.7})
    olivier = player("olivier", ["RW", "F"], date(1997, 2, 11), gp=0, career_gp=311, pct_owned=23.0)
    olivier.lines["projected"] = StatLine(split="projected", gp=65, stats={"GP": 65, "HIT": 224, "PIM": 106})
    vals = {"stenberg": pv(stenberg, 3.17), "olivier": pv(olivier, 4.13)}
    # fitted age curves (a 19-year-old forward is at 76% of peak, not 55%) narrow the gap:
    # balanced 1.13x, rebuild 1.20x, contend 1.07x (hand-set curves: > 1.2x / > 1.1x)
    for mode, margin in (("balanced", 1.1), ("rebuild", 1.15), ("contend", 1.05)):
        out = apply_dynasty(vals, ctx_for([stenberg, olivier], mode=mode))
        assert out["stenberg"].value > margin * out["olivier"].value, mode
    # with a league-wide market (>= 50 owned players) the gap widens further
    fillers = [player(f"f{i}", ["C", "F"], born(27), pct_owned=float(i)) for i in range(60)]
    fvals = {f.cid: pv(f, 1.0 + i * 0.05) for i, f in enumerate(fillers)}
    ctx = ctx_for([stenberg, olivier, *fillers], mode="contend")
    out = apply_dynasty({**vals, **fvals}, ctx)
    assert out["stenberg"].value > 1.25 * out["olivier"].value
    assert "MARKET_PRIOR" in {r.code for r in out["stenberg"].reasons}


# -- market prior --------------------------------------------------------------------------

def test_market_values_rank_mapping():
    model = {"a": 10.0, "b": 8.0, "c": 6.0, "d": 4.0}
    owned = {"d": 99.0, "c": 50.0, "a": 10.0}                       # b unknown
    assert market_values(model, owned) == {"d": 10.0, "c": 8.0, "a": 6.0}
    ties = market_values(model, {"a": 50.0, "b": 50.0, "c": 1.0})
    assert ties == {"a": 9.0, "b": 9.0, "c": 6.0}
    assert market_values({}, {"a": 1.0}) == {}


def test_market_blend_needs_fifty_owned_players():
    ps = [player(f"p{i}", ["C", "F"], born(27), pct_owned=float(100 - i)) for i in range(49)]
    vals = {p.cid: pv(p, 1.0 + i * 0.1) for i, p in enumerate(ps)}     # reversed vs ownership
    out = apply_dynasty(vals, ctx_for(ps))
    assert all(d.market_value is None and d.value == d.model_value for d in out.values())
    ps.append(player("p49", ["C", "F"], born(27), pct_owned=1.0))
    vals["p49"] = pv(ps[-1], 0.5)
    out = apply_dynasty(vals, ctx_for(ps))
    top_owned = out["p0"]                                            # 100% owned, lowest model value
    top_model = max(d.model_value for d in out.values())
    assert top_owned.market_value == pytest.approx(top_model)
    assert top_owned.value == pytest.approx(0.65 * top_owned.model_value + 0.35 * top_model)
    mp = next(r for r in top_owned.reasons if r.code == "MARKET_PRIOR")
    assert mp.baseline == 100.0


# -- misc --------------------------------------------------------------------------------------

def test_unknown_age_and_override():
    nob = player("nob", ["D"], None)
    out = apply_dynasty({"nob": pv(nob, 2.0)}, ctx_for([nob], horizon=2))
    d = out["nob"]
    assert d.age is None and d.age_mult == 1.0
    assert d.value == pytest.approx(2.0 * (1.0 + 0.8) + 0.8 ** 2 * 2.0 * 1.5)
    assert any(r.code == "AGE_UNKNOWN" for r in d.reasons)
    over = apply_dynasty({"nob": pv(nob, 2.0)}, ctx_for([nob], horizon=2), ages={"nob": 33})["nob"]
    assert over.age == 33 and over.age_mult == pytest.approx(0.831)
    assert over.value < d.value                                     # declining at 33


def test_horizon_one_year_and_rank():
    a = player("a", ["C", "F"], born(26))
    b = player("b", ["C", "F"], born(26))
    ctx = ctx_for([a, b], horizon=1)
    out = apply_dynasty({"a": pv(a, 2.0), "b": pv(b, 3.0)}, ctx)
    assert out["a"].value == pytest.approx(2.0 + 0.8 * 2.0 * growth_ratio("F", out["a"].age, 1) * 1.5)
    ranked = dynasty_rank(out)
    assert [d.player.cid for d in ranked] == ["b", "a"] and isinstance(ranked[0], DynastyValue)
    assert dynasty_rank(list(out.values()))[0].player.cid == "b"
    assert horizon_value(1.0, "F", None, 3) == pytest.approx(1 + 0.8 + 0.64 + 0.512 * 1.5)
