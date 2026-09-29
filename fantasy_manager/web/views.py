"""View models for the dashboard templates.

Pure functions of a ``LoadResult`` (``res``): one dict per player row (values, rates, schedule
with opponents), side-by-side move comparisons, predicted gain / strength / rank per move, the
"This week" lineup panel, standings, free agents, injuries around the league, a trade
counterparty's weakest slots, a player's 14-day schedule and stat splits. Lineup solves are
memoised on ``res.memo`` so a page renders them once per cached result.
"""
from __future__ import annotations

from datetime import date, timedelta
from typing import Any, Iterable, Mapping

from ..models import GOALIE_STATS, SKATER_STATS, FantasyTeam, LeagueContext, Player, Recommendation

SKATER_RATES = ("G", "A", "PTS", "SOG", "HIT", "BLK", "PPP", "PIM")
GOALIE_RATES = ("GS", "W", "GAA", "SVPCT", "SO")
RATE_LABEL = {"SVPCT": "SV%"}
ALERT_STATUSES = ("dtd", "out", "ir", "ltir", "suspended")
SLOT_ORDER = {s: i for i, s in enumerate(("C", "LW", "RW", "F", "D", "UTIL", "G", "BN", "IR"))}
MOVE_LABEL = {"waiver": ("Add", "Drop"), "injury": ("Add", "Drop"), "trade": ("Get", "Give"),
              "lineup": ("Start", "Sit"), "sell_high": ("Buy", "Sell"), "buy_low": ("Buy", "Sell"),
              "alert": ("Add", "Drop")}
KIND_PLURAL = {"injury": "injury moves", "lineup": "lineup moves", "waiver": "waivers", "trade": "trades",
               "sell_high": "sell-high flags", "buy_low": "buy-low flags", "alert": "alerts"}
GAIN_UNITS = {"week_pts": "pts/wk", "season_fpg": "FPG", "lineup_fpg": "lineup FPG", "dynasty": "dynasty value"}
SPLIT_ORDER = ("season", "last30", "last15", "last7", "prior", "prior2", "prior3", "projected")
SPLIT_LABEL = {"season": "Season", "last7": "Last 7 days", "last15": "Last 15 days", "last30": "Last 30 days",
               "projected": "Projected", "prior": "Prior season", "prior2": "2 seasons ago",
               "prior3": "3 seasons ago"}
POSITION_FILTERS = ("C", "LW", "RW", "F", "D", "G")
# Same rule as the CLI roster table (cli._is_prospect)
PROSPECT_MAX_AGE = 22
PROSPECT_MAX_GP = 82


# --------------------------------------------------------------------------- small helpers

def pos_str(p: Player) -> str:
    return "/".join(x for x in p.positions if x != "F") or "/".join(p.positions) or "-"


def my_team(ctx: LeagueContext) -> FantasyTeam | None:
    try:
        return ctx.my_team
    except LookupError:
        return ctx.teams[0] if ctx.teams else None


def player_age(p: Player, as_of: Any, dynasty: Mapping[str, Any] | None = None) -> float | None:
    a = getattr((dynasty or {}).get(p.cid), "age", None)
    if isinstance(a, (int, float)):
        return float(a)
    return (as_of - p.birth_date).days / 365.25 if p.birth_date and as_of else None


def is_prospect(p: Player, age: float | None) -> bool:
    """Age <= 22 with fewer than 82 career NHL games (same rule as the CLI roster table)."""
    if age is None or age >= PROSPECT_MAX_AGE + 1:
        return False
    gp = p.career_gp if p.career_gp is not None else p.gp("season") + p.gp("prior")
    return gp < PROSPECT_MAX_GP


def _num(v: Any) -> float | None:
    return float(v) if isinstance(v, (int, float)) and not isinstance(v, bool) else None


def _memo(res: Any, key: str, fn):
    memo = getattr(res, "memo", None)
    if memo is None:
        return fn()
    if key not in memo:
        memo[key] = fn()
    return memo[key]


# --------------------------------------------------------------------------- schedule

def week_start(ctx: LeagueContext) -> tuple[date, int, bool]:
    """(start, days, preseason) of the projection window valuation uses (the season opener
    when it is still preseason), or (as_of, 7, False) without a schedule."""
    if ctx.schedule:
        try:
            from ..valuation.schedule import context_window

            w = context_window(ctx)
            if w is not None:
                return w.start, w.days, w.preseason
        except Exception:
            pass
    return ctx.as_of, 7, False


def team_games(ctx: LeagueContext, team: str | None, start: date, days: int) -> list[dict[str, Any]]:
    """NHL games for ``team`` in [start, start + days): date, opponent label, off-night flag."""
    if not team:
        return []
    try:
        from ..valuation.schedule import OFFNIGHT_THRESHOLD
    except Exception:
        OFFNIGHT_THRESHOLD = 8
    end = start + timedelta(days=days - 1)
    opp = (getattr(ctx, "opponents", None) or {}).get(team, {})
    out = []
    for d in ctx.schedule.get(team, []):
        if start <= d <= end:
            n = ctx.games_per_day.get(d, 0)
            out.append({"date": d, "opp": opp.get(d), "off": 0 < n < OFFNIGHT_THRESHOLD, "league_games": n})
    return out


def games_text(games: list[dict[str, Any]]) -> str:
    """"3: @TOR, BOS (off), @MTL" style summary (opponent, or weekday when unknown)."""
    if not games:
        return "0"
    parts = [(g["opp"] or g["date"].strftime("%a")) + (" (off)" if g["off"] else "") for g in games]
    return f"{len(games)}: " + ", ".join(parts)


# --------------------------------------------------------------------------- player rows

def rate_line(p: Player, pv: Any) -> dict[str, float | None]:
    """Per-game box-score line: blended model rates, else the season (or prior) per-game line."""
    cols = GOALIE_RATES if p.is_goalie else SKATER_RATES
    rates = dict(getattr(pv, "rates", None) or {})
    if not rates:
        for split in ("season", "prior", "prior2"):
            line = p.lines.get(split)
            if line is not None and line.gp > 0:
                rates = line.per_game()
                break
    if "PTS" not in rates and ("G" in rates or "A" in rates):
        rates["PTS"] = rates.get("G", 0.0) + rates.get("A", 0.0)
    if "PPP" not in rates and ("PPG" in rates or "PPA" in rates):
        rates["PPP"] = rates.get("PPG", 0.0) + rates.get("PPA", 0.0)
    return {c: _num(rates.get(c)) for c in cols}


def unit_badges(p: Player) -> list[str]:
    """Daily Faceoff line / power-play unit labels, e.g. ["F1", "PP1"] (goalies: "G1" / "G2" is
    not known here, so "G" only when listed)."""
    out = []
    if p.line:
        out.append(p.line.upper())
    if p.pp_unit:
        out.append(p.pp_unit.upper())
    return out


def xg_luck(p: Player) -> dict[str, Any] | None:
    """MoneyPuck finishing luck of a skater: goals - ixG (season total and per game), ixG/GP and
    whether it is last season's line. None without xG data."""
    from ..valuation.regression import luck_signals

    try:
        sig = luck_signals(p)
    except Exception:  # display only
        return None
    if sig is None or sig.gp <= 0:
        return None
    return {"gmx": sig.goals_minus_ixg, "per_gp": sig.goals_minus_ixg / sig.gp, "ixg": sig.ixg_per_game,
            "goals_pg": sig.goals_per_game, "gp": sig.gp, "prior": sig.from_prior,
            "onice_sh_delta": sig.onice_sh_pct_delta}


def credits(ctx: LeagueContext) -> list[str]:
    """Data credits owed on pages that show MoneyPuck xG or Daily Faceoff lines / starts."""
    from ..report.digest import credits as digest_credits

    try:
        return digest_credits(ctx)
    except Exception:  # never let a footer line sink a page
        return []


def player_row(res: Any, p: Player, slot: Any = None, owner: str | None = None) -> dict[str, Any]:
    ctx = res.ctx
    pv = res.values.get(p.cid)
    d = (res.dynasty or {}).get(p.cid)
    start, days, _ = _memo(res, "week_start", lambda: week_start(ctx))
    age = player_age(p, ctx.as_of, res.dynasty)
    games = team_games(ctx, p.team, start, days)
    g7 = getattr(pv, "games_next7", None)
    return {
        "p": p, "cid": p.cid, "name": p.name, "slot": slot, "owner": owner, "pos": pos_str(p),
        "team": p.team or "FA", "age": age, "prospect": is_prospect(p, age), "status": p.status,
        "note": p.status_note, "gp": p.gp("season"), "goalie": p.is_goalie,
        "fpg": _num(getattr(pv, "fpg", None)), "fpg_season": _num(getattr(pv, "fpg_season", None)),
        "fpg_week": _num(getattr(pv, "fpg_week", None)), "proj_week": _num(getattr(pv, "proj_week", None)),
        "games": games, "games_n": g7 if isinstance(g7, int) else (len(games) if games else None),
        "games_text": games_text(games) if games else (str(g7) if isinstance(g7, int) else "-"),
        "offnight": getattr(pv, "offnight_next7", None), "vorp": _num(getattr(pv, "vorp", None)),
        "dyn": _num(getattr(d, "value", None)), "pct": _num(p.pct_owned),
        "start_share": _num(getattr(pv, "start_share", None)), "rates": rate_line(p, pv),
        "badges": unit_badges(p), "line_change": p.line_change, "confirmed_start": p.confirmed_start,
        "xg": xg_luck(p),
    }


# --------------------------------------------------------------------------- moves

NET_KEYS = ("fpg", "fpg_season", "proj_week", "games_n", "vorp", "dyn")


def compare(res: Any, r: Recommendation) -> dict[str, Any]:
    """Side-by-side players of a move (in vs out) plus a net column (in minus out) when a move
    has both sides; rate rows are skater or goalie lines depending on who is involved."""
    lab = MOVE_LABEL.get(r.kind, ("Add", "Drop"))
    cols = [{"side": "in", "verb": lab[0], "row": player_row(res, p)} for p in r.add]
    cols += [{"side": "out", "verb": lab[1], "row": player_row(res, p)} for p in r.drop]
    shown = {c["row"]["cid"] for c in cols}
    if r.kind in ("injury", "waiver") and "to IR" in r.title:
        subj_verb = "To IR"
    elif r.kind == "lineup":
        subj_verb = "Moves"          # starters shifting slots in a lineup chain (LINEUP_MOVE reasons)
    else:
        subj_verb = "Player"
    cols += [{"side": "subj", "verb": subj_verb, "row": player_row(res, p)}
             for p in getattr(r, "subjects", None) or [] if p.cid not in shown]
    rows = [c["row"] for c in cols]
    goalies = [x["goalie"] for x in rows]
    rate_cols: list[str] = []
    if not all(goalies):
        rate_cols += list(SKATER_RATES)
    if any(goalies):
        rate_cols += list(GOALIE_RATES)
    net: dict[str, float | None] | None = None
    if r.add and r.drop:
        net = {}
        ins = [c["row"] for c in cols if c["side"] == "in"]
        outs = [c["row"] for c in cols if c["side"] == "out"]

        def total(side: list[dict[str, Any]], get) -> float | None:
            vals = [get(x) for x in side]
            return sum(v for v in vals if v is not None) if any(v is not None for v in vals) else None

        for k in NET_KEYS:
            a, b = total(ins, lambda x: x[k]), total(outs, lambda x: x[k])
            net[k] = a - b if a is not None and b is not None else None
        same_type = len(set(goalies)) == 1
        for k in rate_cols:
            if not same_type or k in ("GAA", "SVPCT"):
                net[k] = None
                continue
            a, b = total(ins, lambda x: x["rates"].get(k)), total(outs, lambda x: x["rates"].get(k))
            net[k] = a - b if a is not None and b is not None else None
    return {"cols": cols, "rate_cols": rate_cols, "net": net,
            "has_dyn": res.dynasty is not None, "has_pct": any(x["pct"] is not None for x in rows),
            "has_proj": any(x["proj_week"] is not None for x in rows),
            "has_xg": any(x["xg"] is not None for x in rows)}


def _confidence(r: Recommendation) -> tuple[float | None, str | None]:
    c = None
    for x in r.reasons:
        if x.code == "CONFIDENCE" and _num(x.value) is not None:
            c = float(x.value)
        elif x.code == "GP" and _num(x.baseline) is not None and c is None:
            c = float(x.baseline)
    if c is None:
        return None, None
    return c, "low confidence" if c < 0.5 else ("medium confidence" if c < 0.8 else "high confidence")


def horizon_label(days: Any) -> str | None:
    if not isinstance(days, (int, float)):
        return None
    days = int(days)
    if days <= 7:
        return "next 7 days" if days == 7 else f"next {days} days"
    if days >= 120:
        return "season"
    return f"next {days} days"


def _reason(r: Recommendation, code: str):
    return next((x for x in r.reasons if x.code == code), None)


def gain_info(r: Recommendation, res: Any = None) -> dict[str, Any] | None:
    """Predicted gain of a move with explicit units and horizon: the recommendation's own
    ``predicted_gain`` when the engine sets it, else derived from its reasons."""
    conf, conf_label = _confidence(r)
    value = _num(getattr(r, "predicted_gain", None))
    unit = horizon = None
    dyn = None
    if value is not None:
        units = getattr(r, "gain_units", None)
        unit = GAIN_UNITS.get(units, units or "")
        horizon = horizon_label(getattr(r, "horizon_days", None))
        if horizon is None:  # the model's contract: no horizon_days = rest of season
            horizon = {"season_fpg": "rest of season", "lineup_fpg": "rest of season",
                       "dynasty": "multi-season"}.get(units)
    else:
        if r.kind == "lineup" and (x := _reason(r, "LINEUP_GAIN")) is not None:
            value = _num(x.value)
            week = "(week)" in x.text
            has_proj = res is not None and any(
                _num(getattr(res.values.get(p.cid), "proj_week", None)) is not None for p in r.add)
            unit = "pts/wk" if week and has_proj else "FPG"
            horizon = "this week" if week else "season"
        elif r.kind == "waiver" and (x := _reason(r, "VORP_DELTA")) is not None:
            value, unit = _num(x.value), "FPG"
            horizon = "this week" if "(week)" in x.text else "season"
        elif r.kind == "trade" and (x := _reason(r, "DELTA_ME")) is not None:
            value, unit, horizon = _num(x.value), "lineup FPG", "season"
        elif r.kind == "injury" and (x := _reason(r, "WAIVER_ADD")) is not None:
            value, unit, horizon = _num(x.value), "FPG added", None
        if (x := _reason(r, "DYNASTY_GAIN")) is not None:
            dyn = _num(x.value)
        elif (x := _reason(r, "DYNASTY_DELTA")) is not None:
            dyn = _num(x.baseline)
    if value is None:
        return None
    return {"value": value, "unit": unit, "horizon": horizon, "confidence": conf, "confidence_label": conf_label,
            "dynasty": dyn}


def strength_info(r: Recommendation, recs: Iterable[Recommendation] = ()) -> dict[str, Any]:
    """Absolute strength (0-10) when the engine provides it, else the within-kind rank score
    (labelled as a rank, not "/10"); plus "1 of 4 waivers"."""
    s = _num(getattr(r, "strength", None))
    is_rank = s is None
    value = float(r.score) if is_rank else s
    level = "low" if value < 4 else ("mid" if value <= 7 else "high")
    rank, total = getattr(r, "rank_in_kind", None), getattr(r, "kind_total", None)
    if not isinstance(rank, int) or not isinstance(total, int):
        same = sorted((x for x in recs if x.kind == r.kind), key=lambda x: -x.score)
        rank = next((i + 1 for i, x in enumerate(same) if x is r), None)
        total = len(same) if rank is not None else None
    plural = KIND_PLURAL.get(r.kind, "moves")
    if total == 1:
        plural = plural[:-1] if plural.endswith("s") else plural
    return {"value": value, "is_rank": is_rank, "level": level, "pct": max(0, min(100, round(value * 10))),
            "rank_text": f"{rank} of {total} {plural}" if rank and total else None}


def involves_position(r: Recommendation, pos: str) -> bool:
    for p in (*r.add, *r.drop):
        ps = set(p.positions)
        if pos == "F" and ps & {"C", "LW", "RW", "F"}:
            return True
        if pos in ps:
            return True
    return False


def counterparty(res: Any, r: Recommendation) -> dict[str, Any] | None:
    """For a trade: the other team's weakest starting slots (lowest-VORP starter per slot type
    in its best season lineup) and its roster rows."""
    name = (r.counterparty or "").strip()
    if not name:
        return None
    team = next((t for t in res.ctx.teams if t.name.strip() == name), None)
    if team is None:
        return None
    lineup = team_lineup(res, team, "season")
    by_type: dict[str, list[str]] = {}
    for cid, slot in lineup["opt_slot"].items():
        by_type.setdefault(slot, []).append(cid)
    shape = res.ctx.roster_shape
    weak = []
    for slot in dict.fromkeys(lineup["slots"]):
        if slot == "UTIL":
            continue
        cids = by_type.get(slot, [])
        need = int(shape.get(slot, 0))
        if len(cids) < need:
            weak.append({"slot": slot, "player": None, "vorp": None, "empty": need - len(cids)})
            continue
        worst = min(cids, key=lambda c: _num(getattr(res.values.get(c), "vorp", None)) or 0.0)
        weak.append({"slot": slot, "player": next((p for p in team.players if p.cid == worst), None),
                     "vorp": _num(getattr(res.values.get(worst), "vorp", None)), "empty": 0})
    weak.sort(key=lambda w: (w["player"] is not None, w["vorp"] if w["vorp"] is not None else -99.0))
    rows = [player_row(res, s.player, slot=s.slot) for s in
            sorted((s for s in team.slots if s.player), key=lambda s: (SLOT_ORDER.get(s.slot, 99), s.player.name))]
    return {"team": team, "weakest": weak[:3], "rows": rows}


# --------------------------------------------------------------------------- lineups, standings

def team_lineup(res: Any, team: FantasyTeam, horizon: str = "week") -> dict[str, Any]:
    """Optimal vs current lineup for ``team`` (week horizon: projected points when a schedule
    is known, else FPG)."""
    def build() -> dict[str, Any]:
        from ..recommend.lineup import current_total, optimal_lineup, starting_slots

        shape = res.ctx.roster_shape
        slots = starting_slots(shape)
        assign, opt_total = optimal_lineup(team, res.values, shape, horizon)
        cur_total = current_total(team, res.values, horizon)
        return {"slots": slots, "opt_slot": {cid: slots[i] for i, cid in assign.items()},
                "optimal": float(opt_total), "current": float(cur_total)}
    return _memo(res, f"lineup:{team.team_id}:{horizon}", build)


def week_panel(res: Any, team: FantasyTeam | None = None) -> dict[str, Any] | None:
    """"This week": current vs optimal lineup with projected week points, games (with
    opponents and off-nights) per player, and per-player start/sit changes."""
    team = team or my_team(res.ctx)
    if team is None:
        return None
    lu = team_lineup(res, team, "week")
    start, days, preseason = _memo(res, "week_start", lambda: week_start(res.ctx))
    gains = lineup_changes(res) if team.owner_is_me else {}
    rows = []
    for s in sorted((s for s in team.slots if s.player), key=lambda s: (SLOT_ORDER.get(s.slot, 99), s.player.name)):
        row = player_row(res, s.player, slot=s.slot)
        now = bool(s.starting and s.slot != "IR")
        best = s.player.cid in lu["opt_slot"]
        row.update(now_start=now, opt_start=best, opt_slot=lu["opt_slot"].get(s.player.cid),
                   change="start" if best and not now else ("sit" if now and not best else None),
                   change_gain=gains.get(s.player.cid))
        rows.append(row)
    has_proj = any(r["proj_week"] is not None for r in rows)
    starts = [{"p": r["p"], "starting": bool(r["confirmed_start"]), "source": r["p"].start_source}
              for r in rows if r["goalie"] and r["confirmed_start"] is not None]
    return {"team": team, "rows": rows, "current": lu["current"], "optimal": lu["optimal"], "starts": starts,
            "delta": lu["optimal"] - lu["current"], "units": "pts/wk" if has_proj else "FPG",
            "start": start, "end": start + timedelta(days=days - 1), "preseason": preseason,
            "games": sum(r["games_n"] or 0 for r in rows if r["now_start"]),
            "offnights": sum(r["offnight"] or 0 for r in rows if r["now_start"]),
            "changes": sum(1 for r in rows if r["change"])}


def lineup_changes(res: Any) -> dict[str, dict[str, Any]]:
    """cid -> {"change": "start"|"sit", "gain": lineup gain, "other": player} from lineup recs."""
    out: dict[str, dict[str, Any]] = {}
    for r in res.recs:
        if r.kind != "lineup":
            continue
        x = _reason(r, "LINEUP_GAIN")
        g = _num(x.value) if x is not None else None
        if r.add:
            out.setdefault(r.add[0].cid, {"change": "start", "gain": g, "other": r.drop[0] if r.drop else None})
        if r.drop:
            out.setdefault(r.drop[0].cid, {"change": "sit", "gain": g, "other": r.add[0] if r.add else None})
    return out


def standings(res: Any) -> list[dict[str, Any]]:
    """Every team: record, win %, best-lineup projected week points and season lineup FPG,
    roster VORP and dynasty totals, injured count; sorted by win % then projection."""
    def build() -> list[dict[str, Any]]:
        rows = []
        for t in res.ctx.teams:
            rec = t.record
            gp = sum(rec) if rec else 0
            pct = (rec[0] + 0.5 * rec[2]) / gp if rec and gp else None
            wk = team_lineup(res, t, "week")
            szn = team_lineup(res, t, "season")
            vorps = [_num(getattr(res.values.get(p.cid), "vorp", None)) for p in t.players]
            dyns = [_num(getattr((res.dynasty or {}).get(p.cid), "value", None)) for p in t.players]
            rows.append({"team": t, "record": rec, "pct": pct, "proj_week": wk["optimal"],
                         "lineup_fpg": szn["optimal"], "vorp": sum(v for v in vorps if v is not None),
                         "dyn": sum(v for v in dyns if v is not None) if res.dynasty is not None else None,
                         "injured": sum(1 for p in t.players if p.status in ALERT_STATUSES),
                         "players": len(t.players), "mine": t.owner_is_me})
        by_proj = sorted(rows, key=lambda x: -x["proj_week"])
        for i, x in enumerate(by_proj):
            x["proj_rank"] = i + 1
        rows.sort(key=lambda x: (x["pct"] is None, -(x["pct"] or 0.0), -x["proj_week"]))
        return rows
    return _memo(res, "standings", build)


def free_agents(res: Any, limit: int = 15) -> list[dict[str, Any]]:
    fas = sorted(res.ctx.free_agents, key=lambda p: -(_num(getattr(res.values.get(p.cid), "vorp", None)) or -99.0))
    return [player_row(res, p, owner="FA") for p in fas[:limit]]


def league_injuries(res: Any, limit: int = 15) -> list[dict[str, Any]]:
    """Injured / suspended players on the other fantasy teams, most valuable (healthy FPG) first."""
    rows = []
    for t in res.ctx.teams:
        if t.owner_is_me:
            continue
        for s in t.slots:
            if s.player is not None and s.player.status in ALERT_STATUSES:
                rows.append(player_row(res, s.player, slot=s.slot, owner=t.name))
    rows.sort(key=lambda x: -(x["fpg"] or 0.0))
    return rows[:limit]


# --------------------------------------------------------------------------- player page

def split_table(p: Player, weights: Mapping[str, float] | None) -> tuple[list[str], list[dict[str, Any]]]:
    """Stat splits as rows (season, last 30/15/7, prior seasons, projected) with per-game
    columns and totals; FPG from the league weights when it is a points league."""
    order = GOALIE_STATS if p.is_goalie else SKATER_STATS
    present = {k for line in p.lines.values() for k in line.stats}
    cols = [k for k in order if k in present and k != "GP"]
    cols += sorted(present - set(cols) - {"GP"})
    rows = []
    for split in [s for s in SPLIT_ORDER if s in p.lines] + sorted(set(p.lines) - set(SPLIT_ORDER)):
        line = p.lines[split]
        pg = line.per_game()
        fpg = sum(w * pg.get(k, 0.0) for k, w in weights.items()) if weights and line.gp > 0 else None
        rows.append({"label": SPLIT_LABEL.get(split, split), "gp": line.gp, "fpg": fpg,
                     "pg": {k: pg.get(k) for k in cols}, "tot": {k: line.stats.get(k) for k in cols}})
    return cols, rows


def dynasty_breakdown(d: Any) -> dict[str, Any] | None:
    if d is None or _num(getattr(d, "value", None)) is None:
        return None
    try:
        from ..valuation.dynasty import MODE_WEIGHTS
        weights, terminal = MODE_WEIGHTS.get(d.mode, (None, None))
    except Exception:
        weights, terminal = None, None
    return {"value": d.value, "age": _num(getattr(d, "age", None)), "age_mult": _num(getattr(d, "age_mult", None)),
            "upside": _num(getattr(d, "upside", None)), "pedigree": _num(getattr(d, "pedigree", None)),
            "model": _num(getattr(d, "model_value", None)), "market": _num(getattr(d, "market_value", None)),
            "mode": getattr(d, "mode", None), "horizon": getattr(d, "horizon_years", None),
            "weights": list(weights) if weights else None, "terminal": terminal}


def status_history(res: Any, cid: str) -> list[dict[str, Any]]:
    from datetime import datetime

    out = []
    for snap in (getattr(res, "status_log", None) or {}).get(cid, []):
        seen = getattr(snap, "seen_at", None)
        out.append({"when": datetime.fromtimestamp(seen) if isinstance(seen, (int, float)) else None,
                    "status": getattr(snap, "status", "unknown"), "note": getattr(snap, "note", None)})
    return list(reversed(out))


# --------------------------------------------------------------------------- schedule grid + matchup

def provider_for(loader: Any, league: str) -> Any:
    """The provider object behind a cached pipeline load (``PipelineLoader`` keeps it on its
    BaseLoad), or None (test loaders, other loaders): pages then fall back to calendar weeks and
    projection-only matchups, with a warning."""
    bases = getattr(loader, "_bases", None)
    hit = bases.get(league) if isinstance(bases, dict) else None
    base = hit[1] if isinstance(hit, tuple) and len(hit) == 2 else hit
    return getattr(base, "provider", None)


def _parse_week(text: str | None) -> date | None:
    if not text:
        return None
    try:
        return date.fromisoformat(str(text).strip()[:10])
    except ValueError:
        return None


def schedule_page(res: Any, week: str | None = None, provider: Any = None, limit: int = 10) -> dict[str, Any]:
    """/schedule: the week's team x day grid, streaming targets by slot, the teams with the most
    games, the fantasy-playoff games table and the season-long team x week counts."""
    from ..analysis.schedule_grid import (default_week, monday, playoff_weeks, season_grid, streaming_targets,
                                          week_rows)

    ctx = res.ctx
    day = _parse_week(week)
    start = monday(day) if day else default_week(ctx)
    view = _memo(res, f"sched:week:{start}", lambda: week_rows(ctx, start))
    plan = _memo(res, f"sched:stream:{start}", lambda: streaming_targets(ctx, res.values, start, limit=limit))
    notes: list[str] = []

    def playoffs() -> Any:
        try:
            return playoff_weeks(ctx, provider)
        except Exception as e:  # never let the optional table sink the page
            notes.append(f"Fantasy-playoff table unavailable: {e}")
            return None

    po = _memo(res, "sched:playoffs", playoffs)
    grid = _memo(res, "sched:season", lambda: season_grid(ctx))
    week_idx = grid.week_index(start) if grid.weeks else None
    return {"view": view, "plan": plan, "playoffs": po, "grid": grid, "start": start,
            "prev": start - timedelta(days=7), "next": start + timedelta(days=7),
            "this_week": default_week(ctx), "week_idx": week_idx, "notes": notes,
            "has_schedule": bool(ctx.schedule)}


def matchup_strip(res: Any, provider: Any = None) -> dict[str, Any] | None:
    """The overview's "This week's matchup" strip: both teams' projected totals and my win
    probability (the /matchup preview, memoised with it). None without an opponent or on error."""
    try:
        page = matchup_page(res, provider)
    except Exception:  # the strip is optional
        return None
    m = page["m"]
    if not m.opponent_team:
        return None
    return {"m": m, "pct": page["pct"]}


def matchup_page(res: Any, provider: Any = None) -> dict[str, Any]:
    """/matchup: the current head-to-head preview (memoised per cached result)."""
    from ..analysis.matchup import current_matchup

    def build() -> Any:
        return current_matchup(res.ctx, provider, res.values)

    m = _memo(res, "matchup", build)
    pct = round(m.win_probability * 100) if m.win_probability is not None else None
    top = max([abs(g.gap) for g in m.gap_by_position.values()] + [1.0])
    gaps = [{"group": g.group, "mine": g.mine, "theirs": g.theirs, "gap": g.gap,
             "width": round(min(50.0, abs(g.gap) / top * 50.0), 1), "side": "pos" if g.gap >= 0 else "neg"}
            for g in m.gap_by_position.values()]
    return {"m": m, "pct": pct, "gaps": gaps}
