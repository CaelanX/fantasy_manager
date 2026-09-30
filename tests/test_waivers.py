from datetime import date

import pytest

from fantasy_manager.models import (FantasyTeam, LeagueContext, Player, RosterSlot, ScoringConfig,
                                    StatLine)
from fantasy_manager.recommend.base import protection_reason
from fantasy_manager.recommend.waivers import recommend_waivers
from fantasy_manager.valuation.blend import K_INSEASON
from fantasy_manager.scoring import PointsScoring
from fantasy_manager.valuation.valuate import valuate_league


def mk(cid, pos, goals_pg, gp=0, status="healthy", proj_gp=80):
    lines = {"projected": StatLine(split="projected", gp=proj_gp, stats={"G": goals_pg * proj_gp, "GP": proj_gp})}
    if gp:
        lines["season"] = StatLine(split="season", gp=gp, stats={"G": goals_pg * gp, "GP": gp})
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team="EDM", positions=pos, status=status,
                  lines=lines)


def league(fas, mine, shape=None):
    """My team with `mine` as (slot, player). The default roster shape is exactly the slots
    given (plus one IR slot), i.e. a full roster: a pickup then needs a drop."""
    slots = [RosterSlot(slot=s, player=p, starting=s not in ("BN", "IR")) for s, p in mine]
    if shape is None:
        shape = {"IR": 1}
        for s, _ in mine:
            shape[s] = shape.get(s, 0) + (s != "IR")
    return LeagueContext(provider="test", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights={"G": 1.0}),
                         roster_shape=shape,
                         teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True, slots=slots)],
                         free_agents=fas, matchup_period=1, as_of=date(2026, 10, 1))


def test_waiver_pick_and_drop_candidate():
    fa_good = mk("fa_c", ["C"], 1.0, gp=20)
    fa_meh = mk("fa_d", ["D"], 0.2)
    my_c = mk("my_c", ["C"], 0.2)
    my_lw = mk("my_lw", ["LW"], 0.1)       # lowest overall but not a C
    my_ir = mk("my_ir", ["C"], 0.0, status="ir")
    ctx = league([fa_good, fa_meh], [("C", my_c), ("LW", my_lw), ("IR", my_ir)])
    values = valuate_league(ctx, PointsScoring({"G": 1.0}))
    assert values["fa_c"].fpg == pytest.approx(1.0)
    assert values["fa_c"].vorp > 0
    recs = recommend_waivers(ctx, values)
    assert len(recs) == 1
    r = recs[0]
    assert r.add[0].cid == "fa_c" and r.drop[0].cid == "my_c"  # IR-slot player never dropped
    assert [x.code for x in r.reasons][:4] == ["VORP_DELTA", "FPG_ADD", "FPG_DROP", "GP"]
    assert r.score == pytest.approx(0.8 * 20 / (20 + K_INSEASON["skater"]))   # confidence uses in-season k


def test_week_horizon_skips_injured_fa():
    fa = mk("fa_c", ["C"], 1.0, status="out")
    my_c = mk("my_c", ["C"], 0.2)
    ctx = league([fa, mk("fa_c2", ["C"], 0.1)], [("C", my_c)])
    values = valuate_league(ctx, PointsScoring({"G": 1.0}))
    assert recommend_waivers(ctx, values, horizon="week") == []
    season = recommend_waivers(ctx, values, horizon="season")
    assert season and season[0].score == pytest.approx((0.6 - 0.2) * 0.25)


def test_valuation_reasons_carry_evidence():
    p = mk("x", ["C"], 0.5, gp=10)
    ctx = league([], [("C", p)])
    pv = valuate_league(ctx, PointsScoring({"G": 1.0}))["x"]
    codes = {r.code: r for r in pv.reasons}
    assert codes["FPG_BLEND"].value == pytest.approx(0.5)
    assert codes["SHRINK_GP"].value == 10 and codes["SHRINK_GP"].baseline == K_INSEASON["skater"]
    assert "BASELINE" in codes and "VORP" in codes


def test_open_roster_spot_means_no_drop():
    fa = mk("fa_c", ["C"], 1.0, gp=20)
    my_c = mk("my_c", ["C"], 0.2)
    ctx = league([fa], [("C", my_c)], shape={"C": 1, "BN": 1, "IR": 1})
    recs = recommend_waivers(ctx, valuate_league(ctx, PointsScoring({"G": 1.0})))
    assert len(recs) == 1 and recs[0].drop == []
    codes = {r.code: r for r in recs[0].reasons}
    assert "OPEN_SPOT" in codes and "my_c" in codes["VORP_DELTA"].text


def test_ir_status_player_moves_to_free_ir_slot_instead_of_drop():
    fa = mk("fa_c", ["C"], 1.0, gp=20)
    my_c = mk("my_c", ["C"], 0.2)
    hurt = mk("hurt", ["LW"], 0.9, status="ir")          # IR status but in an active slot
    ctx = league([fa], [("C", my_c), ("LW", hurt)], shape={"C": 1, "LW": 1, "IR": 1})
    values = valuate_league(ctx, PointsScoring({"G": 1.0}))
    r = recommend_waivers(ctx, values)[0]
    assert r.drop == [] and r.title == "Add fa_c, move hurt to IR"
    assert "IR_MOVE" in {x.code for x in r.reasons}
    # no IR slot in the league (e.g. Fantrax): back to a normal drop
    ctx2 = league([fa], [("C", my_c), ("LW", hurt)], shape={"C": 1, "LW": 1})
    r2 = recommend_waivers(ctx2, valuate_league(ctx2, PointsScoring({"G": 1.0})))[0]
    assert [p.cid for p in r2.drop] == ["my_c"] and "IR_MOVE" not in {x.code for x in r2.reasons}


def test_dynasty_drop_is_lowest_dynasty_value():
    fa = mk("fa_c", ["C"], 1.0, gp=20)
    vet = mk("vet", ["C"], 0.2)            # lowest FPG but long-term value
    kid = mk("kid", ["C"], 0.3)
    ctx = league([fa], [("C", vet), ("BN", kid)])
    ctx.dynasty = True
    values = valuate_league(ctx, PointsScoring({"G": 1.0}))
    dyn = {"fa_c": 5.0, "vet": 9.0, "kid": 1.0}
    r = recommend_waivers(ctx, values, dynasty_values=dyn)[0]
    assert [p.cid for p in r.drop] == ["kid"] and "DYNASTY_DROP" in {x.code for x in r.reasons}
    # never cut a better long-term asset
    assert recommend_waivers(ctx, values, dynasty_values={"fa_c": 0.5, "vet": 9.0, "kid": 1.0}) == []


def test_owned_reason_is_provider_neutral():
    fa = mk("fa_c", ["C"], 1.0, gp=20)
    fa.pct_owned = 32.0
    ctx = league([fa], [("C", mk("my_c", ["C"], 0.2))])
    r = recommend_waivers(ctx, valuate_league(ctx, PointsScoring({"G": 1.0})))[0]
    owned = next(x for x in r.reasons if x.code == "OWNED")
    assert owned.text == "fa_c rostered in 32.0% of leagues"


def test_dynasty_never_drops_a_player_without_stats():
    fa = mk("fa_c", ["C"], 1.0, gp=20)
    prospect = Player(cid="prospect", name="prospect", name_norm="prospect", ids={}, team="EDM",
                      positions=["C"])                       # no lines at all -> NO_DATA
    vet = mk("vet", ["C"], 0.2)
    ctx = league([fa], [("C", vet), ("BN", prospect)])
    ctx.dynasty = True
    values = valuate_league(ctx, PointsScoring({"G": 1.0}))
    assert values["prospect"].fpg == 0.0
    r = recommend_waivers(ctx, values, dynasty_values={"fa_c": 5.0, "vet": 2.0, "prospect": 0.0})[0]
    assert [p.cid for p in r.drop] == ["vet"]


# -- dynasty prospect protection ------------------------------------------------------------

def stenberg_league(mode="contend", proj_gp=52, olivier_fpg=4.13):
    """The Fantrax complaint: 19-year-old #2 overall pick (3.17 FPG projected) vs a 29-year-old
    enforcer FA (4.13 FPG)."""
    olivier = mk("olivier", ["C"], olivier_fpg, gp=20)
    stenberg = mk("stenberg", ["C"], 3.17, proj_gp=proj_gp)
    stenberg.birth_date, stenberg.draft_overall, stenberg.draft_round = date(2007, 9, 30), 2, 1
    ctx = league([olivier], [("C", stenberg)])
    ctx.dynasty, ctx.dynasty_mode = True, mode
    return ctx, valuate_league(ctx, PointsScoring({"G": 1.0}))


def test_waiver_protection_blocks_the_stenberg_drop():
    ctx, values = stenberg_league()
    # even with the old (buggy) dynasty values the protected prospect is not cut
    dyn = {"olivier": 9.47, "stenberg": 8.56}
    debug = []
    assert recommend_waivers(ctx, values, dynasty_values=dyn, debug=debug) == []
    assert debug and debug[0]["drop"] == "stenberg"
    assert debug[0]["reason"].code == "PROSPECT_PROTECTED" and "#2 overall pick" in debug[0]["reason"].text
    # a clearly bigger asset (>= 1.5x) may still replace him
    r = recommend_waivers(ctx, values, dynasty_values={"olivier": 13.0, "stenberg": 8.56})[0]
    assert [p.cid for p in r.drop] == ["stenberg"]


def test_widely_owned_young_player_is_protected_and_next_drop_is_used():
    fa = mk("fa_c", ["C"], 1.0, gp=20)
    star = mk("star", ["C"], 0.2)
    star.pct_owned, star.birth_date = 92.0, date(2004, 1, 1)     # 22
    vet = mk("vet", ["C"], 0.3)
    ctx = league([fa], [("C", star), ("BN", vet)])
    ctx.dynasty = True
    values = valuate_league(ctx, PointsScoring({"G": 1.0}))
    r = recommend_waivers(ctx, values, dynasty_values={"fa_c": 5.9, "star": 4.0, "vet": 5.0})[0]
    assert [p.cid for p in r.drop] == ["vet"]                    # star (lowest) is protected


def test_widely_owned_veteran_is_not_protected():
    """Protecting everyone >= 80% rostered covered most of a competitive dynasty roster, so
    waivers never fired; veterans now face only the 1.15x dynasty-value bar."""
    fa = mk("fa_c", ["C"], 1.0, gp=20)
    star = mk("star", ["C"], 0.2)
    star.pct_owned, star.birth_date = 92.0, date(1997, 1, 1)     # 29
    vet = mk("vet", ["C"], 0.3)
    ctx = league([fa], [("C", star), ("BN", vet)])
    ctx.dynasty = True
    values = valuate_league(ctx, PointsScoring({"G": 1.0}))
    r = recommend_waivers(ctx, values, dynasty_values={"fa_c": 5.9, "star": 4.0, "vet": 5.0})[0]
    assert [p.cid for p in r.drop] == ["star"]


def _prospect(age=None, pick=None, owned=None, career_gp=None):
    return Player(cid="p", name="P", name_norm="p", ids={}, team="EDM", positions=["C"],
                  draft_overall=pick, pct_owned=owned, career_gp=career_gp)


def test_protection_rule_young_and_pedigree_or_owned():
    assert "#12 overall pick" in protection_reason(_prospect(pick=12), 21.0)
    assert protection_reason(_prospect(pick=12), 23.0) is not None          # 23 still counts
    assert protection_reason(_prospect(pick=12), 23.5) is None              # older: normal comparison
    assert protection_reason(_prospect(pick=40), 20.0) is None
    assert "80%" in protection_reason(_prospect(owned=80.0, career_gp=300), 22.0)
    assert protection_reason(_prospect(owned=95.0), 27.0) is None           # veterans never
    # unproven (< 82 NHL GP) and >= 60% rostered
    assert "30 NHL GP" in protection_reason(_prospect(owned=65.0, career_gp=30), 21.0)
    assert protection_reason(_prospect(owned=65.0, career_gp=90), 21.0) is None
    assert protection_reason(_prospect(owned=55.0, career_gp=30), 21.0) is None
    assert protection_reason(_prospect(pick=1, owned=99.0), None) is None   # age unknown


def test_dynasty_add_must_beat_drop_by_fifteen_percent_and_scores_by_dynasty_gain():
    fa = mk("fa_c", ["C"], 1.0, gp=20)
    vet = mk("vet", ["C"], 0.2)
    ctx = league([fa], [("C", vet)])
    ctx.dynasty = True
    values = valuate_league(ctx, PointsScoring({"G": 1.0}))
    assert recommend_waivers(ctx, values, dynasty_values={"fa_c": 5.5, "vet": 5.0}) == []   # only +10%
    r = recommend_waivers(ctx, values, dynasty_values={"fa_c": 6.0, "vet": 5.0})[0]
    assert r.score == pytest.approx((6.0 - 5.0) * 20 / (20 + K_INSEASON["skater"]))   # dynasty gain x confidence
    assert next(x for x in r.reasons if x.code == "DYNASTY_GAIN").value == pytest.approx(1.0)


def test_contend_mode_may_drop_a_prospect_who_will_not_play_this_season():
    # protected, but projected for only 20 NHL games; the FA is a clear this-season upgrade
    dyn = {"olivier": 10.0, "stenberg": 8.0}                    # 1.25x: above 1.15, below 1.5
    ctx, values = stenberg_league("contend", proj_gp=20, olivier_fpg=5.5)
    r = recommend_waivers(ctx, values, dynasty_values=dyn)[0]
    assert [p.cid for p in r.drop] == ["stenberg"]
    contend = next(x for x in r.reasons if x.code == "CONTEND_DROP")
    assert "projected 20 GP" in contend.text
    ctx, values = stenberg_league("balanced", proj_gp=20, olivier_fpg=5.5)
    assert recommend_waivers(ctx, values, dynasty_values=dyn) == []
    ctx, values = stenberg_league("contend", proj_gp=52, olivier_fpg=5.5)   # he plays: stays protected
    assert recommend_waivers(ctx, values, dynasty_values=dyn) == []


def test_waiver_predicted_gain_units_and_strength():
    fa_good = mk("fa_c", ["C"], 1.0, gp=20)
    my_c = mk("my_c", ["C"], 0.2)
    ctx = league([fa_good], [("C", my_c)])
    values = valuate_league(ctx, PointsScoring({"G": 1.0}))
    r = recommend_waivers(ctx, values)[0]
    assert r.predicted_gain == pytest.approx(values["fa_c"].fpg_season - values["my_c"].fpg_season)
    assert r.gain_units == "season_fpg" and r.horizon_days is None
    assert r.strength is not None and 0 < r.strength <= 10
    assert (r.rank_in_kind, r.kind_total) == (1, 1)
    week = recommend_waivers(ctx, values, horizon="week")[0]
    assert week.gain_units == "season_fpg" and week.horizon_days == 7   # no schedule: per-game week value


# -- market timing: % rostered trend ------------------------------------------

def test_market_multiplier_thresholds():
    from fantasy_manager.recommend.waivers import market_multiplier

    assert market_multiplier(None) == 1.0 and market_multiplier(2.9) == 1.0 and market_multiplier(-8) == 1.0
    assert market_multiplier(3.0) == pytest.approx(1.15)
    assert market_multiplier(9.5) == pytest.approx(1.475)
    assert market_multiplier(10.0) == pytest.approx(1.5) and market_multiplier(40.0) == pytest.approx(1.5)


def _trend_ctx(change, pct=32.0):
    fa = mk("fa_c", ["C"], 1.0, gp=20)
    fa.pct_owned, fa.pct_owned_change = pct, change
    ctx = league([fa], [("C", mk("my_c", ["C"], 0.2))])
    return ctx, valuate_league(ctx, PointsScoring({"G": 1.0}))


def test_waiver_riser_gets_score_boost_and_reason():
    base_ctx, base_vals = _trend_ctx(None)
    base = recommend_waivers(base_ctx, base_vals)[0]
    assert "OWNERSHIP_TREND" not in {r.code for r in base.reasons}
    ctx, vals = _trend_ctx(9.5)
    r = recommend_waivers(ctx, vals)[0]
    assert r.score == pytest.approx(base.score * 1.475)
    trend = next(x for x in r.reasons if x.code == "OWNERSHIP_TREND")
    assert "rostered 32% (+9.5 this week)" in trend.text and trend.value == 9.5 and trend.baseline == 32.0
    assert "OWNED" not in {x.code for x in r.reasons}          # folded into OWNERSHIP_TREND
    assert r.strength == base.strength and r.predicted_gain == base.predicted_gain   # gain untouched


def test_waiver_faller_gets_a_note_only():
    base_ctx, base_vals = _trend_ctx(None)
    base = recommend_waivers(base_ctx, base_vals)[0]
    ctx, vals = _trend_ctx(-4.0)
    r = recommend_waivers(ctx, vals)[0]
    assert r.score == pytest.approx(base.score)
    trend = next(x for x in r.reasons if x.code == "OWNERSHIP_TREND")
    assert "(-4.0 this week)" in trend.text and "falling" in trend.text
    small_ctx, small_vals = _trend_ctx(2.0)                   # below +3: reason, no boost
    small = recommend_waivers(small_ctx, small_vals)[0]
    assert small.score == pytest.approx(base.score) and "rising" not in small.reasons[-1].text


def test_trending_alerts_thresholds():
    from fantasy_manager.recommend.waivers import recommend_trending_alerts

    riser = mk("riser", ["C"], 1.0, gp=20)                    # positive VORP
    riser.pct_owned, riser.pct_owned_change = 32.0, 9.5
    edge = mk("edge", ["C"], 1.0, gp=20)
    edge.pct_owned, edge.pct_owned_change = 20.0, 5.0         # exactly the threshold
    small = mk("small", ["C"], 1.0, gp=20)
    small.pct_owned_change = 4.9                              # not enough of a rise
    dud = mk("dud", ["C"], 0.0)                               # rising but no value, no deployment data
    dud.pct_owned_change = 12.0
    deployed = mk("deployed", ["C"], 0.0)                     # no value, but more ice time
    deployed.pct_owned_change, deployed.toi_trend = 6.0, 1.8
    pp_guy = mk("pp_guy", ["C"], 0.0)
    pp_guy.pct_owned_change, pp_guy.pp_share_trend = 30.0, 0.2
    ctx = league([riser, edge, small, dud, deployed, pp_guy], [("C", mk("my_c", ["C"], 0.2))])

    class PV:                                                 # VORP +0.5 for the valued, -0.5 otherwise
        def __init__(self, vorp):
            self.vorp = vorp

        def vorp_for(self, horizon):
            return self.vorp

        def fpg_for(self, horizon):
            return 1.0 + self.vorp
    vals = {p.cid: PV(0.5 if p.cid in ("riser", "edge", "small") else -0.5) for p in ctx.free_agents}
    recs = recommend_trending_alerts(ctx, values=vals)
    assert [r.subjects[0].cid for r in recs] == ["pp_guy", "riser", "deployed", "edge"]
    top = recs[1]
    assert top.kind == "alert" and top.title == "Rising: riser (+9.5% rostered this week)"
    assert top.counterparty == "FA" and top.horizon_days == 7 and top.add == [] and top.drop == []
    assert top.strength == pytest.approx(3 + 9.5 / 3, abs=0.01)
    assert recs[0].strength == 10.0                           # capped
    assert {r.code for r in top.reasons} >= {"OWNERSHIP_TREND", "VORP"}
    assert "TOI_TREND" in {r.code for r in recs[2].reasons}
    assert (recs[0].rank_in_kind, recs[0].kind_total) == (1, 4)
    assert len(recommend_trending_alerts(ctx, limit=2, values=vals)) == 2
    # values omitted: computed from the context (here only the deployment risers qualify)
    assert [r.subjects[0].cid for r in recommend_trending_alerts(ctx)] == ["pp_guy", "deployed"]


# -- per-position roster maximums (ESPN positionLimits / Fantrax "Total Max") ----------------

def _goalie_cap_league(cap=3, fa_gpg=1.0):
    """The Lankinen scenario: three goalies rostered, one open bench spot, a strong FA goalie."""
    lank = mk("Lankinen", ["G"], fa_gpg, gp=20)
    fa_c = mk("fa_c", ["C"], 0.9, gp=20)
    mine = [("G", mk("Hellebuyck", ["G"], 0.8)), ("G", mk("Skinner", ["G"], 0.3)),
            ("BN", mk("Bussi", ["G"], 0.1)), ("C", mk("my_c", ["C"], 0.2))]
    ctx = league([lank, fa_c], mine, shape={"C": 1, "G": 2, "BN": 2, "IR": 1})
    if cap is not None:
        ctx.position_limits = {"G": cap}
    return ctx


def test_goalie_cap_blocks_open_spot_add_and_drops_a_goalie_instead():
    from fantasy_manager.recommend.base import position_room, roster_legal_after
    ctx = _goalie_cap_league()
    team = ctx.my_team
    lank = next(p for p in ctx.free_agents if p.cid == "Lankinen")
    assert position_room(team, ctx, lank) == 0 and position_room(team, ctx, ["C"]) > 100
    ok, why = roster_legal_after(team, ctx, add=[lank])
    assert not ok and why == "G limit 3: would roster 4 goalies"
    bussi = next(p for p in team.players if p.cid == "Bussi")
    my_c = next(p for p in team.players if p.cid == "my_c")
    assert roster_legal_after(team, ctx, add=[lank], drop=[bussi]) == (True, None)
    assert not roster_legal_after(team, ctx, add=[lank], drop=[my_c])[0]

    recs = recommend_waivers(ctx, valuate_league(ctx, PointsScoring({"G": 1.0})))
    goalie = [r for r in recs if r.add[0].cid == "Lankinen"]
    assert goalie, [r.title for r in recs]
    r = goalie[0]
    assert r.title == "Add Lankinen, drop Bussi" and [d.cid for d in r.drop] == ["Bussi"]
    codes = [x.code for x in r.reasons]
    assert "POSITION_CAP" in codes and "OPEN_SPOT" not in codes
    assert "G limit 3" in next(x.text for x in r.reasons if x.code == "POSITION_CAP")
    # skaters still use the open spot
    skater = next(r for r in recs if r.add[0].cid == "fa_c")
    assert skater.drop == [] and "OPEN_SPOT" in [x.code for x in skater.reasons]


def test_without_a_cap_the_goalie_takes_the_open_spot():
    ctx = _goalie_cap_league(cap=None)
    recs = recommend_waivers(ctx, valuate_league(ctx, PointsScoring({"G": 1.0})))
    r = next(r for r in recs if r.add[0].cid == "Lankinen")
    assert r.drop == [] and r.title == "Add Lankinen (open roster spot)"


def test_goalie_cap_with_no_worthwhile_goalie_drop_gives_no_goalie_add():
    ctx = _goalie_cap_league(fa_gpg=0.15)     # barely better than Bussi (0.1): below MIN_GAIN
    debug: list = []
    recs = recommend_waivers(ctx, valuate_league(ctx, PointsScoring({"G": 1.0})), debug=debug)
    assert not [r for r in recs if r.add[0].cid == "Lankinen"]


def test_roster_legal_after_max_roster_size_and_ir_moves():
    from fantasy_manager.recommend.base import position_limits_text, roster_legal_after
    ctx = _goalie_cap_league()
    ctx.max_roster_size = 4
    team = ctx.my_team
    fa_c = next(p for p in ctx.free_agents if p.cid == "fa_c")
    ok, why = roster_legal_after(team, ctx, add=[fa_c])
    assert not ok and why == "roster limit 4: would roster 5 players"
    my_c = next(p for p in team.players if p.cid == "my_c")
    assert roster_legal_after(team, ctx, add=[fa_c], to_ir=[my_c]) == (True, None)
    assert position_limits_text(ctx) == "max G 3 (incl. IR); roster max 4 (excl. IR)"


def _sched_league(fa, my):
    ctx = league([fa], [("C", my)])
    days = [date(2026, 10, d) for d in (1, 3, 5, 7, 8)]
    ctx.schedule = {"EDM": days}
    ctx.games_per_day = {d: 4 for d in days}
    return ctx


def test_waiver_player_week_gain_counts_only_games_after_clear_date():
    def run(waiver_until):
        fa = mk("fa_c", ["C"], 3.0, gp=20).model_copy(update={"waiver_until": waiver_until})
        ctx = _sched_league(fa, mk("my_c", ["C"], 0.2))
        values = valuate_league(ctx, PointsScoring({"G": 1.0}))
        (r,) = recommend_waivers(ctx, values, horizon="week")
        return r, values

    free, values = run(None)
    waiv, _ = run(date(2026, 10, 5))         # clears Mon Oct 5: only Oct 7 counts (window Oct 1-7)
    assert not any(x.code == "WAIVER_CLAIM" for x in free.reasons)
    claim = next(x for x in waiv.reasons if x.code == "WAIVER_CLAIM")
    assert "on waivers until Mon: claim, won't be available before then" in claim.text
    assert claim.value == 1 and claim.baseline == 4
    full = values["fa_c"].proj_week
    assert free.gain_units == "week_pts"
    assert free.predicted_gain - waiv.predicted_gain == pytest.approx(full * 3 / 4)


def test_waiver_already_cleared_is_a_plain_add():
    fa = mk("fa_c", ["C"], 3.0, gp=20).model_copy(update={"waiver_until": date(2026, 9, 30)})
    ctx = _sched_league(fa, mk("my_c", ["C"], 0.2))
    values = valuate_league(ctx, PointsScoring({"G": 1.0}))
    (r,) = recommend_waivers(ctx, values, horizon="week")
    assert not any(x.code == "WAIVER_CLAIM" for x in r.reasons)
