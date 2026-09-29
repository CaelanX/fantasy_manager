"""Sell-high / buy-low flags from recent form versus a shrunk season baseline.

form_ratio = FPG over the last 15 days / season FPG shrunk toward the projection/prior
(requires >= 5 GP in the L15 split, otherwise the player is skipped).

* sell_high (my players): form_ratio > 1.35 AND a luck signal - L15 shooting% more than 1.6x
  the baseline (season + prior pooled, else projection); goalies use an L15 SV% at least
  .015 above baseline instead.
* buy_low (other teams' players): form_ratio < 0.7 while L15 SOG/GP is >= 90% of the
  baseline rate (the process is intact) and the player is healthy. Skaters only.
"""
from __future__ import annotations

from typing import Any, Mapping

from ..models import LeagueContext, Player, Reason, Recommendation
from ..scoring import ScoringSystem, fit_to_context, from_config
from ..valuation.blend import shrunk_rates
from .strength import apply_ranks, apply_strength

MIN_GP_L15 = 5
SELL_FORM = 1.35
BUY_FORM = 0.7
SH_SPIKE = 1.6
SV_SPIKE = 0.015
SOG_STABLE = 0.9
MIN_BASE_SOG = 20.0
MIN_BASE_FPG = 0.05
HEALTHY = ("healthy", "unknown")
# predicted_gain of a flag = expected FPG change from L15 form back to the shrunk baseline
# (negative for sell-high, positive for buy-low), graded over the next 28 days.
FLAG_HORIZON_DAYS = 28


def _baseline_line(p: Player):
    proj = p.lines.get("projected")
    if proj and proj.gp > 0:
        return proj
    prior = p.lines.get("prior")
    return prior if prior and prior.gp > 0 else None


def _shooting(p: Player, splits: tuple[str, ...]) -> tuple[float | None, float]:
    """(G/SOG, SOG) pooled over the first splits that carry shots."""
    g = sog = 0.0
    for s in splits:
        ln = p.lines.get(s)
        if ln and ln.stats.get("SOG"):
            g += float(ln.stats.get("G", 0.0))
            sog += float(ln.stats["SOG"])
    return (g / sog if sog > 0 else None), sog


def _baseline_shooting(p: Player) -> tuple[float | None, float]:
    pct, sog = _shooting(p, ("season", "prior"))
    if pct is None or sog < MIN_BASE_SOG:
        pct2, sog2 = _shooting(p, ("projected",))
        if pct2 is not None and sog2 >= sog:
            return pct2, sog2
    return pct, sog


def _svpct(stats: Mapping[str, float]) -> float | None:
    if "SVPCT" in stats:
        v = float(stats["SVPCT"])
        return v / 100 if v > 1 else v
    sa = stats.get("SA")
    if sa:
        sv = stats.get("SV", sa - stats.get("GA", 0.0))
        return float(sv) / float(sa)
    return None


def _form(p: Player, scoring: ScoringSystem) -> tuple[float, float, float, dict[str, float]] | None:
    """(form_ratio, fpg_l15, fpg_base, shrunk season rates) or None when not measurable."""
    l15 = p.lines.get("last15")
    if not l15 or l15.gp < MIN_GP_L15:
        return None
    rates = shrunk_rates(p.lines.get("season"), _baseline_line(p), p.is_goalie)
    if not rates:
        return None
    base = scoring.value(rates)
    if base <= MIN_BASE_FPG:
        return None
    fpg15 = scoring.value(l15.per_game())
    return fpg15 / base, fpg15, base, rates


def _pv_weight(values: Mapping[str, Any], p: Player) -> float:
    pv = values.get(p.cid)
    return max(float(getattr(pv, "vorp", 0.0) or 0.0), 0.0) + 0.5


def recommend_flags(ctx: LeagueContext, values: Mapping[str, Any],
                    scoring: ScoringSystem | None = None, limit: int | None = None
                    ) -> list[Recommendation]:
    scoring = scoring or fit_to_context(from_config(ctx.scoring), ctx)
    me = ctx.my_team
    recs: list[Recommendation] = []

    for p in me.players:
        f = _form(p, scoring)
        if f is None or f[0] <= SELL_FORM:
            continue
        ratio, fpg15, base, _ = f
        l15 = p.lines["last15"]
        reasons = [Reason(code="FORM_RATIO", text=f"L15 {fpg15:.2f} FPG vs {base:.2f} shrunk season "
                          f"baseline ({ratio:.2f}x)", value=ratio, baseline=SELL_FORM),
                   Reason(code="GP_L15", text=f"{l15.gp} GP in the last 15 days",
                          value=float(l15.gp), baseline=float(MIN_GP_L15))]
        if p.is_goalie:
            cur = _svpct(l15.stats)
            bl = _baseline_line(p)
            ref = _svpct(p.lines["season"].stats) if "season" in p.lines else None
            ref = ref if ref is not None else (_svpct(bl.stats) if bl else None)
            if cur is None or ref is None or cur - ref < SV_SPIKE:
                continue
            reasons.append(Reason(code="SAVE_PCT", text=f"L15 SV% {cur:.3f} vs baseline {ref:.3f}",
                                  value=cur, baseline=ref))
            luck = 1.0 + (cur - ref) * 20
        else:
            cur, sog15 = _shooting(p, ("last15",))
            ref, _ = _baseline_shooting(p)
            if cur is None or not ref or cur / ref <= SH_SPIKE:
                continue
            reasons.append(Reason(code="SHOOTING_PCT",
                                  text=f"L15 shooting {cur:.1%} vs baseline {ref:.1%} ({cur / ref:.1f}x)",
                                  value=cur, baseline=ref))
            luck = cur / ref
        pv = values.get(p.cid)
        recs.append(Recommendation(
            kind="sell_high", score=(ratio - 1.0) * min(luck, 3.0) * _pv_weight(values, p),
            title=f"Sell high on {p.name}", drop=[p],
            predicted_gain=round(base - fpg15, 4), gain_units="season_fpg", horizon_days=FLAG_HORIZON_DAYS,
            reasons=reasons
            + ([Reason(code="VORP", text=f"Season VORP {pv.vorp:+.2f}", value=pv.vorp)] if pv else [])))

    for t in ctx.teams:
        if t.team_id == me.team_id:
            continue
        for p in t.players:
            if p.is_goalie or p.status not in HEALTHY:
                continue
            pv = values.get(p.cid)
            if pv is not None and getattr(pv, "vorp", 0.0) <= 0:
                continue
            f = _form(p, scoring)
            if f is None or f[0] >= BUY_FORM:
                continue
            ratio, fpg15, base, rates = f
            l15 = p.lines["last15"]
            sog15 = l15.stats.get("SOG", 0.0) / l15.gp
            sog_base = rates.get("SOG")
            if not sog_base or sog15 < SOG_STABLE * sog_base:
                continue
            reasons = [Reason(code="FORM_RATIO", text=f"L15 {fpg15:.2f} FPG vs {base:.2f} shrunk season "
                              f"baseline ({ratio:.2f}x)", value=ratio, baseline=BUY_FORM),
                       Reason(code="SOG_RATE", text=f"Still {sog15:.2f} SOG/GP vs {sog_base:.2f} baseline",
                              value=sog15, baseline=sog_base),
                       Reason(code="GP_L15", text=f"{l15.gp} GP in the last 15 days",
                              value=float(l15.gp), baseline=float(MIN_GP_L15))]
            cur, _ = _shooting(p, ("last15",))
            ref, _ = _baseline_shooting(p)
            if cur is not None and ref:
                reasons.append(Reason(code="SHOOTING_PCT",
                                      text=f"L15 shooting {cur:.1%} vs baseline {ref:.1%}",
                                      value=cur, baseline=ref))
            recs.append(Recommendation(
                kind="buy_low", score=(1.0 - ratio) * _pv_weight(values, p),
                title=f"Buy low on {p.name} ({t.name})", add=[p], counterparty=t.name,
                predicted_gain=round(base - fpg15, 4), gain_units="season_fpg", horizon_days=FLAG_HORIZON_DAYS,
                reasons=reasons))

    recs.sort(key=lambda r: r.score, reverse=True)
    return apply_ranks(apply_strength(recs[:limit] if limit else recs))
