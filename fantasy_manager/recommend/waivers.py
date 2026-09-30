"""Waiver-wire recommendations: free agents that beat my weakest same-slot player.

Roster spot for the pickup, in order of preference:

0. An open roster (bench/reserve) spot: nobody is dropped (OPEN_SPOT reason).
1. IR first (only when the league has IR slots): when my team has a free IR slot and an active-roster player with IR/LTIR status,
   that player moves to IR and nobody is dropped (``drop`` is empty, IR_MOVE reason).
Per-position roster maximums (``ctx.position_limits``, e.g. ESPN's "max 3 goalies"): when the
no-drop add of 0./1. would put the roster over a maximum, the pickup instead drops the weakest
same-position player (POSITION_CAP reason); drops are always limited to players whose removal
keeps the roster legal (``base.roster_legal_after``).

2. Otherwise drop the weakest same-slot player: lowest dynasty value in dynasty leagues
   (never a player with no stats at all, whose value is unknown), else lowest FPG for the
   horizon.

With a drop, the gain is FA FPG minus the dropped player's FPG. A no-drop add (open spot or IR
move) is scored by its marginal lineup value: the optimal starting lineup's FPG total with him
minus without him (``lineup.starting_slots``), i.e. his full FPG when he fills an empty starting
slot, else his FPG minus the starter he displaces. The gain must exceed MIN_GAIN.

Dynasty leagues add three rules:

* the add must be worth at least DYNASTY_MIN_RATIO (1.15) x the drop's dynasty value;
* protected players (age <= 23 and a top-32 pick, >= 80% rostered league-wide, or < 82 career
  NHL GP and >= 60% rostered; see ``base.protection_reason``) are skipped as drop candidates unless the add is worth PROTECT_OVERRIDE (1.5) x theirs. In contend mode a
  protected player can still be dropped for a clear this-season upgrade (>= CONTEND_CLEAR_GAIN
  FPG) when his own this-season projection is low (< 40 GP or < 2.0 FPG) - reason
  CONTEND_DROP. Skipped drops are reported in ``debug`` (PROSPECT_PROTECTED), never as recs;
* recommendations are scored by dynasty gain (x confidence), not season gain.

Transaction budget (``ctx.moves_limit_per_period`` etc., see ``base.moves_left``): the gain
threshold rises as moves run out (``base.move_scarcity_threshold``: 0.3 FPG with >= 4 moves left or
no limit, 0.6 with 2-3, 1.0 with the last one unless the add replaces an out / IR player - a suspension
is not an injury); with no
moves left there are no pickups at all ([], the CLI says when they resume). Every pickup carries a
MOVE_BUDGET reason ("2 of 4 moves left this week") when a limit applies.

Churn guard: a player I added fewer than 7 days ago (``ctx.recent_adds``) is never proposed as the
drop unless the pickup beats him by >= 1.5 FPG or he is out / IR / suspended. A skipped drop is
recorded in ``debug`` (CHURN_GUARD) and, when another drop is used instead, noted on that pickup.

Market timing: when the provider reports the weekly change in % rostered (``pct_owned_change``,
ESPN ``percentChange`` / Fantrax "+/-"), an OWNERSHIP_TREND reason (value = change, baseline = %
rostered) replaces the plain OWNED one. Risers (change >= RISER_MIN points) get their score
multiplied by ``1 + min(0.5, change / 20)`` (act before the rest of the league does); fallers
(change <= -RISER_MIN) only get a note. Eligibility and the gain thresholds are unchanged. ``recommend_trending_alerts`` separately flags free agents
rising >= ALERT_MIN_CHANGE points this week who also have positive VORP or a rising TOI / PP
share (kind "alert").
"""
from __future__ import annotations

from datetime import timedelta
from typing import Any, Mapping

from ..matching.normalize import normalize_team
from ..models import LeagueContext, Player, Reason, Recommendation
from ..valuation.adjust import Horizon
from ..valuation.schedule import context_window
from ..valuation.valuate import PlayerValue
from .lineup import _dp, starting_slots
from .strength import apply_ranks, apply_strength
from .base import (CHURN_EXEMPT_STATUSES, CHURN_OVERRIDE_GAIN, INJURY_REPLACEMENT_STATUSES, PROTECT_OVERRIDE,
                   churn_blocked, churn_text, confidence, droppable_players, dynasty_map, free_ir_slots,
                   has_data, ir_movable, move_scarcity_threshold, moves_budget_reason, open_roster_spots,
                   player_age, protection_reason, recently_added, roster_legal_after, shares_slot)

MIN_GAIN = 0.3
DYNASTY_MIN_RATIO = 1.15
CONTEND_CLEAR_GAIN = 1.0
CONTEND_LOW_GP = 40
CONTEND_LOW_FPG = 2.0
# Market timing (% rostered change, in percentage points over the provider's trend window)
RISER_MIN = 3.0
RISER_MAX_BOOST = 0.5
RISER_SCALE = 20.0
ALERT_MIN_CHANGE = 5.0
ALERT_TOI_TREND_MIN = 0.5      # minutes per game above the season baseline
ALERT_PP_SHARE_TREND_MIN = 0.05


def market_multiplier(change: float | None) -> float:
    """Score multiplier for a pickup whose % rostered moved ``change`` points this week:
    1 + min(0.5, change / 20) for risers (change >= RISER_MIN), else 1."""
    if change is None or change < RISER_MIN:
        return 1.0
    return 1.0 + min(RISER_MAX_BOOST, change / RISER_SCALE)


def ownership_trend_reason(p: Player) -> Reason | None:
    """OWNERSHIP_TREND: "X rostered 32% (+9.5 this week)", with a note for risers / fallers."""
    chg = p.pct_owned_change
    if chg is None:
        return None
    owned = f"rostered {p.pct_owned:.0f}%" if p.pct_owned is not None else "% rostered"
    text = f"{p.name} {owned} ({chg:+.1f} this week)"
    mult = market_multiplier(chg)
    if mult > 1.0:
        text += f": rising, grab him before the league does (score x{mult:.2f})"
    elif chg <= -RISER_MIN:
        text += ": falling league-wide, no rush (others are dropping him)"
    return Reason(code="OWNERSHIP_TREND", text=text, value=chg, baseline=p.pct_owned)


def waiver_adjust(ctx: LeagueContext, fa: Player, fv: PlayerValue) -> tuple[PlayerValue, Reason | None]:
    """A free agent still on waivers (``waiver_until`` on/after today) can't be added before the claim
    clears: WAIVER_CLAIM reason, and ``fv`` with proj_week / fpg_week scaled to the games his team plays
    after the clear date (a copy; ``fv`` is returned unchanged for real free agents)."""
    until = fa.waiver_until
    if until is None or until < ctx.as_of:
        return fv, None
    text = f"{fa.name} is on waivers until {until:%a}: claim, won't be available before then"
    window = context_window(ctx)
    dates = ctx.schedule.get(normalize_team(fa.team) or "", []) if window is not None else []
    if window is None or fv.proj_week is None or not dates:
        return fv, Reason(code="WAIVER_CLAIM", text=text)
    in_win = {d for d in dates if window.start <= d < window.start + timedelta(days=window.days)}
    left = sum(1 for d in in_win if d > until)
    share = left / len(in_win) if in_win else 0.0
    text += f" ({left} of {len(in_win)} games {window.label()} count)"
    adj = fv.model_copy(update={"proj_week": fv.proj_week * share, "fpg_week": fv.fpg_week * share,
                                "games_next7": left})
    return adj, Reason(code="WAIVER_CLAIM", text=text, value=float(left), baseline=float(len(in_win)))


def _low_this_season(p: Player, pv: PlayerValue) -> str | None:
    """Why `p` projects to contribute little this season, or None."""
    proj = p.lines.get("projected")
    gp = proj.gp if proj is not None else 0
    if gp < CONTEND_LOW_GP:
        return f"projected {gp} GP this season"
    if pv.fpg_season < CONTEND_LOW_FPG:
        return f"projected {pv.fpg_season:.2f} FPG this season"
    return None


def _lineup_fpg(players: list[Player], slots: list[str], values: Mapping[str, PlayerValue],
                horizon: Horizon) -> tuple[float, set[str]]:
    """(FPG total, starter cids) of the optimal starting lineup from ``players``."""
    total, assign = _dp(players, slots,
                        lambda p, t: values[p.cid].fpg_for(horizon) if p.cid in values else 0.0)
    return total, set(assign.values())


def recommend_waivers(ctx: LeagueContext, values: dict[str, PlayerValue], limit: int = 10,
                      horizon: Horizon = "season", dynasty_values: Mapping[str, Any] | None = None,
                      debug: list[dict[str, Any]] | None = None) -> list[Recommendation]:
    """Ranked waiver pickups. `debug` (a list) collects drops that prospect protection, the
    churn guard or a position cap blocked: {"add", "drop", "reason", "add_value", "drop_value"}.
    [] when the league's move limit is used up for this period."""
    if move_scarcity_threshold(ctx, injury_replacement=True) is None:
        return []   # no moves left this period (base.no_moves_text says when they resume)
    budget = moves_budget_reason(ctx)
    team = ctx.my_team
    dyn = dynasty_map(ctx, values, dynasty_values)
    contend = getattr(ctx, "dynasty_mode", "contend") == "contend"
    free_ir = free_ir_slots(team, ctx.roster_shape)
    to_ir = [p for p in ir_movable(team, values) if p.cid in values][:free_ir]
    open_spots = open_roster_spots(team, ctx.roster_shape)
    ir_move = to_ir[0] if to_ir and not open_spots else None
    moving = {p.cid for p in to_ir}
    mine = [p for p in droppable_players(team) if p.cid in values and p.cid not in moving]
    slots = starting_slots(ctx.roster_shape)
    active = droppable_players(team)
    base_lineup: dict[str | None, tuple[float, set[str]]] = {}   # IR-move player (or None) -> optimal lineup without the FA

    def no_drop_gain(fa: Player, fv: PlayerValue, to_ir: Player | None) -> tuple[float, Player | None]:
        """Marginal lineup FPG of adding ``fa`` (valued at ``fv``) without a drop, and the starter he
        displaces (None: he fills an empty starting slot)."""
        key = to_ir.cid if to_ir is not None else None
        pool = [p for p in active if to_ir is None or p.cid != to_ir.cid]
        if key not in base_lineup:
            base_lineup[key] = _lineup_fpg(pool, slots, values, horizon)
        before, starters = base_lineup[key]
        after, new_starters = _lineup_fpg(pool + [fa], slots, {**values, fa.cid: fv}, horizon)
        out = [p for p in pool if p.cid in starters - new_starters]
        return after - before, min(out, key=lambda p: values[p.cid].fpg_for(horizon)) if out else None
    protected = {}
    if dyn is not None:
        for p in mine:
            why = protection_reason(p, player_age(p, ctx, dynasty_values))
            if why:
                protected[p.cid] = why

    def drop_key(p: Player) -> tuple[float, float]:
        fpg = values[p.cid].fpg_for(horizon)
        return (dyn[p.cid], fpg) if dyn is not None and p.cid in dyn else (fpg, fpg)

    recs: list[Recommendation] = []
    for fa in ctx.free_agents:
        fv = values.get(fa.cid)
        if fv is None or fv.vorp_for(horizon) <= 0:
            continue
        fv, waiver = waiver_adjust(ctx, fa, fv)
        candidates = [p for p in mine if shares_slot(fa, p)]
        drop = None
        extra: list[Reason] = []
        # per-position roster maximums: a no-drop add that would break one needs a same-position drop
        cap_why: str | None = None
        spot_open, spot_ir = bool(open_spots), ir_move
        if spot_open or spot_ir is not None:
            ok, why = roster_legal_after(team, ctx, add=[fa], to_ir=[spot_ir] if spot_ir is not None else [])
            if not ok:
                cap_why, spot_open, spot_ir = why, False, None
        if not spot_open and spot_ir is None:
            # dynasty: a player with no stats at all has unknown (not zero) value -> keep him
            pool = sorted((p for p in candidates if dyn is None or has_data(values[p.cid])), key=drop_key)
            legal = [p for p in pool if roster_legal_after(team, ctx, add=[fa], drop=[p])[0]]
            if len(legal) < len(pool) and cap_why is None:
                cap_why = roster_legal_after(team, ctx, add=[fa])[1]
            pool = legal
            # churn guard: keep players I just added unless the upgrade is big or they are hurt
            kept_recent: list[Player] = []
            unguarded = []
            for p in pool:
                g_p = fv.fpg_for(horizon) - values[p.cid].fpg_for(horizon)
                if churn_blocked(ctx, p, g_p):
                    kept_recent.append(p)
                    if debug is not None:
                        debug.append({"add": fa.name, "drop": p.name, "add_value": fv.fpg_for(horizon),
                                      "drop_value": values[p.cid].fpg_for(horizon),
                                      "reason": Reason(code="CHURN_GUARD",
                                                       text=f"Not dropping {p.name} ({churn_text(ctx, p)}): "
                                                            f"{fa.name} is only {g_p:+.2f} FPG over him "
                                                            f"(needs +{CHURN_OVERRIDE_GAIN:g} or an injury)",
                                                       value=g_p, baseline=CHURN_OVERRIDE_GAIN)})
                else:
                    unguarded.append(p)
            pool = unguarded
            if not pool and cap_why is not None and debug is not None:
                debug.append({"add": fa.name, "drop": None, "add_value": None, "drop_value": None,
                              "reason": Reason(code="POSITION_CAP",
                                               text=f"Not adding {fa.name}: {cap_why} and no droppable "
                                                    f"same-position player")})
            if dyn is not None:
                a_val = dyn.get(fa.cid, 0.0)
                chosen = None
                for p in pool:
                    d_val = dyn.get(p.cid, 0.0)
                    why = protected.get(p.cid)
                    if why is None or a_val >= PROTECT_OVERRIDE * d_val:
                        chosen = p
                        break
                    gain_p = fv.fpg_for(horizon) - values[p.cid].fpg_for(horizon)
                    low = _low_this_season(p, values[p.cid])
                    if contend and gain_p >= CONTEND_CLEAR_GAIN and low:
                        chosen = p
                        extra.append(Reason(
                            code="CONTEND_DROP",
                            text=f"Contend mode: {why}, but {low}; {fa.name} is a clear this-season "
                                 f"upgrade (+{gain_p:.2f} FPG)", value=gain_p, baseline=CONTEND_CLEAR_GAIN))
                        break
                    if debug is not None:
                        debug.append({"add": fa.name, "drop": p.name, "add_value": a_val, "drop_value": d_val,
                                      "reason": Reason(code="PROSPECT_PROTECTED",
                                                       text=f"Not dropping {p.name}: {why} "
                                                            f"({fa.name} {a_val:.2f} < {PROTECT_OVERRIDE:g} x "
                                                            f"{d_val:.2f})",
                                                       value=a_val, baseline=d_val)})
                pool = [chosen] if chosen is not None else []
            if not pool:
                continue
            drop = pool[0]
            if kept_recent:
                better = [p for p in kept_recent if drop_key(p) <= drop_key(drop)]
                if better:
                    names = ", ".join(f"{p.name} ({churn_text(ctx, p)})" for p in better)
                    extra.append(Reason(code="CHURN_GUARD",
                                        text=f"Keeping {names}: just added, so {drop.name} is the drop "
                                             f"instead (a recent add is only dropped for "
                                             f"+{CHURN_OVERRIDE_GAIN:g} FPG or an injury)"))
            if recently_added(ctx, drop):
                g_d = fv.fpg_for(horizon) - values[drop.cid].fpg_for(horizon)
                why_ok = (f"he is {drop.status}" if drop.status in CHURN_EXEMPT_STATUSES
                          else f"{fa.name} is +{g_d:.2f} FPG over him")
                extra.append(Reason(code="CHURN_GUARD",
                                    text=f"{drop.name} was {churn_text(ctx, drop)}, dropping him anyway: {why_ok}",
                                    value=g_d, baseline=CHURN_OVERRIDE_GAIN))
        if drop is not None:
            cmp = drop
            gain = fv.fpg_for(horizon) - values[cmp.cid].fpg_for(horizon)
        else:
            gain, cmp = no_drop_gain(fa, fv, spot_ir)
        cv = values[cmp.cid] if cmp is not None else None
        injury_replacement = spot_ir is not None or (drop is not None and drop.status in INJURY_REPLACEMENT_STATUSES)
        threshold = move_scarcity_threshold(ctx, injury_replacement)
        if threshold is None or gain <= threshold:
            continue
        dyn_gain = None
        if dyn is not None:
            a_val = dyn.get(fa.cid, 0.0)
            if drop is not None:
                d_val = dyn.get(drop.cid, 0.0)
                if a_val < DYNASTY_MIN_RATIO * d_val or a_val <= d_val:
                    continue  # dynasty: never cut a comparable long-term asset for a short-term bump
                dyn_gain = a_val - d_val
            else:
                dyn_gain = a_val
        conf = confidence(fa)
        gp = fa.gp("season")
        fill = f"fills empty {'/'.join(fa.positions)} slot"
        if cmp is None:
            delta = f"{fill} (+{gain:.2f} FPG, {horizon})"
        else:
            over = f"over dropping {drop.name}" if drop else f"over benching {cmp.name}"
            delta = f"+{gain:.2f} FPG ({horizon}) {over}"
        reasons = [
            Reason(code="VORP_DELTA", text=delta, value=gain, baseline=threshold),
            Reason(code="FPG_ADD", text=f"{fa.name}: {fv.fpg_for(horizon):.2f} FPG, VORP {fv.vorp_for(horizon):+.2f}",
                   value=fv.fpg_for(horizon)),
        ]
        if cmp is not None:
            reasons.append(Reason(code="FPG_DROP", text=f"{cmp.name}: {cv.fpg_for(horizon):.2f} FPG, "
                                  f"VORP {cv.vorp_for(horizon):+.2f}", value=cv.fpg_for(horizon)))
        reasons.append(Reason(code="GP", text=f"{gp} GP this season -> confidence {conf:.2f}", value=float(gp),
                              baseline=conf))
        if waiver is not None:
            reasons.insert(1, waiver)
        if cap_why is not None and drop is not None:
            reasons.append(Reason(code="POSITION_CAP",
                                  text=f"League roster maximum ({cap_why}): drop {drop.name} "
                                       f"(same position) to make room"))
        if spot_open:
            reasons.append(Reason(code="OPEN_SPOT",
                                  text=f"{open_spots} open roster spot{'s' if open_spots != 1 else ''}: "
                                       f"no drop needed ("
                                       + (f"{cmp.name} goes to the bench)" if cmp is not None else f"{fill})"),
                                  value=float(open_spots)))
        elif spot_ir is not None:
            reasons.append(Reason(code="IR_MOVE",
                                  text=f"Move {spot_ir.name} ({spot_ir.status.upper()}) to IR "
                                       f"({free_ir} IR slot{'s' if free_ir != 1 else ''} free): no drop needed",
                                  value=float(free_ir)))
        if dyn is not None and drop is not None:
            reasons.append(Reason(code="DYNASTY_DROP",
                                  text=f"Dynasty value {fa.name} {dyn.get(fa.cid, 0.0):.2f} vs "
                                       f"{drop.name} {dyn.get(drop.cid, 0.0):.2f} (lowest-dynasty droppable "
                                       f"same-slot player; needs x{DYNASTY_MIN_RATIO:g})",
                                  value=dyn.get(fa.cid, 0.0), baseline=dyn.get(drop.cid, 0.0)))
        reasons.extend(extra)
        if budget is not None:
            reasons.append(budget.model_copy())
        if dyn_gain is not None:
            if drop is None:
                gain_text = f"Dynasty value added {dyn_gain:+.2f} (no drop, so the whole value counts; score basis)"
            else:
                gain_text = f"Dynasty gain {dyn_gain:+.2f} over {drop.name} (score basis)"
            reasons.append(Reason(code="DYNASTY_GAIN", text=gain_text, value=dyn_gain))
        for pl, pv in ((fa, fv), (cmp, cv)):
            if pl is not None and pl.status not in ("healthy", "unknown"):
                reasons.append(Reason(code="STATUS", text=f"{pl.name} is {pl.status}"
                                      + (f" ({pl.status_note})" if pl.status_note else ""),
                                      value=pv.fpg_for(horizon) / pv.fpg if pv.fpg else None))
        if (trend := ownership_trend_reason(fa)) is not None:   # % rostered and its weekly change
            reasons.append(trend)
        elif fa.pct_owned is not None:
            reasons.append(Reason(code="OWNED", text=f"{fa.name} rostered in {fa.pct_owned:.1f}% of leagues",
                                  value=fa.pct_owned))
        if drop is not None:
            title = f"Add {fa.name}, drop {drop.name}"
        elif spot_open:
            title = f"Add {fa.name} (open roster spot)"
        else:
            title = f"Add {fa.name}, move {spot_ir.name} to IR"
        score = (dyn_gain if dyn_gain is not None else gain) * conf * market_multiplier(fa.pct_owned_change)
        pred, units, days = predicted_gain(fv, cv, gain, horizon)
        recs.append(Recommendation(kind="waiver", score=score, title=title,
                                   add=[fa], drop=[drop] if drop is not None else [], reasons=reasons,
                                   predicted_gain=pred, gain_units=units, horizon_days=days,
                                   subjects=[spot_ir] if drop is None and not spot_open and spot_ir else []))
    recs.sort(key=lambda r: r.score, reverse=True)
    return apply_ranks(apply_strength(recs[:limit]))


def predicted_gain(add: PlayerValue, cmp: PlayerValue | None, fpg_gain: float, horizon: Horizon
                   ) -> tuple[float, str, int | None]:
    """(gain, units, horizon days) of a pickup: week points over the 7-day window when the
    week horizon has a schedule, else the FPG gain (rest of season, or the week's per-game
    value with horizon_days 7). ``cmp`` None: the add fills an empty starting slot (vs 0)."""
    if horizon == "week":
        if add.proj_week is not None and (cmp is None or cmp.proj_week is not None):
            return float(add.proj_week - (cmp.proj_week if cmp is not None else 0.0)), "week_pts", 7
        return float(fpg_gain), "season_fpg", 7
    return float(fpg_gain), "season_fpg", None


def recommend_trending_alerts(ctx: LeagueContext, values: Mapping[str, PlayerValue] | None = None,
                              limit: int = 10, horizon: Horizon = "season") -> list[Recommendation]:
    """Kind "alert": free agents whose % rostered rose >= ALERT_MIN_CHANGE points this week
    and who back it up with positive VORP (``values``; computed from ``ctx`` when omitted) or a
    rising deployment (``toi_trend`` >= 0.5 min or ``pp_share_trend`` >= 0.05, when present).
    Title "Rising: <name> (+9.5% rostered this week)", counterparty "FA", the player in
    ``subjects``, strength min(10, 3 + change / 3), horizon 7 days; biggest risers first."""
    if values is None:
        try:
            from ..scoring import from_config
            from ..valuation.valuate import valuate_league

            values = valuate_league(ctx, from_config(ctx.scoring))
        except Exception:  # noqa: BLE001 - alerts still work from deployment trends
            values = {}
    recs: list[Recommendation] = []
    seen: set[str] = set()
    for fa in ctx.free_agents:
        chg = fa.pct_owned_change
        if fa.cid in seen or chg is None or chg < ALERT_MIN_CHANGE:
            continue
        seen.add(fa.cid)
        pv = values.get(fa.cid)
        vorp = pv.vorp_for(horizon) if pv is not None else None
        toi = getattr(fa, "toi_trend", None)
        pp = getattr(fa, "pp_share_trend", None)
        support: list[Reason] = []
        if vorp is not None and vorp > 0:
            support.append(Reason(code="VORP", text=f"VORP {vorp:+.2f} ({horizon}), "
                                  f"{pv.fpg_for(horizon):.2f} FPG", value=vorp, baseline=0.0))
        if toi is not None and toi >= ALERT_TOI_TREND_MIN:
            support.append(Reason(code="TOI_TREND", text=f"TOI {toi:+.1f} min/game vs his season baseline",
                                  value=toi, baseline=ALERT_TOI_TREND_MIN))
        if pp is not None and pp >= ALERT_PP_SHARE_TREND_MIN:
            support.append(Reason(code="PP_TREND", text=f"PP share {pp * 100:+.0f} pts vs his season baseline",
                                  value=pp, baseline=ALERT_PP_SHARE_TREND_MIN))
        if not support:
            continue
        reasons = [ownership_trend_reason(fa)] + support  # type: ignore[list-item]
        if fa.status not in ("healthy", "unknown"):
            reasons.append(Reason(code="STATUS", text=f"{fa.name} is {fa.status}"
                                  + (f" ({fa.status_note})" if fa.status_note else "")))
        recs.append(Recommendation(kind="alert", score=float(chg),
                                   title=f"Rising: {fa.name} ({chg:+.1f}% rostered this week)",
                                   counterparty="FA", subjects=[fa], reasons=reasons, horizon_days=7,
                                   strength=round(min(10.0, 3.0 + chg / 3.0), 2)))
    recs.sort(key=lambda r: r.score, reverse=True)
    return apply_ranks(recs[:limit])
