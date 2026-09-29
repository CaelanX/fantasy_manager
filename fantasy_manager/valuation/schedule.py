"""Schedule factor for the week horizon.

``ProjWeek = FPG * avail_week * games_next7 * (1 + 0.05 * offnight_games)``; goalies are also
multiplied by a shrunk start share ``(GS + k*0.5) / (team GP + k)`` with k = 10 (the same
share scales goalie season values in valuate.py).

Before the regular season (no NHL games in the window starting at ``as_of`` and
``season_start`` in the future) the window is the first 7 days from ``season_start``.

The off-night bonus and the start-share prior / k are read from ``params`` at call time (a
harness version may override them); the module constants are import-time snapshots.

Actual starts (``harness.deployment`` ledger, opt-in): ``start_share(..., actual=(starts, team
games))`` shrinks this season's per-game starts / team games toward the projection-based or
prior share with k = ``ACTUAL_SHARE_K`` (10): ``(starts + 10*base) / (team games + 10)``, where
``base`` is the share from the prior (else projected) line only, so this season is not counted
twice. ``actual_start_share`` does the same for a base share computed elsewhere.

Back-to-backs (opt-in, week horizon): ``week_start_share`` / ``schedule_factor(b2b_p=...)``
re-weight a goalie's share for the second nights of back-to-backs in the window. The #1 (share
>= 0.5) starts a second night with probability ``b2b_p`` (0.35 until the ledger has 5 of the
team's back-to-backs, then ``harness.deployment.b2b_second_night_rate``); a backup (share
0.25-0.5) with ``1 - b2b_p``. The other nights get the share that keeps the season average
unchanged given ``SEASON_B2B_FRAC`` of games are second nights.

Absent teammates (:func:`teammate_shares`, reason TEAMMATE_OUT in valuate.py): when a team's
higher-share goalie is out / IR / LTIR / suspended, his share goes to the team's other goalies
(league pool + NHL roster goalies) in proportion to their shares. Week: times the fraction of the
window's games he misses (``adjust.return_estimate``), the new #1 capped at ``TEAMMATE_CAP`` (0.9).
Season: ``share + absent share x games missed / games remaining`` (split the same way).
"""
from __future__ import annotations

from datetime import date, timedelta
from typing import Any, Iterable, Mapping

from pydantic import BaseModel

from ..matching.normalize import normalize_team
from ..models import LeagueContext, Player
from . import params as _params

WEEK_DAYS = 7
OFFNIGHT_THRESHOLD = 8
# import-time snapshots (read-only aliases; live code uses the params accessors)
OFFNIGHT_BONUS = _params.offnight_bonus()
START_SHARE_K = _params.start_share_k()
START_SHARE_PRIOR = _params.start_share_prior()
# actual starts (harness ledger) shrunk toward the projection / prior share
ACTUAL_SHARE_K = 10.0
# goalie back-to-backs: P(#1 starts the second night) until the ledger has B2B_MIN_SAMPLE of the
# team's back-to-backs (then the observed rate shrunk toward it with k = B2B_SHRINK_K)
B2B_SECOND_NIGHT_P = 0.35
B2B_MIN_SAMPLE = 5
B2B_SHRINK_K = 5.0
SEASON_B2B_FRAC = 0.15      # share of a team's games that are the second night of a back-to-back
STARTER_SHARE = 0.5
BACKUP_MIN_SHARE = 0.25


def _in_window(d: date, start: date, days: int) -> bool:
    return start <= d < start + timedelta(days=days)


def games_in_next(dates: Iterable[date], as_of: date, days: int = WEEK_DAYS) -> int:
    """Games on dates in [as_of, as_of + days)."""
    return sum(1 for d in set(dates) if _in_window(d, as_of, days))


def off_night_games(dates: Iterable[date], games_per_day: Mapping[date, int], as_of: date,
                    days: int = WEEK_DAYS, threshold: int = OFFNIGHT_THRESHOLD) -> int:
    """A team's games in the window played on nights with fewer than ``threshold`` NHL games."""
    return sum(1 for d in set(dates)
               if _in_window(d, as_of, days) and 0 < games_per_day.get(d, 0) < threshold)


def proj_week(fpg: float, avail_week: float, games_next7: int, offnight_games: int,
              params: Mapping[str, Any] | None = None) -> float:
    return fpg * avail_week * games_next7 * (1 + _params.offnight_bonus(params) * offnight_games)


def shrunk_share(starts: float, team_games: float, k: float | None = None, prior: float | None = None,
                 params: Mapping[str, Any] | None = None) -> float:
    """``(GS + k*prior) / (team GP + k)`` with k / prior from ``params`` unless given."""
    k = _params.start_share_k(params) if k is None else k
    prior = _params.start_share_prior(params) if prior is None else prior
    return (starts + k * prior) / (team_games + k)


def start_share(player: Player, k: float | None = None, prior: float | None = None,
                team_games: Mapping[str, float] | None = None,
                actual: tuple[float, float] | None = None, actual_k: float = ACTUAL_SHARE_K) -> float:
    """Shrunk goalie start share (1.0 for skaters): ``(GS + k*prior) / (team GP + k)``.

    Starts and games are pooled over the season and prior lines (the projected line when
    neither carries games). GS falls back to the goalie's GP when a line has no GS column.
    ``team_games`` ({split: team games played}) gives the denominator per split; without it
    (or for a split it does not cover) the goalie's own GP is used, i.e. GS/GP. The
    denominator is never below the goalie's own GP.

    ``actual`` = (starts, team games) this season from per-game goalie starts (the harness
    ledger): the result is ``(starts + actual_k*base) / (team games + actual_k)`` with ``base``
    the shrunk share of the prior line (else the projected line), leaving the season line out
    because ``actual`` already counts this season."""
    if not player.is_goalie:
        return 1.0
    if actual is not None and actual[1] > 0:
        base = start_share_parts(player, k, prior, team_games, splits=("prior",))[0]
        return actual_start_share(base, actual[0], actual[1], actual_k)
    return start_share_parts(player, k, prior, team_games)[0]


def actual_start_share(base: float, starts: float, team_games: float, k: float = ACTUAL_SHARE_K) -> float:
    """This season's per-game starts / team games shrunk toward ``base`` (the projection-based
    or prior share) with ``k`` games: ``(starts + k*base) / (team games + k)``."""
    if team_games <= 0:
        return base
    starts = min(max(float(starts), 0.0), float(team_games))
    return (starts + k * base) / (float(team_games) + k)


def start_share_parts(player: Player, k: float | None = None, prior: float | None = None,
                      team_games: Mapping[str, float] | None = None,
                      splits: tuple[str, ...] = ("season", "prior")) -> tuple[float, float, float]:
    """(share, starts, team games) behind :func:`start_share` (pooled over ``splits``; the
    projected line when those carry no games)."""
    def pooled(splits: tuple[str, ...]) -> tuple[float, float]:
        gs = tg = 0.0
        for split in splits:
            line = player.lines.get(split)
            if line is None:
                continue
            starts = float(line.stats.get("GS", line.gp))
            own = float(max(line.gp, starts))
            denom = max(float((team_games or {}).get(split, own)), own)
            if denom <= 0:
                continue
            gs += starts
            tg += denom
        return gs, tg

    gs, tg = pooled(splits)
    if tg <= 0:
        gs, tg = pooled(("projected",))
    return shrunk_share(gs, tg, k, prior), gs, tg


class WeekWindow(BaseModel):
    start: date
    days: int = WEEK_DAYS
    preseason: bool = False
    avg_team_games: float = 0.0   # mean games per NHL team in the window

    @property
    def end(self) -> date:
        return self.start + timedelta(days=self.days - 1)

    def label(self) -> str:
        return f"{self.start:%b %d}-{self.end:%b %d}"


def week_window(as_of: date, season_start: date | None, schedule: Mapping[str, list[date]],
                games_per_day: Mapping[date, int], days: int = WEEK_DAYS) -> WeekWindow | None:
    """Window to project over, or None when no schedule is loaded."""
    if not schedule:
        return None
    start, preseason = as_of, False
    league_games = sum(n for d, n in games_per_day.items() if _in_window(d, as_of, days))
    if league_games == 0 and not games_per_day:
        league_games = sum(games_in_next(ds, as_of, days) for ds in schedule.values())
    if league_games == 0 and season_start is not None and season_start > as_of:
        start, preseason = season_start, True
    counts = [games_in_next(ds, start, days) for ds in schedule.values()]
    avg = sum(counts) / len(counts) if counts else 0.0
    return WeekWindow(start=start, days=days, preseason=preseason, avg_team_games=avg)


def context_window(ctx: LeagueContext, days: int = WEEK_DAYS) -> WeekWindow | None:
    return week_window(ctx.as_of, ctx.season_start, ctx.schedule, ctx.games_per_day, days)


class ScheduleFactor(BaseModel):
    games: int
    offnight: int
    start_share: float = 1.0
    window: WeekWindow

    @property
    def multiplier(self) -> float:
        """Games-weighted multiplier applied to availability-adjusted FPG."""
        return self.games * (1 + _params.offnight_bonus() * self.offnight) * self.start_share


def second_nights(dates: Iterable[date], start: date, days: int = WEEK_DAYS) -> int:
    """Games in [start, start + days) played the day after another game (the previous game
    may fall before the window)."""
    ds = set(dates)
    return sum(1 for d in ds if _in_window(d, start, days) and d - timedelta(days=1) in ds)


def week_start_share(share: float, dates: Iterable[date], start: date, days: int = WEEK_DAYS,
                     b2b_p: float = B2B_SECOND_NIGHT_P, season_b2b_frac: float = SEASON_B2B_FRAC) -> float:
    """Expected fraction of the team's games in the window that the goalie starts, given his
    season ``share`` and the window's back-to-backs (see the module docstring). Unchanged for a
    third goalie (share < 0.25) or a window without games."""
    ds = set(dates)
    games = games_in_next(ds, start, days)
    if games <= 0:
        return share
    if share >= STARTER_SHARE:
        p2 = b2b_p
    elif share >= BACKUP_MIN_SHARE:
        p2 = 1.0 - b2b_p
    else:
        return share
    b = second_nights(ds, start, days)
    f = min(max(season_b2b_frac, 0.0), 0.9)
    p1 = min(1.0, max(0.0, (share - f * p2) / (1.0 - f)))
    return ((games - b) * p1 + b * p2) / games


def schedule_factor(player: Player, schedule: Mapping[str, list[date]], games_per_day: Mapping[date, int],
                    window: WeekWindow, threshold: int = OFFNIGHT_THRESHOLD,
                    share: float | None = None, b2b_p: float | None = None) -> ScheduleFactor | None:
    """Games / off-night games / start share for one player's NHL team, or None if unknown team.

    ``b2b_p`` (goalies, opt-in): P(#1 starts a back-to-back's second night) for
    :func:`week_start_share`; None keeps the season share."""
    team = normalize_team(player.team)
    if not team or team not in schedule:
        return None
    dates = schedule[team]
    s = start_share(player) if share is None else share
    if b2b_p is not None and player.is_goalie:
        s = week_start_share(s, dates, window.start, window.days, b2b_p)
    return ScheduleFactor(games=games_in_next(dates, window.start, window.days),
                          offnight=off_night_games(dates, games_per_day, window.start, window.days, threshold),
                          start_share=s,
                          window=window)


# --------------------------------------------------------------------------- teammate-aware shares

TEAMMATE_CAP = 0.9            # a goalie absorbing an absent teammate's starts never goes above this
ROSTER_ONLY_SHARE = 0.10      # weight of an NHL roster goalie outside the league pool (or without any history)
ABSENT_STATUSES = ("out", "ir", "ltir", "suspended")
_STATUS_WORDS = {"out": "out", "ir": "on IR", "ltir": "on LTIR", "suspended": "suspended"}


def redistribute_shares(shares: Mapping[str, float], absent: Mapping[str, float],
                        cap: float = TEAMMATE_CAP) -> dict[str, float]:
    """New start shares of the goalies of one team not in ``absent`` ({key: fraction of the
    horizon he misses, 0..1}).

    Only an absent goalie whose share is above every remaining goalie's (the #1, or a tandem
    goalie ahead of the rest) donates: ``share x fraction`` goes to the remaining goalies in
    proportion to their own shares. A recipient never ends above ``cap`` (or his own share, if
    that is higher); what he cannot take goes to the others, and is lost (an emergency call-up's
    starts) when nobody can take it."""
    rest = {k: max(0.0, float(v)) for k, v in shares.items() if k not in absent}
    if not rest:
        return {}
    top = max(rest.values())
    pool = sum(float(shares[k]) * min(max(float(f), 0.0), 1.0)
               for k, f in absent.items() if k in shares and float(shares[k]) > top)
    out = dict(rest)
    open_ = set(rest)
    while pool > 1e-12 and open_:
        wsum = sum(rest[k] for k in open_)
        spill = 0.0
        n_open = len(open_)
        for k in sorted(open_):
            w = rest[k] / wsum if wsum > 0 else 1.0 / n_open
            lim = max(cap, rest[k])
            new = out[k] + pool * w
            if new >= lim:
                spill += new - lim
                out[k] = lim
                open_.discard(k)
            else:
                out[k] = new
        pool = spill
    return out


class TeammateAbsence(BaseModel):
    """An absent goalie whose starts are redistributed: the fraction of the week window / of the
    rest of the season he misses."""
    name: str
    status: str
    return_date: date | None = None
    week_frac: float
    season_frac: float
    games_missed: float | None = None
    games_remaining: float | None = None


class TeammateShare(BaseModel):
    """A goalie's start shares after his absent teammates' starts are redistributed."""
    base: float
    week: float
    season: float
    absent: list[TeammateAbsence]
    window_label: str | None = None

    def text(self, player: Player) -> str:
        me = _last_name(player.name)
        parts = []
        for a in self.absent:
            who = f"{_last_name(a.name)} {_STATUS_WORDS.get(a.status, a.status)}"
            if a.return_date is not None:
                who += f" until ~{a.return_date:%b} {a.return_date.day}"
            elif a.season_frac >= 0.999:
                who += " for the rest of the season"
            if a.games_missed is not None and a.games_remaining:
                who += f" (~{a.games_missed:.0f} of {a.games_remaining:.0f} games)"
            parts.append(who)
        week = f"{self.base:.2f} -> {self.week:.2f} this week"
        if self.window_label:
            week += f" ({self.window_label})"
        return (f"{' and '.join(parts)}: {me}'s start share {week}, {self.base:.2f} -> {self.season:.2f} "
                f"rest of season")


def _last_name(name: str) -> str:
    parts = (name or "").split()
    return parts[-1] if parts else name


def _week_fraction(status: str, est: Any, team_dates: Iterable[date], window: WeekWindow | None) -> float:
    """Fraction of the team's games in ``window`` an absent goalie misses (the flat week
    availability when the window has no games or nothing can be inferred: 1.0 while out)."""
    from .adjust import availability_multiplier

    flat = min(max(1.0 - availability_multiplier(status, "week"), 0.0), 1.0)
    if window is None:
        return flat
    games = sorted(d for d in set(team_dates) if _in_window(d, window.start, window.days))
    if not games or est is None:
        return flat
    if est.return_date is None:
        return 1.0 if est.games_missed > 0 else 0.0
    return sum(1 for d in games if d < est.return_date) / len(games)


def teammate_shares(ctx: LeagueContext, base: Mapping[str, float], window: WeekWindow | None = None,
                    weights: Mapping[str, float] | None = None, cap: float = TEAMMATE_CAP
                    ) -> dict[str, TeammateShare]:
    """cid -> teammate-adjusted week / season start shares of the goalies whose team's #1 (a
    goalie with a higher share) is out / IR / LTIR / suspended.

    Per NHL team: the league pool's goalies (``ctx.all_players()``, keyed by cid with their
    ``base`` share) plus the club's NHL roster goalies missing from the pool
    (``ctx.nhl_goalies``, weight ``ROSTER_ONLY_SHARE``); a healthy pool goalie with an NHL id
    that is not on a known NHL roster (a minor-leaguer) takes nothing. ``weights`` overrides a
    goalie's share for the redistribution (e.g. ``ROSTER_ONLY_SHARE`` for one whose share is the
    bare prior, so an injured minor-leaguer never donates and a call-up without history does not
    take half of the starts).

    Week: the absent goalie's share x the fraction of his team's games in ``window`` he misses
    (``adjust.return_estimate``; the whole window while out without a timetable) is spread over
    the others in proportion to their shares (:func:`redistribute_shares`, new #1 capped at
    ``cap``). Season: ``share + absent share x games missed / games remaining`` (same split)."""
    from .adjust import availability_multiplier, return_estimate

    by_team: dict[str, list[Player]] = {}
    for p in ctx.all_players():
        if p.is_goalie and p.cid in base:
            team = normalize_team(p.team)
            if team:
                by_team.setdefault(team, []).append(p)
    out: dict[str, TeammateShare] = {}
    for team, goalies in by_team.items():
        absent = {g.cid: g for g in goalies if g.status in ABSENT_STATUSES}
        if not absent:
            continue
        roster = ctx.nhl_goalies.get(team) or {}
        team_dates = ctx.schedule.get(team, [])
        shares: dict[str, float] = {}
        for g in goalies:
            if g.cid in absent or not roster or g.nhl_id is None or g.nhl_id in roster:
                shares[g.cid] = float((weights or {}).get(g.cid, base[g.cid]))
        pool_ids = {g.nhl_id for g in goalies if g.nhl_id is not None}
        for nid in roster:
            if nid not in pool_ids:
                shares[f"nhl:{nid}"] = ROSTER_ONLY_SHARE
        info: dict[str, TeammateAbsence] = {}
        for cid, a in absent.items():
            est = return_estimate(a.status, a.status_note, ctx.as_of, team_dates, ctx.season_start)
            if est is not None:
                season_frac = est.games_missed / est.games_remaining if est.games_remaining > 0 else 1.0
            else:
                season_frac = 1.0 - availability_multiplier(a.status, "season")
            info[cid] = TeammateAbsence(
                name=a.name, status=a.status, return_date=est.return_date if est else None,
                week_frac=_week_fraction(a.status, est, team_dates, window),
                season_frac=min(max(season_frac, 0.0), 1.0),
                games_missed=round(est.games_missed, 2) if est else None,
                games_remaining=round(est.games_remaining, 2) if est else None)
        week = redistribute_shares(shares, {c: i.week_frac for c, i in info.items()}, cap)
        season = redistribute_shares(shares, {c: i.season_frac for c, i in info.items()}, cap)
        rest_top = max((v for k, v in shares.items() if k not in info), default=0.0)
        donors = [i for c, i in info.items() if shares[c] > rest_top]
        for g in goalies:
            if g.cid in info or g.cid not in week:
                continue
            b = float(base[g.cid])
            w0 = shares[g.cid]
            lim = max(cap, b)
            wk = min(lim, b + week[g.cid] - w0)
            sn = min(lim, b + season[g.cid] - w0)
            if wk - b <= 1e-4 and sn - b <= 1e-4:
                continue
            out[g.cid] = TeammateShare(base=b, week=wk, season=sn, absent=donors,
                                       window_label=window.label() if window is not None else None)
    return out
