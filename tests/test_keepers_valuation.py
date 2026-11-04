"""Keeper leagues (LeagueContext.keepers_per_team): dynasty value split into this season + a keeper's
long-term surplus, and draft picks as picks in a redraft of everyone not kept."""
from datetime import date

import pytest

from fantasy_manager.models import FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig, StatLine
from fantasy_manager.valuation.dynasty import (DISCOUNT, MARKET_WEIGHT, apply_dynasty, keeper_count, keeper_total,
                                               mode_weights, spot_future, trajectory_factor)
from fantasy_manager.valuation.picks import keeper_slot_curve, next_season_weight, value_picks
from fantasy_manager.valuation.valuate import PlayerValue, replacement_for

AS_OF = date(2026, 10, 1)
SHAPE = {"C": 1, "LW": 1, "BN": 4, "IR": 1}

# (cid, age, healthy fpg, this season's fpg or None = same, slot)
TEAMS = {
    "mine": [("m1", 26, 3.0, None, "C"), ("m2", 27, 2.5, None, "LW"), ("m3", 30, 2.0, None, "BN"),
             ("m_ir", 24, 2.2, 0.4, "IR"), ("m_old", 35, 0.9, None, "BN")],
    "stack": [("s1", 24, 5.0, None, "C"), ("s2", 25, 4.8, None, "LW"), ("s3", 23, 4.6, None, "BN"),
              ("kid", 20, 3.0, None, "BN"), ("s5", 31, 1.5, None, "BN")],
    "thin": [("t1", 28, 2.5, None, "C"), ("t2", 34, 0.8, None, "LW")],
}
FAS = [("fa1", 29, 1.0, None), ("fa2", 29, 1.0, None), ("fa3", 29, 1.0, None),
       ("fa_star", 21, 3.0, 0.5), ("fa_mid", 25, 2.0, 0.3), ("fa_old", 35, 0.9, None)]


def born(age):
    return date(AS_OF.year - int(age), AS_OF.month, AS_OF.day)


def player(cid, age, prospect=False, **kw):
    """Proven veterans by default (a 70-GP prior season, 400 career GP); prospects have neither."""
    lines = {} if prospect else {"prior": StatLine(split="prior", gp=70, stats={"GP": 70})}
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team="BOS", positions=["C", "LW"],
                  birth_date=born(age), status="healthy", lines=lines, career_gp=10 if prospect else 400, **kw)


def pv(p, fpg, season=None):
    s = fpg if season is None else season
    return PlayerValue(player=p, fpg=fpg, fpg_season=s, fpg_week=s, vorp=0.0,
                       horizon_values={"season": s, "week": s, "vorp_season": 0.0, "vorp_week": 0.0})


def league(keepers=3, me="mine", mode="balanced", horizon=3, prospects=(), fillers=0):
    values, teams, fas = {}, [], []
    for tid, rows in TEAMS.items():
        slots = []
        for cid, age, f, s, slot in rows:
            p = player(cid, age)
            values[cid] = pv(p, f, s)
            slots.append(RosterSlot(slot=slot, player=p, starting=slot not in ("BN", "IR")))
        teams.append(FantasyTeam(team_id=tid, name=f"Team {tid}", owner_is_me=tid == me, slots=slots))
    if fillers:   # a fourth team of owned veterans, so >= 50 players carry a % rostered (market prior)
        slots = []
        for i in range(fillers):
            p = player(f"fill{i}", 27, pct_owned=float(i))
            values[p.cid] = pv(p, 1.0 + 0.02 * i)
            slots.append(RosterSlot(slot="BN", player=p, starting=False))
        teams.append(FantasyTeam(team_id="fill", name="Team fill", owner_is_me=False, slots=slots))
    for cid, age, f, s in FAS:
        p = player(cid, age)
        values[cid] = pv(p, f, s)
        fas.append(p)
    for cid, age, f in prospects:
        p = player(cid, age, prospect=True)
        values[cid] = pv(p, f)
        fas.append(p)
    picks = {tid: {2027: [{"round": r, "original_owner": tid} for r in (1, 2)],
                   2028: [{"round": 1, "original_owner": tid}]} for tid in TEAMS}
    ctx = LeagueContext(provider="fantrax", league_id="x", season=2027, name="t",
                        scoring=ScoringConfig(kind="points"), roster_shape=SHAPE, as_of=AS_OF,
                        season_start=date(2026, 10, 7), teams=teams, free_agents=fas, matchup_period=None,
                        dynasty=True, keeper_horizon_years=horizon, dynasty_mode=mode,
                        keepers_per_team=keepers, draft_picks=picks)
    return ctx, values


def team(ctx, tid):
    return next(t for t in ctx.teams if t.team_id == tid)


def kth_surplus(ctx, d, tid, k=3):
    xs = sorted((d[p.cid].keeper_surplus for p in team(ctx, tid).players), reverse=True)
    return xs[k - 1]


def reason(dv, code):
    return next(r for r in dv.reasons if r.code == code)


# -- keep-everyone dynasty: unchanged values, consistent new fields ---------------------------

def test_keep_everyone_fills_the_keeper_fields_consistently():
    ctx, values = league(keepers=None, fillers=60)
    assert keeper_count(ctx) is None
    d = apply_dynasty(values, ctx)
    assert any(dv.market_value is not None for dv in d.values())          # the market prior applied
    for cid, dv in d.items():
        assert dv.season_part == pytest.approx(values[cid].fpg_season)    # balanced w_0 = 1.0
        assert dv.season_part + dv.future == pytest.approx(dv.value)
        assert dv.keeper is True and dv.keeper_surplus == dv.future
        assert not {"KEEPER", "NOT_KEEPER"} & {r.code for r in dv.reasons}
    assert keeper_total(team(ctx, "stack").players, d, None) == \
        pytest.approx(sum(d[p.cid].future for p in team(ctx, "stack").players))


# -- keeper flags ---------------------------------------------------------------------------------

def test_each_team_keeps_its_top_k_by_surplus_ir_included():
    ctx, values = league(keepers=3)
    d = apply_dynasty(values, ctx)
    kept = {t.team_id: {p.cid for p in t.players if d[p.cid].keeper} for t in ctx.teams}
    assert kept == {"mine": {"m1", "m2", "m_ir"}, "stack": {"s1", "s2", "s3"}, "thin": {"t1"}}
    for t in ctx.teams:   # the rule itself: top 3 by surplus, surplus > 0
        ranked = sorted(t.players, key=lambda p: -d[p.cid].keeper_surplus)
        for i, p in enumerate(ranked):
            assert d[p.cid].keeper == (i < 3 and d[p.cid].keeper_surplus > 0)
    r = reason(d["m_ir"], "KEEPER")    # an injured player in an IR slot is still a keeper candidate
    assert "of 3 on Team mine" in r.text and "waiver-level spot" in r.text
    assert r.value == pytest.approx(d["m_ir"].keeper_surplus)
    assert reason(d["s1"], "KEEPER").text.startswith("Keeper #1 of 3 on Team stack")


def test_strong_young_fourth_best_on_a_stacked_team_is_valued_on_this_season_only():
    ctx, values = league(keepers=3)
    d = apply_dynasty(values, ctx)
    kid = d["kid"]
    assert kid.keeper_surplus > kth_surplus(ctx, d, "mine")     # he would be one of MY keepers ...
    assert kid.keeper_surplus < kth_surplus(ctx, d, "stack")    # ... but is 4th on his own team
    assert kid.keeper is False
    assert kid.value == pytest.approx(kid.season_part) == pytest.approx(3.0)
    assert kid.model_value == pytest.approx(kid.season_part)
    assert kid.upside > 0                                        # growth premium still reported
    r = reason(kid, "NOT_KEEPER")
    assert "Not among Team stack's top 3 keepers (3rd keeper surplus" in r.text
    assert "this season only" in r.text and r.baseline == pytest.approx(kth_surplus(ctx, d, "stack"))


def test_keeper_value_is_season_plus_surplus_and_future_is_the_trajectory_after_this_season():
    ctx, values = league(keepers=3)
    d = apply_dynasty(values, ctx)
    ke = apply_dynasty(values, league(keepers=None)[0])                  # same league, keep everyone
    for cid, dv in d.items():
        assert dv.season_part == pytest.approx(values[cid].fpg_season)
        assert dv.future == pytest.approx(ke[cid].value - ke[cid].season_part)   # no market here
        assert dv.value == pytest.approx(dv.season_part + (dv.keeper_surplus if dv.keeper else 0.0))
        assert dv.model_value == pytest.approx(dv.value) and dv.market_value is None
        assert dv.upside == pytest.approx(ke[cid].upside)
    h = reason(d["s1"], "DYNASTY_HORIZON")
    assert "this season" in h.text and "future" in h.text and "only for a keeper" in h.text
    assert h.value == pytest.approx(ke["s1"].value)


def test_surplus_is_future_over_a_waiver_level_spot_floored_at_zero():
    ctx, values = league(keepers=3)
    d = apply_dynasty(values, ctx)
    repl = min(replacement_for(ctx, values, "season").values())
    assert repl == pytest.approx(1.0)                              # top-3 free agents at 1.0 FPG
    w, terminal = mode_weights("balanced", 3)
    spot = repl * (sum(w[1:]) + DISCOUNT ** 3 * terminal)
    assert spot_future(ctx, values) == pytest.approx(spot)
    for dv in d.values():
        assert dv.keeper_surplus == pytest.approx(max(0.0, dv.future - spot))
    # thin keeps only one: t2's future does not beat the spot, though a keeper slot is free
    t2 = d["t2"]
    assert 0 < t2.future < spot and t2.keeper_surplus == 0.0 and t2.keeper is False
    assert t2.value == pytest.approx(t2.season_part)
    assert reason(t2, "NOT_KEEPER").text.startswith("No keeper value")


# -- free agents --------------------------------------------------------------------------------

def test_free_agents_are_keepers_only_if_they_beat_my_kth_keeper():
    ctx, values = league(keepers=3)
    d = apply_dynasty(values, ctx)
    bar = kth_surplus(ctx, d, "mine")
    star, mid, fa1 = d["fa_star"], d["fa_mid"], d["fa1"]
    assert star.keeper_surplus > bar and star.keeper is True
    assert star.value == pytest.approx(0.5 + star.keeper_surplus)        # injured now, kept for later
    assert "Free agent: would be keeper #1 of 3 on Team mine" in reason(star, "KEEPER").text
    assert 0 < mid.keeper_surplus <= bar and mid.keeper is False
    assert mid.value == pytest.approx(mid.season_part)
    nk = reason(mid, "NOT_KEEPER")
    assert "would not crack Team mine's top 3 keepers" in nk.text and nk.baseline == pytest.approx(bar)
    assert fa1.keeper_surplus == 0.0 and fa1.keeper is False


def test_any_positive_surplus_free_agent_is_a_keeper_when_i_have_fewer_than_k():
    ctx, values = league(keepers=3, me="thin")      # thin has a single positive-surplus player
    d = apply_dynasty(values, ctx)
    assert d["fa_mid"].keeper is True and d["fa_star"].keeper is True
    assert d["fa_mid"].keeper_surplus < d["t1"].keeper_surplus           # ranks behind thin's t1
    assert reason(d["fa_mid"], "KEEPER").text.startswith("Free agent: would be keeper #2 of 3 on Team thin")
    assert d["fa1"].keeper is False                  # no surplus: still not a keeper
    # rostered keepers do not depend on who I am
    mine_view = apply_dynasty(values, league(keepers=3)[0])
    assert {c for c, dv in d.items() if dv.keeper and not c.startswith("fa")} == \
        {c for c, dv in mine_view.items() if dv.keeper and not c.startswith("fa")}


# -- K = 0, keeper_total ---------------------------------------------------------------------------

def test_zero_keepers_values_everyone_on_this_season():
    ctx, values = league(keepers=0)
    assert keeper_count(ctx) == 0
    d = apply_dynasty(values, ctx)
    for dv in d.values():
        assert dv.keeper is False and dv.value == pytest.approx(dv.season_part)
        assert reason(dv, "NOT_KEEPER").text.startswith("No keepers in this league")
    assert keeper_total(team(ctx, "stack").players, d, 0) == 0.0


def test_keeper_total_sums_the_k_largest_surpluses():
    ctx, values = league(keepers=3)
    d = apply_dynasty(values, ctx)
    stack, mine = team(ctx, "stack").players, team(ctx, "mine").players
    assert keeper_total(stack, d, 3) == pytest.approx(sum(d[c].keeper_surplus for c in ("s1", "s2", "s3")))
    assert keeper_total(stack, d, None) == pytest.approx(sum(d[p.cid].keeper_surplus for p in stack))
    # trading for the kid replaces my 3rd keeper: the long-term gain is his surplus over it
    kid = next(p for p in stack if p.cid == "kid")
    gain = keeper_total(mine + [kid], d, 3) - keeper_total(mine, d, 3)
    assert gain == pytest.approx(d["kid"].keeper_surplus - kth_surplus(ctx, d, "mine"))


# -- market prior ------------------------------------------------------------------------------

def test_market_prior_blends_the_future_only():
    ctx, values = league(keepers=3, fillers=60)
    d = apply_dynasty(values, ctx)
    ke = apply_dynasty(values, league(keepers=None, fillers=60)[0])
    spot = spot_future(ctx, values)
    blended = [c for c, dv in d.items() if dv.market_value is not None]
    assert len(blended) == 60
    for cid in blended:
        dv = d[cid]
        model_future = ke[cid].model_value - ke[cid].season_part
        assert dv.season_part == pytest.approx(values[cid].fpg_season)         # untouched by the market
        assert dv.future == pytest.approx((1 - MARKET_WEIGHT) * model_future + MARKET_WEIGHT * dv.market_value)
        assert dv.model_value == pytest.approx(dv.season_part + (max(0.0, model_future - spot) if dv.keeper else 0.0))
        assert "market future value" in reason(dv, "MARKET_PRIOR").text
    # the market's future values are a rank transform of the future values, not of whole values
    futures = sorted(ke[c].model_value - ke[c].season_part for c in d)
    assert max(d[c].market_value for c in blended) == pytest.approx(futures[-1])


# -- draft picks in keeper leagues ------------------------------------------------------------------

def _nv(values, d, cid):
    return values[cid].fpg * trajectory_factor("F", d[cid].age, 1, d[cid].pedigree)


@pytest.mark.parametrize("horizon,w1", [(3, 0.8), (1, DISCOUNT)])
def test_first_pick_is_the_best_non_keeper_next_season(horizon, w1):
    ctx, values = league(keepers=3, horizon=horizon)
    d = apply_dynasty(values, ctx)
    assert next_season_weight(ctx) == pytest.approx(w1)
    rostered = {p.cid for t in ctx.teams for p in t.players}
    pool = [c for c in values if not (c in rostered and d[c].keeper)]
    assert "fa_star" in pool and "kid" in pool and "s1" not in pool       # FAs are always in the pool
    curve = keeper_slot_curve(ctx, values, d)
    best = max(pool, key=lambda c: _nv(values, d, c))
    assert curve[0] == pytest.approx(w1 * _nv(values, d, best))
    assert len(curve) == len(pool) and curve == sorted(curve, reverse=True)
    assert w1 * _nv(values, d, "s1") > curve[0]                           # keepers are not drafted


def test_keeper_league_picks_are_worth_more_than_dynasty_picks():
    prospects = [("pros", 19, 0.4)]      # keep-everyone mode anchors its curve on unproven players
    ctx, values = league(keepers=3, prospects=prospects)
    kctx, _ = league(keepers=None, prospects=prospects)
    keeper_picks = {a.key: a for a in value_picks(ctx, values)}
    dynasty_picks = {a.key: a for a in value_picks(kctx, values)}
    assert keeper_picks.keys() == dynasty_picks.keys()
    for key, a in keeper_picks.items():
        assert a.expected_slot == dynasty_picks[key].expected_slot         # draft order logic unchanged
        if a.round == 1:   # the first round reaches the young non-keepers (kid, fa_star)
            assert a.value > 1.5 * dynasty_picks[key].value, key
            assert a.net > 0.0 == dynasty_picks[key].net, key               # dynasty: below the spot
    assert sum(a.value for a in keeper_picks.values()) > 1.3 * sum(a.value for a in dynasty_picks.values())


def test_keeper_pick_net_subtracts_next_seasons_roster_spot():
    ctx, values = league(keepers=3)
    repl = min(replacement_for(ctx, values, "season").values())
    picks = value_picks(ctx, values)
    for a in picks:
        assert a.net == pytest.approx(max(0.0, a.value - repl * 0.8 * DISCOUNT ** a.years_ahead))
    assert any(a.net > 0 for a in picks)


def test_value_picks_in_keeper_mode_accepts_dynasty_values_floats_or_none():
    ctx, values = league(keepers=3)
    d = apply_dynasty(values, ctx)

    def rows(dyn):
        return [(a.key, round(a.value, 9), round(a.net, 9)) for a in value_picks(ctx, values, dyn)]

    expected = rows(d)
    assert expected and rows(None) == expected
    assert rows({c: dv.value for c, dv in d.items()}) == expected      # floats: keeper flags recomputed
