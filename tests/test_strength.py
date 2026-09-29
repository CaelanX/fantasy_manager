
import pytest

from fantasy_manager.models import Player, Reason, Recommendation, StatLine
from fantasy_manager.recommend.strength import (FLAG_SCALE, TRADE_EV_SCALE, TRADE_SCALE, WAIVER_SCALE, WEEK_SCALE,
                                                apply_ranks, gain_text, interp, strength_for)


def pl(cid, gp=0):
    lines = {"season": StatLine(split="season", gp=gp, stats={"GP": gp})} if gp else {}
    return Player(cid=cid, name=cid, name_norm=cid, ids={}, team="EDM", positions=["C"], lines=lines)


def waiver(gain, gp=5000):
    return Recommendation(kind="waiver", score=gain, title="w", add=[pl("a", gp)], predicted_gain=gain,
                          gain_units="season_fpg")


@pytest.mark.parametrize("scale", [WAIVER_SCALE, WEEK_SCALE, TRADE_SCALE, TRADE_EV_SCALE, FLAG_SCALE])
def test_scales_are_monotone_and_capped(scale):
    xs = [i / 10 for i in range(0, 120)]
    ys = [interp(x, scale) for x in xs]
    assert ys == sorted(ys) and ys[0] == 0.0 and max(ys) <= 10.0
    for x, y in scale:
        assert interp(x, scale) == pytest.approx(y)


def test_waiver_anchor_points_with_full_confidence():
    # gp=5000 -> confidence ~0.995; anchors 0.3->3, 1.0->6, 2.0->9, >=3 -> 10
    assert strength_for(waiver(0.3)) == pytest.approx(3.0, abs=0.1)
    assert strength_for(waiver(1.0)) == pytest.approx(6.0, abs=0.1)
    assert strength_for(waiver(2.0)) == pytest.approx(9.0, abs=0.1)
    assert strength_for(waiver(5.0)) == 10.0
    assert [strength_for(waiver(g)) for g in (0.2, 0.5, 0.9, 1.5)] == sorted(
        strength_for(waiver(g)) for g in (0.2, 0.5, 0.9, 1.5))


def test_confidence_scales_preseason_moves_down():
    pre, late = waiver(0.95, gp=0), waiver(0.95, gp=1000)
    assert strength_for(pre) < strength_for(late)
    assert strength_for(pre) == pytest.approx(interp(0.95 * 0.25, WAIVER_SCALE), abs=0.01)   # floor 0.25


def test_legacy_trade_without_acceptance_keeps_the_dynasty_bump():
    base = dict(kind="trade", score=1.0, title="t", add=[pl("a", 1000)], drop=[pl("b", 1000)], predicted_gain=1.0,
                gain_units="lineup_fpg")
    plain = strength_for(Recommendation(**base))
    up = strength_for(Recommendation(**base, reasons=[Reason(code="DYNASTY_DELTA", text="", value=0.5)]))
    down = strength_for(Recommendation(**base, reasons=[Reason(code="DYNASTY_DELTA", text="", value=-0.5)]))
    assert up == pytest.approx(plain + 1.0) and down == pytest.approx(plain - 1.0)


def test_flags_injury_and_lineup_rules():
    flag = Recommendation(kind="sell_high", score=1, title="f", reasons=[Reason(code="FORM_RATIO", text="", value=1.6)])
    assert strength_for(flag) == pytest.approx(7.0)
    ir = Recommendation(kind="injury", score=1, title="Move x to IR", predicted_gain=0.1, gain_units="season_fpg",
                        reasons=[Reason(code="IR_SLOT", text="")])
    assert strength_for(ir) == 5.0
    bench = Recommendation(kind="lineup", score=1, title="Bench x", reasons=[Reason(code="STATUS", text="")])
    assert strength_for(bench) == 5.0
    wk = Recommendation(kind="lineup", score=1, title="s", predicted_gain=3.0, gain_units="week_pts", horizon_days=7)
    assert strength_for(wk) == pytest.approx(6.0) and gain_text(wk) == "+3.0 pts/wk"


def test_apply_ranks_groups_flags():
    recs = [Recommendation(kind=k, score=1, title=k) for k in ("waiver", "sell_high", "waiver", "buy_low")]
    apply_ranks(recs)
    assert [(r.rank_in_kind, r.kind_total) for r in recs] == [(1, 2), (1, 2), (2, 2), (2, 2)]


def trade(edge, p, gp=5000, units="lineup_fpg"):
    return Recommendation(kind="trade", score=edge * p, title="t", add=[pl("a", gp)], drop=[pl("b", gp)],
                          predicted_gain=edge, gain_units=units,
                          reasons=[Reason(code="MY_EDGE", text="", value=edge), Reason(code="P_ACCEPT", text="", value=p)])


def test_trade_strength_anchors_on_expected_value():
    # gp=5000 -> confidence ~0.995
    assert strength_for(trade(0.5, 0.7)) == pytest.approx(6.0, abs=0.1)     # EV 0.35: a realistic small win
    assert strength_for(trade(1.0, 0.6)) == pytest.approx(8.0, abs=0.1)     # EV 0.6
    assert strength_for(trade(2.0, 0.5)) == pytest.approx(10.0, abs=0.1)    # EV 1.0
    assert strength_for(trade(0.3, 0.5)) == pytest.approx(3.0, abs=0.1)     # EV 0.15
    assert strength_for(trade(3.0, 0.9)) == 10.0
    # the likely small deal outranks the unlikely big one
    assert strength_for(trade(0.5, 0.7)) > strength_for(trade(1.5, 0.2))


def test_trade_strength_keeps_confidence_scaling():
    pre, late = trade(0.5, 0.7, gp=0), trade(0.5, 0.7, gp=1000)
    assert strength_for(pre) < strength_for(late)
    assert strength_for(pre) == pytest.approx(interp(0.35 * 0.25, TRADE_EV_SCALE), abs=0.01)   # floor 0.25


def test_future_only_trade_strength_uses_the_dynasty_gain():
    r = trade(0.4, 0.5, units="dynasty")
    r.reasons[0] = Reason(code="MY_EDGE", text="", value=-0.9)          # this season's edge is negative
    assert strength_for(r) == pytest.approx(interp(0.2 * 0.995, TRADE_EV_SCALE), abs=0.05)
