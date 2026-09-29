"""Head-to-head matchup preview: score so far, projected remaining points, win probability and
advice for the current matchup period.

* Scores so far - ESPN: the league's ``mMatchupScore`` schedule for ``currentMatchupPeriod``
  (``totalPointsLive``, else ``totalPoints``; the same request ``League.scoreboard()`` makes, which
  is the fallback); Fantrax: ``getStandings`` view SCHEDULE (one table per scoring period with
  both teams' FPts). Both best effort: failures become warnings and the preview still projects.
* Projections - each starter's ``PlayerValue.proj_week`` apportioned to the days left by games:
  ``proj_week * games_left / games_next7`` (per-game value x games when the week projection has no
  games). Daily-lineup leagues (ESPN, and Fantrax leagues whose Rules page says lineup changes
  are executed daily) use each team's optimal week lineup; weekly-lock Fantrax leagues use the
  current starters (``providers.fantrax.lineup_lock_for``, the single parsed value).
* Win probability - Monte Carlo (2,000 draws): each starter's remaining total is normal with the
  apportioned mean and per-game SD ``1.3 * sqrt(FPG)`` for skaters, ``2.5 * sqrt(FPG)`` for goalies
  (a start is win-or-bust), added to the points already banked.
* Advice - "chase" when behind (stream extra games, take on variance), "protect" when ahead
  (safe floors, confirmed goalie starts), "even" otherwise.
"""
from __future__ import annotations

import math
import random
from datetime import date, datetime, timedelta
from typing import Any, Iterable, Literal, Mapping

from pydantic import BaseModel, Field

from ..models import FantasyTeam, LeagueContext, Player
from .schedule_grid import (UNAVAILABLE, PeriodCalendar, _group, espn_league, league_calendar, per_day,
                            per_game_value, streaming_targets, window_counts)

DRAWS = 2000
SKATER_SD_K = 1.3
GOALIE_SD_K = 2.5
CHASE_BELOW = 0.40
PROTECT_ABOVE = 0.60
HOT_RATIO = 1.3
HOT_MIN_GP = 3
KEY_PLAYERS = 6
NON_STARTING = ("BN", "IR", "MIN", "TAXI")
ALERT = ("dtd", "out", "ir", "ltir", "suspended")

Stance = Literal["chase", "protect", "even"]


class ScoreInfo(BaseModel):
    """Where the matchup stands according to the provider."""
    period: int | None = None
    start: date | None = None
    end: date | None = None
    opponent_id: str | None = None
    my_points: float = 0.0
    their_points: float = 0.0
    playoffs: bool = False
    source: str = ""


class PlayerProjection(BaseModel):
    cid: str
    name: str
    team: str | None
    pos: str
    group: str
    slot: str | None = None
    status: str = "healthy"
    note: str | None = None
    games_left: int = 0
    proj_remaining: float = 0.0
    sd: float = 0.0
    fpg: float | None = None


class KeyPlayer(BaseModel):
    cid: str
    name: str
    team: str | None
    pos: str
    tag: Literal["injured", "hot", "top"]
    status: str
    note: str | None = None
    games_left: int = 0
    proj_remaining: float = 0.0
    fpg: float | None = None
    recent_fpg: float | None = None


class PositionGap(BaseModel):
    group: str
    mine: float
    theirs: float

    @property
    def gap(self) -> float:
        return self.mine - self.theirs


class Matchup(BaseModel):
    league: str
    my_team: str
    opponent_team: str | None = None
    opponent_id: str | None = None
    period: int | None = None
    start: date | None = None
    end: date | None = None
    from_day: date | None = None
    days_left: int = 0
    playoffs: bool = False
    my_points_so_far: float = 0.0
    their_points_so_far: float = 0.0
    my_projected_remaining: float = 0.0
    their_projected_remaining: float = 0.0
    my_games_left: int = 0
    their_games_left: int = 0
    my_sd: float = 0.0
    their_sd: float = 0.0
    win_probability: float | None = None
    stance: Stance | None = None
    key_players_theirs: list[KeyPlayer] = Field(default_factory=list)
    gap_by_position: dict[str, PositionGap] = Field(default_factory=dict)
    advice: list[str] = Field(default_factory=list)
    my_players: list[PlayerProjection] = Field(default_factory=list)
    their_players: list[PlayerProjection] = Field(default_factory=list)
    source: str = ""
    lineup_basis: str = ""
    warnings: list[str] = Field(default_factory=list)
    draws: int = DRAWS

    @property
    def my_projected_total(self) -> float:
        return self.my_points_so_far + self.my_projected_remaining

    @property
    def their_projected_total(self) -> float:
        return self.their_points_so_far + self.their_projected_remaining

    @property
    def margin(self) -> float:
        return self.my_projected_total - self.their_projected_total

    def to_json(self) -> dict[str, Any]:
        d = self.model_dump(mode="json")
        d.update(my_projected_total=self.my_projected_total, their_projected_total=self.their_projected_total,
                 margin=self.margin)
        for k, g in self.gap_by_position.items():
            d["gap_by_position"][k]["gap"] = g.gap
        return d


# --------------------------------------------------------------------------- projections

def apportion(pv: Any, games_left: int) -> float:
    """A player's projected points over ``games_left`` games: the week projection apportioned by
    games (``proj_week * games_left / games_next7``), else per-game value x games."""
    if pv is None or games_left <= 0:
        return 0.0
    proj, g7 = getattr(pv, "proj_week", None), getattr(pv, "games_next7", None)
    if isinstance(proj, (int, float)) and isinstance(g7, int) and g7 > 0:
        return float(proj) * games_left / g7
    pg = per_game_value(pv)
    return (pg or 0.0) * games_left


def game_sd(per_game: float, goalie: bool) -> float:
    """Per-game SD of fantasy points: 1.3 x sqrt(FPG) skaters, 2.5 x sqrt(FPG) goalies."""
    return (GOALIE_SD_K if goalie else SKATER_SD_K) * math.sqrt(max(per_game, 0.0))


def _pos(p: Player) -> str:
    return "/".join(x for x in p.positions if x != "F") or "/".join(p.positions) or "-"


def lineup_players(team: FantasyTeam, ctx: LeagueContext, values: Mapping[str, Any],
                   locked: bool) -> tuple[list[tuple[Player, str | None]], str]:
    """(starters with their slot, basis): the current starters when lineups are locked for the
    period, else the optimal week lineup (falls back to the current starters)."""
    current = [(s.player, s.slot) for s in team.slots
               if s.player is not None and s.starting and s.slot not in NON_STARTING]
    if locked:
        return current, "current lineup (locked for the period)"
    try:
        from ..recommend.lineup import optimal_lineup, starting_slots

        slots = starting_slots(ctx.roster_shape)
        assign, _ = optimal_lineup(team, values, ctx.roster_shape, "week")
        by_cid = {p.cid: p for p in team.players}
        chosen = [(by_cid[cid], slots[i]) for i, cid in assign.items() if cid in by_cid]
        if chosen:
            return chosen, "optimal week lineup"
    except Exception:  # noqa: BLE001 - fall back to the lineup as set
        pass
    return current, "current lineup"


def project_team(ctx: LeagueContext, team: FantasyTeam, values: Mapping[str, Any], start: date, end: date,
                 locked: bool) -> tuple[list[PlayerProjection], str]:
    """Each starter's games, projected points and SD over [start, end]."""
    gpd = per_day(ctx)
    starters, basis = lineup_players(team, ctx, values, locked)
    out = []
    for p, slot in starters:
        games = 0
        if start <= end and p.team and p.team in ctx.schedule:
            games = window_counts(ctx.schedule[p.team], gpd, start, end)[0]
        pv = values.get(p.cid)
        proj = apportion(pv, games)
        pg = proj / games if games else 0.0
        out.append(PlayerProjection(
            cid=p.cid, name=p.name, team=p.team, pos=_pos(p), group=_group(p), slot=slot, status=p.status,
            note=p.status_note, games_left=games, proj_remaining=proj,
            sd=math.sqrt(games) * game_sd(pg, p.is_goalie) if games else 0.0,
            fpg=getattr(pv, "fpg", None)))
    out.sort(key=lambda x: -x.proj_remaining)
    return out, basis


def team_sd(players: Iterable[PlayerProjection]) -> float:
    return math.sqrt(sum(p.sd ** 2 for p in players))


def win_probability(my_so_far: float, their_so_far: float, mine: Iterable[tuple[float, float]],
                    theirs: Iterable[tuple[float, float]], draws: int = DRAWS, seed: int | None = None) -> float:
    """Monte Carlo P(my final > their final); ties count half. ``mine`` / ``theirs`` are
    (mean, sd) of each starter's remaining points."""
    mine, theirs = list(mine), list(theirs)
    if not any(sd > 0 for _, sd in mine + theirs):
        a = my_so_far + sum(m for m, _ in mine)
        b = their_so_far + sum(m for m, _ in theirs)
        return 1.0 if a > b + 1e-9 else (0.0 if b > a + 1e-9 else 0.5)
    rng = random.Random(seed)
    wins = 0.0
    for _ in range(max(1, draws)):
        a = my_so_far + sum(rng.gauss(m, sd) if sd > 0 else m for m, sd in mine)
        b = their_so_far + sum(rng.gauss(m, sd) if sd > 0 else m for m, sd in theirs)
        wins += 1.0 if a > b else (0.5 if a == b else 0.0)
    return wins / max(1, draws)


# --------------------------------------------------------------------------- key players

def _recent_fpg(ctx: LeagueContext, p: Player) -> tuple[float | None, int]:
    if ctx.scoring.kind != "points":
        return None, 0
    try:
        from ..scoring import PointsScoring

        sc = PointsScoring(ctx.scoring.weights, ctx.scoring.goalie_weights or None)
    except Exception:  # noqa: BLE001
        return None, 0
    for split in ("last15", "last7", "last30"):
        line = p.lines.get(split)
        if line is not None and line.gp >= HOT_MIN_GP:
            try:
                return float(sc.value(line.per_game())), line.gp
            except Exception:  # noqa: BLE001
                return None, 0
    return None, 0


def key_players(ctx: LeagueContext, team: FantasyTeam, projections: list[PlayerProjection],
                values: Mapping[str, Any], limit: int = KEY_PLAYERS) -> list[KeyPlayer]:
    """Their injured / suspended players (most valuable first), hot starters (recent FPG >= 1.3x
    the model's) and top projected starters."""
    proj = {x.cid: x for x in projections}
    out: dict[str, KeyPlayer] = {}

    def add(p: Player, tag: str, recent: float | None = None) -> None:
        if p.cid in out:
            return
        x = proj.get(p.cid)
        pv = values.get(p.cid)
        out[p.cid] = KeyPlayer(cid=p.cid, name=p.name, team=p.team, pos=_pos(p), tag=tag, status=p.status,  # type: ignore[arg-type]
                               note=p.status_note, games_left=x.games_left if x else 0,
                               proj_remaining=x.proj_remaining if x else 0.0, fpg=getattr(pv, "fpg", None),
                               recent_fpg=recent)

    hurt = sorted((p for p in team.players if p.status in ALERT),
                  key=lambda p: -(getattr(values.get(p.cid), "fpg", None) or 0.0))
    for p in hurt:
        add(p, "injured")
    by_cid = {p.cid: p for p in team.players}
    for x in projections:
        p = by_cid.get(x.cid)
        base = getattr(values.get(x.cid), "fpg", None)
        if p is None or not base or base <= 0:
            continue
        recent, _ = _recent_fpg(ctx, p)
        if recent is not None and recent >= HOT_RATIO * base:
            add(p, "hot", recent)
    for x in projections[:3]:
        if (p := by_cid.get(x.cid)) is not None and x.proj_remaining > 0:
            add(p, "top")
    return list(out.values())[:limit]


# --------------------------------------------------------------------------- provider scores

def _num(v: Any) -> float | None:
    if isinstance(v, bool):
        return None
    if isinstance(v, (int, float)):
        return float(v)
    try:
        return float(str(v).replace(",", ""))
    except (TypeError, ValueError):
        return None


def espn_scores(ctx: LeagueContext, provider: Any, calendar: PeriodCalendar) -> ScoreInfo | None:
    """Current matchup period, opponent and live points from ESPN."""
    league = espn_league(provider)
    mp = int(getattr(league, "currentMatchupPeriod", 0) or 0) or None
    my_id = int(ctx.my_team.team_id)
    found = None
    try:
        data = league.espn_request.league_get(params={"view": "mMatchupScore"}) or {}
        for m in data.get("schedule") or []:
            if m.get("matchupPeriodId") != mp:
                continue
            home, away = m.get("home") or {}, m.get("away") or {}
            if my_id not in (home.get("teamId"), away.get("teamId")):
                continue
            me, them = (home, away) if home.get("teamId") == my_id else (away, home)

            def pts(side: Mapping[str, Any]) -> float:
                live = _num(side.get("totalPointsLive"))
                return live if live is not None else (_num(side.get("totalPoints")) or 0.0)

            found = ScoreInfo(period=mp, opponent_id=str(them["teamId"]) if them.get("teamId") is not None else None,
                              my_points=pts(me), their_points=pts(them) if them else 0.0,
                              playoffs=str(m.get("playoffTierType") or "NONE") != "NONE", source="ESPN scoreboard")
            break
    except Exception:  # noqa: BLE001 - fall back to League.scoreboard()
        found = None
    if found is None:
        for m in league.scoreboard(mp) or []:
            h, a = getattr(m, "home_team", None), getattr(m, "away_team", None)
            hid, aid = getattr(h, "team_id", h), getattr(a, "team_id", a)
            if my_id not in (hid, aid):
                continue
            mine_home = hid == my_id
            found = ScoreInfo(period=mp, opponent_id=str(aid if mine_home else hid) if (aid if mine_home else hid) else None,
                              my_points=_num(getattr(m, "home_final_score" if mine_home else "away_final_score", 0)) or 0.0,
                              their_points=_num(getattr(m, "away_final_score" if mine_home else "home_final_score", 0)) or 0.0,
                              source="ESPN scoreboard")
            break
    if found is None:
        return None
    per = calendar.get(mp) if mp else None
    if per is not None:
        found.start, found.end = per.start, per.end
        found.playoffs = found.playoffs or per.playoffs
    return found


def parse_fantrax_schedule(data: Mapping[str, Any]) -> list[dict[str, Any]]:
    """getStandings view SCHEDULE -> [{number, start, end, games: [(away_id, away_pts, home_id,
    home_pts)]}] (one per scoring-period table)."""
    import re

    out = []
    for t in (data or {}).get("tableList") or []:
        cap = str(t.get("caption") or "")
        m = re.search(r"(\d+)\s*$", cap)
        number = int(m.group(1)) if m else None
        start = end = None
        sub = str(t.get("subCaption") or "").strip("() ")
        if " - " in sub:
            a, b = sub.split(" - ", 1)
            try:
                start = datetime.strptime(a.strip(), "%a %b %d, %Y").date()
                end = datetime.strptime(b.strip(), "%a %b %d, %Y").date()
            except ValueError:
                pass
        games = []
        for row in t.get("rows") or []:
            cells = row.get("cells") or []
            if len(cells) < 4:
                continue
            games.append((cells[0].get("teamId"), _num(cells[1].get("content")) or 0.0,
                          cells[2].get("teamId"), _num(cells[3].get("content")) or 0.0))
        out.append({"number": number, "start": start, "end": end, "playoffs": cap.lower().startswith("playoff"),
                    "games": games})
    return out


def fantrax_scores(ctx: LeagueContext, provider: Any, calendar: PeriodCalendar, today: date) -> ScoreInfo | None:
    """This scoring period's opponent and FPts from Fantrax's schedule view."""
    client = provider._get_client()
    data = client.call(("getStandings", {"view": "SCHEDULE"}))[0]
    tables = parse_fantrax_schedule(data)
    per = calendar.current(today) if calendar.source.startswith("Fantrax") else None
    table = None
    if per is not None:
        table = next((t for t in tables if t["number"] == per.number), None)
    if table is None:
        dated = [t for t in tables if t["start"] and t["end"]]
        table = next((t for t in dated if t["start"] <= today <= t["end"]), None) or \
            next((t for t in sorted(dated, key=lambda t: t["start"]) if t["start"] > today), None)
    if table is None:
        return None
    my_id = ctx.my_team.team_id
    for away, apts, home, hpts in table["games"]:
        if my_id not in (away, home):
            continue
        mine_away = away == my_id
        return ScoreInfo(period=table["number"], start=table["start"] or (per.start if per else None),
                         end=table["end"] or (per.end if per else None),
                         opponent_id=str(home if mine_away else away) if (home if mine_away else away) else None,
                         my_points=apts if mine_away else hpts, their_points=hpts if mine_away else apts,
                         playoffs=table["playoffs"] or bool(per and per.playoffs), source="Fantrax schedule")
    return None


def fetch_scores(ctx: LeagueContext, provider: Any, calendar: PeriodCalendar, today: date,
                 warnings: list[str]) -> ScoreInfo | None:
    if provider is None:
        warnings.append("Live matchup scores unavailable: no provider handle")
        return None
    try:
        if ctx.provider == "espn":
            return espn_scores(ctx, provider, calendar)
        if ctx.provider == "fantrax":
            return fantrax_scores(ctx, provider, calendar, today)
        warnings.append(f"Live matchup scores are not supported for provider {ctx.provider!r}")
    except Exception as e:  # noqa: BLE001 - best effort
        warnings.append(f"Live matchup scores unavailable ({type(e).__name__}: {e})")
    return None


# --------------------------------------------------------------------------- advice

def stance_for(p: float | None) -> Stance | None:
    if p is None:
        return None
    return "chase" if p < CHASE_BELOW else ("protect" if p > PROTECT_ABOVE else "even")


def build_advice(m: Matchup, ctx: LeagueContext, values: Mapping[str, Any], locked: bool,
                 stream_names: list[str] | None = None, stream_teams: list[str] | None = None) -> list[str]:
    out: list[str] = []
    p = m.win_probability
    if p is None or m.opponent_team is None:
        return ["No opponent found for this period; the projection covers your lineup only."]
    lead = f"{m.my_projected_total:.1f} vs {m.their_projected_total:.1f} projected ({p:.0%} to win)"
    if m.days_left <= 0:
        out.append(f"The period is over: {m.my_points_so_far:.1f} - {m.their_points_so_far:.1f}.")
        return out
    extra = ""
    if stream_names:
        extra = " Best adds for the days left: " + ", ".join(stream_names) + "."
    teams = f" Teams with the most games left: {', '.join(stream_teams)}." if stream_teams else ""
    if m.stance == "chase" and locked:
        out.append(f"Chase: you trail, {lead}. Your lineup is set for this period, so the gap closes only with "
                   "the players you started; line up extra games (and goalie starts) for the next period.")
    elif m.stance == "chase":
        out.append(f"Chase: you trail, {lead}. Add games - stream free agents who play the most remaining "
                   f"nights (off-nights first) and start every goalie with a game; you need variance.{extra}{teams}")
    elif m.stance == "protect":
        out.append(f"Protect: you lead, {lead}. Take the safe floor - start confirmed goalies only, keep steady "
                   "starters over one-game streamers and avoid risky adds.")
    else:
        out.append(f"Even: {lead}. Maximize games played - fill every open slot on game nights and favour "
                   f"off-night starts.{extra}{teams}")
    hurt = [x for x in m.my_players if x.status in UNAVAILABLE]
    if hurt:
        out.append("In your lineup but unavailable: " + ", ".join(f"{x.name} ({x.status.upper()})" for x in hurt)
                   + " - replace them" + (" when lineups unlock." if locked else "."))
    idle = [x for x in m.my_players if x.games_left == 0 and x.status not in UNAVAILABLE]
    if idle and not locked:
        out.append("No games left for " + ", ".join(x.name for x in idle[:4]) + ": swap in players who still play.")
    worst = min(m.gap_by_position.values(), key=lambda g: g.gap, default=None)
    if worst is not None and worst.gap < -1.0:
        out.append(f"Biggest gap: {worst.group} ({worst.gap:+.1f} projected) - target that position when streaming.")
    out_theirs = [k for k in m.key_players_theirs if k.tag == "injured"]
    if out_theirs:
        out.append("Their unavailable players: " + ", ".join(f"{k.name} ({k.status.upper()})" for k in out_theirs[:3])
                   + ".")
    if locked:
        out.append("Lineups lock for the scoring period: adds and swaps count from the next period.")
    return out


def lineups_locked(ctx: LeagueContext, provider: Any = None) -> bool:
    """Whether lineups are locked for the whole period: ``providers.fantrax.lineup_lock_for`` is
    "weekly" (a Fantrax Rules page saying weekly, or saying nothing). ESPN lineups are daily."""
    from ..providers.fantrax import lineup_lock_for

    return lineup_lock_for(ctx, provider) == "weekly"


# --------------------------------------------------------------------------- entry point

def _values(ctx: LeagueContext) -> Mapping[str, Any]:
    from ..scoring import fit_to_context, from_config
    from ..valuation.valuate import valuate_league

    return valuate_league(ctx, fit_to_context(from_config(ctx.scoring), ctx))


def current_matchup(ctx: LeagueContext, provider: Any = None, values: Mapping[str, Any] | None = None, *,
                    today: date | None = None, draws: int = DRAWS, seed: int | None = None,
                    scores: ScoreInfo | None = None, calendar: PeriodCalendar | None = None,
                    streaming: bool = True) -> Matchup:
    """The current matchup preview for my team. ``scores`` (tests) skips the provider request;
    ``calendar`` defaults to ``league_calendar(ctx, provider)``."""
    today = today or ctx.as_of
    values = values if values is not None else _values(ctx)
    warnings: list[str] = []
    cal = calendar or league_calendar(ctx, provider)
    if scores is None:
        scores = fetch_scores(ctx, provider, cal, today, warnings)
    per = cal.get(scores.period) if scores is not None and scores.period else None
    per = per or cal.current(today)
    start = (scores.start if scores and scores.start else None) or (per.start if per else today)
    end = (scores.end if scores and scores.end else None) or (per.end if per else today + timedelta(days=6))
    from_day = max(today, start)
    days_left = max(0, (end - from_day).days + 1)
    locked = lineups_locked(ctx, provider)
    me = ctx.my_team
    opp = None
    if scores is not None and scores.opponent_id is not None:
        opp = next((t for t in ctx.teams if str(t.team_id) == str(scores.opponent_id)), None)
        if opp is None:
            warnings.append(f"Opponent team {scores.opponent_id} is not in the loaded league")
    mine, basis = project_team(ctx, me, values, from_day, end, locked)
    theirs, _ = project_team(ctx, opp, values, from_day, end, locked) if opp is not None else ([], basis)
    m = Matchup(league=ctx.provider, my_team=me.name, opponent_team=opp.name if opp else None,
                opponent_id=opp.team_id if opp else None, period=scores.period if scores else (per.number if per else None),
                start=start, end=end, from_day=from_day, days_left=days_left,
                playoffs=bool((scores and scores.playoffs) or (per and per.playoffs)),
                my_points_so_far=scores.my_points if scores else 0.0,
                their_points_so_far=scores.their_points if scores else 0.0,
                my_projected_remaining=sum(x.proj_remaining for x in mine),
                their_projected_remaining=sum(x.proj_remaining for x in theirs),
                my_games_left=sum(x.games_left for x in mine), their_games_left=sum(x.games_left for x in theirs),
                my_sd=team_sd(mine), their_sd=team_sd(theirs), my_players=mine, their_players=theirs,
                source=(scores.source if scores else "projection only"), lineup_basis=basis, draws=draws,
                warnings=warnings + list(cal.notes if not cal.source.startswith(("ESPN", "Fantrax")) else []))
    if opp is not None:
        m.win_probability = win_probability(m.my_points_so_far, m.their_points_so_far,
                                            [(x.proj_remaining, x.sd) for x in mine],
                                            [(x.proj_remaining, x.sd) for x in theirs], draws=draws, seed=seed)
        m.stance = stance_for(m.win_probability)
        m.key_players_theirs = key_players(ctx, opp, theirs, values)
        for g in ("F", "D", "G"):
            a = sum(x.proj_remaining for x in mine if x.group == g)
            b = sum(x.proj_remaining for x in theirs if x.group == g)
            if a or b or any(x.group == g for x in mine + theirs):
                m.gap_by_position[g] = PositionGap(group=g, mine=a, theirs=b)
    names: list[str] = []
    teams: list[str] = []
    if streaming and not locked and m.stance in ("chase", "even") and days_left > 0 and ctx.schedule:
        try:
            plan = streaming_targets(ctx, values, start, limit=3, today=from_day, end=end, team_limit=3)
            best = sorted((t for ts in plan.by_slot.values() for t in ts), key=lambda t: -t.proj)[:3]
            names = [f"{t.name} ({t.team}, {t.games} GP, {t.proj:.1f} pts"
                     + (f"; {t.needs_drop}" if t.needs_drop else "") + ")" for t in best]
            teams = [f"{r.team} {r.games}" for r in plan.teams if r.games > 0]
        except Exception as e:  # noqa: BLE001
            m.warnings.append(f"Streaming targets unavailable: {e}")
    m.advice = build_advice(m, ctx, values, locked, names, teams)
    return m
