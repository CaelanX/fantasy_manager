"""Claim planner (``fm claims``): which pickups to place now, in which order, dropping whom.

Candidates are the waiver engine's own pickups (``waivers.recommend_waivers``), so scoring, drop
choice, roster legality, protected dynasty players, the churn guard and the move-budget thresholds
are the existing rules, never a second copy:

1. **Season upgrades first** (rest-of-season gain x confidence, the waiver score), then
   **this-matchup streams** (``recommend_matchup_waivers``, daily-lineup leagues; matchup points).
   A season upgrade is a lasting roster change; stream timing belongs to the weekly plan.
2. **Order with dependent drops.** Claims are chosen one at a time and each is applied to my roster
   before the next is chosen (the add joins the bench, the drop leaves, an IR move happens), so the
   next claim's drop is a player still on the roster when it processes and the roster stays legal
   after every claim in order (Fantrax rejects a claim that would leave it illegal). Each applied
   claim also uses a move, so the move-budget threshold rises as it does in the league. An injured
   player is never the drop (his hold-or-cut verdict in ``fm injuries`` decides that).
3. **When it counts.** A free agent is instant: today, or tomorrow once today's games locked him
   (``plan._earliest``: Fantrax's day period locks at the first game). A player on waivers clears on
   ``waiver_until`` (Fantrax "W (Wed)" status, ESPN ``waiverProcessDate``) and counts from the day
   after. ``games`` are the days he is in my best daily lineup from then to the end of the matchup
   (bench games do not count), ``games_7d`` his team's games in the 7 days from then.
4. **FAAB.** Only when the league uses a budget (``ctx.faab_remaining``): a bid of
   ``remaining x min(FAAB_MAX_SHARE, gain / FAAB_GAIN_SCALE x FAAB_MAX_SHARE)`` x the
   % rostered riser multiplier (``waivers.market_multiplier``), clamped to [FAAB_MIN_BID, remaining].

``rule_lines`` states the league's claim rules from the provider (Fantrax Rules page, ESPN
``acquisitionSettings``) and my waiver priority, so the list reads in the league's own terms.
"""
from __future__ import annotations

from datetime import date, datetime, timedelta, timezone
from typing import Any, Literal, Mapping

from pydantic import BaseModel, Field

from ..matching.normalize import normalize_team
from ..models import LeagueContext, Player, Recommendation
from .base import moves_left, no_moves_text
from .headline import market_suffix
from .lineup import UNAVAILABLE

CLAIM_LIMIT = 5                # claims listed (also capped by the moves left)
CANDIDATES = 15                # pickups asked from the waiver engine per step
FAAB_MAX_SHARE = 0.35          # of the remaining budget, for the best pickup
FAAB_GAIN_SCALE = 2.0          # season FPG gain that earns the full share
FAAB_MIN_BID = 1.0

FANTRAX_RULE_LABELS = ("Free Agent Claim System", "Waiver Wire claim system", "# of days players remain on waivers for",
                       "Waiver Wire players are processed", "Rotate Waiver Wire Claim Order After Each Claim",
                       "Ensure 23+ hour waiver period", 'Prevent Waiver "Churning"', "Max # of claims per week",
                       "Lineup changes are executed")


class ClaimItem(BaseModel):
    order: int
    kind: Literal["claim", "free agent"]
    horizon: Literal["season", "matchup"]
    add: Player
    drop: Player | None = None
    to_ir: Player | None = None
    clears: date | None = None          # waiver clear date (claims)
    effective: date                     # first day he counts
    games: list[date] = Field(default_factory=list)   # days in my best lineup, rest of the matchup
    games_7d: int = 0
    gain: float | None = None
    gain_units: str | None = None
    headline: str = ""
    market: str | None = None           # "market agrees" / "model disagrees with market" / ...
    bid: float | None = None
    title: str = ""


class ClaimPlan(BaseModel):
    league: str
    rules: list[str] = Field(default_factory=list)
    priority: str | None = None
    moves_left: int | None = None
    claims: list[ClaimItem] = Field(default_factory=list)
    notes: list[str] = Field(default_factory=list)


def faab_bid(gain: float, remaining: float, change: float | None = None, min_bid: float = FAAB_MIN_BID) -> float:
    """Suggested FAAB bid for a pickup worth ``gain`` season FPG (module docstring, step 4)."""
    from .waivers import market_multiplier

    if remaining <= 0:
        return 0.0
    share = min(FAAB_MAX_SHARE, max(0.0, gain) / FAAB_GAIN_SCALE * FAAB_MAX_SHARE)
    bid = round(remaining * share * market_multiplier(change))
    return float(min(remaining, max(min(min_bid, remaining), bid)))


def rule_lines(ctx: LeagueContext, provider: Any = None) -> tuple[list[str], str | None]:
    """(the league's claim rules in its own words, my waiver priority) from the provider."""
    lines: list[str] = []
    priority = None
    rules = getattr(provider, "rules", None)
    if ctx.provider == "fantrax" and rules is not None and hasattr(rules, "get"):
        lines = [f"{lab}: {v}" for lab in FANTRAX_RULE_LABELS if (v := rules.get(lab))]
        mine = next((t.team_id for t in ctx.teams if t.owner_is_me), None)
        order = (getattr(provider, "standings", {}) or {}).get(str(mine), {}).get("waiver_order")
        if order is not None:
            priority = f"{order} of {len(ctx.teams)}"
    acq = getattr(provider, "acquisition_settings", None) or {}
    if ctx.provider == "espn" and acq:
        kind = "FAAB" if acq.get("isUsingAcquisitionBudget") else "waiver priority"
        lines.append(f"Waivers: {acq.get('acquisitionType', '?')} ({kind}), players stay on waivers "
                     f"{acq.get('waiverHours', '?')} h after a drop")
        if acq.get("waiverProcessDays"):
            lines.append(f"Waiver processing: {', '.join(str(d).title() for d in acq['waiverProcessDays'])}"
                         f" at {acq.get('waiverProcessHour', '?')}:00")
        rank = getattr(provider, "waiver_rank", None)
        if rank is not None:
            priority = f"{rank} of {len(ctx.teams)}"
    if ctx.faab_remaining is not None:
        lines.append(f"FAAB left: ${ctx.faab_remaining:g}")
    return lines, priority


def _apply(ctx: LeagueContext, add: Player, drop: Player | None, to_ir: Player | None) -> LeagueContext:
    """``ctx`` after the claim processes: my roster changed, the add gone from the pool, one move used."""
    from .waivers import _plan_team

    teams = [_plan_team(t, add, drop, to_ir) if t.owner_is_me else t for t in ctx.teams]
    used = (ctx.moves_used_this_period or 0) + 1
    upd: dict[str, Any] = {"teams": teams, "free_agents": [p for p in ctx.free_agents if p.cid != add.cid],
                           "moves_used_this_period": used}
    if ctx.moves_used_season is not None:
        upd["moves_used_season"] = ctx.moves_used_season + 1
    return ctx.model_copy(update=upd)


def _to_ir(rec: Recommendation) -> Player | None:
    return rec.subjects[0] if not rec.drop and rec.subjects and "to IR" in rec.title else None


def _lineup_days(ctx: LeagueContext, values: Mapping[str, Any], add: Player, effective: date,
                 now: datetime) -> list[date]:
    from .lineup import daily_plan

    plan = daily_plan(ctx, ctx.my_team, values, now=now)
    if plan is None:
        return []
    return [d.day for d in plan.days if d.day >= effective and add.cid in d.assign.values()]


def _games_7d(ctx: LeagueContext, p: Player, start: date) -> int:
    dates = ctx.schedule.get(normalize_team(p.team) or "", []) or []
    return sum(1 for d in dates if start <= d < start + timedelta(days=7))


def _pick(ctx: LeagueContext, values: Mapping[str, Any], dynasty_values: Mapping[str, Any] | None,
          seen: set[str]) -> tuple[Recommendation, str] | None:
    """The next claim: the best season upgrade, else the best this-matchup stream."""
    from .waivers import recommend_matchup_waivers, recommend_waivers

    # an injured player is never the drop here: his hold-or-cut verdict decides (as in the plan)
    hurt = {p.cid for p in ctx.my_team.players if p.status in UNAVAILABLE}
    for horizon, fn in (("season", lambda: recommend_waivers(ctx, dict(values), limit=CANDIDATES,
                                                              dynasty_values=dynasty_values, exclude_drops=hurt)),
                        ("matchup", lambda: recommend_matchup_waivers(ctx, dict(values), limit=CANDIDATES,
                                                                      dynasty_values=dynasty_values,
                                                                      exclude_drops=hurt))):
        for r in fn():
            if r.add and r.add[0].cid not in seen:
                return r, horizon
    return None


def plan_claims(ctx: LeagueContext, values: Mapping[str, Any], *, dynasty_values: Mapping[str, Any] | None = None,
                provider: Any = None, now: datetime | None = None, limit: int = CLAIM_LIMIT) -> ClaimPlan:
    """The ordered claim list (module docstring)."""
    from .plan import _earliest

    now = now or datetime.now(timezone.utc)
    plan = ClaimPlan(league=ctx.provider)
    plan.rules, plan.priority = rule_lines(ctx, provider)
    left = moves_left(ctx)
    plan.moves_left = left
    if left == 0:
        plan.notes.append(f"{no_moves_text(ctx)}: no claims until the limit resets.")
        return plan
    cap = limit if left is None else min(limit, left)
    work = ctx
    seen: set[str] = set()
    for k in range(cap):
        picked = _pick(work, values, dynasty_values, seen)
        if picked is None:
            break
        rec, horizon = picked
        add = rec.add[0]
        drop = rec.drop[0] if rec.drop else None
        to_ir = _to_ir(rec)
        seen.add(add.cid)
        claim = add.waiver_until is not None and add.waiver_until >= ctx.as_of
        effective = _earliest(work, add, now)
        after = _apply(work, add, drop, to_ir)
        item = ClaimItem(order=k + 1, kind="claim" if claim else "free agent", horizon=horizon, add=add,
                         drop=drop, to_ir=to_ir, clears=add.waiver_until if claim else None, effective=effective,
                         games=_lineup_days(after, values, add, effective, now),
                         games_7d=_games_7d(ctx, add, effective), gain=rec.predicted_gain,
                         gain_units=rec.gain_units, headline=rec.headline or "", title=rec.title,
                         market=market_suffix(rec).strip(" ·") or None)
        if ctx.faab_remaining is not None:
            fpg = next((x.value for x in rec.reasons if x.code == "VORP_DELTA" and x.value is not None), None)
            item.bid = faab_bid(fpg if fpg is not None else 0.0, ctx.faab_remaining, add.pct_owned_change)
        plan.claims.append(item)
        work = after
    if not plan.claims:
        plan.notes.append("No pickup clears the waiver thresholds right now.")
    if any(c.kind == "claim" for c in plan.claims):
        plan.notes.append("Place claims in this order: each drop is chosen with the earlier claims already "
                          "processed, so the roster stays legal at every step.")
    return plan


def claim_watch_lines(ctx: LeagueContext, values: Mapping[str, Any], dynasty_values: Mapping[str, Any] | None,
                      exclude: set[str], limit: int = 2) -> list[str]:
    """Watch lines for the weekly plan: season-upgrade claims of players on waivers the plan does
    not already schedule ("Claim X (on waivers until Wed): ..."). Each surface adds its own pointer
    to the full claim list (the CLI "fm claims", the dashboard a link to the Moves page)."""
    from .waivers import recommend_waivers

    out = []
    for r in recommend_waivers(ctx, dict(values), limit=CANDIDATES, dynasty_values=dynasty_values):
        a = r.add[0] if r.add else None
        if a is None or a.cid in exclude or a.waiver_until is None or a.waiver_until < ctx.as_of:
            continue
        drop = f", drop {r.drop[0].name}" if r.drop else ""
        out.append(f"Claim {a.name} (on waivers until {a.waiver_until:%a}){drop}: "
                   f"{r.headline or r.title}")
        if len(out) >= limit:
            break
    return out


__all__ = ["CLAIM_LIMIT", "ClaimItem", "ClaimPlan", "claim_watch_lines", "faab_bid", "plan_claims", "rule_lines"]
