"""Line / power-play / goalie-start alerts from the Daily Faceoff snapshot (kind "alert").

Inputs are the Player fields filled by ``providers.lines_enrich.enrich_lines``
(``line_change``, ``line``, ``pp_unit``, ``confirmed_start``, ``start_source``). One rec per
player, ``strength`` set directly (the strongest trigger wins):

* PP1 promotion ("PP2 -> PP1", "no PP -> PP1")             -> 7   my players + top free agents
* top-line / top-pair promotion ("F3 -> F1", "D2 -> D1",
  "new to lineup" onto F1)                                 -> 5   my players + top free agents
* confirmed (or likely) start tonight                      -> 6   my goalies; free-agent goalies
                                                                  only against a weak offense
* demotion off PP1 ("PP1 -> PP2" / "PP1 -> no PP"), out of
  the lineup ("scratched"), or my goalie sits tonight
  because the other goalie is confirmed                    -> 4   my players only (sell / bench caution)

Reasons: LINE_CHANGE, PP_UNIT and CONFIRMED_START, each naming the Daily Faceoff source and
time; free-agent streamer alerts add MOVE_BUDGET ("1 of 2 moves left this week") when the league
limits moves. A weak offense is a bottom-``WEAK_OFFENSE_TEAMS`` team by goals per game summed over
the league's skaters (season line once teams have played ``MIN_TEAM_GP`` games, else prior
season) - a heuristic, since the pool only holds rostered players and listed free agents.
"""
from __future__ import annotations

import re
from datetime import datetime
from typing import Any, Iterable, Mapping

from ..matching.normalize import normalize_team
from ..models import LeagueContext, Player, Reason, Recommendation

PP1_PROMOTION = 7.0
TOP_LINE_PROMOTION = 5.0
DEMOTION = 4.0
CONFIRMED_START = 6.0
FA_LIMIT = 5                 # free-agent line promotions reported per run
FA_GOALIE_LIMIT = 3
WEAK_OFFENSE_TEAMS = 10
MIN_TEAMS_FOR_RANK = 16
MIN_TEAM_GP = 10

_GAME_RE = re.compile(r"\bstarts ([A-Z]{2,4})@([A-Z]{2,4})\b")
_STRENGTH_RE = re.compile(r"\bDFO (Confirmed|Likely|Expected)\b", re.I)


def _parts(change: str | None) -> list[str]:
    return [c.strip() for c in (change or "").split(";") if c.strip()]


def classify(p: Player) -> list[tuple[str, float]]:
    """[(trigger, strength)] for one player's ``line_change``."""
    out: list[tuple[str, float]] = []
    for part in _parts(p.line_change):
        up = part.upper()
        if up.endswith("-> PP1") and not up.startswith("PP1"):
            out.append(("pp1_promotion", PP1_PROMOTION))
        elif up.startswith("PP1 ->"):
            out.append(("pp1_demotion", DEMOTION))
        elif re.fullmatch(r"F[234] -> F1|D[234] -> D1", up):
            out.append(("top_line", TOP_LINE_PROMOTION))
        elif part == "new to lineup" and (p.line or "") in ("f1", "d1"):
            out.append(("top_line", TOP_LINE_PROMOTION))
        elif part == "scratched":
            out.append(("scratched", DEMOTION))
    return out


def _team_meta(team: str | None, as_of: Any, team_meta: Mapping[str, Mapping[str, Any]] | None
               ) -> tuple[str | None, str | None]:
    meta: Mapping[str, Any] | None = None
    if team_meta is not None:
        meta = team_meta.get(team or "")
    else:
        try:
            from ..providers.lines_enrich import team_meta_for
            meta = (team_meta_for(as_of) or {}).get(team or "")
        except Exception:
            meta = None
    if not meta:
        return None, None
    when = meta.get("updated_at")
    if isinstance(when, str):
        try:
            when = datetime.fromisoformat(when.replace("Z", "+00:00"))
        except ValueError:
            pass
    when_txt = when.strftime("%b %d %H:%M UTC") if isinstance(when, datetime) else (str(when) if when else None)
    return meta.get("source"), when_txt


def _source_text(team: str | None, as_of: Any, team_meta: Mapping[str, Mapping[str, Any]] | None) -> str:
    src, when = _team_meta(team, as_of, team_meta)
    bits = [b for b in (src, f"updated {when}" if when else None) if b]
    return f"Daily Faceoff ({', '.join(bits)})" if bits else "Daily Faceoff"


def _fa_rank_value(p: Player, values: Mapping[str, Any] | None) -> float:
    pv = (values or {}).get(p.cid)
    for attr in ("fpg_season", "fpg"):
        v = getattr(pv, attr, None) if pv is not None else None
        if isinstance(v, (int, float)):
            return float(v)
    return float(p.pct_owned or 0.0)


def plays_today(ctx: LeagueContext, p: Player) -> bool:
    """False only when the NHL schedule is loaded for his team and has no game on as_of."""
    team = normalize_team(p.team)
    dates = ctx.schedule.get(team or "") if ctx.schedule else None
    return True if not dates else ctx.as_of in dates


def opponent_today(ctx: LeagueContext, p: Player) -> str | None:
    team = normalize_team(p.team)
    opp = (ctx.opponents.get(team or "") or {}).get(ctx.as_of)
    if opp:
        return opp.lstrip("@")
    m = _GAME_RE.search(p.start_source or "")
    if m and team:
        away, home = m.groups()
        return home if team == away else away if team == home else None
    return None


def weak_offenses(ctx: LeagueContext, n: int = WEAK_OFFENSE_TEAMS) -> dict[str, float]:
    """{team: goals per game} for the ``n`` lowest-scoring NHL teams (empty when too few
    teams have data)."""
    skaters = [p for p in ctx.all_players() if not p.is_goalie and normalize_team(p.team)]
    season_gp = max((p.gp("season") for p in skaters), default=0)
    split = "season" if season_gp >= MIN_TEAM_GP else "prior"
    goals: dict[str, float] = {}
    games: dict[str, int] = {}
    for p in skaters:
        line = p.lines.get(split)
        if line is None or line.gp <= 0:
            continue
        t = normalize_team(p.team)
        goals[t] = goals.get(t, 0.0) + float(line.stats.get("G", 0.0))
        games[t] = max(games.get(t, 0), line.gp)
    gpg = {t: goals[t] / games[t] for t in goals if games.get(t)}
    if len(gpg) < MIN_TEAMS_FOR_RANK:
        return {}
    return dict(sorted(gpg.items(), key=lambda kv: kv[1])[:n])


def _rec(p: Player, title: str, strength: float, reasons: list[Reason]) -> Recommendation:
    return Recommendation(kind="alert", score=strength, title=title, subjects=[p], reasons=reasons,
                          strength=strength)


def _line_alert(p: Player, mine: bool, ctx: LeagueContext,
                team_meta: Mapping[str, Mapping[str, Any]] | None) -> Recommendation | None:
    triggers = classify(p)
    if not mine:
        triggers = [t for t in triggers if t[0] in ("pp1_promotion", "top_line")]
    if not triggers:
        return None
    kinds = {t for t, _ in triggers}
    strength = max(s for _, s in triggers)
    src = _source_text(normalize_team(p.team), ctx.as_of, team_meta)
    reasons = [Reason(code="LINE_CHANGE", text=f"{p.name}: {p.line_change} per {src}", value=strength)]
    if p.pp_unit or "pp1_demotion" in kinds:
        unit = p.pp_unit.upper() if p.pp_unit else "no power-play unit"
        reasons.append(Reason(code="PP_UNIT", text=f"{p.name} now on {unit} ({src})",
                              value=float(p.pp_unit[-1]) if p.pp_unit and p.pp_unit[-1].isdigit() else None))
    who = p.name if mine else f"{p.name} (free agent)"
    if "pp1_promotion" in kinds:
        tail = "start him / hold" if mine else "waiver watch"
        title = f"{who} promoted to PP1 ({p.line_change}): {tail}"
    elif "top_line" in kinds:
        tail = "role boost" if mine else "waiver watch"
        title = f"{who} moved up to the top unit ({p.line_change}): {tail}"
    elif "pp1_demotion" in kinds:
        title = f"{who} dropped off PP1 ({p.line_change}): sell / bench caution"
    else:
        title = f"{who} out of the lineup ({p.line_change}): bench caution"
    return _rec(p, title, strength, reasons)


def _budget_reason(ctx: LeagueContext) -> Reason | None:
    """MOVE_BUDGET note for a streamer pickup ("1 of 2 moves left this week", or "No moves left
    this week (2/2 used); ..."); None when moves are unlimited."""
    from .base import moves_budget_reason, moves_left, no_moves_text

    if moves_left(ctx) == 0:
        return Reason(code="MOVE_BUDGET", text=no_moves_text(ctx), value=0.0)
    return moves_budget_reason(ctx)


def _strength_word(p: Player) -> str:
    m = _STRENGTH_RE.search(p.start_source or "")
    return m.group(1).lower() if m else "confirmed"


def _start_alert(p: Player, ctx: LeagueContext, mine: bool, weak: Mapping[str, float]) -> Recommendation | None:
    if p.confirmed_start is None or not plays_today(ctx, p):
        return None
    word = _strength_word(p)
    opp = opponent_today(ctx, p)
    vs = f" vs {opp}" if opp else ""
    src = p.start_source or "Daily Faceoff"
    if p.confirmed_start:
        if mine:
            title = f"{p.name} {word} to start tonight{vs}: start him"
        else:
            if not opp or opp not in weak:
                return None
            title = f"{p.name} (free agent) {word} to start tonight{vs} (weak offense): streamer"
        reasons = [Reason(code="CONFIRMED_START", text=src, value=1.0)]
        if not mine and opp in weak:
            gpg = weak[opp]
            txt = f"{opp} scores {gpg:.2f} goals per game (bottom-{len(weak)} offense)" if gpg else                 f"{opp} is a weak offense"
            reasons.append(Reason(code="OPPONENT", text=txt, value=gpg or None))
        if not mine and ctx.position_limits:
            from .base import cap_text, capped_positions
            try:
                capped = capped_positions(ctx.my_team, ctx, p)
            except LookupError:
                capped = []
            if capped:
                reasons.append(Reason(code="POSITION_CAP",
                                      text=", ".join(cap_text(ctx, k) for k in capped)
                                           + f" reached: streaming him needs a {'/'.join(capped)} drop"))
        if not mine:
            budget = _budget_reason(ctx)
            if budget is not None:
                reasons.append(budget)
        return _rec(p, title, CONFIRMED_START, reasons)
    if mine and word == "confirmed":
        return _rec(p, f"{p.name} not starting tonight{vs}: bench caution", DEMOTION,
                    [Reason(code="CONFIRMED_START", text=src, value=0.0)])
    return None


def recommend_line_alerts(ctx: LeagueContext, values: Mapping[str, Any] | None = None, *,
                          team_meta: Mapping[str, Mapping[str, Any]] | None = None,
                          weak_teams: Mapping[str, float] | Iterable[str] | None = None,
                          fa_limit: int = FA_LIMIT, fa_goalie_limit: int = FA_GOALIE_LIMIT,
                          **_: Any) -> list[Recommendation]:
    """Line / unit / goalie-start alerts, strongest first. ``team_meta`` ({team: {"source",
    "updated_at"}}) defaults to the last ``enrich_lines`` run for ``ctx.as_of``;
    ``weak_teams`` overrides :func:`weak_offenses`."""
    try:
        mine_team = ctx.my_team
    except LookupError:
        return []
    mine = list({p.cid: p for p in mine_team.players}.values())
    mine_ids = {p.cid for p in mine}
    fas = [p for p in ctx.free_agents if p.cid not in mine_ids]
    recs: list[Recommendation] = []

    for p in mine:
        r = _line_alert(p, True, ctx, team_meta)
        if r is not None:
            recs.append(r)
    fa_line = [r for r in (_line_alert(p, False, ctx, team_meta) for p in fas if p.line_change) if r is not None]
    fa_line.sort(key=lambda r: (-(r.strength or 0), -_fa_rank_value(r.subjects[0], values)))
    recs.extend(fa_line[:fa_limit])

    if weak_teams is None:
        weak: Mapping[str, float] = weak_offenses(ctx) if any(p.confirmed_start for p in fas if p.is_goalie) else {}
    elif isinstance(weak_teams, Mapping):
        weak = weak_teams
    else:
        weak = {normalize_team(t) or t: 0.0 for t in weak_teams}
    seen = {r.subjects[0].cid for r in recs}
    for p in mine:
        if p.is_goalie and p.cid not in seen:
            r = _start_alert(p, ctx, True, weak)
            if r is not None:
                recs.append(r)
    fa_starts = [r for r in (_start_alert(p, ctx, False, weak) for p in fas if p.is_goalie) if r is not None]
    fa_starts.sort(key=lambda r: -_fa_rank_value(r.subjects[0], values))
    recs.extend(fa_starts[:fa_goalie_limit])

    recs.sort(key=lambda r: (-(r.strength or 0.0), r.subjects[0].cid not in mine_ids, r.title))
    for i, r in enumerate(recs, 1):
        r.rank_in_kind, r.kind_total = i, len(recs)
    return recs


__all__ = ["recommend_line_alerts", "classify", "weak_offenses", "opponent_today", "plays_today"]
