"""Shared helpers for recommendation engines."""
from __future__ import annotations

from datetime import date, timedelta
from typing import Any, Iterable, Mapping

from ..models import FantasyTeam, LeagueContext, Player, Reason
from ..valuation.blend import shrink_k
from ..valuation.replacement import DEFAULT_SLOTS, eligible_slots

MIN_CONFIDENCE = 0.25


def confidence(player: Player) -> float:
    """GP/(GP+k) from current-season games played, floored at MIN_CONFIDENCE."""
    gp = player.gp("season")
    k = shrink_k(player.is_goalie)
    return max(MIN_CONFIDENCE, gp / (gp + k)) if gp > 0 else MIN_CONFIDENCE


def droppable_players(team: FantasyTeam) -> list[Player]:
    """Rostered players outside IR slots."""
    return [s.player for s in team.slots if s.player is not None and s.slot != "IR"]


def shares_slot(a: Player, b: Player, slots=DEFAULT_SLOTS) -> bool:
    return bool(set(eligible_slots(a.positions, slots)) & set(eligible_slots(b.positions, slots)))


IR_SLOT_NAMES = ("IR", "IR+")
IR_MOVE_STATUSES = ("ltir", "ir")      # most severe first


def free_ir_slots(team: FantasyTeam, roster_shape: Mapping[str, int]) -> int:
    """IR slots in the league's roster shape not yet occupied on `team`."""
    cap = sum(int(roster_shape.get(s, 0)) for s in IR_SLOT_NAMES)
    used = sum(1 for s in team.slots if s.player is not None and s.slot in IR_SLOT_NAMES)
    return max(0, cap - used)


def ir_movable(team: FantasyTeam, values: Mapping[str, Any] | None = None) -> list[Player]:
    """Players in active/bench slots whose status (IR/LTIR) lets them move to an IR slot,
    most severe first, then most valuable first."""
    def fpg(p: Player) -> float:
        pv = (values or {}).get(p.cid)
        return float(getattr(pv, "fpg", 0.0) or 0.0)

    out = [s.player for s in team.slots
           if s.player is not None and s.slot not in IR_SLOT_NAMES and s.player.status in IR_MOVE_STATUSES]
    return sorted(out, key=lambda p: (IR_MOVE_STATUSES.index(p.status), -fpg(p)))


def dynasty_map(ctx: LeagueContext, values: Mapping[str, Any],
                dynasty_values: Mapping[str, Any] | None = None) -> dict[str, float] | None:
    """{cid: dynasty value} in dynasty leagues (computed from `values` when not supplied),
    else None."""
    if not ctx.dynasty:
        return None
    if dynasty_values is None:
        try:
            from ..valuation.dynasty import apply_dynasty
            dynasty_values = apply_dynasty(values, ctx)
        except Exception:
            return None
    out: dict[str, float] = {}
    for cid, dv in (dynasty_values or {}).items():
        v = dv if isinstance(dv, (int, float)) else getattr(dv, "value", None)
        if v is not None:
            out[cid] = float(v)
    return out or None


def open_roster_spots(team: FantasyTeam, roster_shape: Mapping[str, int]) -> int:
    """Empty non-IR roster spots (starting + bench/reserve) on `team`."""
    cap = sum(int(n) for s, n in roster_shape.items() if s not in IR_SLOT_NAMES)
    used = sum(1 for s in team.slots if s.player is not None and s.slot not in IR_SLOT_NAMES)
    return max(0, cap - used)


# -- per-position roster maximums (LeagueContext.position_limits / max_roster_size) ----------

NO_LIMIT = 10 ** 6
FORWARD_POSITIONS = ("C", "LW", "RW")
# Slots that do not count toward LeagueContext.max_roster_size (IR and minors / not-active).
SIZE_EXEMPT_SLOTS = ("IR", "IR+", "MIN", "NA")
POSITION_PLURAL = {"C": "centers", "LW": "left wings", "RW": "right wings", "F": "forwards",
                   "D": "defensemen", "G": "goalies"}


def limit_positions(p: Player, limits: Mapping[str, int]) -> set[str]:
    """The capped positions `p` counts toward: his platform default position when the provider
    reports one (ESPN counts position limits by default position), else every position he is
    eligible at. Any forward (C/LW/RW) also counts toward an "F" limit."""
    pos = {p.primary_position} if p.primary_position else set(p.positions)
    if pos & set(FORWARD_POSITIONS):
        pos.add("F")
    return pos & set(limits)


def _position_counts(players: Iterable[Player], limits: Mapping[str, int]) -> dict[str, int]:
    out = {k: 0 for k in limits}
    for p in players:
        for k in limit_positions(p, limits):
            out[k] += 1
    return out


def position_counts(team: FantasyTeam, ctx: LeagueContext) -> dict[str, int]:
    """{capped position: players on `team` (every slot, IR included) counting toward it}."""
    return _position_counts(team.players, ctx.position_limits or {})


def position_room(team: FantasyTeam, ctx: LeagueContext, positions: Player | Iterable[str]) -> int:
    """How many more players with `positions` (a Player: see ``limit_positions``) `team` can
    add before hitting a per-position roster maximum; NO_LIMIT when none applies."""
    limits = ctx.position_limits or {}
    if isinstance(positions, Player):
        keys = limit_positions(positions, limits)
    else:
        keys = set(positions)
        if keys & set(FORWARD_POSITIONS):
            keys.add("F")
        keys &= set(limits)
    if not keys:
        return NO_LIMIT
    counts = position_counts(team, ctx)
    return max(0, min(int(limits[k]) - counts[k] for k in keys))


def capped_positions(team: FantasyTeam, ctx: LeagueContext, p: Player) -> list[str]:
    """The capped positions of `p` at which `team` is already full."""
    limits = ctx.position_limits or {}
    counts = position_counts(team, ctx)
    return sorted(k for k in limit_positions(p, limits) if counts[k] >= int(limits[k]))


def cap_text(ctx: LeagueContext, key: str, n: int | None = None) -> str:
    """"G limit 3" (+ ": would roster 4 goalies" when `n` is given)."""
    txt = f"{key} limit {ctx.position_limits.get(key)}"
    return txt + (f": would roster {n} {POSITION_PLURAL.get(key, key)}" if n is not None else "")


def roster_legal_after(team: FantasyTeam, ctx: LeagueContext, add: Iterable[Player] = (),
                       drop: Iterable[Player] = (), to_ir: Iterable[Player] = ()) -> tuple[bool, str | None]:
    """(ok, reason): is `team` within the league's per-position maximums and maximum roster
    size after adding `add`, dropping `drop` and moving `to_ir` into IR slots? A limit the roster
    already exceeds only fails when the move makes it worse. Position limits count every
    rostered player (IR included); the roster size excludes IR / minors slots."""
    add, drop, to_ir = list(add), list(drop), list(to_ir)
    limits = ctx.position_limits or {}
    have = {p.cid for p in team.players}
    gone = {p.cid for p in drop}
    new = [p for p in add if p.cid not in have]
    after_players = [p for p in team.players if p.cid not in gone] + new
    if limits:
        before = _position_counts(team.players, limits)
        after = _position_counts(after_players, limits)
        for k in sorted(limits):
            if after[k] > int(limits[k]) and after[k] > before[k]:
                return False, cap_text(ctx, k, after[k])
    cap = ctx.max_roster_size
    if cap:
        moved = {p.cid for p in to_ir}
        size_before = sum(1 for s in team.slots if s.player is not None and s.slot not in SIZE_EXEMPT_SLOTS)
        size_after = sum(1 for s in team.slots if s.player is not None and s.slot not in SIZE_EXEMPT_SLOTS
                         and s.player.cid not in gone and s.player.cid not in moved) + len(new)
        if size_after > cap and size_after > size_before:
            return False, f"roster limit {cap}: would roster {size_after} players"
    return True, None


def position_limits_text(ctx: LeagueContext) -> str | None:
    """"max G 3 (incl. IR); roster max 22 (excl. IR)" for settings / footers, or None when
    nothing is known."""
    order = {k: i for i, k in enumerate(("C", "LW", "RW", "F", "D", "G"))}
    parts = []
    if ctx.position_limits:
        parts.append("max " + ", ".join(f"{k} {v}" for k, v in sorted(ctx.position_limits.items(),
                                                                       key=lambda kv: order.get(kv[0], 99)))
                     + " (incl. IR)")
    if ctx.max_roster_size:
        parts.append(f"roster max {ctx.max_roster_size} (excl. IR)")
    return "; ".join(parts) or None


def has_data(pv: Any) -> bool:
    """False when valuation found no stats at all (NO_DATA): the value is unknown, not zero
    (typically a prospect), so dynasty drop logic must not treat it as the weakest asset."""
    return pv is not None and not any(getattr(r, "code", None) == "NO_DATA" for r in getattr(pv, "reasons", []))


# -- dynasty prospect protection ----------------------------------------------------

PROTECT_MAX_AGE = 23.0
PROTECT_MAX_PICK = 32
PROTECT_MIN_OWNED = 80.0
PROTECT_UNPROVEN_GP = 82          # career NHL GP below this = still unproven
PROTECT_UNPROVEN_OWNED = 60.0     # ... and rostered at least this widely
PROTECT_OVERRIDE = 1.5      # dynasty(add) must be at least this multiple of dynasty(drop)


def player_age(p: Player, ctx: LeagueContext, dynasty_values: Mapping[str, Any] | None = None) -> float | None:
    """Age from the dynasty value object (which includes league-reported ages), else the
    birth date."""
    dv = (dynasty_values or {}).get(p.cid)
    age = getattr(dv, "age", None)
    if isinstance(age, (int, float)):
        return float(age)
    if p.birth_date is not None:
        return (ctx.as_of - p.birth_date).days / 365.25
    return None


def protection_reason(p: Player, age: float | None) -> str | None:
    """Why `p` is a protected young dynasty asset, or None. Protected players are never
    proposed as drops unless the incoming value is at least PROTECT_OVERRIDE times theirs.

    Protected = age <= 23 AND (a top-32 pick, OR rostered in >= 80% of leagues, OR unproven
    (< 82 career NHL GP) and rostered in >= 60%). Veterans are never protected: the normal
    dynasty-value comparison (DYNASTY_MIN_RATIO) governs them. (A rule protecting anyone
    >= 80% rostered covered most of a competitive dynasty roster, so waivers never fired.)"""
    if age is None or age > PROTECT_MAX_AGE:
        return None
    if p.draft_overall is not None and p.draft_overall <= PROTECT_MAX_PICK:
        return f"{p.name} is {age:.0f} and a #{p.draft_overall} overall pick"
    owned = p.pct_owned
    if owned is not None and owned >= PROTECT_MIN_OWNED:
        return f"{p.name} is {age:.0f} and rostered in {owned:.0f}% of leagues"
    if owned is not None and owned >= PROTECT_UNPROVEN_OWNED and p.career_gp is not None             and p.career_gp < PROTECT_UNPROVEN_GP:
        return f"{p.name} is {age:.0f}, {p.career_gp} NHL GP and rostered in {owned:.0f}% of leagues"
    return None


# -- transaction budget and churn guard (LeagueContext.moves_* / recent_adds) -----------------

BASE_MIN_GAIN = 0.3          # FPG a waiver add must clear with plenty of moves (waivers.MIN_GAIN)
SCARCE_MIN_GAIN = 0.6        # 2-3 moves left
LAST_MOVE_MIN_GAIN = 1.0     # 1 move left (unless it replaces an injured player)
PLENTY_MOVES = 4
RECENT_ADD_DAYS = 7          # churn guard: a player I added this recently is not dropped ...
CHURN_OVERRIDE_GAIN = 1.5    # ... unless the add beats him by this much FPG ...
CHURN_EXEMPT_STATUSES = ("out", "ir", "ltir", "suspended")   # ... or he is unavailable
# An add that replaces one of these (IR move, or dropping him) may use the last move. A suspension
# is not an injury: dropping a suspended regular is a normal swap and must clear the full bar.
INJURY_REPLACEMENT_STATUSES = ("out", "ir", "ltir")
RECENT_ADDS_WINDOW = 14      # days of my adds kept in LeagueContext.recent_adds


def moves_left(ctx: LeagueContext) -> int | None:
    """Acquisitions I can still make now: the per-period limit minus my adds this period, capped
    by what is left of a season limit. None = unlimited / unknown. Unknown usage counts as 0."""
    lefts = []
    per = getattr(ctx, "moves_limit_per_period", None)
    if per is not None and per >= 0:
        lefts.append(max(0, int(per) - int(ctx.moves_used_this_period or 0)))
    season = getattr(ctx, "moves_limit_season", None)
    if season is not None and season >= 0:
        lefts.append(max(0, int(season) - int(ctx.moves_used_season or 0)))
    return min(lefts) if lefts else None


def recently_added(ctx: LeagueContext, player: Player, days: int = RECENT_ADD_DAYS) -> bool:
    """True when I added `player` fewer than `days` days before ``ctx.as_of``."""
    when = (getattr(ctx, "recent_adds", None) or {}).get(player.cid)
    return when is not None and (ctx.as_of - when).days < days


def added_days_ago(ctx: LeagueContext, player: Player) -> int | None:
    when = (getattr(ctx, "recent_adds", None) or {}).get(player.cid)
    return None if when is None else max(0, (ctx.as_of - when).days)


def churn_blocked(ctx: LeagueContext, drop: Player, gain: float) -> bool:
    """Churn guard: never drop a player I added < RECENT_ADD_DAYS days ago unless the add gains
    >= CHURN_OVERRIDE_GAIN FPG over him or he is out / IR / suspended."""
    return (recently_added(ctx, drop) and gain < CHURN_OVERRIDE_GAIN
            and drop.status not in CHURN_EXEMPT_STATUSES)


def churn_text(ctx: LeagueContext, drop: Player) -> str:
    """"added 1 day ago" / "added today"."""
    n = added_days_ago(ctx, drop)
    if n is None:
        return "added recently"
    return "added today" if n == 0 else f"added {n} day{'s' if n != 1 else ''} ago"


def move_scarcity_threshold(ctx: LeagueContext, injury_replacement: bool = False) -> float | None:
    """Minimum FPG gain a waiver add must clear given the moves left: unlimited / unknown or
    >= 4 left -> 0.3; 2-3 -> 0.6; 1 -> 1.0 (0.3 when the add replaces an injured player);
    0 -> None (no adds until the period resets; see ``no_moves_text``)."""
    left = moves_left(ctx)
    if left is None or left >= PLENTY_MOVES:
        return BASE_MIN_GAIN
    if left >= 2:
        return SCARCE_MIN_GAIN
    if left == 1:
        return BASE_MIN_GAIN if injury_replacement else LAST_MOVE_MIN_GAIN
    return None


def _day(d: date) -> str:
    return f"{d:%a %b} {d.day}"


def moves_reset(ctx: LeagueContext) -> date | None:
    """First day of the next period (when the per-period count resets)."""
    end = getattr(ctx, "period_end", None)
    return end + timedelta(days=1) if end is not None else None


def _period_word(ctx: LeagueContext) -> str:
    start, end = getattr(ctx, "period_start", None), getattr(ctx, "period_end", None)
    if start is not None and end is not None and (end - start).days > 6:
        return "this period"
    return "this week"


def _binding(ctx: LeagueContext) -> tuple[int, int, str] | None:
    """(used, limit, "this week"/"this season") of the limit that binds now, or None."""
    per, season = ctx.moves_limit_per_period, ctx.moves_limit_season
    per_left = max(0, per - int(ctx.moves_used_this_period or 0)) if per is not None and per >= 0 else None
    season_left = (max(0, season - int(ctx.moves_used_season or 0))
                   if season is not None and season >= 0 else None)
    if season_left is not None and (per_left is None or season_left < per_left):
        return int(ctx.moves_used_season or 0), int(season), "this season"
    if per_left is not None:
        return int(ctx.moves_used_this_period or 0), int(per), _period_word(ctx)
    return None


def moves_text(ctx: LeagueContext) -> str:
    """"4 per matchup period (2 used, 2 left, resets Mon Oct 5)" or "unlimited" (plus the
    adds made this period when known, and any season limit)."""
    per = ctx.moves_limit_per_period
    used = ctx.moves_used_this_period
    reset = moves_reset(ctx)
    label = ctx.moves_period_label or "period"
    if per is not None and per >= 0:
        bits = [f"{used} used, {max(0, per - used)} left" if used is not None else "usage unknown"]
        if reset is not None:
            bits.append(f"resets {_day(reset)}")
        text = f"{per} per {label} ({', '.join(bits)})"
    else:
        text = "unlimited"
        if used is not None:
            text += f" ({used} add{'s' if used != 1 else ''} {_period_word(ctx)})"
    season = ctx.moves_limit_season
    if season is not None and season >= 0:
        su = ctx.moves_used_season
        text += (f"; season limit {season} ({su} used, {max(0, season - su)} left)" if su is not None
                 else f"; season limit {season}")
    return text


def moves_note(ctx: LeagueContext) -> str | None:
    """"2 of 4 moves left this week" when a limit applies, else None."""
    b = _binding(ctx)
    if b is None:
        return None
    used, limit, when = b
    return f"{max(0, limit - used)} of {limit} moves left {when}"


def moves_budget_reason(ctx: LeagueContext) -> Reason | None:
    """MOVE_BUDGET reason for waiver / streaming recs (None when moves are unlimited)."""
    note = moves_note(ctx)
    if note is None:
        return None
    left = moves_left(ctx)
    thr = move_scarcity_threshold(ctx)
    if left is not None and 0 < left < PLENTY_MOVES and thr is not None:
        note += f": adds must gain >= {thr:.1f} FPG" + (" (or replace an injured player)" if left == 1 else "")
    b = _binding(ctx)
    return Reason(code="MOVE_BUDGET", text=note, value=float(left) if left is not None else None,
                  baseline=float(b[1]) if b else None)


def no_moves_text(ctx: LeagueContext) -> str:
    """"No moves left this week (4/4 used); waiver suggestions resume Mon Oct 5"."""
    b = _binding(ctx)
    if b is None:
        return "No moves left"
    used, limit, when = b
    text = f"No moves left {when} ({used}/{limit} used)"
    reset = moves_reset(ctx) if when != "this season" else None
    if reset is not None:
        text += f"; waiver suggestions resume {_day(reset)}"
    return text


def count_adds(items: Iterable[Any], team_id: str, start: date | None, end: date | None = None) -> int:
    """My acquisitions in [start, end]: ADD activity items (free-agent adds and waiver claims)
    of `team_id`. Drops, trades, IR moves and other teams' adds do not count."""
    n = 0
    for it in items or []:
        if getattr(it, "action", None) != "ADD" or str(getattr(it, "team_id", None)) != str(team_id):
            continue
        d = it.ts.date()
        if (start is None or d >= start) and (end is None or d <= end):
            n += 1
    return n


def recent_adds_from(items: Iterable[Any], team_id: str, as_of: date,
                     days: int = RECENT_ADDS_WINDOW) -> dict[str, date]:
    """{cid: date of my latest add} for my ADD items in the `days` days up to `as_of`."""
    out: dict[str, date] = {}
    since = as_of - timedelta(days=days)
    for it in items or []:
        if getattr(it, "action", None) != "ADD" or str(getattr(it, "team_id", None)) != str(team_id):
            continue
        cid = getattr(it, "cid", None)
        d = it.ts.date()
        if cid and since <= d <= as_of and (cid not in out or d > out[cid]):
            out[cid] = d
    return out
