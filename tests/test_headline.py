"""Recommendation headlines: the deciding reason in one sentence, shown before the numbers."""
from datetime import date

import pytest

from fantasy_manager.models import Player, Reason, Recommendation
from fantasy_manager.recommend.headline import apply_headlines, day_swap_headline, headline_for
from fantasy_manager.recommend.strength import DAY_SCALE, apply_ranks, interp, strength_for


def pl(name, status="healthy", waiver_until=None):
    return Player(cid=name, name=name, name_norm=name.lower(), ids={}, team="EDM", positions=["C"],
                  status=status, waiver_until=waiver_until)


def rec(kind, reasons, **kw):
    return Recommendation(kind=kind, score=1.0, title="t", reasons=reasons, **kw)


def test_day_swap_headlines():
    n, s = pl("Nylander"), pl("Stenberg")
    assert day_swap_headline(n, s, 5.4, 0.0, False, "F", "@CGY") == "Stenberg has no game today; Nylander plays @CGY"
    assert day_swap_headline(n, s, 5.4, 0.0, True) == "Stenberg isn't expected to play today; Nylander has a game"
    assert day_swap_headline(n, s, 5.4, 3.1, True) == "Nylander projects 5.4 pts today vs 3.1 for Stenberg"
    assert day_swap_headline(n, None, 5.4, 0.0, False, "F", "LAK") == "Empty F slot today: Nylander plays LAK"


def test_lineup_headline_from_reasons_without_ctx():
    n, s = pl("Nylander"), pl("Stenberg")
    r = rec("lineup", [Reason(code="DAY_VALUE", text="Nylander: 5.40 projected pts today", value=5.4),
                       Reason(code="DAY_VALUE", text="Stenberg: no game today", value=0.0)], add=[n], drop=[s])
    assert headline_for(r) == "Stenberg has no game today; Nylander has a game"
    weekly = rec("lineup", [Reason(code="GAMES_NEXT7", text="Nylander: 4 games next 7 days", value=4.0),
                            Reason(code="GAMES_NEXT7", text="Stenberg: 2 games next 7 days", value=2.0)],
                 add=[n], drop=[s])
    assert headline_for(weekly) == "Nylander: 4 games this week vs 2 for Stenberg"


def test_ir_swap_and_bench_headlines():
    hy, ma = pl("Hyman", "dtd"), pl("Marchand", "ir")
    swap = rec("lineup", [Reason(code="IR_SWAP", text="...")], add=[hy], subjects=[ma])
    assert headline_for(swap) == "Hyman is back (dtd); Marchand (ir) takes his IR slot"
    bench = rec("lineup", [Reason(code="STATUS", text="Marchand is ir")], drop=[ma])
    assert headline_for(bench) == "Marchand is ir: he scores nothing from a starting slot"


def test_waiver_headlines_lead_with_matchup_points_and_market():
    a, d = pl("Brodin"), pl("Giroux")
    stream = rec("waiver", [Reason(code="VORP_DELTA", text="+0.5 FPG (week)", value=0.5),
                            Reason(code="MATCHUP_GAIN", text="+1.5 pts through Sun", value=1.5),
                            Reason(code="MARKET_CHECK", text="Brodin: market 40th vs model 45th, agrees", value=5.0)],
                 add=[a], drop=[d])
    assert headline_for(stream) == "+1.5 pts over the rest of this matchup with your best daily lineup · market agrees"
    fill = rec("waiver", [Reason(code="VORP_DELTA", text="fills empty D slot (+3.76 FPG, season); drop bench F X",
                                 value=3.76),
                          Reason(code="MARKET_CHECK", text="Pionk: model disagrees with market, rates him higher",
                                 value=30.0)], add=[a], drop=[d])
    assert headline_for(fill) == "Fills your empty D slot · model disagrees with market"
    season = rec("waiver", [Reason(code="VORP_DELTA", text="+0.72 FPG (season)", value=0.72),
                            Reason(code="OPEN_SPOT", text="1 open roster spot", value=1.0)], add=[a])
    assert headline_for(season) == "+0.72 FPG for the rest of the season; no drop needed"
    claim = rec("waiver", [Reason(code="VORP_DELTA", text="+0.72", value=0.72),
                           Reason(code="WAIVER_CLAIM", text="claim")],
                add=[pl("X", waiver_until=date(2026, 10, 28))], drop=[d])
    assert headline_for(claim).endswith("; claim (on waivers until Wed)")


def test_trade_headline_flags_overpay():
    r = rec("trade", [Reason(code="MY_EDGE", text="edge", value=0.81), Reason(code="P_ACCEPT", text="p", value=0.9),
                      Reason(code="MARKET_CHECK", text="Model disagrees with market: by market value you overpay",
                             value=None, baseline=21.0)])
    assert headline_for(r) == "Your lineup gains +0.81 FPG; ~90% they accept · market says you overpay"


def test_injury_and_flag_headlines():
    hold = rec("injury", [Reason(code="HOLD_VERDICT", text="Swapping IR slots makes keeping Marchand free")])
    assert headline_for(hold) == "Swapping IR slots makes keeping Marchand free"
    cut = rec("injury", [Reason(code="HOLD_PTS", text="h", value=40.0), Reason(code="CUT_PTS", text="c", value=90.0),
                         Reason(code="BREAK_EVEN", text="Cut only if he misses more than ~20 games")],
              drop=[pl("Tippett", "ir")], add=[pl("Pionk")])
    assert headline_for(cut) == "Cutting wins by ~50 pts rest of season; cut only if he misses more than ~20 games"
    hot = rec("sell_high", [Reason(code="FORM_RATIO", text="f", value=1.6), Reason(code="XG_LUCK", text="x", value=0.4)])
    assert headline_for(hot) == "Running hot: last 15 days 1.60x his baseline, scoring above his expected goals"
    cold = rec("buy_low", [Reason(code="FORM_RATIO", text="f", value=0.59), Reason(code="SOG_RATE", text="s"),
                           Reason(code="XG_LUCK", text="x", value=-0.5)])
    assert headline_for(cold) == "Cold stretch: last 15 days 0.59x his baseline; his shots held up, finishing below expected"


def test_apply_ranks_fills_missing_headlines_only():
    a = rec("alert", [Reason(code="X", text="Judd Caulfield now on PP1")])
    keep = rec("lineup", [], headline="already set")
    apply_ranks([a, keep])
    assert a.headline == "Judd Caulfield now on PP1" and keep.headline == "already set"
    apply_headlines([rec("waiver", [])])      # nothing to say: stays None, never raises


def test_day_scale_keeps_routine_swaps_below_ten():
    assert interp(5.4, DAY_SCALE) == pytest.approx(5.6)
    r = Recommendation(kind="lineup", score=5.4, title="t", predicted_gain=5.4, gain_units="day_pts", horizon_days=1)
    assert 5.0 < strength_for(r) < 6.5
    assert strength_for(r.model_copy(update={"predicted_gain": 25.0})) == 10.0


def test_cli_why_cell_puts_the_headline_first():
    from fantasy_manager.cli import _why

    r = rec("waiver", [], headline="Adds 2 games [on light nights]")
    assert _why(r, ["reason one"]).splitlines() == ["[bold]Adds 2 games \\[on light nights][/]", "reason one"]
    assert _why(rec("waiver", []), ["only"]) == "only"


def test_cli_why_cell_skips_a_reason_equal_to_the_headline():
    from fantasy_manager.cli import _why

    r = rec("injury", [], headline="Hold, don't cut")
    assert _why(r, ["Hold, don't cut", "other"]).splitlines() == ["[bold]Hold, don't cut[/]", "other"]
