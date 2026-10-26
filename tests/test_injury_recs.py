from datetime import date

import pytest

from fantasy_manager.models import FantasyTeam, LeagueContext, RosterSlot, ScoringConfig
from fantasy_manager.recommend.injuries import StatusHistory, StatusSnapshot, recommend_injuries
from fantasy_manager.scoring import PointsScoring
from fantasy_manager.valuation.valuate import valuate_league

from .test_waivers import mk


def ctx_for(mine, fas, shape):
    slots = [RosterSlot(slot=s, player=p, starting=s not in ("BN", "IR")) for s, p in mine]
    return LeagueContext(provider="t", league_id="1", season=2027, name="T",
                         scoring=ScoringConfig(kind="points", weights={"G": 1.0}), roster_shape=shape,
                         teams=[FantasyTeam(team_id="1", name="me", owner_is_me=True, slots=slots)],
                         free_agents=fas, matchup_period=1, as_of=date(2026, 10, 7))


def snap(**statuses):
    return {cid: StatusSnapshot(cid=cid, status=st, seen_at=1.0) for cid, st in statuses.items()}


def test_status_history_records_only_changes(tmp_path):
    h = StatusHistory(tmp_path)
    a, b = mk("a", ["C"], 0.5), mk("b", ["D"], 0.3, status="dtd")
    assert h.last() == {}
    assert h.record([a, b], when=100.0) == 2
    assert h.record([a, b], when=200.0) == 0             # unchanged -> nothing written
    b.status = "out"
    assert h.record([a, b], when=300.0) == 1
    last = h.last()
    assert last["b"].status == "out" and last["b"].seen_at == 300.0 and last["a"].seen_at == 100.0
    h.close()
    assert StatusHistory(tmp_path).last()["b"].status == "out"   # persisted


def test_worsened_status_since_snapshot():
    worse, better, same = mk("worse", ["C"], 0.8, status="out"), mk("better", ["LW"], 0.5, status="dtd"), \
        mk("same", ["D"], 0.3, status="dtd")
    ctx = ctx_for([("C", worse), ("LW", better), ("D", same)], [], {"C": 1, "LW": 1, "D": 1})
    vals = valuate_league(ctx, PointsScoring({"G": 1.0}))
    recs = recommend_injuries(ctx, vals, snap(worse="dtd", better="ir", same="dtd"))
    assert [r.title for r in recs] == ["worse now OUT (was dtd)"]
    r = recs[0]
    assert r.kind == "injury" and r.reasons[0].code == "STATUS_CHANGE"
    assert r.reasons[0].value == 2 and r.reasons[0].baseline == 1
    # no previous snapshot -> nothing reported as "worsened"
    assert not [x for x in recommend_injuries(ctx, vals, {}) if "now" in x.title]


def test_move_to_ir_chains_best_waiver_add(tmp_path):
    hurt = mk("hurt", ["C"], 1.0, status="ir")
    other = mk("other", ["LW"], 0.4)
    fa_c, fa_d = mk("fa_c", ["C"], 0.6), mk("fa_d", ["D"], 0.9)
    ctx = ctx_for([("C", hurt), ("LW", other)], [fa_c, fa_d], {"C": 1, "LW": 1, "BN": 1, "IR": 1})
    vals = valuate_league(ctx, PointsScoring({"G": 1.0}))
    history = StatusHistory(tmp_path)
    recs = recommend_injuries(ctx, vals, history)
    move = [r for r in recs if r.title.startswith("Move hurt to IR")]
    assert move and move[0].title == "Move hurt to IR and add fa_c"   # same-slot add, not the better D
    assert move[0].add[0].cid == "fa_c" and move[0].drop == []
    codes = [x.code for x in move[0].reasons]
    assert codes[:2] == ["STATUS", "IR_SLOT"] and "WAIVER_ADD" in codes
    assert ctx.my_team.slots[0].slot == "C"      # the context is not mutated


def test_no_ir_move_without_free_slot_and_out_flagged():
    stash = mk("stash", ["D"], 0.1, status="ir")
    out = mk("out", ["C"], 1.0, status="out")
    full = ctx_for([("IR", stash), ("C", out)], [], {"C": 1, "IR": 1})
    vals = valuate_league(full, PointsScoring({"G": 1.0}))
    assert not [r for r in recommend_injuries(full, vals, {}) if r.title.startswith("Move")]
    free = ctx_for([("C", out)], [], {"C": 1, "IR": 1})
    vals = valuate_league(free, PointsScoring({"G": 1.0}))
    move = [r for r in recommend_injuries(free, vals, {}) if r.title.startswith("Move")]
    assert move and any(x.code == "IR_RULES" for x in move[0].reasons)   # OUT is not always IR-eligible


def test_activate_from_ir_suggests_drop_when_full():
    back = mk("back", ["C"], 1.0, status="healthy")
    weak, strong = mk("weak", ["C"], 0.1), mk("strong", ["LW"], 0.9)
    ctx = ctx_for([("IR", back), ("C", weak), ("LW", strong)], [], {"C": 1, "LW": 1, "IR": 1})
    vals = valuate_league(ctx, PointsScoring({"G": 1.0}))
    recs = [r for r in recommend_injuries(ctx, vals, {}) if r.title.startswith("Activate")]
    assert len(recs) == 1
    assert recs[0].add[0].cid == "back" and recs[0].drop[0].cid == "weak"
    assert recs[0].score == pytest.approx(vals["back"].fpg + 1.0)


def test_injury_recs_carry_the_waiver_adds_gain(tmp_path):
    hurt = mk("hurt", ["C"], 1.0, status="ir")
    other = mk("other", ["LW"], 0.4)
    fa_c = mk("fa_c", ["C"], 0.6)
    ctx = ctx_for([("C", hurt), ("LW", other)], [fa_c], {"C": 1, "LW": 1, "BN": 1, "IR": 1})
    vals = valuate_league(ctx, PointsScoring({"G": 1.0}))
    move = [r for r in recommend_injuries(ctx, vals, {}) if r.title.startswith("Move hurt to IR")][0]
    assert move.predicted_gain is not None and move.gain_units in ("week_pts", "season_fpg")
    assert [p.cid for p in move.subjects] == ["hurt"]                 # the IR target is recorded
    assert move.strength is not None and move.strength >= 5.0         # frees an IR slot: floor 5


def test_activation_gain_is_season_fpg_over_the_drop():
    back = mk("back", ["C"], 1.0, status="healthy")
    weak, strong = mk("weak", ["C"], 0.1), mk("strong", ["LW"], 0.9)
    ctx = ctx_for([("IR", back), ("C", weak), ("LW", strong)], [], {"C": 1, "LW": 1, "IR": 1})
    vals = valuate_league(ctx, PointsScoring({"G": 1.0}))
    r = [r for r in recommend_injuries(ctx, vals, {}) if r.title.startswith("Activate")][0]
    assert r.gain_units == "season_fpg"
    assert r.predicted_gain == pytest.approx(vals["back"].fpg_season - vals["weak"].fpg_season)


def test_ir_chain_add_respects_goalie_cap():
    hurt = mk("hurt_g", ["G"], 1.0, status="ir")
    g2, g3 = mk("g2", ["G"], 0.5), mk("g3", ["G"], 0.3)
    fa_g = mk("fa_g", ["G"], 0.9)
    shape = {"G": 2, "BN": 1, "IR": 1}
    ctx = ctx_for([("G", hurt), ("G", g2), ("BN", g3)], [fa_g], shape)
    vals = valuate_league(ctx, PointsScoring({"G": 1.0}))
    free = [r for r in recommend_injuries(ctx, vals, {}) if r.title.startswith("Move hurt_g")]
    assert free and free[0].title == "Move hurt_g to IR and add fa_g"      # no cap: the goalie add
    ctx.position_limits = {"G": 3}                  # hurt_g still counts in IR: no 4th goalie
    r = next(r for r in recommend_injuries(ctx, vals, {}) if r.title.startswith("Move hurt_g"))
    assert r.title == "Move hurt_g to IR" and r.add == []
    cap = next(x for x in r.reasons if x.code == "POSITION_CAP")
    assert "G limit 3" in cap.text


# -- hold or cut ------------------------------------------------------------------------------

def verdicts(ctx, vals, **kw):
    return [r for r in recommend_injuries(ctx, vals, {}, **kw) if r.title.startswith(("Hold", "Cut"))]


def test_hold_a_star_through_a_short_injury():
    star = mk("star", ["C"], 1.5, status="out")
    star.status_note = "Upper Body: out (est. return 2026-10-20)"
    fa = mk("fa", ["C"], 0.6)
    ctx = ctx_for([("C", star)], [fa], {"C": 1})      # no IR slot: he blocks the active spot
    vals = valuate_league(ctx, PointsScoring({"G": 1.0}))
    [r] = verdicts(ctx, vals)
    assert r.title.startswith("Hold star through his injury (back ~Oct 20)") and r.add == r.drop == []
    assert r.strength == 1.0
    codes = {x.code: x for x in r.reasons}
    assert codes["HOLD_PTS"].value > codes["CUT_PTS"].value
    assert "Cut only if he misses more than" in codes["BREAK_EVEN"].text


def test_cut_a_marginal_player_out_long_term():
    meh = mk("meh", ["C"], 0.5, status="ir")
    meh.status_note = "Knee: out (est. return 2027-03-01)"
    fa = mk("fa", ["C"], 0.6)
    ctx = ctx_for([("C", meh)], [fa], {"C": 1})
    vals = valuate_league(ctx, PointsScoring({"G": 1.0}))
    [r] = verdicts(ctx, vals)
    assert r.title.startswith("Cut meh, add fa:")
    assert [p.cid for p in r.add] == ["fa"] and [p.cid for p in r.drop] == ["meh"]
    assert r.gain_units == "season_fpg" and r.predicted_gain > 0
    codes = {x.code: x for x in r.reasons}
    assert codes["CUT_PTS"].value > codes["HOLD_PTS"].value


def test_free_ir_slot_means_hold_without_a_verdict_rec():
    meh = mk("meh", ["C"], 0.5, status="ir")
    meh.status_note = "Knee: out (est. return 2027-03-01)"
    fa = mk("fa", ["C"], 0.6)
    ctx = ctx_for([("C", meh)], [fa], {"C": 1, "BN": 1, "IR": 1})
    vals = valuate_league(ctx, PointsScoring({"G": 1.0}))
    recs = recommend_injuries(ctx, vals, {})
    assert not [r for r in recs if r.title.startswith(("Hold", "Cut"))]
    move = next(r for r in recs if r.title.startswith("Move meh to IR"))
    assert any(x.code == "HOLD_VERDICT" for x in move.reasons)
    # already in an IR slot: holding is free, said as a low-strength hold
    stashed = ctx_for([("IR", meh), ("C", mk("c", ["C"], 0.4))], [fa], {"C": 1, "IR": 1})
    vals = valuate_league(stashed, PointsScoring({"G": 1.0}))
    [r] = verdicts(stashed, vals)
    assert r.title.startswith("Hold meh in IR") and r.strength == 1.0


def test_dynasty_never_cuts_a_protected_player():
    kid = mk("kid", ["C"], 0.5, status="ir")
    kid.status_note = "Knee: out (est. return 2027-03-01)"
    kid.birth_date, kid.draft_overall = date(2006, 1, 1), 3
    fa = mk("fa", ["C"], 0.6)
    ctx = ctx_for([("C", kid)], [fa], {"C": 1})
    ctx.dynasty = True
    vals = valuate_league(ctx, PointsScoring({"G": 1.0}))
    [r] = verdicts(ctx, vals, dynasty_values={"kid": 9.0, "fa": 3.0})
    assert r.title.startswith("Hold kid") and "protected dynasty asset" in r.title
    assert any(x.code == "DYNASTY_COST" for x in r.reasons)


def test_guessed_return_needs_a_bigger_edge_to_cut():
    p = mk("p", ["C"], 1.0, status="ir")       # IR without a timetable: 30 days assumed
    fa = mk("fa", ["C"], 0.6)
    ctx = ctx_for([("C", p)], [fa], {"C": 1})
    vals = valuate_league(ctx, PointsScoring({"G": 1.0}))
    [r] = verdicts(ctx, vals)
    est = next(x for x in r.reasons if x.code == "RETURN_ESTIMATE")
    assert "a guess" in est.text
    assert r.title.startswith("Hold p")


def test_replacement_must_have_an_nhl_team():
    meh = mk("meh", ["C"], 0.5, status="ir")
    meh.status_note = "Knee: out (est. return 2027-03-01)"
    unsigned, fa = mk("unsigned", ["C"], 2.0), mk("fa", ["C"], 0.6)
    unsigned.team = None
    ctx = ctx_for([("C", meh)], [unsigned, fa], {"C": 1})
    vals = valuate_league(ctx, PointsScoring({"G": 1.0}))
    [r] = verdicts(ctx, vals)
    assert [p.cid for p in r.add] == ["fa"]


def test_ir_slot_held_by_a_dtd_player_is_a_swap_not_a_cut():
    meh = mk("meh", ["C"], 0.5, status="ir")
    meh.status_note = "Knee: out (est. return 2027-03-01)"
    dtd = mk("dtd", ["LW"], 0.8, status="dtd")
    fa = mk("fa", ["C"], 0.6)
    ctx = ctx_for([("C", meh), ("IR", dtd)], [fa], {"C": 1, "IR": 1})
    vals = valuate_league(ctx, PointsScoring({"G": 1.0}))
    [r] = verdicts(ctx, vals)
    assert r.title.startswith("Hold meh") and "activate dtd (dtd)" in r.title and r.add == r.drop == []
