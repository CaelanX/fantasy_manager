"""Plan memory: what changed since the last weekly plan the user saw (``recommend.plan``).

Every plan built with a ``data_dir`` (the CLI, the dashboard, the harness) is compared with the
earlier plans for the same matchup period, kept in ``<data_dir>/plans/<league>-<period start>.json``
(a snapshot is appended only when the steps or the roster changed; at most ``SNAPSHOT_LIMIT`` per
period, periods older than ``KEEP_DAYS`` are deleted). A snapshot holds the steps (add / drop / IR
cids and names, day, gain), today's IR items and my roster (cid -> name, in an IR slot or not).

* **Stability** (``stabilize``): the steps of the last plan that are still possible (not done, the
  add still a free agent and healthy, the drop still on my active roster or added by an earlier
  step) are re-scored on today's timeline, topped up with the new plan's other moves up to the
  moves left. The new plan replaces them only when it adds at least ``STABLE_PTS`` points or
  ``STABLE_PCT`` of the kept plan's gain more; otherwise the earlier steps stay ("kept").
* **Since your last plan** (``compare``, against the latest snapshot whose steps or roster differ
  from this plan, so the block stays until something changes again):
  done steps (the add is on my roster and the drop gone), IR swaps made, other roster changes,
  removed steps with a reason from the data (picked up by another team, injured, now on waivers,
  the drop is gone, fewer moves left, or replaced by a better step: both gains, the player an IR
  swap made droppable, the best N moves when moves are the limit, else its gain today vs the bar),
  new steps, and kept steps. A removed step whose add is a better long-term asset than his drop
  becomes a "Long-term option" watch item (consider him when moves reset; % rostered trend as risk).
"""
from __future__ import annotations

import json
from datetime import date, datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Mapping

from ..models import LeagueContext, Player
from .lineup import UNAVAILABLE

STABLE_PTS = 1.5               # matchup points a new plan must add over the kept steps to replace them
STABLE_PCT = 0.20              # ... or this share of the kept steps' gain, whichever is larger
SNAPSHOT_LIMIT = 30            # snapshots kept per matchup period
KEEP_DAYS = 56                 # periods older than this are deleted
RISER_HINT = 3.0               # % rostered gained this week that makes "he may not last" worth saying


def move_key(m: Any) -> tuple:
    """(add, drop, to_ir) cids of a plan move, a _Tx or a snapshot step."""
    def cid(x: Any) -> str | None:
        if x is None:
            return None
        return x.get("cid") if isinstance(x, dict) else x.cid
    get = (lambda k: m.get(k)) if isinstance(m, dict) else (lambda k: getattr(m, k))
    return cid(get("add")), cid(get("drop")), cid(get("to_ir"))


# --------------------------------------------------------------------------- storage

def _path(data_dir: Any, league: str, start: date) -> Path:
    return Path(data_dir) / "plans" / f"{league}-{start.isoformat()}.json"


def _period_start(plan: Any, ctx: LeagueContext) -> date | None:
    return plan.start or getattr(ctx, "period_start", None)


def _ref(p: Player | None) -> dict | None:
    if p is None:
        return None
    return {"cid": p.cid, "name": p.name, "positions": list(p.positions)}


def snapshot(plan: Any, ctx: LeagueContext, now: datetime) -> dict:
    roster = {s.player.cid: {"name": s.player.name, "ir": s.slot in ("IR", "IR+")}
              for s in ctx.my_team.slots if s.player is not None}
    steps = [{"day": m.day.isoformat(), "effective": m.effective.isoformat(), "add": _ref(m.add),
              "drop": _ref(m.drop), "to_ir": _ref(m.to_ir), "gain_pts": m.gain_pts,
              "waiver": m.add.waiver_until.isoformat() if m.add.waiver_until else None} for m in plan.moves]
    return {"created": now.astimezone(timezone.utc).isoformat(timespec="seconds"), "as_of": ctx.as_of.isoformat(),
            "moves_left": plan.moves_left, "moves": steps, "today": [a.title for a in plan.today],
            "roster": roster, "signature": _signature(steps, roster)}


def _signature(steps: list[dict], roster: Mapping[str, Any]) -> str:
    keys = sorted(json.dumps(move_key(s)) for s in steps)
    return json.dumps([keys, sorted((c, bool(v.get("ir"))) for c, v in roster.items())])


def load_snapshots(data_dir: Any, plan: Any, ctx: LeagueContext) -> list[dict]:
    """Earlier snapshots for this matchup period, oldest first ([] when none or unreadable). Seeds
    from that period's newest plan archive (``archive_plan``) when no snapshot exists yet."""
    start = _period_start(plan, ctx)
    if start is None:
        return []
    path = _path(data_dir, plan.league, start)
    try:
        snaps = json.loads(path.read_text(encoding="utf-8"))
        if isinstance(snaps, list):
            return snaps
    except (OSError, ValueError):
        pass
    return _from_archive(data_dir, plan.league, start, plan.end)


def _from_archive(data_dir: Any, league: str, start: date, end: date | None) -> list[dict]:
    from ..backtest.archive import archive_dir

    try:
        files = sorted(archive_dir(data_dir).glob(f"plan-{league}-*.json"))
    except OSError:
        return []
    for f in reversed(files):
        try:
            day = date.fromisoformat(f.stem[-10:])
        except ValueError:
            continue
        if day < start or (end is not None and day > end):
            continue
        try:
            body = json.loads(f.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        p = body.get("plan") or {}
        steps = [{"day": m.get("day"), "effective": m.get("effective"),
                  "add": _strip(m.get("add")), "drop": _strip(m.get("drop")), "to_ir": _strip(m.get("to_ir")),
                  "gain_pts": m.get("gain_pts"), "waiver": (m.get("add") or {}).get("waiver_until")}
                 for m in p.get("moves") or []]
        return [{"created": body.get("created"), "as_of": body.get("as_of"), "moves_left": p.get("moves_left"),
                 "moves": steps, "today": [a.get("title") for a in p.get("today") or []], "roster": None,
                 "signature": _signature(steps, {})}]
    return []


def _strip(p: Any) -> dict | None:
    if not isinstance(p, dict):
        return None
    return {"cid": p.get("cid"), "name": p.get("name"), "positions": p.get("positions") or []}


def save_snapshot(data_dir: Any, plan: Any, ctx: LeagueContext, now: datetime) -> None:
    """Append this plan when its steps or my roster differ from the last snapshot (best effort)."""
    start = _period_start(plan, ctx)
    if start is None or not plan.available:
        return
    path = _path(data_dir, plan.league, start)
    snaps = []
    try:
        snaps = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(snaps, list):
            snaps = []
    except (OSError, ValueError):
        pass
    snap = snapshot(plan, ctx, now)
    if snaps and snaps[-1].get("signature") == snap["signature"]:
        return
    snaps = (snaps + [snap])[-SNAPSHOT_LIMIT:]
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(snaps, indent=1), encoding="utf-8")
        tmp.replace(path)
        _prune(path.parent, ctx.as_of)
    except OSError:
        pass


def _prune(folder: Path, today: date) -> None:
    for f in folder.glob("*.json"):
        try:
            start = date.fromisoformat(f.stem[-10:])
        except ValueError:
            continue
        if start < today - timedelta(days=KEEP_DAYS):
            try:
                f.unlink()
            except OSError:
                pass


# --------------------------------------------------------------------------- stability

def _done(step: Mapping[str, Any], mine: set[str]) -> bool:
    add, drop, _ = move_key(step)
    return add in mine and (drop is None or drop not in mine)


def stabilize(ctx: LeagueContext, tl: Any, txs: list, prev: Mapping[str, Any], pairs: list, max_moves: int,
              min_pts: float, now: datetime) -> tuple[list, dict[tuple, str]]:
    """(moves to plan, {kept step key: why}) - the module docstring's stability rule."""
    from .plan import _Tx, _best_day, _earliest, _move_day

    mine = {p.cid for p in ctx.my_team.players}
    fa = {p.cid: p for p in ctx.free_agents}
    active = {p.cid: p for p in tl.base}
    rec_for = {(a.cid, d.cid if d else None, i.cid if i else None): r for a, d, i, r in pairs}
    cur = {move_key(t) for t in txs}
    forced: list = []
    for step in prev.get("moves") or []:
        if len(forced) >= max_moves or _done(step, mine):
            continue
        add_id, drop_id, ir_id = move_key(step)
        add = fa.get(add_id)
        if add is None or add.status in UNAVAILABLE or ir_id is not None:
            continue
        rented = {f.add.cid: f.add for f in forced}
        drop = active.get(drop_id) or rented.get(drop_id) if drop_id is not None else None
        if drop_id is not None and drop is None:
            continue
        hit = _best_day(tl, forced, add, drop, None, _earliest(ctx, add, now))
        if hit is None or hit[0] < min_pts:
            continue
        forced.append(_Tx(day=_move_day(ctx, add, hit[1]), effective=hit[1], add=add, drop=drop,
                          rec=rec_for.get((add_id, drop_id, None))))
    if not forced or all(move_key(f) in cur for f in forced):
        return txs, {}
    for t in txs:          # top up with the new plan's other moves
        if len(forced) >= max_moves:
            break
        adds = {f.add.cid for f in forced}
        gone = {x.cid for f in forced for x in (f.drop, f.to_ir) if x is not None}
        if t.add.cid in adds or any(x is not None and (x.cid in gone or x.cid in adds) for x in (t.drop, t.to_ir)):
            continue
        hit = _best_day(tl, forced, t.add, t.drop, t.to_ir, _earliest(ctx, t.add, now))
        if hit is not None and hit[0] >= min_pts:
            forced.append(_Tx(day=_move_day(ctx, t.add, hit[1]), effective=hit[1], add=t.add, drop=t.drop,
                              to_ir=t.to_ir, rec=t.rec))
    base = tl.total([])
    new_gain, old_gain = tl.total(txs) - base, tl.total(forced) - base
    bar = max(STABLE_PTS, STABLE_PCT * max(old_gain, 0.0))
    if new_gain - old_gain >= bar:
        return txs, {}
    kept = {}
    for f in forced:
        k = move_key(f)
        if k not in cur:
            kept[k] = (f"Kept {_desc_tx(f)} from your earlier plan: changing it would add only "
                       f"{new_gain - old_gain:+.1f} pts (under the {bar:.1f}-pt bar for changing a plan)")
    return sorted(forced, key=lambda t: (t.day, t.effective)), kept


def _desc_tx(t: Any) -> str:
    return f"add {t.add.name}" + (f", drop {t.drop.name}" if t.drop is not None else "")


# --------------------------------------------------------------------------- since your last plan

def _desc(step: Mapping[str, Any]) -> str:
    add = (step.get("add") or {}).get("name", "?")
    drop = (step.get("drop") or {}).get("name")
    ir = (step.get("to_ir") or {}).get("name")
    return f"{add} for {drop}" if drop else (f"{add} ({ir} to IR)" if ir else add)


def _wd(iso: str | None) -> str:
    try:
        return f"{date.fromisoformat(iso):%a}" if iso else ""
    except ValueError:
        return ""


def _when(created: str | None) -> str:
    from .plan import _clock, _local_zone

    try:
        t = datetime.fromisoformat(created) if created else None
    except ValueError:
        t = None
    if t is None:
        return ""
    loc = (t if t.tzinfo else t.replace(tzinfo=timezone.utc)).astimezone(_local_zone())
    return f"{loc:%a} {_clock(t)}"


def _value(values: Mapping[str, Any], dyn: Mapping[str, float] | None, p: Player) -> float:
    if dyn is not None:
        return float(dyn.get(p.cid, 0.0))
    return float(getattr(values.get(p.cid), "fpg_season", 0.0) or 0.0)


def compare(plan: Any, ctx: LeagueContext, snaps: list[dict], kept: Mapping[tuple, str], tl: Any, txs: list,
            values: Mapping[str, Any], dyn: Mapping[str, float] | None, now: datetime) -> Any:
    """The "Since your last plan" block (module docstring), or None without an earlier plan."""
    from .plan import PlanSince, _best_day, _earliest, _slot_label

    cur_steps = [{"add": _ref(m.add), "drop": _ref(m.drop), "to_ir": _ref(m.to_ir)} for m in plan.moves]
    cur_roster = {s.player.cid: {"name": s.player.name, "ir": s.slot in ("IR", "IR+")}
                  for s in ctx.my_team.slots if s.player is not None}
    def same(s: Mapping[str, Any]) -> bool:      # an archive seed has no roster: compare steps only
        return s.get("signature") == _signature(cur_steps, cur_roster if s.get("roster") is not None else {})

    base = next((s for s in reversed(snaps) if not same(s)), None)
    if base is None:      # the plan is unchanged; say so when stability is what kept it
        return PlanSince(at=_when(snaps[-1].get("created")), kept=list(kept.values())) if kept and snaps else None
    since = PlanSince(at=_when(base.get("created")))
    mine = set(cur_roster)
    fa = {p.cid: p for p in ctx.free_agents}
    owner = {p.cid: t.name for t in ctx.teams if t is not ctx.my_team for p in t.players}
    prev_roster = base.get("roster")
    cur_keys = {move_key(m) for m in plan.moves}
    prev_steps = base.get("moves") or []
    prev_keys = {move_key(s) for s in prev_steps}

    # done steps and roster changes
    planned_adds = set()
    for s in prev_steps:
        if _done(s, mine):
            add, drop = (s.get("add") or {}).get("name"), (s.get("drop") or {}).get("name")
            since.done.append(f"Done: added {add}" + (f", dropped {drop}" if drop else "")
                              + (f" ({_wd(s.get('day'))})" if s.get("day") else ""))
            planned_adds.add(move_key(s)[0])
            if move_key(s)[1]:
                planned_adds.add(move_key(s)[1])
    ir_freed: set[str] = set()
    if isinstance(prev_roster, dict):
        out_ir = [c for c, v in cur_roster.items() if not v["ir"] and (prev_roster.get(c) or {}).get("ir")]
        in_ir = [c for c, v in cur_roster.items() if v["ir"] and c in prev_roster and not prev_roster[c].get("ir")]
        ir_freed = set(out_ir)
        if out_ir or in_ir:
            names = [cur_roster[c]["name"] for c in out_ir + in_ir]
            planned = any(n in t for t in base.get("today") or [] for n in names)
            parts = ([f"activated {', '.join(cur_roster[c]['name'] for c in out_ir)}"] if out_ir else []) \
                + ([f"moved {', '.join(cur_roster[c]['name'] for c in in_ir)} to IR"] if in_ir else [])
            (since.done if planned else since.roster).append(
                ("Done: IR swap, " if planned else "IR change: ") + " and ".join(parts))
        added = [v["name"] for c, v in cur_roster.items() if c not in prev_roster and c not in planned_adds]
        dropped = [v["name"] for c, v in prev_roster.items() if c not in cur_roster and c not in planned_adds]
        if added or dropped:
            since.roster.append("Roster change outside the plan: "
                                + "; ".join(([f"added {', '.join(added)}"] if added else [])
                                            + ([f"dropped {', '.join(dropped)}"] if dropped else [])))

    # new steps
    new_moves = [m for m in plan.moves if move_key(m) not in prev_keys and m.status != "kept"]
    for m in new_moves:
        m.status = "new"
        since.new.append(f"New: add {m.add.name}" + (f", drop {m.drop.name}" if m.drop else "")
                         + f" ({m.day:%a}, +{m.gain_pts:.1f} pts)")
    since.kept.extend(kept.values())

    # removed steps, each with its reason
    pending = [s for s in prev_steps if not _done(s, mine) and move_key(s) not in cur_keys]
    used: set[str] = set()
    active = {p.cid: p for p in tl.base}
    binding = plan.moves_left is not None and len(plan.moves) >= plan.moves_left > 0
    reset = (plan.end + timedelta(days=1)) if plan.end else None
    for s in pending:
        add_id, drop_id, _ = move_key(s)
        name = (s.get("add") or {}).get("name", "?")
        old = float(s.get("gain_pts") or 0.0)
        add = fa.get(add_id)
        head = f"{_desc(s)} (+{old:.1f}) dropped from the plan: "
        if add is None:
            why = f"{name} was picked up by {owner[add_id]}" if add_id in owner else f"{name} is no longer available"
        elif add.status in UNAVAILABLE:
            why = f"{name} is now {add.status}" + (f" ({add.status_note})" if add.status_note else "")
        elif add.waiver_until is not None and add.waiver_until >= ctx.as_of and not s.get("waiver"):
            why = f"{name} went on waivers (clears {add.waiver_until:%a}), so he can't count when planned"
        elif drop_id is not None and drop_id not in mine:
            why = f"you no longer have {(s.get('drop') or {}).get('name', '?')}"
        elif plan.moves_left is not None and plan.moves_left < len([x for x in prev_steps if not _done(x, mine)]) \
                and not new_moves:
            why = f"only {plan.moves_left} move{'s' if plan.moves_left != 1 else ''} left"
        else:
            drop = active.get(drop_id) if drop_id else None
            now_gain = None
            if drop_id is None or drop is not None:
                hit = _best_day(tl, txs, add, drop, None, _earliest(ctx, add, now))
                now_gain = hit[0] if hit else 0.0
            repl = next((m for m in new_moves if m.add.cid not in used and _shares(m.add, add)), None) \
                or next((m for m in sorted(new_moves, key=lambda m: -m.gain_pts) if m.add.cid not in used), None)
            if repl is not None:
                used.add(repl.add.cid)
                why = (f"replaced by {repl.add.name}" + (f" for {repl.drop.name}" if repl.drop else "")
                       + f" (+{repl.gain_pts:.1f})")
                if repl.drop is not None and repl.drop.cid in ir_freed:
                    why += f"; {repl.drop.name} became droppable after your IR swap"
                if binding:
                    best = ", ".join(f"{m.add.name} (+{m.gain_pts:.1f})" for m in plan.moves)
                    n = plan.moves_left
                    why += f"; with {n} move{'s' if n != 1 else ''} left, the best {'one is' if n == 1 else f'{n} are'} {best}"
            elif now_gain is not None:
                why = f"it's now worth {now_gain:+.1f} pts, under the bar for a move"
            else:
                why = "the plan found better uses for your moves"
        since.changed.append(head + why)
        # a better long-term asset than his drop: worth a move when they reset
        drop_p = active.get(drop_id) if drop_id else None
        if add is not None and add.status not in UNAVAILABLE and drop_p is not None \
                and _value(values, dyn, add) > _value(values, dyn, drop_p):
            unit = "dynasty value" if dyn is not None else "FPG rest of season"
            fmt = "{:.1f}" if dyn is not None else "{:.2f}"
            a, d = _value(values, dyn, add), _value(values, dyn, drop_p)
            line = (f"Long-term option: {add.name} is a better long-term {_slot_label(add)} than {drop_p.name} "
                    f"({fmt.format(a)} vs {fmt.format(d)} {unit}, +{fmt.format(a - d)}); consider him "
                    + (f"{reset:%a %b} {reset.day} when moves reset" if reset else "next week")
                    + " if he's still available")
            chg = add.pct_owned_change
            if chg is not None and chg >= RISER_HINT:
                line += f" · rising: rostered {add.pct_owned or 0:.0f}% ({chg:+.1f} this week), he may not last"
            elif add.pct_owned is not None:
                line += f" · rostered {add.pct_owned:.0f}%" + (f" ({chg:+.1f} this week)" if chg is not None else "")
            since.long_term.append(line)
    return None if since.empty else since


# --------------------------------------------------------------------------- "Did it": verify a move

LEAGUE_LABEL = {"espn": "ESPN", "fantrax": "Fantrax"}


def latest_snapshot(data_dir: Any, league: str) -> dict | None:
    """The newest snapshot of ``league`` (newest matchup period first), or None."""
    folder = Path(data_dir) / "plans"
    try:
        files = sorted(folder.glob(f"{league}-*.json"), key=lambda f: f.stem[-10:], reverse=True)
    except OSError:
        return None
    for f in files:
        try:
            snaps = json.loads(f.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        if isinstance(snaps, list) and snaps:
            return snaps[-1]
    return None


def find_snapshot(data_dir: Any, league: str, created: str) -> dict | None:
    for f in sorted((Path(data_dir) / "plans").glob(f"{league}-*.json"), key=lambda f: f.stem[-10:], reverse=True):
        try:
            snaps = json.loads(f.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        for s in reversed(snaps if isinstance(snaps, list) else []):
            if s.get("created") == created:
                return s
    return None


def roster_diff(prev: Mapping[str, Any] | None, ctx: LeagueContext) -> tuple[list[str], int]:
    """(changes, open roster spots) of my roster now vs a snapshot's: "Added X", "Dropped Y",
    "X → IR", "X off IR"."""
    from .base import SIZE_EXEMPT_SLOTS, open_roster_spots

    team = ctx.my_team
    now = {s.player.cid: (s.player.name, s.slot in ("IR", "IR+")) for s in team.slots if s.player is not None}
    out: list[str] = []
    if isinstance(prev, dict):
        for c, (name, ir) in now.items():
            if c not in prev:
                out.append(f"Added {name}" + (" (IR)" if ir else ""))
            elif ir and not prev[c].get("ir"):
                out.append(f"{name} → IR")
            elif not ir and prev[c].get("ir"):
                out.append(f"{name} off IR")
        out.extend(f"Dropped {v.get('name', '?')}" for c, v in prev.items() if c not in now)
    try:
        spots = int(open_roster_spots(team, ctx.roster_shape) or 0)
        cap = getattr(ctx, "max_roster_size", None)      # e.g. Fantrax "16 total" below the slot sum
        if cap:
            used = sum(1 for s in team.slots if s.player is not None and s.slot not in SIZE_EXEMPT_SLOTS)
            spots = min(spots, max(0, int(cap) - used))
    except Exception:  # noqa: BLE001 - the spot count is a nicety
        spots = 0
    return out, spots


def moved_banner(data_dir: Any, league: str, since: str | None, ctx: LeagueContext, label: str = "",
                 retry: bool = False) -> tuple[str, bool, bool]:
    """(banner text, is_warning, retry once more) after "Did it": the roster now vs the plan
    snapshot shown when the button was pressed."""
    snap = find_snapshot(data_dir, league, since) if since else None
    changes, spots = roster_diff(snap.get("roster") if snap else None, ctx)
    spot_text = f"{spots} open roster spot{'s' if spots != 1 else ''}" if spots else ""
    if changes:
        return "✓ Seen: " + " · ".join(changes + ([spot_text] if spot_text else [])) + ". The plan is updated.", \
            False, False
    name = LEAGUE_LABEL.get(league, league)
    what = f" ({label})" if label else ""
    if retry:
        return (f"Still not showing{what}: {name} hasn't reported it yet. Check the move went through, "
                f"then press Refresh data."), True, False
    return f"Not showing yet{what}: {name} can take a minute; we'll keep checking.", True, True


def _shares(a: Player, b: Player) -> bool:
    from .base import shares_slot

    return shares_slot(a, b)


def since_line(plan: Any) -> str | None:
    """One line for the pre-game post and the morning digest."""
    s = getattr(plan, "since", None)
    if s is None or s.empty:
        return None
    parts = []
    if s.done:
        parts.append(f"{len(s.done)} done")
    if s.changed:
        parts.append("changed: " + "; ".join(c.split(" dropped from the plan: ")[0] + " -> "
                                            + c.split(" dropped from the plan: ")[-1] for c in s.changed[:2]))
    if s.new:
        parts.append(f"{len(s.new)} new")
    if s.kept:
        parts.append(f"{len(s.kept)} kept")
    return f"Since your last plan{f' ({s.at})' if s.at else ''}: " + ", ".join(parts) if parts else None


__all__ = ["KEEP_DAYS", "SNAPSHOT_LIMIT", "STABLE_PCT", "STABLE_PTS", "compare", "find_snapshot", "latest_snapshot",
           "load_snapshots", "move_key", "moved_banner", "roster_diff", "save_snapshot", "since_line", "snapshot",
           "stabilize"]
