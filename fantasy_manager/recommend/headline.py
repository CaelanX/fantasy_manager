"""One plain-English sentence per recommendation: the deciding reason, shown before the numbers.

``Recommendation.headline`` is filled by ``strength.apply_ranks`` (every recommender ends with it)
from the rec's kind and reason codes, unless the recommender already set a sharper one (daily
lineup swaps add the opponent: ``lineup.recommend_lineup``). Examples:

* lineup: "Stenberg has no game today; Nylander plays @CGY", "Hyman is back (dtd); Marchand takes
  his IR slot", "Nylander: 4 games this week vs 2 for Stenberg"
* waiver: "+1.5 pts over the rest of this matchup with your best daily lineup · market agrees",
  "Fills your empty D slot; no drop needed"
* trade: "Your lineup gains +0.98 FPG (+1.95 with long-term value); ~70% they accept · market agrees"
  (the lineup number is DELTA_ME, the table's ΔMe; the blend is MY_EDGE when it differs)
* injury: the hold / cut verdict or break-even; sell high / buy low: the form and luck signal

Market suffix (waivers / trades with MARKET_CHECK reasons): " · market agrees" or " · model
disagrees with market" (" · market says you overpay" for a trade the market calls clearly theirs).
"""
from __future__ import annotations

from typing import Iterable

from ..models import Player, Reason, Recommendation

MAX_LEN = 140


def _r(rec: Recommendation, code: str) -> Reason | None:
    return next((x for x in rec.reasons if x.code == code), None)


def _v(rec: Recommendation, code: str) -> float | None:
    x = _r(rec, code)
    return float(x.value) if x is not None and x.value is not None else None


def _player_reason(rec: Recommendation, code: str, p: Player) -> Reason | None:
    return next((x for x in rec.reasons if x.code == code and x.text.startswith(f"{p.name}:")), None)


def _cut(text: str) -> str:
    return text if len(text) <= MAX_LEN else text[:MAX_LEN - 1].rstrip() + "…"


def market_suffix(rec: Recommendation) -> str:
    checks = [x for x in rec.reasons if x.code == "MARKET_CHECK"]
    if not checks:
        return ""
    if any(x.value is None and x.text.startswith("Model disagrees") for x in checks):
        return " · market says you overpay"
    if any("disagrees" in x.text.lower() for x in checks):
        return " · model disagrees with market"
    return " · market agrees"


def day_swap_headline(pin: Player | None, pout: Player | None, pin_pts: float, pout_pts: float,
                      pout_plays: bool, slot: str | None = None, opp: str | None = None) -> str:
    """Daily start/sit: the deciding fact first (no game / not expected to play / more points)."""
    where = f"plays {opp}" if opp else "has a game"
    if pin is None and pout is not None:
        return f"{pout.name} isn't expected to score today; leave the slot empty"
    if pin is None:
        return ""
    if pout is None:
        return f"Empty {slot + ' ' if slot else ''}slot today: {pin.name} {where}"
    if not pout_plays:
        return f"{pout.name} has no game today; {pin.name} {where}"
    if pout_pts <= 0:
        return f"{pout.name} isn't expected to play today; {pin.name} {where}"
    return f"{pin.name} projects {pin_pts:.1f} pts today vs {pout_pts:.1f} for {pout.name}"


def _lineup(rec: Recommendation) -> str:
    codes = {x.code for x in rec.reasons}
    if "IR_SWAP" in codes and rec.add and rec.subjects:
        a, s = rec.add[0], rec.subjects[0]
        return f"{a.name} is back ({a.status}); {s.name} ({s.status}) takes his IR slot"
    if "RETURN_DUE" in codes:
        pts = _v(rec, "RETURN_DUE")
        who = rec.subjects[0].name if rec.subjects else "He"
        return f"{who} is due back this matchup" + (f": ~{pts:.1f} pts once activated" if pts else "")
    pin = rec.add[0] if rec.add else None
    pout = rec.drop[0] if rec.drop else None
    if pin is not None or pout is not None:
        di = _player_reason(rec, "DAY_VALUE", pin) if pin else None
        do = _player_reason(rec, "DAY_VALUE", pout) if pout else None
        if di is not None or do is not None:
            return day_swap_headline(pin, pout, float(di.value or 0.0) if di else 0.0,
                                     float(do.value or 0.0) if do else 0.0,
                                     do is not None and "no game today" not in do.text)
        gi = _player_reason(rec, "GAMES_NEXT7", pin) if pin else None
        go = _player_reason(rec, "GAMES_NEXT7", pout) if pout else None
        if pin and pout and gi and go and gi.value is not None and go.value is not None and gi.value != go.value:
            return f"{pin.name}: {gi.value:.0f} games this week vs {go.value:.0f} for {pout.name}"
    if "STATUS" in codes and pout is not None and pin is None:
        return f"{pout.name} is {pout.status}: he scores nothing from a starting slot"
    if "STATUS" in codes and pin is not None and pout is None and "IR" in rec.title:
        h = f"{pin.name} is {pin.status} and can play: he is wasting an IR slot"
        return h + ("; roster is full, so it needs a drop or an IR swap" if "ROSTER_FULL" in codes else "")
    gain = _v(rec, "LINEUP_GAIN")
    return f"{gain:+.2f} from this lineup change" if gain is not None else ""


def _waiver(rec: Recommendation) -> str:
    drop = rec.drop[0] if rec.drop else None
    vd = _r(rec, "VORP_DELTA")
    mp = _v(rec, "MATCHUP_GAIN")
    if mp is not None:
        h = f"{mp:+.1f} pts over the rest of this matchup with your best daily lineup"
    elif vd is not None and vd.text.startswith("fills empty"):
        h = "Fills your " + vd.text.split(" (")[0].replace("fills ", "")
    elif vd is not None and vd.value is not None:
        h = f"{vd.value:+.2f} FPG" + (f" over {drop.name}" if drop else "") + " for the rest of the season"
    else:
        h = ""
    if _r(rec, "OPEN_SPOT") is not None:
        h += "; no drop needed"
    elif _r(rec, "IR_MOVE") is not None and drop is None:
        h += f"; {rec.subjects[0].name if rec.subjects else 'an injured player'} goes to IR, no drop"
    if _r(rec, "WAIVER_CLAIM") is not None and rec.add and rec.add[0].waiver_until is not None:
        h += f"; claim (on waivers until {rec.add[0].waiver_until:%a})"
    return h.lstrip("; ")


def _trade(rec: Recommendation) -> str:
    """Leads with ΔMe (DELTA_ME, the table's lineup number); a dynasty blend that differs is named."""
    edge, me, p = _v(rec, "MY_EDGE"), _v(rec, "DELTA_ME"), _v(rec, "P_ACCEPT")
    if rec.gain_units == "dynasty" and rec.predicted_gain is not None:
        h = f"Dynasty value {rec.predicted_gain:+.2f} for you"
    elif me is not None:
        h = f"Your lineup {'gains' if me >= 0 else 'changes'} {me:+.2f} FPG"
        if edge is not None and abs(edge - me) >= 0.05:
            h += f" ({edge:+.2f} with long-term value)"
    elif edge is not None:
        h = f"Your lineup gains {edge:+.2f} FPG"
    else:
        h = ""
    if p is not None:
        h += f"; ~{p:.0%} they accept"
    return h.lstrip("; ")


def _injury(rec: Recommendation) -> str:
    if (hv := _r(rec, "HOLD_VERDICT")) is not None:
        return hv.text
    cut, hold, be = _v(rec, "CUT_PTS"), _v(rec, "HOLD_PTS"), _r(rec, "BREAK_EVEN")
    if cut is not None and hold is not None:
        verdict = "Cutting" if rec.drop else "Holding"
        diff = abs(cut - hold)
        return f"{verdict} wins by ~{diff:.0f} pts rest of season" + (f"; {be.text[0].lower()}{be.text[1:]}" if be else "")
    if (sc := _r(rec, "STATUS_CHANGE")) is not None:
        return sc.text
    if _r(rec, "IR_SLOT") is not None and rec.subjects:
        return f"An IR slot is free: {rec.subjects[0].name} costs no roster spot there"
    st = _r(rec, "STATUS")
    return st.text if st is not None else ""


def _flag(rec: Recommendation) -> str:
    ratio, luck = _v(rec, "FORM_RATIO"), _v(rec, "XG_LUCK")
    if rec.kind == "sell_high":
        h = f"Running hot: last 15 days {ratio:.2f}x his baseline" if ratio is not None else "Running hot"
        if luck is not None and luck > 0:
            h += ", scoring above his expected goals"
    else:
        h = f"Cold stretch: last 15 days {ratio:.2f}x his baseline" if ratio is not None else "Cold stretch"
        if _r(rec, "SOG_RATE") is not None:
            h += "; his shots held up"
        if luck is not None and luck < 0:
            h += ", finishing below expected"
    return h


def headline_for(rec: Recommendation) -> str | None:
    """The deciding reason of ``rec`` in one sentence (None when nothing says more than the title)."""
    if rec.kind == "lineup":
        h = _lineup(rec)
    elif rec.kind == "waiver":
        h = _waiver(rec) + market_suffix(rec)
    elif rec.kind == "trade":
        h = _trade(rec) + market_suffix(rec)
    elif rec.kind == "injury":
        h = _injury(rec)
    elif rec.kind in ("sell_high", "buy_low"):
        h = _flag(rec)
    else:
        h = rec.reasons[0].text if rec.reasons else ""
    h = h.strip(" ·;")
    return _cut(h[0].upper() + h[1:]) if h else None


def apply_headlines(recs: Iterable[Recommendation]) -> None:
    """Set ``headline`` (in place) on every rec that has none; a failure leaves it None."""
    for r in recs:
        if r.headline is None:
            try:
                r.headline = headline_for(r)
            except Exception:  # never let a display field sink a recommender
                r.headline = None
