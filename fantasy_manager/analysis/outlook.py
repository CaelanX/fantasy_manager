"""Contention window and mode advisor (``fm outlook``, the dashboard's Outlook section).

For every team in the league:

* **This season.** ``now`` = the best starting lineup's rest-of-season per-game total
  (``PlayerValue.fpg_season``, injuries and goalie start shares included; IR-slot players count),
  ranked across the league.
* **Next seasons** (dynasty leagues). Each player's healthy per-game value
  (``valuation.dynasty._healthy_fpg``) times his age-curve / pedigree trajectory
  (``trajectory_factor``) for 1..FUTURE_YEARS seasons ahead; the best lineup of those values per
  year, averaged, ranked. Same units as ``now`` (a lineup's FPG), so a team that ages out shows a
  falling line.
* **Playoff odds this season.** A Monte Carlo of the rest of the fantasy regular season
  (SIMS draws, fixed seed): every week each team meets a random opponent and wins with the normal
  probability of its weekly points (lineup FPG x GAMES_PER_WEEK, sd WEEKLY_CV of the mean) beating
  theirs, added to its current record; the top ``playoff_teams`` make it (ties broken by
  strength). Weeks left: until the league's first playoff period (``schedule_grid.playoff_weeks``),
  else the NHL regular season's last date minus 3 weeks. Playoff spots: the league's setting when
  the provider gives it, else half the league. Opponents are random because the remaining fantasy
  schedule is not loaded, so these are approximate.

The recommended mode (dynasty) follows the playoff odds and the future rank:

* ``contend``  playoff odds >= CONTEND_ODDS;
* ``rebuild``  odds <= REBUILD_ODDS, or <= LONGSHOT_ODDS and the core ranks better in the future
  than now (the window is ahead);
* ``balanced`` otherwise.

Redraft leagues (no dynasty) get a label instead: contender / bubble / pretender. The mode is
never changed here: the output says "recommended: rebuild (currently balanced)" and ``fm mode``
stays the only switch.

Sell and buy lists follow the recommended direction:

* contend: sell my players with long-term value who do not start this season (prospects, stashes);
  buy other teams' players who would start for me now, from teams with low playoff odds first.
* rebuild: sell my players whose value falls the most over the next seasons (veterans at their
  peak now); buy young risers (future value above today's) from contending teams first.
* balanced: sell my steepest decliners aged >= BALANCED_SELL_AGE; buy players in their prime who
  hold their value both now and later.
"""
from __future__ import annotations

import math
import random
from dataclasses import dataclass, field
from datetime import date, timedelta
from typing import Any, Mapping, Sequence

from ..models import FantasyTeam, LeagueContext, Player

FUTURE_YEARS = 3
SIMS = 2000
SEED = 7
GAMES_PER_WEEK = 3.4          # a starter's NHL games in a fantasy week (league-average)
WEEKLY_CV = 0.20              # sd of a team's weekly points as a share of its mean (fantasy weeks are noisy)
CONTEND_ODDS = 0.55
REBUILD_ODDS = 0.15
LONGSHOT_ODDS = 0.30
PRETENDER_ODDS = 0.20         # redraft: "pretender" at or below this
BALANCED_SELL_AGE = 30.0
REBUILD_BUY_MAX_AGE = 25.0
PRIME_AGES = (23.0, 29.0)
LIST_N = 3
DEFAULT_REG_SEASON_TAIL = timedelta(days=21)   # fantasy playoffs ~ the NHL season's last 3 weeks


class _V:
    """Minimal value object for the trade module's lineup solver (``fpg_for`` / ``vorp``)."""
    __slots__ = ("fpg", "vorp")

    def __init__(self, fpg: float):
        self.fpg = fpg
        self.vorp = fpg

    def fpg_for(self, horizon: str = "season") -> float:  # noqa: ARG002 - one horizon here
        return self.fpg


@dataclass
class TeamOutlook:
    team: FantasyTeam
    record: tuple[int, int, int] | None
    win_pct: float | None
    now: float                                  # best lineup, rest-of-season FPG
    future: list[float] = field(default_factory=list)   # best lineup FPG 1..FUTURE_YEARS seasons ahead
    now_rank: int = 0
    future_rank: int | None = None
    playoff_odds: float | None = None
    label: str = ""                             # recommended mode (dynasty) or contender / bubble / pretender

    @property
    def future_avg(self) -> float | None:
        return sum(self.future) / len(self.future) if self.future else None

    @property
    def mine(self) -> bool:
        return self.team.owner_is_me


@dataclass
class Asset:
    player: Player
    owner: str | None
    now: float
    future: float | None
    age: float | None
    why: str


@dataclass
class Outlook:
    teams: list[TeamOutlook]                     # best now first
    me: TeamOutlook | None
    dynasty: bool
    recommended: str | None                      # contend / balanced / rebuild (dynasty) or label (redraft)
    current: str | None                          # the mode in use (dynasty)
    reasons: list[str] = field(default_factory=list)
    sell: list[Asset] = field(default_factory=list)
    buy: list[Asset] = field(default_factory=list)
    weeks_left: int = 0
    playoff_teams: int = 0
    notes: list[str] = field(default_factory=list)

    def direction(self, team_id: str) -> str | None:
        """A team's recommended mode / label."""
        return next((t.label for t in self.teams if t.team.team_id == team_id), None)


# --------------------------------------------------------------------------- per-player values

def _age(p: Player, ctx: LeagueContext, dyn: Mapping[str, Any] | None) -> float | None:
    a = getattr((dyn or {}).get(p.cid), "age", None)
    if isinstance(a, (int, float)):
        return float(a)
    return (ctx.as_of - p.birth_date).days / 365.25 if p.birth_date else None


def _now(values: Mapping[str, Any], p: Player) -> float:
    pv = values.get(p.cid)
    return max(0.0, float(pv.fpg_for("season"))) if pv is not None else 0.0


def future_values(p: Player, ctx: LeagueContext, values: Mapping[str, Any],
                  dyn: Mapping[str, Any] | None, years: int = FUTURE_YEARS) -> list[float]:
    """His healthy per-game value 1..years seasons ahead (age curve x pedigree)."""
    from ..valuation.dynasty import _healthy_fpg, position_group, trajectory_factor

    pv = values.get(p.cid)
    if pv is None:
        return [0.0] * years
    base = max(0.0, _healthy_fpg(pv, float(pv.fpg_for("season"))))
    ped = getattr((dyn or {}).get(p.cid), "pedigree", 1.0) or 1.0
    group, age = position_group(p), _age(p, ctx, dyn)
    return [base * trajectory_factor(group, age, y, ped) for y in range(1, years + 1)]


def _lineup(players: Sequence[Player], vals: Mapping[str, float], ctx: LeagueContext) -> float:
    from ..recommend.trades import _greedy_lineup_value

    return _greedy_lineup_value(players, {c: _V(v) for c, v in vals.items()}, ctx.roster_shape)


# --------------------------------------------------------------------------- season length

def _weeks_left(ctx: LeagueContext, provider: Any = None) -> tuple[int, str]:
    """(fantasy regular-season weeks left, how it was found)."""
    start: date | None = None
    src = ""
    try:
        from .schedule_grid import playoff_weeks

        po = playoff_weeks(ctx, provider)
        if po.periods:
            start, src = po.periods[0].start, "until the league's first playoff period"
    except Exception:  # noqa: BLE001 - the calendar is optional
        start = None
    if start is None:
        last = max((d for ds in ctx.schedule.values() for d in ds), default=None)
        if last is None:
            return 0, "no schedule loaded"
        start, src = last - DEFAULT_REG_SEASON_TAIL, "NHL season end minus 3 weeks (assumed)"
    return max(0, (start - ctx.as_of).days // 7), src


def _playoff_teams(ctx: LeagueContext, provider: Any = None) -> tuple[int, str]:
    """Playoff spots: ESPN ``settings.playoff_team_count``, the Fantrax Rules page "Number of
    teams qualifying for playoffs", else half the league."""
    n = len(ctx.teams)
    candidates: list[Any] = []
    league = getattr(provider, "_league_obj", None)
    candidates.append(getattr(getattr(league, "settings", None), "playoff_team_count", None))
    rules = getattr(provider, "rules", None)
    if rules is not None and hasattr(rules, "get"):
        candidates.append(rules.get("Number of teams qualifying for playoffs"))
    for v in candidates:
        try:
            k = int(str(v).strip()) if v not in (None, "") else None
        except ValueError:
            k = None
        if k is not None and 0 < k <= n:
            return k, "league setting"
    return max(1, n // 2), "assumed half the league"


def playoff_odds(rows: Sequence[TeamOutlook], weeks: int, spots: int, sims: int = SIMS,
                 seed: int = SEED) -> dict[str, float]:
    """team_id -> P(top ``spots`` after ``weeks`` more random-opponent weeks) (module docstring)."""
    if not rows:
        return {}
    rng = random.Random(seed)
    ids = [r.team.team_id for r in rows]
    mean = {r.team.team_id: r.now * GAMES_PER_WEEK for r in rows}
    base = {r.team.team_id: (r.record[0] + 0.5 * r.record[2]) if r.record else 0.0 for r in rows}

    def p_win(a: str, b: str) -> float:
        sd = math.hypot(WEEKLY_CV * mean[a], WEEKLY_CV * mean[b]) or 1.0
        return 0.5 * (1.0 + math.erf((mean[a] - mean[b]) / (sd * math.sqrt(2.0))))

    pw = {(a, b): p_win(a, b) for a in ids for b in ids if a != b}
    made = {t: 0 for t in ids}
    for _ in range(sims):
        wins = dict(base)
        for _w in range(weeks):
            order = ids[:]
            rng.shuffle(order)
            for a, b in zip(order[::2], order[1::2]):
                if rng.random() < pw[(a, b)]:
                    wins[a] += 1
                else:
                    wins[b] += 1
        ranked = sorted(ids, key=lambda t: (-wins[t], -mean[t], rng.random()))
        for t in ranked[:spots]:
            made[t] += 1
    return {t: made[t] / sims for t in ids}


# --------------------------------------------------------------------------- recommendation

def recommend_mode(me: TeamOutlook, n: int) -> tuple[str, list[str]]:
    """(contend / balanced / rebuild, reasons) for my team (module docstring)."""
    odds = me.playoff_odds or 0.0
    why = [f"Playoff odds this season about {odds:.0%} (lineup #{me.now_rank} of {n} right now"
           + (f", record {'-'.join(map(str, me.record))}" if me.record else "") + ")"]
    fr = me.future_rank
    if fr is not None:
        trend = "better" if fr < me.now_rank else ("worse" if fr > me.now_rank else "the same")
        why.append(f"Your core ranks #{fr} of {n} over the next {FUTURE_YEARS} seasons ({trend} than now)")
    if odds >= CONTEND_ODDS:
        return "contend", why + ["Good odds: trade future value for this season's points"]
    if odds <= REBUILD_ODDS:
        return "rebuild", why + ["A long shot this season: sell this season's points for younger value"]
    if odds <= LONGSHOT_ODDS and fr is not None and fr < me.now_rank:
        return "rebuild", why + ["Your window is ahead: build toward it rather than chase this season"]
    return "balanced", why + ["On the bubble: only make trades that help both now and later"]


def redraft_label(odds: float) -> str:
    if odds >= CONTEND_ODDS:
        return "contender"
    if odds <= PRETENDER_ODDS:
        return "pretender"
    return "bubble"


def _assets(o: Outlook, ctx: LeagueContext, values: Mapping[str, Any], dyn: Mapping[str, Any] | None,
            fut: Mapping[str, float]) -> None:
    """Fill ``o.sell`` / ``o.buy`` for the recommended direction (module docstring)."""
    from ..recommend.trades import _greedy_assignment

    me = ctx.my_team
    mine = me.players
    starters = {c for cids in _greedy_assignment(mine, values, ctx.roster_shape).values() for c in cids}
    weakest = min((_now(values, p) for p in mine if p.cid in starters), default=0.0)
    direction = {t.team.team_id: t for t in o.teams}
    rec = o.recommended
    sell: list[tuple[float, Asset]] = []
    for p in mine:
        now, f, age = _now(values, p), fut.get(p.cid), _age(p, ctx, dyn)
        dv = getattr((dyn or {}).get(p.cid), "value", None)
        if rec == "contend":
            if p.cid in starters or not isinstance(dv, (int, float)) or dv <= 0:
                continue
            sell.append((float(dv), Asset(p, None, now, f, age,
                                          f"not in your lineup this season (dynasty value {dv:.1f}): "
                                          f"turn him into a starter")))
        elif f is not None and now > 0 and (rec == "rebuild" or (age or 0.0) >= BALANCED_SELL_AGE):
            drop = now - f
            if drop <= 0:
                continue
            a = f"age {age:.0f}, " if age is not None else ""
            sell.append((drop * now, Asset(p, None, now, f, age,
                                           f"{a}{now:.2f} FPG now, about {f:.2f} over the next "
                                           f"{FUTURE_YEARS} seasons: sell while his value is high")))
    o.sell = [a for _, a in sorted(sell, key=lambda x: -x[0])[:LIST_N]]

    buy: list[tuple[float, Asset]] = []
    for t in ctx.teams:
        if t.owner_is_me:
            continue
        their = direction.get(t.team_id)
        for p in t.players:
            if p.status in ("ir", "ltir", "out", "suspended"):
                continue
            now, f, age = _now(values, p), fut.get(p.cid), _age(p, ctx, dyn)
            if rec in ("contend", "contender"):
                gain = now - weakest
                if gain <= 0:
                    continue
                seller = their is not None and their.label in ("rebuild", "pretender")
                score = gain * (1.2 if seller else 1.0)
                why = f"{now:.2f} FPG now, +{gain:.2f} over your weakest starter" + (
                    f"; {t.name} is a likely seller" if seller else "")
            elif rec == "rebuild":
                if f is None or f <= now or (age is not None and age > REBUILD_BUY_MAX_AGE):
                    continue
                buyer = their is not None and their.label == "contend"
                score = f * (1.2 if buyer else 1.0)
                a = f"age {age:.0f}, " if age is not None else ""
                why = f"{a}{now:.2f} FPG now rising to about {f:.2f}" + (
                    f"; {t.name} is contending and may pay in vets" if buyer else "")
            elif rec == "balanced":
                if f is None or age is None or not (PRIME_AGES[0] <= age <= PRIME_AGES[1]):
                    continue
                score = min(now, f)
                why = f"age {age:.0f}, {now:.2f} FPG now and about {f:.2f} later: value both ways"
            else:
                continue
            buy.append((score, Asset(p, t.name, now, f, age, why)))
    o.buy = [a for _, a in sorted(buy, key=lambda x: -x[0])[:LIST_N]]


def build_outlook(ctx: LeagueContext, values: Mapping[str, Any], dynasty_values: Mapping[str, Any] | None = None,
                  provider: Any = None, sims: int = SIMS) -> Outlook:
    """Every team's this-season and future lineup strength, playoff odds, and my recommended
    mode with sell / buy lists (module docstring)."""
    dyn = dynasty_values if ctx.dynasty else None
    now_vals = {p.cid: _now(values, p) for t in ctx.teams for p in t.players}
    fut_by_year: list[dict[str, float]] = [{} for _ in range(FUTURE_YEARS)]
    fut_avg: dict[str, float] = {}
    if dyn is not None:
        for t in ctx.teams:
            for p in t.players:
                fv = future_values(p, ctx, values, dyn)
                for y, v in enumerate(fv):
                    fut_by_year[y][p.cid] = v
                fut_avg[p.cid] = sum(fv) / len(fv)
    rows: list[TeamOutlook] = []
    for t in ctx.teams:
        rec = t.record
        gp = sum(rec) if rec else 0
        rows.append(TeamOutlook(
            team=t, record=rec, win_pct=(rec[0] + 0.5 * rec[2]) / gp if rec and gp else None,
            now=_lineup(t.players, now_vals, ctx),
            future=[_lineup(t.players, fut_by_year[y], ctx) for y in range(FUTURE_YEARS)] if dyn is not None else []))
    for i, r in enumerate(sorted(rows, key=lambda r: -r.now)):
        r.now_rank = i + 1
    if dyn is not None:
        for i, r in enumerate(sorted(rows, key=lambda r: -(r.future_avg or 0.0))):
            r.future_rank = i + 1
    weeks, wsrc = _weeks_left(ctx, provider)
    spots, ssrc = _playoff_teams(ctx, provider)
    odds = playoff_odds(rows, weeks, spots, sims=sims)
    n = len(rows)
    for r in rows:
        r.playoff_odds = odds.get(r.team.team_id)
        if dyn is not None:
            r.label = recommend_mode(r, n)[0]
        else:
            r.label = redraft_label(r.playoff_odds or 0.0)
    rows.sort(key=lambda r: r.now_rank)
    me = next((r for r in rows if r.mine), None)
    out = Outlook(teams=rows, me=me, dynasty=dyn is not None, recommended=None,
                  current=ctx.dynasty_mode if ctx.dynasty else None, weeks_left=weeks, playoff_teams=spots)
    out.notes = [f"Playoff odds: {weeks} regular-season weeks left ({wsrc}), {spots} playoff spots ({ssrc}); "
                 f"random opponents, {sims} simulations (approximate)"]
    if me is not None:
        if dyn is not None:
            out.recommended, out.reasons = recommend_mode(me, n)
        else:
            out.recommended = me.label
            odds_me = me.playoff_odds or 0.0
            out.reasons = [f"Playoff odds about {odds_me:.0%} (lineup #{me.now_rank} of {n})",
                           {"contender": "Buy this season's points: every week counts",
                            "bubble": "Make moves that raise your weekly floor; avoid long shots",
                            "pretender": "Chase upside: hot streaks and high-variance adds"}[me.label]]
        _assets(out, ctx, values, dyn, fut_avg)
    return out


def recommendation_text(o: Outlook) -> str:
    """"recommended: rebuild (currently balanced)" / "contender"."""
    if o.recommended is None:
        return "no recommendation (your team was not found)"
    if not o.dynasty:
        return o.recommended
    cur = f" (currently {o.current})" if o.current else ""
    same = " - matches your setting" if o.current == o.recommended else ""
    return f"recommended: {o.recommended}{cur}{same}"


def outlook_json(o: Outlook) -> dict[str, Any]:
    def asset(a: Asset) -> dict[str, Any]:
        return {"cid": a.player.cid, "name": a.player.name, "owner": a.owner, "now_fpg": round(a.now, 3),
                "future_fpg": None if a.future is None else round(a.future, 3),
                "age": None if a.age is None else round(a.age, 1), "why": a.why}

    return {"recommended": o.recommended, "current": o.current, "dynasty": o.dynasty,
            "text": recommendation_text(o), "reasons": o.reasons, "weeks_left": o.weeks_left,
            "playoff_teams": o.playoff_teams, "notes": o.notes,
            "teams": [{"team_id": r.team.team_id, "name": r.team.name, "mine": r.mine,
                       "record": list(r.record) if r.record else None, "now_fpg": round(r.now, 3),
                       "now_rank": r.now_rank, "future_fpg": [round(x, 3) for x in r.future],
                       "future_rank": r.future_rank,
                       "playoff_odds": None if r.playoff_odds is None else round(r.playoff_odds, 3),
                       "label": r.label} for r in o.teams],
            "sell": [asset(a) for a in o.sell], "buy": [asset(a) for a in o.buy]}
