"""Sell-high / buy-low flags from recent form versus a shrunk season baseline.

form_ratio = FPG over the last 15 days / season FPG shrunk toward the projection/prior
(requires >= 5 GP in the L15 split, otherwise the player is skipped).

* sell_high (my players): form_ratio > 1.35 AND a luck signal - L15 shooting% more than 1.6x
  the baseline (season + prior pooled, else projection); goalies use an L15 SV% at least
  .015 above baseline instead.
* buy_low (other teams' players): form_ratio < 0.7 while L15 SOG/GP is >= 90% of the
  baseline rate (the process is intact) and the player is healthy. Skaters only.

Expected goals (MoneyPuck, ``valuation.regression.luck_signals``): when a skater carries xG
fields, the luck test uses them instead of the raw shooting% ratio. Luck = (goals - ixG) per
game of the MoneyPuck season used; its fantasy size = that x the league's goal weight
(``goal_luck_fpg``). sell_high needs goals at least ``XG_LUCK_GPG`` (0.06) per game above ixG
(about 5 goals over 82 games); buy_low skips a cold player who is still scoring above his ixG
(his slump is not bad luck) and ranks the ones below it higher. Reason XG_LUCK (credit
MoneyPuck.com). Without xG fields the shooting% heuristic above is unchanged.

Role-change alerts (``recommend_role_alerts``, kind "alert"): TOI / PP-share changes from the NHL
per-game deployment reports; see the section at the end of this module. It also returns the
"Preseason standout" alerts (``recommend_preseason_alerts``) and "Rookie role signal" alerts
(``recommend_rookie_role_alerts``, last section).
"""
from __future__ import annotations

from typing import Any, Mapping

from ..models import LeagueContext, Player, Reason, Recommendation
from ..scoring import ScoringSystem, fit_to_context, from_config
from ..valuation.blend import shrunk_rates
from ..valuation.regression import luck_signals
from .strength import apply_ranks, apply_strength, interp

MIN_GP_L15 = 5
SELL_FORM = 1.35
BUY_FORM = 0.7
SH_SPIKE = 1.6
SV_SPIKE = 0.015
SOG_STABLE = 0.9
MIN_BASE_SOG = 20.0
MIN_BASE_FPG = 0.05
HEALTHY = ("healthy", "unknown")
XG_LUCK_GPG = 0.06          # goals - ixG per game that counts as luck (sell-high threshold)
XG_LUCK_SCALE = 10.0        # luck multiplier = 1 + 10 x |goals - ixG| per game (0.06 -> 1.6, like SH_SPIKE)
XG_CREDIT = "MoneyPuck.com"
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


def xg_luck(p: Player, scoring: Any = None) -> tuple[float, Reason] | None:
    """(goals - ixG per game, XG_LUCK reason) from MoneyPuck fields, None without them."""
    sig = luck_signals(p, scoring=scoring)
    if sig is None or sig.gp <= 0:
        return None
    gpg = sig.goals_minus_ixg / sig.gp
    unit = "FPG" if scoring is not None and getattr(scoring, "kind", "points") == "points" else "goals/GP"
    prior = "last season, " if sig.from_prior else ""
    text = (f"Goals {sig.goals_per_game:.2f}/GP vs ixG {sig.ixg_per_game:.2f}/GP over {sig.gp} GP "
            f"({prior}{sig.goals_minus_ixg:+.1f} goals vs expected): about {sig.goal_luck_fpg:+.2f} {unit} of "
            f"finishing luck ({XG_CREDIT})")
    return gpg, Reason(code="XG_LUCK", text=text, value=round(sig.goal_luck_fpg, 4), baseline=0.0)


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
            xl = xg_luck(p, ctx.scoring)
            if xl is not None:                         # expected goals replace the shooting% test
                gpg, reason = xl
                if gpg < XG_LUCK_GPG:
                    continue
                reasons.append(reason)
                luck = 1.0 + XG_LUCK_SCALE * gpg
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
            xl = xg_luck(p, ctx.scoring)
            bonus = 1.0
            if xl is not None:
                gpg, reason = xl
                if gpg >= XG_LUCK_GPG:                 # cold overall but still finishing above ixG
                    continue
                reasons.append(reason)
                bonus = 1.0 + min(2.0, XG_LUCK_SCALE * max(0.0, -gpg))
            else:
                cur, _ = _shooting(p, ("last15",))
                ref, _ = _baseline_shooting(p)
                if cur is not None and ref:
                    reasons.append(Reason(code="SHOOTING_PCT",
                                          text=f"L15 shooting {cur:.1%} vs baseline {ref:.1%}",
                                          value=cur, baseline=ref))
            recs.append(Recommendation(
                kind="buy_low", score=(1.0 - ratio) * bonus * _pv_weight(values, p),
                title=f"Buy low on {p.name} ({t.name})", add=[p], counterparty=t.name,
                predicted_gain=round(base - fpg15, 4), gain_units="season_fpg", horizon_days=FLAG_HORIZON_DAYS,
                reasons=reasons))

    recs.sort(key=lambda r: r.score, reverse=True)
    return apply_ranks(apply_strength(recs[:limit] if limit else recs))


# --------------------------------------------------------------------------- role-change alerts
#
# Deployment changes from the NHL per-game TOI / PP reports (providers.deployment_enrich fills
# Player.toi_trend / pp_share_trend = last-N GP minus the baseline). An alert (kind "alert", the
# player in ``subjects``, counterparty = the owning fantasy team or "FA") fires when:
#   * ROLE_TOI: TOI up >= 1.5 min per game vs baseline;
#   * ROLE_PP:  PP share up >= 0.15, or PP share from < 0.2 to >= 0.5 (PP1 promotion by usage).
# For my own players the mirror image (TOI down >= 1.5, PP share down >= 0.15, or from >= 0.5 to
# < 0.2) is a "role loss" alert. Free agents and other teams' players only get the "up" alerts
# (a free agent's promotion is waiver gold; a rival's is a buy target).
# Strength (0-10, set here because recommend.strength has no "alert" scale): |TOI change| 1.5 -> 4,
# 3 -> 7, 5 -> 10; |PP share change| 0.15 -> 4, 0.3 -> 7, 0.5 -> 10; a PP1 promotion / loss is
# at least 7; +1 when both TOI and PP moved; capped at 10. No predicted gain (not graded).

ROLE_TOI_DELTA = 1.5
ROLE_PP_DELTA = 0.15
PP1_FROM = 0.2
PP1_TO = 0.5
ROLE_TOI_SCALE: tuple[tuple[float, float], ...] = ((0.0, 0.0), (1.5, 4.0), (3.0, 7.0), (5.0, 10.0))
ROLE_PP_SCALE: tuple[tuple[float, float], ...] = ((0.0, 0.0), (0.15, 4.0), (0.3, 7.0), (0.5, 10.0))
PP1_FLOOR = 7.0
BOTH_BONUS = 1.0
FA_LABEL = "FA"


def _role_numbers(p: Player, d: Mapping[str, Any] | None) -> dict[str, Any]:
    """Recent / baseline / trend for TOI and PP share. With a deployment summary ``d`` the exact
    numbers; otherwise baseline = the season value on the Player and recent = baseline + trend."""
    if d:
        return {"toi": d.get("toi_avg"), "toi_base": d.get("baseline_toi"), "toi_trend": d.get("toi_trend"),
                "pp": d.get("pp_share_avg"), "pp_base": d.get("baseline_pp_share"),
                "pp_trend": d.get("pp_share_trend"), "n": d.get("last_n")}
    tt, pt = p.toi_trend, p.pp_share_trend
    return {"toi": None if tt is None or p.toi_per_game is None else p.toi_per_game + tt,
            "toi_base": p.toi_per_game, "toi_trend": tt,
            "pp": None if pt is None or p.pp_share is None else p.pp_share + pt,
            "pp_base": p.pp_share, "pp_trend": pt, "n": None}


def role_change(p: Player, d: Mapping[str, Any] | None = None, sign: int = 1) -> dict[str, Any] | None:
    """Which role thresholds ``p`` crossed in direction ``sign`` (+1 up, -1 down), with the
    numbers and the 0-10 strength; None when none did."""
    n = _role_numbers(p, d)
    tt, pt = n["toi_trend"], n["pp_trend"]
    toi_hit = tt is not None and sign * tt >= ROLE_TOI_DELTA
    pp_hit = pt is not None and sign * pt >= ROLE_PP_DELTA
    pp1 = False
    if n["pp"] is not None and n["pp_base"] is not None:
        pp1 = (n["pp_base"] < PP1_FROM and n["pp"] >= PP1_TO) if sign > 0 else \
              (n["pp_base"] >= PP1_TO and n["pp"] < PP1_FROM)
    if not (toi_hit or pp_hit or pp1):
        return None
    s = 0.0
    if toi_hit:
        s = max(s, interp(abs(tt), ROLE_TOI_SCALE))
    if pp_hit:
        s = max(s, interp(abs(pt), ROLE_PP_SCALE))
    if pp1:
        s = max(s, PP1_FLOOR)
    if toi_hit and (pp_hit or pp1):
        s += BOTH_BONUS
    return {**n, "toi_hit": toi_hit, "pp_hit": pp_hit, "pp1": pp1, "strength": round(min(10.0, s), 2)}


def _role_reasons(c: Mapping[str, Any]) -> list[Reason]:
    window = f"last {c['n']} GP" if c.get("n") else "recent games"
    out = []
    if c["toi_hit"] or (c["toi_trend"] is not None and abs(c["toi_trend"]) >= ROLE_TOI_DELTA):
        base = f" vs {c['toi_base']:.1f} baseline" if c["toi_base"] is not None else ""
        now = f"{c['toi']:.1f} min" if c["toi"] is not None else "TOI"
        out.append(Reason(code="ROLE_TOI", text=f"TOI {now} per game over the {window}{base} "
                          f"({c['toi_trend']:+.1f} min)", value=c["toi"], baseline=c["toi_base"]))
    if c["pp_hit"] or c["pp1"]:
        base = f" vs {c['pp_base']:.0%} baseline" if c["pp_base"] is not None else ""
        now = f"{c['pp']:.0%}" if c["pp"] is not None else "?"
        trend = f" ({c['pp_trend'] * 100:+.0f} pts)" if c["pp_trend"] is not None else ""
        tag = ""
        if c["pp1"]:
            tag = " - PP1 usage" if (c["pp"] or 0) >= PP1_TO else " - off PP1"
        out.append(Reason(code="ROLE_PP", text=f"PP share {now} of team PP time over the {window}{base}{trend}{tag}",
                          value=c["pp"], baseline=c["pp_base"]))
    return out


def recommend_role_alerts(ctx: LeagueContext, values: Mapping[str, Any] | None = None, *,
                          details: Mapping[Any, Mapping[str, Any]] | None = None,
                          limit: int | None = None) -> list[Recommendation]:
    """Role-change alerts for every skater with deployment trends: my players (up and down),
    other teams' players and free agents (up only). ``details`` = {cid or nhl_id: deployment
    summary} (``providers.deployment_enrich.enrich_deployment(...)["details"]``, kept on
    ``ctx.deployment_details`` by ``providers.enrich``) for exact numbers; without either the
    Player fields are used. ``values`` is accepted (advise passes it) but not needed."""
    if details is None and ctx.deployment_details:
        details = ctx.deployment_details
    try:
        me = ctx.my_team
    except LookupError:
        me = None
    owner: dict[str, tuple[str, bool]] = {}
    for t in ctx.teams:
        for p in t.players:
            owner.setdefault(p.cid, (t.name, bool(me is not None and t.team_id == me.team_id)))
    recs: list[Recommendation] = []
    for p in ctx.all_players():
        if p.is_goalie or (p.toi_trend is None and p.pp_share_trend is None
                           and not (details and (p.cid in details or p.nhl_id in details))):
            continue
        d = None
        if details:
            d = details.get(p.cid) or (details.get(p.nhl_id) if p.nhl_id is not None else None)
        team_name, mine = owner.get(p.cid, (FA_LABEL, False))
        sign, c = 1, role_change(p, d, 1)
        if c is None and mine:
            sign, c = -1, role_change(p, d, -1)
        if c is None:
            continue
        what = []
        if c["toi_hit"]:
            what.append(f"TOI {c['toi_trend']:+.1f}")
        if c["pp1"]:
            what.append("PP1" if sign > 0 else "off PP1")
        elif c["pp_hit"]:
            what.append(f"PP share {c['pp_trend'] * 100:+.0f} pts")
        where = "free agent" if team_name == FA_LABEL else ("my team" if mine else team_name)
        title = f"{'Role up' if sign > 0 else 'Role loss'}: {p.name} ({', '.join(what)}; {where})"
        recs.append(Recommendation(kind="alert", score=c["strength"], title=title, subjects=[p],
                                   counterparty=team_name, reasons=_role_reasons(c), strength=c["strength"]))
    # preseason standouts and rookie news-role signals ride along (the advise "alerts" engines are
    # fixed in recommend.advise)
    try:
        recs.extend(recommend_preseason_alerts(ctx))
    except Exception:  # an optional signal must never hide the role alerts
        pass
    try:
        recs.extend(recommend_rookie_role_alerts(ctx))
    except Exception:
        pass
    recs.sort(key=lambda r: (-(r.strength or 0.0), r.title))
    return apply_ranks(recs[:limit] if limit else recs)


# --------------------------------------------------------------------------- preseason standouts
#
# Unproven skaters (providers.preseason_enrich.is_unproven: career NHL GP < 82 or no prior season
# with >= 20 GP) whose preseason line (registered by the enrich preseason step, NHL box scores)
# shows >= 3 GP and either >= 1.0 PTS/GP or >= 16 min TOI per game. My players, other teams'
# players (counterparty = their team) and free agents (counterparty "FA"). Only while the
# player has < 5 regular-season GP (after that his real games speak louder).
# Strength 4-6 (preseason is weak evidence, so it never outranks a real role change):
# PTS/GP 1.0 -> 4.5, 2.0+ -> 6; TOI 16 -> 4, 20+ -> 5; +0.5 when both thresholds are met; capped 6.
# No predicted gain (not graded).

PRESEASON_MIN_GP = 3
PRESEASON_PTS_PG = 1.0
PRESEASON_TOI = 16.0
PRESEASON_MAX_SEASON_GP = 5
PRESEASON_STRENGTH = (4.0, 6.0)


def _ramp(x: float, x0: float, x1: float, y0: float, y1: float) -> float:
    return y0 + (y1 - y0) * min(1.0, max(0.0, (x - x0) / (x1 - x0)))


def preseason_standout(line: Any) -> dict[str, Any] | None:
    """Thresholds and 4-6 strength of a preseason line; None when it is not a standout."""
    if line is None or line.gp < PRESEASON_MIN_GP:
        return None
    ppg, toi = line.pts_per_game, line.toi_per_game
    pts_hit = ppg >= PRESEASON_PTS_PG
    toi_hit = toi is not None and toi >= PRESEASON_TOI
    if not (pts_hit or toi_hit):
        return None
    s = 0.0
    if pts_hit:
        s = max(s, _ramp(ppg, PRESEASON_PTS_PG, 2.0, 4.5, 6.0))
    if toi_hit:
        s = max(s, _ramp(toi, PRESEASON_TOI, 20.0, 4.0, 5.0))
    if pts_hit and toi_hit:
        s += 0.5
    lo, hi = PRESEASON_STRENGTH
    return {"pts_pg": ppg, "toi": toi, "pts_hit": pts_hit, "toi_hit": toi_hit,
            "strength": round(min(hi, max(lo, s)), 2)}


def _pp_label(p: Player) -> str:
    if p.pp_unit:
        return p.pp_unit.upper()
    return "no PP unit" if p.line else "PP unit unknown"


def recommend_preseason_alerts(ctx: LeagueContext, values: Mapping[str, Any] | None = None, *,
                               lines: Mapping[str, Any] | None = None,
                               limit: int | None = None) -> list[Recommendation]:
    """"Preseason standout" alerts (see the section comment). ``lines`` = {cid: PreseasonLine}
    overrides the registry (``providers.preseason_enrich.preseason_lines``)."""
    if lines is None:
        from ..providers.preseason_enrich import preseason_lines
        lines = preseason_lines(ctx)
    if not lines:
        return []
    from ..providers.preseason_enrich import is_unproven
    try:
        me = ctx.my_team
    except LookupError:
        me = None
    owner: dict[str, tuple[str, bool]] = {}
    for t in ctx.teams:
        for p in t.players:
            owner.setdefault(p.cid, (t.name, bool(me is not None and t.team_id == me.team_id)))
    recs: list[Recommendation] = []
    for p in ctx.all_players():
        line = lines.get(p.cid)
        if line is None or p.is_goalie or not is_unproven(p) or p.gp("season") >= PRESEASON_MAX_SEASON_GP:
            continue
        c = preseason_standout(line)
        if c is None:
            continue
        team_name, mine = owner.get(p.cid, (FA_LABEL, False))
        where = "free agent" if team_name == FA_LABEL else ("my team" if mine else team_name)
        toi = f"TOI {c['toi']:.1f}" if c["toi"] is not None else "TOI n/a"
        title = (f"Preseason standout: {p.name} ({line.gp} GP, {c['pts_pg']:.2f} PTS/GP, {toi}, "
                 f"{_pp_label(p)}; {where})")
        career = f"{p.career_gp} career NHL GP" if p.career_gp is not None else "no NHL season with 20+ GP"
        reasons = [Reason(code="PRESEASON", text=f"Preseason {line.summary()} ({c['pts_pg']:.2f} PTS/GP)",
                          value=round(c["pts_pg"], 4), baseline=PRESEASON_PTS_PG)]
        if c["toi"] is not None:
            reasons.append(Reason(code="PRESEASON_TOI", text=f"{c['toi']:.1f} min TOI per preseason game",
                                  value=round(c["toi"], 2), baseline=PRESEASON_TOI))
        if p.pp_unit:
            reasons.append(Reason(code="PP_UNIT", text=f"Daily Faceoff lists him on {p.pp_unit.upper()}"))
        reasons.append(Reason(code="UNPROVEN", text=f"Unproven: {career}; preseason is a weak signal "
                              f"(blended at most 25% into his value)",
                              value=float(p.career_gp) if p.career_gp is not None else None))
        recs.append(Recommendation(kind="alert", score=c["strength"], title=title, subjects=[p],
                                   counterparty=team_name, reasons=reasons, strength=c["strength"]))
    recs.sort(key=lambda r: (-(r.strength or 0.0), r.title))
    return recs[:limit] if limit else recs


# --------------------------------------------------------------------------- rookie role signals
#
# News role signals (providers.news_roles, registered per player by the enrich rookie step) on
# unproven skaters (career NHL GP < 82 or no prior season with >= 20 GP). One alert per player,
# from his most recent usable signal (<= 10 days old, confidence >= 0.6, not ambiguous):
#   * positive top_line / pp1 / nhl_roster ("skating on the top line", "first power-play unit",
#     "named to the opening-night roster") for my players, other teams' players and free agents:
#     strength 5 + kind bonus (pp1 +1, top_line +0.5, nhl_roster 0) + 2.5 x (confidence - 0.6),
#     clamped to 5-7;
#   * negative (AHL demotion, returned to junior / Europe, scratched, off the top line / PP1, won't
#     make the roster) for MY unproven players only: strength 5.
# Title: "Rookie role signal: <name> — <quote> (source, date)". No predicted gain (not graded).

ROOKIE_ALERT_MAX_AGE_DAYS = 10
ROOKIE_ALERT_STRENGTH = (5.0, 7.0)
ROOKIE_NEG_STRENGTH = 5.0
ROOKIE_KIND_BONUS = {"pp1": 1.0, "top_line": 0.5, "nhl_roster": 0.0}
ROOKIE_NEG_KINDS = ("ahl_demotion", "junior_return", "scratched", "top_line", "pp1", "nhl_roster")


def rookie_signal_strength(sig: Mapping[str, Any]) -> float | None:
    """0-10 strength of a positive rookie role signal, None when the kind does not alert."""
    if sig.get("direction", 0) <= 0 or sig.get("kind") not in ROOKIE_KIND_BONUS:
        return None
    lo, hi = ROOKIE_ALERT_STRENGTH
    s = lo + ROOKIE_KIND_BONUS[sig["kind"]] + 2.5 * (float(sig.get("confidence") or 0.0) - 0.6)
    return round(min(hi, max(lo, s)), 2)


def recommend_rookie_role_alerts(ctx: LeagueContext, values: Mapping[str, Any] | None = None, *,
                                 signals: Mapping[str, list[Any]] | None = None,
                                 limit: int | None = None) -> list[Recommendation]:
    """"Rookie role signal" alerts (see the section comment). ``signals`` = {cid: [RoleSignal or
    dict]} overrides the enrich registry (``providers.rookie_enrich``)."""
    from ..providers.preseason_enrich import is_unproven
    from ..valuation.rookie import usable_news

    if signals is None:
        from ..providers.rookie_enrich import all_signals
        signals = all_signals()
    if not signals:
        return []
    try:
        me = ctx.my_team
    except LookupError:
        me = None
    owner: dict[str, tuple[str, bool]] = {}
    for t in ctx.teams:
        for p in t.players:
            owner.setdefault(p.cid, (t.name, bool(me is not None and t.team_id == me.team_id)))
    recs: list[Recommendation] = []
    for p in ctx.all_players():
        sigs = signals.get(p.cid)
        if not sigs or p.is_goalie or not is_unproven(p):
            continue
        usable = usable_news(sigs, ctx.as_of, max_age_days=ROOKIE_ALERT_MAX_AGE_DAYS)
        usable = [d for d in usable if d["kind"] in ROOKIE_KIND_BONUS or d["kind"] in ROOKIE_NEG_KINDS]
        if not usable:
            continue
        team_name, mine = owner.get(p.cid, (FA_LABEL, False))
        pos = [d for d in usable if rookie_signal_strength(d) is not None]
        neg = [d for d in usable if d["direction"] < 0 and d["kind"] in ROOKIE_NEG_KINDS]
        latest = usable[0]                               # newest first
        if latest["direction"] > 0 and pos:
            sig, strength = pos[0], rookie_signal_strength(pos[0])
        elif mine and neg:
            sig, strength = neg[0], ROOKIE_NEG_STRENGTH
        elif pos:
            sig, strength = pos[0], rookie_signal_strength(pos[0])
        else:
            continue
        pub = sig.get("published")
        when = pub.strftime("%Y-%m-%d") if hasattr(pub, "strftime") else (str(pub)[:10] if pub else "undated")
        src = sig.get("source") or "news"
        title = f"Rookie role signal: {p.name} — {sig['quote']} ({src}, {when})"
        where = "free agent" if team_name == FA_LABEL else ("my team" if mine else team_name)
        kind = str(sig["kind"]).replace("_", " ")
        career = f"{p.career_gp} career NHL GP" if p.career_gp is not None else "no NHL season with 20+ GP"
        reasons = [Reason(code="ROLE_NEWS",
                          text=f"{kind} {'+' if sig['direction'] > 0 else '-'} ({sig.get('origin') or 'rules'}, "
                               f"confidence {float(sig.get('confidence') or 0):.2f}; {where}): \"{sig['quote']}\"",
                          value=float(sig["direction"]), baseline=float(sig.get("confidence") or 0.0)),
                   Reason(code="UNPROVEN", text=f"Unproven: {career}; news is the earliest role signal for a "
                                               f"rookie (the valuation's rookie model uses it too)",
                          value=float(p.career_gp) if p.career_gp is not None else None)]
        if p.line or p.pp_unit:
            reasons.append(Reason(code="ROLE_DFO", text="Daily Faceoff: " + " / ".join(
                x.upper() for x in (p.line, p.pp_unit) if x)))
        recs.append(Recommendation(kind="alert", score=strength, title=title, subjects=[p],
                                   counterparty=team_name, reasons=reasons, strength=strength))
    recs.sort(key=lambda r: (-(r.strength or 0.0), r.title))
    return recs[:limit] if limit else recs
