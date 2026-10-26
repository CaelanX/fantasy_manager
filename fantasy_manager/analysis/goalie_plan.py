"""Goalie start planner for the rest of the matchup (``fm goalies --week``, the matchup page and
the weekly plan).

For each of my goalies and the best free-agent goalies, every remaining game in the matchup gets:

* **Start probability.** Today: Daily Faceoff (``lineup.confirmed_start_probability``: confirmed
  1.0 / 0.0, likely 0.8 / 0.2). Other nights: the season start share (``PlayerValue.start_share``,
  which already carries START_ACTUAL / START_NEWS) split into ordinary nights and back-to-back
  second nights (``valuation.schedule.night_start_probs``: the #1 starts a second night 35% of the
  time, the backup 65%). When the first night of a back-to-back is today and Daily Faceoff knows
  who starts it, the second night is conditioned on it: a #1 who starts tonight starts tomorrow
  with ``b2b_p``, one who rests with ``RESTED_STARTER_P``; a backup who starts tonight repeats
  with ``BACKUP_REPEAT_P``, else ``1 - b2b_p``.
* **Matchup.** Betting-market context (``providers.odds.team_context``: no-vig win probability and
  the opponent's implied goals, archived daily by the harness) when there is a snapshot for that
  day; else the opponent's goals per game this season from the league's skater lines, shrunk
  toward the league average with ``GF_SHRINK_K`` games (no win probability then); else neutral.
* **Expected points per start** (points leagues): the goalie's per-game rates with GA scaled by the
  opponent factor (SV loses the extra goals, SO scaled by the Poisson zero-goal ratio) and W set to
  the win probability times his decision rate, scored with the league's goalie weights.
  Categories / roto leagues keep his FPG (no matchup adjustment).
* **Expected points** = start probability x points per start.

The daily start plan fills my G slots each day with the goalies of mine with the most expected
points; a night with an open G slot is a streaming night. Streaming suggestions are the goalie
pickups of ``waivers.recommend_matchup_waivers`` (the same day-by-day gain, move budget and drop
rules as every other pickup)."""
from __future__ import annotations

import math
from datetime import date, timedelta
from typing import Any, Callable, Mapping

from pydantic import BaseModel, Field

from ..matching.normalize import normalize_team
from ..models import LeagueContext, Player, Recommendation
from ..valuation.schedule import (B2B_SECOND_NIGHT_P, BACKUP_MIN_SHARE, STARTER_SHARE,
                                  night_start_probs)

RESTED_STARTER_P = 0.85      # a #1 who sat the first night of a back-to-back starts the second
BACKUP_REPEAT_P = 0.15       # a backup who started the first night starts the second again
GF_SHRINK_K = 10.0           # games of shrinkage of an opponent's goals per game toward the average
GF_MIN_GAMES = 3             # fewer team games than this: neutral opponent
FA_LIMIT = 3
STREAM_LIMIT = 3

OddsFn = Callable[[date], Mapping[str, Mapping[str, Any]]]


class GoalieGame(BaseModel):
    day: date
    opponent: str                       # "BOS" at home, "@BOS" away
    start_prob: float
    start_source: str                   # "confirmed" / "likely" / "season share 52%" / "B2B 2nd night"
    second_night: bool = False
    win_prob: float | None = None       # betting market, no vig
    opp_goals: float | None = None      # opponent's implied goals (odds) or goals per game (fallback)
    matchup_source: str = "neutral"     # "odds" / "goals per game" / "neutral"
    pts_if_starts: float = 0.0
    exp_pts: float = 0.0


class GoalieRow(BaseModel):
    cid: str
    name: str
    team: str | None
    mine: bool
    status: str = "healthy"
    start_share: float | None = None
    games: list[GoalieGame] = Field(default_factory=list)

    @property
    def exp_total(self) -> float:
        return sum(g.exp_pts for g in self.games)

    @property
    def starts(self) -> float:
        return sum(g.start_prob for g in self.games)


class GoalieDay(BaseModel):
    day: date
    start: list[str] = Field(default_factory=list)     # cids, best first
    sit: list[str] = Field(default_factory=list)       # mine with a game but no G slot
    open_slots: int = 0                                # G slots without a goalie of mine playing


class GoalieWeek(BaseModel):
    days: list[date]
    g_slots: int
    mine: list[GoalieRow]
    free_agents: list[GoalieRow]
    plan: list[GoalieDay]
    streams: list[Recommendation] = Field(default_factory=list)
    notes: list[str] = Field(default_factory=list)

    def row(self, cid: str) -> GoalieRow | None:
        return next((r for r in [*self.mine, *self.free_agents] if r.cid == cid), None)

    @property
    def exp_total(self) -> float:
        """Expected points from my goalies in their G slots over the rest of the matchup."""
        by = {r.cid: r for r in self.mine}
        return sum(g.exp_pts for d in self.plan for c in d.start
                   for g in by[c].games if g.day == d.day)


def team_gf_factors(ctx: LeagueContext) -> dict[str, tuple[float, int]]:
    """{team: (goals per game / league average, shrunk toward 1.0; team games)} from the league's
    skater season lines and the NHL schedule's games played before today. Teams with fewer than
    ``GF_MIN_GAMES`` games are left out (neutral)."""
    goals: dict[str, float] = {}
    for p in ctx.all_players():
        line = p.lines.get("season")
        team = normalize_team(p.team)
        if p.is_goalie or line is None or not team:
            continue
        goals[team] = goals.get(team, 0.0) + float(line.stats.get("G", 0.0) or 0.0)
    played = {t: sum(1 for d in ds if d < ctx.as_of) for t, ds in ctx.schedule.items()}
    rates = {t: (g / played[t], played[t]) for t, g in goals.items() if played.get(t, 0) >= GF_MIN_GAMES}
    if not rates:
        return {}
    avg = sum(r for r, _ in rates.values()) / len(rates)
    if avg <= 0:
        return {}
    return {t: ((n * (r / avg) + GF_SHRINK_K) / (n + GF_SHRINK_K), n) for t, (r, n) in rates.items()}


def _scoring(ctx: LeagueContext) -> Any:
    from ..scoring import PointsScoring, from_config

    s = from_config(ctx.scoring)
    return s if isinstance(s, PointsScoring) else None


def start_points(scoring: Any, pv: Any, win_prob: float | None, ga_factor: float | None) -> float:
    """Expected fantasy points if he starts (see the module docstring); his FPG when the league
    is not a points league or he has no rates."""
    rates = dict(getattr(pv, "rates", None) or {})
    if scoring is None or not rates:
        return float(pv.fpg)
    ga = rates.get("GA")
    if ga is not None and ga_factor is not None and ga_factor > 0:
        new_ga = float(ga) * ga_factor
        extra = new_ga - float(ga)
        if "SV" in rates:
            rates["SV"] = max(0.0, float(rates["SV"]) - extra)
        if "SO" in rates:
            rates["SO"] = float(rates["SO"]) * math.exp(-extra)
        if "GAA" in rates:
            rates["GAA"] = float(rates["GAA"]) * ga_factor
        if rates.get("SA"):
            rates["SVPCT"] = max(0.0, (float(rates["SA"]) - new_ga) / float(rates["SA"]))
        rates["GA"] = new_ga
    if win_prob is not None and "W" in rates:
        w = float(rates["W"])
        decisions = w + float(rates.get("L", 0.0)) + float(rates.get("OTL", 0.0)) or 1.0
        new_w = min(max(win_prob * decisions, 0.0), decisions)
        if "L" in rates:
            rates["L"] = max(0.0, float(rates["L"]) - (new_w - w))
        rates["W"] = new_w
    return max(0.0, float(scoring.value(rates)))


def _matchup(team: str, opp_label: str, day: date, odds: Mapping[str, Mapping[str, Any]],
             gf: Mapping[str, tuple[float, int]]) -> tuple[float | None, float | None, float | None, str]:
    """(win prob, opponent goals shown, GA factor, source) for ``team`` on ``day``."""
    o = odds.get(team) or {}
    implied = [float(v["implied_total"]) for v in odds.values() if v.get("implied_total")]
    if o.get("opp_implied_total") and implied:
        avg = sum(implied) / len(implied)
        opp = float(o["opp_implied_total"])
        return o.get("win_prob"), opp, (opp / avg if avg > 0 else None), "odds"
    opp_team = normalize_team(opp_label.lstrip("@"))
    f = gf.get(opp_team or "")
    if f is not None:
        return o.get("win_prob"), None, f[0], "goals per game"
    return o.get("win_prob"), None, None, "neutral"


def _games(ctx: LeagueContext, p: Player, pv: Any, days: list[date], odds_for: Callable[[date], Mapping],
           gf: Mapping[str, tuple[float, int]], scoring: Any) -> list[GoalieGame]:
    from ..recommend.lineup import available_on, confirmed_start_probability

    team = normalize_team(p.team) or ""
    dates = set(ctx.schedule.get(team) or ())
    share = pv.start_share if pv.start_share is not None else 1.0
    p1, p2 = night_start_probs(share, B2B_SECOND_NIGHT_P)
    today_prob = confirmed_start_probability(p)
    out: list[GoalieGame] = []
    prev_known: float | None = None          # start probability of the previous night, when known
    for d in days:
        if d not in dates or not available_on(p, pv, d):
            prev_known = None
            continue
        second = d - timedelta(days=1) in dates
        if d == ctx.as_of and today_prob is not None:
            prob = today_prob
            src = "confirmed" if today_prob in (0.0, 1.0) else "likely"
            known = prob
        elif second and prev_known is not None and share >= BACKUP_MIN_SHARE:
            q = prev_known
            prob = (q * B2B_SECOND_NIGHT_P + (1 - q) * RESTED_STARTER_P if share >= STARTER_SHARE
                    else q * BACKUP_REPEAT_P + (1 - q) * (1 - B2B_SECOND_NIGHT_P))
            src, known = "B2B 2nd night, after tonight's starter", None
        elif second:
            prob, src, known = p2, "B2B 2nd night", None
        else:
            prob, src, known = p1, f"season share {share:.0%}", None
        prev_known = known
        opp = (ctx.opponents.get(team) or {}).get(d, "?")
        win, opp_goals, ga_f, msrc = _matchup(team, opp, d, odds_for(d), gf)
        pts = start_points(scoring, pv, win, ga_f)
        out.append(GoalieGame(day=d, opponent=opp, start_prob=round(prob, 3), start_source=src, second_night=second,
                              win_prob=win, opp_goals=opp_goals, matchup_source=msrc,
                              pts_if_starts=round(pts, 2), exp_pts=round(prob * pts, 2)))
    return out


def _odds_fn(data_dir: Any) -> OddsFn:
    from ..providers.odds import team_context

    cache: dict[date, Mapping[str, Mapping[str, Any]]] = {}

    def get(d: date) -> Mapping[str, Mapping[str, Any]]:
        if d not in cache:
            try:
                cache[d] = team_context(d, data_dir)
            except Exception:  # noqa: BLE001 - odds are context only
                cache[d] = {}
        return cache[d]
    return get


def goalie_week(ctx: LeagueContext, values: Mapping[str, Any], *, odds: OddsFn | None = None,
                data_dir: Any = None, fa_limit: int = FA_LIMIT, streams: bool = True,
                dynasty_values: Mapping[str, Any] | None = None) -> GoalieWeek:
    """The goalie plan for the rest of the matchup (see the module docstring). ``odds`` maps a
    day to ``providers.odds.team_context`` output (default: the harness archive in ``data_dir``)."""
    from ..recommend.lineup import plan_days, starting_slots

    days = plan_days(ctx)
    odds_for = odds or _odds_fn(data_dir)
    gf = team_gf_factors(ctx)
    scoring = _scoring(ctx)
    g_slots = starting_slots(ctx.roster_shape).count("G")
    notes: list[str] = []

    def row(p: Player, mine: bool) -> GoalieRow | None:
        pv = values.get(p.cid)
        if pv is None:
            return None
        return GoalieRow(cid=p.cid, name=p.name, team=p.team, mine=mine, status=p.status,
                         start_share=pv.start_share, games=_games(ctx, p, pv, days, odds_for, gf, scoring))

    mine = [r for p in ctx.my_team.players if p.is_goalie and (r := row(p, True)) is not None]
    fas = [r for p in ctx.free_agents
           if p.is_goalie and normalize_team(p.team) and (r := row(p, False)) is not None and r.games]
    fas.sort(key=lambda r: r.exp_total, reverse=True)

    plan: list[GoalieDay] = []
    for d in days:
        today = sorted(((r, g) for r in mine for g in r.games if g.day == d and g.exp_pts > 0),
                       key=lambda x: x[1].exp_pts, reverse=True)
        start = [r.cid for r, _ in today[:g_slots]]
        plan.append(GoalieDay(day=d, start=start, sit=[r.cid for r, _ in today[g_slots:]],
                              open_slots=max(0, g_slots - len(start))))

    recs: list[Recommendation] = []
    if streams:
        from ..recommend.strength import apply_ranks
        from ..recommend.waivers import recommend_matchup_waivers

        try:
            recs = apply_ranks([r for r in recommend_matchup_waivers(ctx, dict(values), limit=50,
                                                                     dynasty_values=dynasty_values)
                                if r.add and r.add[0].is_goalie][:STREAM_LIMIT])
        except Exception as e:  # noqa: BLE001 - streaming ideas are optional
            notes.append(f"Goalie streaming suggestions failed: {e}")
    sources = {g.matchup_source for r in [*mine, *fas] for g in r.games}
    if "odds" not in sources:
        notes.append("No betting odds archived for these days (ODDS_API_KEY / `fm harness odds`): matchups use "
                     "the opponent's goals per game" if "goals per game" in sources else
                     "No betting odds or opponent scoring available: matchups are neutral")
    return GoalieWeek(days=days, g_slots=g_slots, mine=mine, free_agents=fas[:fa_limit], plan=plan,
                      streams=recs, notes=notes)


__all__ = ["GoalieGame", "GoalieRow", "GoalieDay", "GoalieWeek", "goalie_week", "start_points",
           "team_gf_factors"]
