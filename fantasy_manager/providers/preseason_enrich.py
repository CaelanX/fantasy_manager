"""NHL preseason lines (gameType 1) for unproven skaters: a weak signal for players with little or
no NHL regular-season history (rookies, prospects, call-ups).

Source. The NHL stats REST reports and the player game logs carry no preseason games (checked
live 2026-09-29, see ``providers.nhl``), so :func:`enrich_preseason` reads every finished
preseason game once: the 32 club schedules (already cached by the enrich schedule step) list the
games, then one ``gamecenter/{id}/boxscore`` (G / A / PTS / +/- / PIM / hits / blocks / PPG / SOG
/ TOI) and one ``gamecenter/{id}/landing`` (goal strength, for PP / SH assists) per game. A game
finished more than two days ago is cached 30 days (final box scores do not change), a more recent
one 12 hours. Only games before ``ctx.as_of`` are read, so nothing is fetched mid-game. No
preseason PP ice time / PP share exists anywhere; the PP unit comes from Daily Faceoff
(``Player.pp_unit``) when the lines step ran.

Unproven (:func:`is_unproven`): a skater with a known ``career_gp`` < 82 (NHL regular season,
from the pedigree step), or without any prior-season line (N-1..N-3) of at least 20 GP. Goalies
are skipped.

Where the line lives. ``models.Split`` is a Literal without "preseason" (and models.py /
LeagueContext are not extended here), so ``StatLine(split="preseason")`` fails validation and
neither Player nor LeagueContext accepts extra attributes. The lines are therefore kept in a
module-level registry keyed by (NHL season id, NHL player id), like ``lines_enrich``'s team meta;
:func:`preseason_line` / :func:`preseason_lines` read it back. Once ``Split`` gains "preseason",
:func:`attach` also stores the line on ``Player.lines["preseason"]`` (it tries on every run).

Consumers: ``valuation.valuate`` (reason PRESEASON: the preseason per-game rates are blended into
the baseline with weight ``min(0.25, GP / 12)``, fading out over the first 20 regular-season
games; see ``valuation.blend.preseason_weight``), ``recommend.flags.recommend_preseason_alerts``
("Preseason standout" alerts) and the player page (a Preseason row in the stat splits).
"""
from __future__ import annotations

import re
from dataclasses import dataclass, field
from datetime import date, timedelta
from typing import Any, Callable, Iterable, Mapping

from pydantic import ValidationError

from ..models import LeagueContext, Player, StatLine
from .nhl import NHL_TEAMS, WEB_BASE, NhlClient, NhlSkaterGameLine, current_season

HOUR = 3600.0
TTL_RECENT_GAME = 12 * HOUR          # games of the last two days (late stat corrections)
TTL_FINAL_GAME = 30 * 24 * HOUR      # older final box scores never change
RECENT_DAYS = 2
TTL_SCHEDULE = 7 * 24 * HOUR         # same as providers.enrich (the club schedules are shared)

UNPROVEN_CAREER_GP = 82              # fewer career NHL regular-season games = unproven
PROVEN_SEASON_GP = 20                # a prior season with this many GP = proven (unless career < 82)
PRIOR_SPLITS = ("prior", "prior2", "prior3")
# Read preseason games until this many days after opening night (the valuation blend fades out
# over the first 20 regular-season games, about 40 days).
WINDOW_DAYS = 45
SPLIT = "preseason"

_GAMECENTER_RE = re.compile(r"/gamecenter/(\d+)/")

# (season id, NHL player id) -> PreseasonLine
_REGISTRY: dict[tuple[int, int], "PreseasonLine"] = {}


@dataclass
class PreseasonLine:
    """One skater's preseason totals (canonical stat keys, ``GP`` included)."""
    nhl_id: int
    season: int
    gp: int
    stats: dict[str, float]
    toi_seconds: float = 0.0
    toi_games: int = 0
    has_strength: bool = False       # PPA / PPP present (every game's scoring summary parsed)
    team: str | None = None
    last_date: date | None = None
    games: list[date] = field(default_factory=list)

    @property
    def toi_per_game(self) -> float | None:
        """Minutes per game (games with a TOI value only)."""
        return self.toi_seconds / self.toi_games / 60.0 if self.toi_games else None

    @property
    def pts_per_game(self) -> float:
        return float(self.stats.get("PTS", 0.0)) / self.gp if self.gp else 0.0

    def per_game(self) -> dict[str, float]:
        if self.gp <= 0:
            return {}
        return {k: float(v) / self.gp for k, v in self.stats.items() if k != "GP"}

    def summary(self) -> str:
        """'4 GP: 5 G, 3 A, 8 PTS, 12 SOG, 2 PPP, 16.4 TOI'."""
        s = self.stats
        parts = [f"{s.get('G', 0):.0f} G", f"{s.get('A', 0):.0f} A", f"{s.get('PTS', 0):.0f} PTS"]
        if "SOG" in s:
            parts.append(f"{s['SOG']:.0f} SOG")
        if "PPP" in s:
            parts.append(f"{s['PPP']:.0f} PPP")
        elif "PPG" in s:
            parts.append(f"{s['PPG']:.0f} PPG")
        toi = self.toi_per_game
        if toi is not None:
            parts.append(f"{toi:.1f} TOI")
        return f"{self.gp} GP: " + ", ".join(parts)


# --------------------------------------------------------------------------- pure helpers

def is_unproven(p: Player) -> bool:
    """Career NHL GP known and < 82, or no prior season (N-1..N-3) with >= 20 GP."""
    if p.career_gp is not None and p.career_gp < UNPROVEN_CAREER_GP:
        return True
    return not any(p.gp(s) >= PROVEN_SEASON_GP for s in PRIOR_SPLITS)


def aggregate(lines: Iterable[NhlSkaterGameLine], season: int) -> dict[int, PreseasonLine]:
    """Sum per-game lines per player (one line per game id). PP / SH assists and points are kept
    only when every game of the player had its scoring summary (else PPP would be undercounted);
    PPG / SHG come from the box score either way."""
    by_player: dict[int, dict[int, NhlSkaterGameLine]] = {}
    for ln in lines:
        by_player.setdefault(ln.player_id, {})[ln.game_id] = ln
    out: dict[int, PreseasonLine] = {}
    for pid, games in by_player.items():
        gl = sorted(games.values(), key=lambda g: (g.date, g.game_id))
        tot: dict[str, float] = {}
        toi = 0.0
        toi_n = 0
        for g in gl:
            for k, v in g.stats.items():
                tot[k] = tot.get(k, 0.0) + float(v)
            if g.toi is not None and g.toi > 0:
                toi += g.toi
                toi_n += 1
        strength = all(g.has_strength for g in gl)
        if not strength:
            for k in ("PPA", "PPP", "SHA", "SHP"):
                tot.pop(k, None)
        tot["GP"] = float(len(gl))
        out[pid] = PreseasonLine(nhl_id=pid, season=season, gp=len(gl), stats=tot, toi_seconds=toi,
                                 toi_games=toi_n, has_strength=strength, team=gl[-1].team,
                                 last_date=gl[-1].date, games=[g.date for g in gl])
    return out


def statline(line: PreseasonLine) -> StatLine | None:
    """``StatLine(split="preseason")`` when ``models.Split`` accepts it (None today)."""
    try:
        return StatLine(split=SPLIT, gp=line.gp, stats=dict(line.stats))
    except ValidationError:
        return None


def attach(p: Player, line: PreseasonLine) -> bool:
    """Put the line on ``p.lines["preseason"]`` if the model allows it; True when stored."""
    sl = statline(line)
    if sl is None:
        return False
    p.lines[SPLIT] = sl
    return True


# --------------------------------------------------------------------------- registry

def register(lines: Mapping[int, PreseasonLine] | Iterable[PreseasonLine]) -> int:
    vals = lines.values() if isinstance(lines, Mapping) else lines
    n = 0
    for ln in vals:
        _REGISTRY[(ln.season, ln.nhl_id)] = ln
        n += 1
    return n


def clear_registry() -> None:
    _REGISTRY.clear()


def preseason_line(p: Player, season: int) -> PreseasonLine | None:
    """The registered preseason line of ``p`` for NHL season id ``season`` (e.g. 20262027)."""
    nid = p.nhl_id
    return _REGISTRY.get((season, nid)) if nid is not None else None


def preseason_lines(ctx: LeagueContext, unproven_only: bool = True) -> dict[str, PreseasonLine]:
    """cid -> preseason line for the context's skaters (unproven ones only by default) in the
    season of ``ctx.as_of``. Empty when nothing was registered."""
    if not _REGISTRY:
        return {}
    season = current_season(ctx.as_of)
    out: dict[str, PreseasonLine] = {}
    for p in ctx.all_players():
        if p.is_goalie or (unproven_only and not is_unproven(p)):
            continue
        ln = preseason_line(p, season)
        if ln is not None and ln.gp > 0:
            out[p.cid] = ln
    return out


# --------------------------------------------------------------------------- orchestration

def _fetch(cache: Any, as_of: date, game_dates: dict[int, date]) -> Callable[[str, dict | None], Any]:
    def fetch(url: str, params: dict | None = None) -> Any:
        m = _GAMECENTER_RE.search(url)
        if m:
            d = game_dates.get(int(m.group(1)))
            ttl = TTL_FINAL_GAME if d is not None and d < as_of - timedelta(days=RECENT_DAYS) else TTL_RECENT_GAME
        else:
            ttl = TTL_SCHEDULE
        return cache.get_json(url, params=params, ttl=ttl)
    return fetch


def in_window(ctx: LeagueContext) -> bool:
    """True until WINDOW_DAYS after opening night (always when it is unknown)."""
    return ctx.season_start is None or ctx.as_of <= ctx.season_start + timedelta(days=WINDOW_DAYS)


def enrich_preseason(ctx: LeagueContext, cache: Any, nhl: NhlClient | None = None, season: int | None = None,
                     *, teams: Iterable[str] = NHL_TEAMS) -> dict[str, Any]:
    """Build preseason lines for the unproven skaters of ``ctx`` from every finished preseason
    game before ``ctx.as_of``; register them (and attach them to ``Player.lines`` when the model
    allows). ``nhl`` replaces the cached client (tests). Returns counts; adds one source note.
    Raises only when the schedules cannot be read at all (the caller's step turns that into a
    warning); a failing box score is counted and skipped."""
    season = season or current_season(ctx.as_of)
    res: dict[str, Any] = {"games": 0, "failed": 0, "unproven": 0, "with_games": 0, "attached": 0}
    if not in_window(ctx):
        ctx.source_notes.append(f"Preseason: skipped ({ctx.as_of} is more than {WINDOW_DAYS} days after "
                                f"opening night)")
        return res
    targets = {p.nhl_id: p for p in ctx.all_players()
               if p.nhl_id is not None and not p.is_goalie and is_unproven(p)}
    res["unproven"] = len(targets)
    if not targets:
        return res
    game_dates: dict[int, date] = {}
    if nhl is None:
        if cache is None:
            return res
        nhl = NhlClient(fetch_json=_fetch(cache, ctx.as_of, game_dates), season=season)
    games = nhl.preseason_games(season, teams, before=ctx.as_of)
    game_dates.update({g.game_id: g.date for g in games})
    lines: list[NhlSkaterGameLine] = []
    for g in games:
        try:
            got = nhl.game_skater_lines(g.game_id)
        except Exception:
            res["failed"] += 1
            continue
        if got:
            res["games"] += 1
            lines.extend(ln for ln in got if ln.player_id in targets)
    agg = aggregate(lines, season)
    register(agg)
    res["with_games"] = len(agg)
    for pid, ln in agg.items():
        res["attached"] += attach(targets[pid], ln)
    if games:
        failed = f", {res['failed']} failed" if res["failed"] else ""
        ctx.source_notes.append(
            f"Preseason (NHL box scores, gameType 1): {res['games']}/{len(games)} games read{failed}; "
            f"{res['with_games']} of {res['unproven']} unproven skaters played (a weak signal: at most 25% of "
            f"their baseline, reason PRESEASON)")
    return res
