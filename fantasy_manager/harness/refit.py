"""Bounded auto-correction of a few valuation parameters (M3, ``fm harness refit``).

Replay table. Every matured weekly projection snapshot in the ledger (``projections`` rows with
archive-v2 ``inputs``, one snapshot per ISO week as in ``harness.metrics``) becomes one
``ReplayObs`` per player: the archived inputs rebuilt into a ``Player`` (season-to-date and
last-30/15/7 lines, league projection, status), the history baseline (rates + GP), the
positional means of that day (archive header ``position_means``) and the realized targets:
FPG over the next 28 days (players with >= ``MIN_GP_28`` GP, the bar's projection metric) and
points over the next 7 days (the week projection).

``inseason_projection(obs, params)`` replays the live valuation for one observation with a
candidate params mapping by calling the very same code the app runs
(``valuation.valuate.player_rates``, which is ``_rates_for`` without reasons), so the replay
cannot drift from the valuation. For a historical checkpoint (``backtest.fit.InseasonObs``,
no league projection) it applies the FPG-space in-season formula with the candidate k and
recency weights. ``week_projection`` adds availability, games, off-nights and start share.

Objective. Tier A groups (``k_inseason``, ``recency_weights``, ``projection_weight``,
``k_projection``) minimise the pooled loss ``(1 - w) * MAE_hist + w * MAE_live`` with
``w = n_live / (n_live + 3000)``: MAE_hist over the ~10k historical in-season checkpoints
(``data/backtest``, rest-of-season FPG, loaded lazily and cached per scoring), MAE_live over
the replay table's 28-day FPG. Tier B groups (``availability`` week multipliers,
``start_share`` prior / k, ``offnight_bonus``) minimise the live 7-day points MAE (there is no
historical analogue). Coordinate descent over a grid inside the per-cycle step bounds, one
group at a time from the current values; the <= 2 groups with the largest marginal gain are
combined. Forward-chained validation: the search sees only the weeks before the two most
recent matured weeks, which are held out for the gate.

Gate (``gate``). >= 400 matured live observations over >= 4 weekly snapshots (goalie knobs
>= 150 goalie observations); per-cycle step bounds (k +-20%, recency +-0.05 each and summing
to 1, projection weight +-0.10, availability +-0.10, start-share prior +-0.05 / k +-20%,
off-night bonus +-0.02); at most 2 parameter groups; holdout MAE >= 1% better with the
player-clustered paired-bootstrap 90% CI of the improvement above 0; historical MAE no more
than 0.5% worse. Not eligible at all: age curves, ``age_yoy``, ``k_baseline``, dynasty weights,
recommender thresholds (they are not knobs, so a candidate cannot carry them).

Calendar. No refit before 2026-11-02 (``--force`` bypasses only this lock); refit days every
14 days from then; auto-apply (``fm harness daily`` on a refit day, ``prefs.harness_auto_apply``)
only from 2026-11-16 and only Tier A; Tier B is proposal-only until 2026-12-01.

Champion / challenger (``shadow_check``, called from ``metrics.grade_week``): after a promotion
the previous version is shadow-scored every graded week on the newly matured snapshot; if it
beats the active version two weeks running, the active version is rolled back automatically.
"""
from __future__ import annotations

import json
import math
from dataclasses import asdict, dataclass, field
from datetime import date, timedelta
from functools import lru_cache
from pathlib import Path
from typing import Any, Callable, Iterable, Mapping, Sequence

from ..backtest.fit import InseasonObs
from ..backtest.fit import inseason_projection as _hist_projection
from ..models import Player, StatLine
from ..valuation import params as vparams
from ..valuation.schedule import proj_week, shrunk_share
from ..valuation.valuate import player_rates, position_group
from .ledger import Ledger
from .metrics import MIN_GP_28, bootstrap_skill, weekly_snapshots
from .outcomes import Realized, days_between, points_scorer, scoring_config
from .params_store import PACKAGED, ParamsStore

# ---- calendar
REFIT_START = date(2026, 11, 2)
AUTO_APPLY_START = date(2026, 11, 16)
TIER_B_APPLY_START = date(2026, 12, 1)
REFIT_EVERY_DAYS = 14

# ---- gate
MIN_LIVE_OBS = 400
MIN_WEEKS = 4
MIN_GOALIE_OBS = 150
MAX_GROUPS = 2
MIN_HOLDOUT_GAIN = 0.01
MAX_HIST_WORSE = 0.005
CI_LEVEL = 0.90
BOOTSTRAP_N = 500
BOOTSTRAP_SEED = 20261102
HIST_POOL_K = 3000
HOLDOUT_WEEKS = 2
SEARCH_ROUNDS = 2
GRID_POINTS = 9            # per knob, evenly spread over [-bound, +bound]
EPS = 1e-9

TIER_A = ("k_inseason", "recency_weights", "projection_weight", "k_projection")
TIER_B = ("availability", "start_share", "offnight_bonus")
GROUPS = TIER_A + TIER_B
NOT_ELIGIBLE = ("age curves", "age_yoy", "k_baseline", "dynasty weights", "recommender thresholds")
AVAIL_STATUSES = ("dtd", "out", "ir", "ltir", "suspended")
RECENCY_SPLITS = ("last30", "last15", "last7")


# --------------------------------------------------------------------------- knobs

@dataclass(frozen=True)
class Knob:
    name: str
    group: str
    path: tuple[str, ...]      # where it lives in the params JSON
    step: float                # per-cycle bound (fraction of the current value when relative)
    relative: bool
    lo: float = 0.0
    hi: float = math.inf
    goalie: bool = False
    objective: str = "fpg"     # fpg (28-day FPG, pooled with history) | week (7-day points)

    def bound_label(self) -> str:
        return f"+-{self.step:.0%}" if self.relative else f"+-{self.step:g}"

    def bounds(self, current: float) -> tuple[float, float]:
        d = abs(current) * self.step if self.relative else self.step
        return max(self.lo, current - d), min(self.hi, current + d)


KNOBS: tuple[Knob, ...] = (
    Knob("k_inseason.skater", "k_inseason", ("inseason", "k_skater"), 0.20, True),
    Knob("k_inseason.goalie", "k_inseason", ("inseason", "k_goalie"), 0.20, True, goalie=True),
    Knob("recency.season", "recency_weights", ("inseason", "recency_weights", "season"), 0.05, False, 0.0, 1.0),
    Knob("recency.last30", "recency_weights", ("inseason", "recency_weights", "last30"), 0.05, False, 0.0, 1.0),
    Knob("recency.last15", "recency_weights", ("inseason", "recency_weights", "last15"), 0.05, False, 0.0, 1.0),
    Knob("recency.last7", "recency_weights", ("inseason", "recency_weights", "last7"), 0.05, False, 0.0, 1.0),
    Knob("projection_weight", "projection_weight", ("projection", "weight"), 0.10, False, 0.0, 1.0),
    Knob("k_projection.skater", "k_projection", ("projection", "k_skater"), 0.20, True),
    Knob("k_projection.goalie", "k_projection", ("projection", "k_goalie"), 0.20, True, goalie=True),
    *(Knob(f"availability.{st}.week", "availability", ("availability", st, "week"), 0.10, False, 0.0, 1.0,
           objective="week") for st in AVAIL_STATUSES),
    Knob("start_share.prior", "start_share", ("schedule", "start_share_prior"), 0.05, False, 0.0, 1.0,
         goalie=True, objective="week"),
    Knob("start_share.k", "start_share", ("schedule", "start_share_k"), 0.20, True, goalie=True, objective="week"),
    Knob("offnight_bonus", "offnight_bonus", ("schedule", "offnight_bonus"), 0.02, False, 0.0, 0.2,
         objective="week"),
)
KNOB = {k.name: k for k in KNOBS}
GROUP_KNOBS: dict[str, tuple[Knob, ...]] = {g: tuple(k for k in KNOBS if k.group == g) for g in GROUPS}
GROUP_OBJECTIVE = {g: GROUP_KNOBS[g][0].objective for g in GROUPS}


def tier(group: str) -> str:
    return "A" if group in TIER_A else "B"


def current_knobs(params: Mapping[str, Any] | None = None) -> dict[str, float]:
    """Every knob's value under ``params`` (default: the loaded params), via the accessors."""
    ki, kp = vparams.k_inseason(params=params), vparams.k_projection(params=params)
    rw, av = vparams.recency_weights(params), vparams.availability_table(params)
    out = {"k_inseason.skater": ki["skater"], "k_inseason.goalie": ki["goalie"],
           **{f"recency.{s}": rw[s] for s in ("season", *RECENCY_SPLITS)},
           "projection_weight": vparams.projection_weight(params),
           "k_projection.skater": kp["skater"], "k_projection.goalie": kp["goalie"],
           **{f"availability.{st}.week": av.get(st, (1.0, 1.0))[0] for st in AVAIL_STATUSES},
           "start_share.prior": vparams.start_share_prior(params), "start_share.k": vparams.start_share_k(params),
           "offnight_bonus": vparams.offnight_bonus(params)}
    return {k: float(v) for k, v in out.items()}


def changed_knobs(candidate: Mapping[str, float], current: Mapping[str, float]) -> list[str]:
    return sorted(k for k, v in candidate.items() if abs(float(v) - float(current.get(k, v))) > EPS)


def changed_groups(candidate: Mapping[str, float], current: Mapping[str, float]) -> list[str]:
    return sorted({KNOB[k].group for k in changed_knobs(candidate, current) if k in KNOB})


def knobs_override(knobs: Mapping[str, float], names: Iterable[str] | None = None) -> dict[str, Any]:
    """The nested params override for the knobs ``names`` (default all given). Recency weights
    are always written as a complete set of four (the accessor rejects partial sets)."""
    names = set(knobs if names is None else names)
    if any(KNOB[n].group == "recency_weights" for n in names if n in KNOB):
        names |= {f"recency.{s}" for s in ("season", *RECENCY_SPLITS)}
    out: dict[str, Any] = {}
    for n in sorted(names):
        if n not in KNOB or n not in knobs:
            continue
        d = out
        path = KNOB[n].path
        for key in path[:-1]:
            d = d.setdefault(key, {})
        d[path[-1]] = float(knobs[n])
    return out


def params_for(knobs: Mapping[str, float], base: Mapping[str, Any] | None = None) -> dict[str, Any]:
    """A full params mapping: ``base`` (default the loaded params) with ``knobs`` applied."""
    return vparams.deep_merge(vparams.load_params() if base is None else base, knobs_override(knobs))


# --------------------------------------------------------------------------- replay table

@dataclass
class ReplayObs:
    league: str
    snap: date
    week: date                      # ISO-week Monday of the snapshot
    player: int                     # NHL id (bootstrap cluster)
    pool: str                       # F / D / G
    p: Player
    means: dict[str, dict[str, float]]
    history: tuple[dict[str, float], int, str]
    sc: Any                         # the league's PointsScoring
    status: str = "unknown"
    games: int | None = None
    offnight: int | None = None
    share: float | None = None
    share_src: str | None = None
    share_starts: float | None = None
    share_tg: float | None = None
    fpg_archived: float | None = None
    proj_week_archived: float | None = None
    real_fpg28: float | None = None  # realized FPG over the next 28 days (>= MIN_GP_28 GP)
    gp28: int = 0
    real_pts7: float | None = None   # realized points over the next 7 days
    gp7: int = 0
    exact: bool = True               # False: inputs predate the replay fields (means / zeros)

    @property
    def is_goalie(self) -> bool:
        return self.pool == "G"

    def to_inseason(self) -> InseasonObs:
        """The FPG-space view of this row (the backtest's ``InseasonObs`` shape)."""
        def f(line: StatLine | None) -> tuple[int, float]:
            return (line.gp, float(self.sc.value(line.per_game()))) if line is not None and line.gp > 0 else (0, 0.0)

        season = self.p.lines.get("season")
        proj = self.p.lines.get("projected")
        hist_rates, hist_gp, _ = self.history
        return InseasonObs(
            season=0, day=self.snap.isoformat(), player_id=self.player, group=self.pool,
            gp_td=season.gp if season else 0, fpg_td=f(season)[1], fpg_base=0.0, has_base=False,
            splits={s: f(self.p.lines.get(s)) for s in ("last30", "last15", "last7")},
            gp_rest=self.gp28, fpg_rest=self.real_fpg28 if self.real_fpg28 is not None else 0.0,
            fpg_app=self.fpg_archived or 0.0,
            fpg_proj=f(proj)[1] if proj is not None and proj.gp > 0 else None,
            fpg_hist=float(self.sc.value(hist_rates)) if hist_rates else None, status=self.status,
            gp_hist3=hist_gp or None, start_share=self.share)


def inseason_projection(obs: ReplayObs | InseasonObs, params: Mapping[str, Any] | None = None) -> float:
    """Healthy FPG for one observation under ``params`` (a full params mapping; default the
    loaded ones). Live rows replay ``valuation.valuate.player_rates`` itself; historical
    checkpoints use the backtest's FPG-space formula with the candidate k and recency weights."""
    if isinstance(obs, InseasonObs):
        k = vparams.k_inseason("goalie" if obs.group == "G" else "skater", params)
        return _hist_projection(obs, k, vparams.recency_weights(params))
    rates = player_rates(obs.p, obs.means, None, params=params, history=obs.history)
    return float(obs.sc.value(rates)) if rates else 0.0


def week_projection(obs: ReplayObs, params: Mapping[str, Any] | None = None,
                    fpg: float | None = None) -> float | None:
    """Projected points over the 7-day window under ``params`` (None without a schedule)."""
    if obs.games is None:
        return None
    fpg = inseason_projection(obs, params) if fpg is None else fpg
    a_week = vparams.availability(obs.status, "week", params)
    share = 1.0
    if obs.is_goalie:
        if obs.share_src == "history" and obs.share_starts is not None and obs.share_tg is not None:
            share = shrunk_share(obs.share_starts, obs.share_tg, params=params)
        elif obs.share is not None:
            share = obs.share
    return proj_week(fpg, a_week, obs.games, obs.offnight or 0, params) * share


def _line(split: str, d: Any) -> StatLine | None:
    if not isinstance(d, Mapping) or d.get("gp") is None:
        return None
    try:
        return StatLine(split=split, gp=int(d["gp"]), stats={k: float(v) for k, v in (d.get("stats") or {}).items()})
    except (TypeError, ValueError):
        return None


_STATUSES = ("healthy", "dtd", "out", "ir", "ltir", "suspended", "unknown")


def obs_from_inputs(league: str, snap: date, row: Mapping[str, Any], inputs: Mapping[str, Any],
                    means: Mapping[str, Mapping[str, float]] | None, sc: Any) -> ReplayObs | None:
    """One replay row from an archived projection row + its v2 inputs (None if unusable)."""
    if row.get("nhl_id") is None:
        return None
    positions = [str(x) for x in (inputs.get("positions") or str(row.get("positions") or "").split(",")) if x]
    status = inputs.get("status") or row.get("status") or "unknown"
    status = status if status in _STATUSES else "unknown"
    lines: dict[str, StatLine] = {}
    for split in ("season", "last30", "last15", "last7"):
        ln = _line(split, inputs.get(split))
        if ln is not None:
            lines[split] = ln
    proj = _line("projected", inputs.get("projection"))
    if proj is not None:
        lines["projected"] = proj
    p = Player(cid=str(row.get("cid") or row["nhl_id"]), name=str(row.get("name") or ""), name_norm="", ids={},
               team=None, positions=positions or ["C"], status=status, lines=lines)
    for f in ("ixg_per_game", "goals_minus_ixg", "xg_split"):   # v3: the in-season xG goal shrink
        if inputs.get(f) is not None:
            setattr(p, f, inputs[f])
    hist = inputs.get("history") or {}
    history = ({k: float(v) for k, v in (hist.get("rates") or {}).items()}, int(hist.get("gp") or 0), "")
    share = inputs.get("share") or {}
    zeros_ok = all((inputs.get(s) or {}).get("zeros", False) or not (inputs.get(s) or {}).get("gp")
                   for s in ("season", "last30", "last15", "last7"))
    return ReplayObs(
        league=league, snap=snap, week=snap - timedelta(days=snap.weekday()), player=int(row["nhl_id"]),
        pool=position_group(p), p=p, means={g: dict(m) for g, m in (means or {}).items()}, history=history, sc=sc,
        status=status, games=inputs.get("games_next7"), offnight=inputs.get("offnight_next7"),
        share=inputs.get("start_share"), share_src=share.get("source"), share_starts=share.get("starts"),
        share_tg=share.get("team_games"),
        fpg_archived=None if row.get("fpg") is None else float(row["fpg"]),
        proj_week_archived=None if row.get("proj_week") is None else float(row["proj_week"]),
        exact=bool(means) and zeros_ok)


def _means(ledger: Ledger, league: str, day: str) -> dict[str, dict[str, float]] | None:
    rows = ledger.query("SELECT value FROM meta WHERE key=?", (f"means:{league}:{day}",))
    if rows:
        try:
            m = json.loads(rows[0]["value"])
            return m if isinstance(m, dict) else None
        except ValueError:
            return None
    # older ingests: read the archive header itself
    path = Path(ledger.data_dir) / "archive" / f"projections-{league}-{day}.json"
    try:
        m = json.loads(path.read_text(encoding="utf-8")).get("position_means")
    except (OSError, ValueError, AttributeError):
        return None
    return m if isinstance(m, dict) and m else None


def known_leagues(ledger: Ledger) -> list[str]:
    return [r["league"] for r in ledger.query("SELECT DISTINCT league FROM projections ORDER BY league")]


def replay_table(ledger: Ledger, as_of: date, leagues: Sequence[str] | None = None
                 ) -> tuple[list[ReplayObs], dict[str, Any]]:
    """Every matured (weekly snapshot, player) observation as of ``as_of`` (targets only from
    windows that ended before ``as_of`` and were fully pulled)."""
    info: dict[str, Any] = {"leagues": {}, "skipped": {}}
    out: list[ReplayObs] = []
    last_day = as_of - timedelta(days=1)
    for league in (list(leagues) if leagues else known_leagues(ledger)):
        cfg, _ = scoring_config(ledger, league)
        sc = points_scorer(cfg)
        if sc is None:
            info["skipped"][league] = "not a points league (or no scoring known)"
            continue
        days = [r["as_of"] for r in ledger.query("SELECT DISTINCT as_of FROM projections WHERE league=? "
                                                 "ORDER BY as_of", (league,))]
        if not days:
            continue
        realized = Realized(ledger, sc, None, as_of)
        n0 = len(out)
        for _, snap in weekly_snapshots(days):
            d28 = days_between(snap + timedelta(days=1), snap + timedelta(days=28))
            d7 = d28[:7]
            ok28 = d28[-1] <= last_day and realized.covered(d28)
            ok7 = d7[-1] <= last_day and realized.covered(d7)
            if not (ok28 or ok7):
                continue
            means = _means(ledger, league, snap.isoformat())
            for r in ledger.query("SELECT cid, name, nhl_id, positions, status, fpg, proj_week, inputs_json "
                                  "FROM projections WHERE league=? AND as_of=?", (league, snap.isoformat())):
                try:
                    inputs = json.loads(r["inputs_json"] or "null")
                except ValueError:
                    inputs = None
                if not isinstance(inputs, dict):
                    continue
                o = obs_from_inputs(league, snap, r, inputs, means, sc)
                if o is None:
                    continue
                if ok28:
                    gp, pts = realized.window(o.player, d28)
                    if gp >= MIN_GP_28:
                        o.real_fpg28, o.gp28 = pts / gp, gp
                if ok7 and o.games is not None:
                    gp, pts = realized.window(o.player, d7)
                    o.real_pts7, o.gp7 = pts, gp
                if o.real_fpg28 is not None or o.real_pts7 is not None:
                    out.append(o)
        info["leagues"][league] = len(out) - n0
    return out, info


# --------------------------------------------------------------------------- historical checkpoints

_HIST_CACHE: dict[tuple[str, str], list[InseasonObs]] = {}


def historical_observations(data_dir: str | Path, sc: Any) -> list[InseasonObs]:
    """The backtest's in-season checkpoint observations (``data/backtest``: player_seasons.json
    + windows.json) under scoring ``sc``; loaded lazily, cached per (data dir, scoring). [] when
    the backtest data is missing."""
    key = (str(Path(data_dir).resolve()), json.dumps([sorted(sc.weights.items()), sorted(sc.goalie_weights.items())]))
    if key not in _HIST_CACHE:
        from ..backtest.data import load_table, load_windows
        from ..backtest.evaluate import inseason_observations

        try:
            table, windows = load_table(data_dir), load_windows(data_dir)
            obs = inseason_observations(table, windows, sc, sorted(windows)) if table.seasons and windows else []
        except Exception:  # noqa: BLE001 - missing / corrupt backtest data: no historical term
            obs = []
        _HIST_CACHE[key] = obs
    return _HIST_CACHE[key]


# --------------------------------------------------------------------------- losses

def mae(errs: Sequence[float]) -> float | None:
    return sum(abs(e) for e in errs) / len(errs) if errs else None


def fpg_errors(obs: Sequence[ReplayObs], params: Mapping[str, Any]) -> list[float]:
    return [inseason_projection(o, params) - o.real_fpg28 for o in obs if o.real_fpg28 is not None]


def week_errors(obs: Sequence[ReplayObs], params: Mapping[str, Any]) -> list[float]:
    out = []
    for o in obs:
        if o.real_pts7 is None:
            continue
        pw = week_projection(o, params)
        if pw is not None:
            out.append(pw - o.real_pts7)
    return out


def hist_mae(hist: Sequence[InseasonObs], params: Mapping[str, Any]) -> float | None:
    return mae([_hist_projection(o, vparams.k_inseason("goalie" if o.group == "G" else "skater", params),
                                 vparams.recency_weights(params)) - o.fpg_rest for o in hist])


def pool_weight(n_live: int) -> float:
    return n_live / (n_live + HIST_POOL_K) if n_live > 0 else 0.0


class Losses:
    """Memoised objective evaluation for one data split."""

    def __init__(self, live: Sequence[ReplayObs], hist: Sequence[InseasonObs], base: Mapping[str, Any]):
        self.fpg_obs = [o for o in live if o.real_fpg28 is not None]
        self.week_obs = [o for o in live if o.real_pts7 is not None and o.games is not None]
        self.hist_obs = list(hist)
        self.base = base
        self.w = pool_weight(len(self.fpg_obs)) if self.hist_obs else 1.0
        self._hist_memo: dict[tuple, float | None] = {}

    def params(self, knobs: Mapping[str, float]) -> dict[str, Any]:
        return params_for(knobs, self.base)

    def hist(self, knobs: Mapping[str, float]) -> float | None:
        key = tuple(round(knobs[k], 10) for k in ("k_inseason.skater", "k_inseason.goalie", "recency.season",
                                                   "recency.last30", "recency.last15", "recency.last7"))
        if key not in self._hist_memo:
            self._hist_memo[key] = hist_mae(self.hist_obs, self.params(knobs)) if self.hist_obs else None
        return self._hist_memo[key]

    def live_fpg(self, knobs: Mapping[str, float]) -> float | None:
        return mae(fpg_errors(self.fpg_obs, self.params(knobs)))

    def live_week(self, knobs: Mapping[str, float]) -> float | None:
        return mae(week_errors(self.week_obs, self.params(knobs)))

    def loss(self, knobs: Mapping[str, float], objective: str) -> float:
        if objective == "week":
            v = self.live_week(knobs)
            return math.inf if v is None else v
        live = self.live_fpg(knobs)
        h = self.hist(knobs)
        if live is None:
            return math.inf if h is None else h
        return live if h is None else (1 - self.w) * h + self.w * live


# --------------------------------------------------------------------------- search

def knob_grid(knob: Knob, current: float, points: int = GRID_POINTS) -> list[float]:
    lo, hi = knob.bounds(current)
    if hi - lo <= EPS:
        return [current]
    vals = {current}
    for i in range(points):
        v = lo + (hi - lo) * i / (points - 1)
        vals.add(min(max(round(v, 6), lo), hi))
    return sorted(vals)


def recency_moves(cur: Mapping[str, float], at: Mapping[str, float], points: int = GRID_POINTS
                  ) -> list[dict[str, float]]:
    """Recency candidates from ``at`` moving one split at a time (the season weight absorbs the
    difference), keeping every weight within +-0.05 of ``cur`` (the cycle's start), >= 0 and the
    sum at 1."""
    out = []
    for split in RECENCY_SPLITS:
        name = f"recency.{split}"
        for v in knob_grid(KNOB[name], cur[name], points):
            cand = {f"recency.{s}": at[f"recency.{s}"] for s in RECENCY_SPLITS}
            cand[name] = v
            season = 1.0 - sum(cand.values())
            lo, hi = KNOB["recency.season"].bounds(cur["recency.season"])
            if season < lo - EPS or season > hi + EPS or season < 0:
                continue
            cand["recency.season"] = season
            out.append(cand)
    return out


def search_group(group: str, current: Mapping[str, float], losses: Losses, goalies_ok: bool = True,
                 rounds: int = SEARCH_ROUNDS) -> tuple[dict[str, float], float, float]:
    """Coordinate descent over ``group``'s knobs from ``current`` inside the step bounds:
    (best knobs, loss before, loss after) on ``losses``' data."""
    objective = GROUP_OBJECTIVE[group]
    best = dict(current)
    before = best_loss = losses.loss(best, objective)
    for _ in range(rounds):
        improved = False
        if group == "recency_weights":
            for cand in recency_moves(current, best):
                trial = {**best, **cand}
                lv = losses.loss(trial, objective)
                if lv < best_loss - 1e-12:
                    best, best_loss, improved = trial, lv, True
        else:
            for knob in GROUP_KNOBS[group]:
                if knob.goalie and not goalies_ok:
                    continue
                for v in knob_grid(knob, current[knob.name]):
                    trial = {**best, knob.name: v}
                    lv = losses.loss(trial, objective)
                    if lv < best_loss - 1e-12:
                        best, best_loss, improved = trial, lv, True
        if not improved:
            break
    return best, before, best_loss


# --------------------------------------------------------------------------- gate

@dataclass
class ObjectiveStats:
    objective: str
    n_holdout: int = 0
    holdout_before: float | None = None
    holdout_after: float | None = None
    ci_lo: float | None = None          # 90% CI of the relative holdout improvement
    ci_hi: float | None = None
    hist_before: float | None = None
    hist_after: float | None = None
    n_hist: int = 0

    @property
    def gain(self) -> float | None:
        if self.holdout_before is None or self.holdout_after is None or self.holdout_before <= 0:
            return None
        return (self.holdout_before - self.holdout_after) / self.holdout_before

    @property
    def hist_change(self) -> float | None:
        if self.hist_before is None or self.hist_after is None or self.hist_before <= 0:
            return None
        return (self.hist_after - self.hist_before) / self.hist_before


@dataclass
class GateStats:
    n_live: int = 0                 # matured 28-day live observations
    n_goalie: int = 0
    weeks: int = 0                  # distinct weekly snapshots among them
    n_week: int = 0                 # matured 7-day observations
    objectives: dict[str, ObjectiveStats] = field(default_factory=dict)


@dataclass
class GateResult:
    passed: bool
    reasons: list[str]
    checks: dict[str, bool] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


def insufficient_reasons(stats: GateStats, goalie_change: bool = False) -> list[str]:
    out = []
    if stats.n_live < MIN_LIVE_OBS:
        out.append(f"insufficient live obs: {stats.n_live} matured 28-day player-windows, need {MIN_LIVE_OBS} "
                   f"({MIN_LIVE_OBS - stats.n_live} more)")
    if stats.weeks < MIN_WEEKS:
        out.append(f"insufficient weekly snapshots: {stats.weeks} matured, need {MIN_WEEKS} "
                   f"({MIN_WEEKS - stats.weeks} more)")
    if goalie_change and stats.n_goalie < MIN_GOALIE_OBS:
        out.append(f"insufficient goalie obs for goalie parameters: {stats.n_goalie}, need {MIN_GOALIE_OBS} "
                   f"({MIN_GOALIE_OBS - stats.n_goalie} more)")
    return out


def gate(candidate: Mapping[str, float], current: Mapping[str, float], stats: GateStats) -> GateResult:
    """Every guardrail a candidate must pass before it may be promoted (see module docstring)."""
    reasons: list[str] = []
    checks: dict[str, bool] = {}
    unknown = sorted(set(candidate) - set(KNOB))
    checks["eligible"] = not unknown
    if unknown:
        reasons.append(f"not eligible for refitting: {', '.join(unknown)}")
    changed = [k for k in changed_knobs(candidate, current) if k in KNOB]
    goalie_change = any(KNOB[k].goalie for k in changed)
    data = insufficient_reasons(stats, goalie_change)
    if changed and any(GROUP_OBJECTIVE[KNOB[k].group] == "week" for k in changed) and stats.n_week < MIN_LIVE_OBS:
        data.append(f"insufficient 7-day live obs: {stats.n_week}, need {MIN_LIVE_OBS} "
                    f"({MIN_LIVE_OBS - stats.n_week} more)")
    checks["data"] = not data
    reasons += data
    checks["changed"] = bool(changed)
    if not changed:
        reasons.append("no parameter change")
    out_of_bounds = []
    for k in changed:
        lo, hi = KNOB[k].bounds(float(current[k]))
        v = float(candidate[k])
        if not (lo - EPS <= v <= hi + EPS) or not math.isfinite(v):
            out_of_bounds.append(f"{k} {current[k]:g} -> {v:g} outside [{lo:g}, {hi:g}] ({KNOB[k].bound_label()})")
    rec = [float(candidate.get(f"recency.{s}", current.get(f"recency.{s}", 0.0))) for s in ("season", *RECENCY_SPLITS)]
    if abs(sum(rec) - 1.0) > EPS or min(rec) < 0:
        out_of_bounds.append(f"recency weights must be >= 0 and sum to 1 (sum {sum(rec):.12f})")
    checks["bounds"] = not out_of_bounds
    reasons += out_of_bounds
    groups = sorted({KNOB[k].group for k in changed})
    checks["groups"] = len(groups) <= MAX_GROUPS
    if len(groups) > MAX_GROUPS:
        reasons.append(f"{len(groups)} parameter groups changed ({', '.join(groups)}), at most {MAX_GROUPS} per cycle")
    objectives = sorted({GROUP_OBJECTIVE[g] for g in groups})
    stat_ok = True
    for obj in objectives:
        s = stats.objectives.get(obj)
        if s is None or s.gain is None:
            stat_ok = False
            reasons.append(f"{obj}: no holdout evaluation")
            continue
        if s.gain < MIN_HOLDOUT_GAIN:
            stat_ok = False
            reasons.append(f"{obj}: holdout MAE {s.holdout_before:.4f} -> {s.holdout_after:.4f} "
                           f"({s.gain:+.2%}), needs >= {MIN_HOLDOUT_GAIN:.0%} better")
        if s.ci_lo is None or s.ci_lo <= 0:
            stat_ok = False
            ci = "n/a" if s.ci_lo is None else f"[{s.ci_lo:+.2%}, {s.ci_hi:+.2%}]"
            reasons.append(f"{obj}: 90% bootstrap CI of the improvement {ci} does not exclude 0")
        if obj == "fpg":
            if s.hist_before is None:
                stat_ok = False
                reasons.append("fpg: historical checkpoints unavailable (run `fm backtest data`): cannot check "
                               "that history does not get worse")
            elif s.hist_change is not None and s.hist_change > MAX_HIST_WORSE:
                stat_ok = False
                reasons.append(f"fpg: historical MAE {s.hist_before:.4f} -> {s.hist_after:.4f} "
                               f"({s.hist_change:+.2%}), may not worsen by more than {MAX_HIST_WORSE:.1%}")
    checks["statistics"] = stat_ok and bool(objectives)
    passed = all(checks.values())
    return GateResult(passed, reasons, checks)


# --------------------------------------------------------------------------- calendar / policy

def is_refit_day(day: date) -> bool:
    return day >= REFIT_START and (day - REFIT_START).days % REFIT_EVERY_DAYS == 0


def next_refit_day(day: date) -> date:
    if day <= REFIT_START:
        return REFIT_START
    k = -(-(day - REFIT_START).days // REFIT_EVERY_DAYS)
    return REFIT_START + timedelta(days=k * REFIT_EVERY_DAYS)


def allowed_groups(mode: str, today: date) -> tuple[str, ...]:
    """Groups the search may change: proposals / dry runs everything; a manual ``--apply`` Tier
    A (plus Tier B from 2026-12-01); the daily auto-apply Tier A only."""
    if mode == "auto":
        return TIER_A
    if mode == "apply":
        return GROUPS if today >= TIER_B_APPLY_START else TIER_A
    return GROUPS


def auto_apply_allowed(today: date, pref: bool) -> bool:
    return pref and today >= AUTO_APPLY_START


def daily_refit_mode(today: date, pref_auto_apply: bool) -> str | None:
    """What ``fm harness daily`` runs today: None (not a refit day), 'auto' or 'propose'."""
    if not is_refit_day(today):
        return None
    return "auto" if auto_apply_allowed(today, pref_auto_apply) else "propose"


# --------------------------------------------------------------------------- orchestration

def split_holdout(obs: Sequence[ReplayObs], target: str, weeks: int = HOLDOUT_WEEKS
                  ) -> tuple[list[ReplayObs], list[ReplayObs]]:
    """(train, holdout): the ``weeks`` most recent snapshot weeks with a ``target`` are held out."""
    have = [o for o in obs if getattr(o, target) is not None]
    wk = sorted({o.week for o in have})
    hold = set(wk[-weeks:]) if len(wk) > weeks else set()
    return [o for o in have if o.week not in hold], [o for o in have if o.week in hold]


def paired_ci(obs: Sequence[ReplayObs], before: Sequence[float], after: Sequence[float],
              level: float = CI_LEVEL) -> tuple[float | None, float | None]:
    """Player-clustered paired bootstrap CI of the relative improvement 1 - MAE_after/MAE_before."""
    pairs = [(o.player, abs(a), abs(b)) for o, b, a in zip(obs, before, after)]
    return bootstrap_skill(pairs, n_boot=BOOTSTRAP_N, seed=BOOTSTRAP_SEED, level=level)


@dataclass
class RefitResult:
    as_of: str
    mode: str
    locked: bool = False
    lock_reason: str | None = None
    active_version: str = PACKAGED
    n_live: int = 0
    n_goalie: int = 0
    n_week: int = 0
    weeks: int = 0
    leagues: dict[str, int] = field(default_factory=dict)
    groups_searched: list[str] = field(default_factory=list)
    group_gains: dict[str, float] = field(default_factory=dict)
    groups_selected: list[str] = field(default_factory=list)
    current: dict[str, float] = field(default_factory=dict)
    candidate: dict[str, float] = field(default_factory=dict)
    rows: list[dict[str, Any]] = field(default_factory=list)
    objectives: dict[str, Any] = field(default_factory=dict)
    gate: GateResult | None = None
    action: str = "none"               # none | dry-run | proposed | applied
    version: str | None = None
    notes: list[str] = field(default_factory=list)
    replay: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        d = asdict(self)
        d["gate"] = self.gate.to_dict() if self.gate else None
        return d


def _replay_check(obs: Sequence[ReplayObs]) -> dict[str, Any]:
    """How well the replay with the loaded params reproduces the archived fpg (exact rows only)."""
    diffs = [abs(inseason_projection(o) - o.fpg_archived) for o in obs if o.exact and o.fpg_archived is not None]
    return {"checked": len(diffs), "max_abs_diff": max(diffs) if diffs else None,
            "over_0.01": sum(d > 0.01 for d in diffs), "approximate_rows": sum(not o.exact for o in obs)}


def _objective_stats(obj: str, candidate: Mapping[str, float], current: Mapping[str, float],
                     holdout: Sequence[ReplayObs], full: Losses) -> ObjectiveStats:
    pc, pn = full.params(current), full.params(candidate)
    s = ObjectiveStats(obj)
    if obj == "fpg":
        rows = [o for o in holdout if o.real_fpg28 is not None]
        before, after = fpg_errors(rows, pc), fpg_errors(rows, pn)
        s.hist_before, s.hist_after, s.n_hist = full.hist(current), full.hist(candidate), len(full.hist_obs)
    else:
        rows = [o for o in holdout if o.real_pts7 is not None and o.games is not None]
        before, after = week_errors(rows, pc), week_errors(rows, pn)
    s.n_holdout = len(before)
    s.holdout_before, s.holdout_after = mae(before), mae(after)
    if before:
        s.ci_lo, s.ci_hi = paired_ci(rows, before, after)
    return s


def run_refit(ledger: Ledger, as_of: date | None = None, mode: str = "propose", only: Iterable[str] | None = None,
              leagues: Sequence[str] | None = None, force: bool = False, store: ParamsStore | None = None,
              hist: Mapping[str, Sequence[InseasonObs]] | None = None, by: str | None = None,
              log: Callable[[str], None] | None = None) -> RefitResult:
    """One refit cycle. ``mode``: 'dry-run' (nothing written), 'propose' (write a proposed version
    when the gate passes), 'apply' (propose + activate when the gate passes) or 'auto' (the daily
    run: Tier A only, applied when the gate passes). ``hist`` injects historical observations per
    league (tests); otherwise they are loaded from ``<data_dir>/backtest``."""
    today = as_of or date.today()
    store = store or ParamsStore(ledger=ledger)
    res = RefitResult(as_of=today.isoformat(), mode=mode, active_version=store.active_name())
    base = vparams.load_params()
    res.current = current_knobs(base)
    res.candidate = dict(res.current)
    if today < REFIT_START and not force:
        res.locked = True
        res.lock_reason = f"locked until {REFIT_START.isoformat()} (preseason; --force bypasses the calendar lock)"
        return res
    if force and today < REFIT_START:
        res.notes.append(f"calendar lock bypassed (--force); the statistical gate still applies")
    wanted = [g for g in (only or GROUPS)]
    bad = [g for g in wanted if g not in GROUPS]
    if bad:
        raise ValueError(f"unknown parameter group(s) {', '.join(bad)}; eligible: {', '.join(GROUPS)} "
                         f"(not eligible: {', '.join(NOT_ELIGIBLE)})")
    allowed = allowed_groups(mode, today)
    dropped = [g for g in wanted if g not in allowed]
    if dropped:
        res.notes.append(f"{', '.join(dropped)}: Tier B, proposal only until {TIER_B_APPLY_START.isoformat()}"
                         if mode == "apply" else f"{', '.join(dropped)}: not auto-applied (Tier B)")
    res.groups_searched = [g for g in wanted if g in allowed]

    live, info = replay_table(ledger, today, leagues)
    res.leagues = info["leagues"]
    fpg_obs = [o for o in live if o.real_fpg28 is not None]
    stats = GateStats(n_live=len(fpg_obs), n_goalie=sum(o.is_goalie for o in fpg_obs),
                      weeks=len({o.week for o in fpg_obs}),
                      n_week=sum(o.real_pts7 is not None and o.games is not None for o in live))
    res.n_live, res.n_goalie, res.weeks, res.n_week = stats.n_live, stats.n_goalie, stats.weeks, stats.n_week
    short = insufficient_reasons(stats)
    if short:
        res.gate = GateResult(False, short, {"data": False})
        return res
    res.replay = _replay_check(live)

    # historical checkpoints, per league scoring (pooled)
    hist_obs: list[InseasonObs] = []
    for lg in sorted({o.league for o in live}):
        if hist is not None:
            hist_obs += list(hist.get(lg, ()))
        else:
            sc = next(o.sc for o in live if o.league == lg)
            hist_obs += historical_observations(Path(ledger.data_dir), sc)
    if hist is None and not hist_obs:
        res.notes.append("historical checkpoints unavailable: Tier A candidates cannot pass the history check")

    train_f, hold_f = split_holdout(live, "real_fpg28")
    train_w, hold_w = split_holdout(live, "real_pts7")
    train = list({id(o): o for o in (*train_f, *train_w)}.values())
    holdout = list({id(o): o for o in (*hold_f, *hold_w)}.values())
    tl = Losses(train, hist_obs, base)
    goalies_ok = sum(o.is_goalie for o in train_f) >= MIN_GOALIE_OBS
    best: dict[str, tuple[dict[str, float], float]] = {}
    for g in res.groups_searched:
        knobs, before, after = search_group(g, res.current, tl, goalies_ok)
        gain = (before - after) / before if before and math.isfinite(before) and before > 0 else 0.0
        res.group_gains[g] = round(gain, 6)
        if gain > 0 and changed_knobs(knobs, res.current):
            best[g] = (knobs, gain)
        if log:
            log(f"searched {g}: training loss {before:.4f} -> {after:.4f}")
    chosen = sorted(best, key=lambda g: (-best[g][1], g))[:MAX_GROUPS]
    res.groups_selected = chosen
    cand = dict(res.current)
    for g in chosen:
        for k in GROUP_KNOBS[g]:
            cand[k.name] = best[g][0][k.name]
    res.candidate = cand

    full = Losses(live, hist_obs, base)
    for obj in sorted({GROUP_OBJECTIVE[g] for g in chosen}):
        stats.objectives[obj] = _objective_stats(obj, cand, res.current, holdout, full)
    res.objectives = {k: {**asdict(v), "gain": v.gain, "hist_change": v.hist_change} for k, v in stats.objectives.items()}
    res.gate = gate(cand, res.current, stats)
    if not chosen:
        res.gate.reasons.insert(0, "no eligible group improves the training loss inside its step bounds")
    for g in chosen:
        s = stats.objectives.get(GROUP_OBJECTIVE[g])
        for k in GROUP_KNOBS[g]:
            res.rows.append({"param": k.name, "group": g, "tier": tier(g), "current": res.current[k.name],
                             "candidate": cand[k.name], "bound": k.bound_label(),
                             "changed": abs(cand[k.name] - res.current[k.name]) > EPS,
                             "holdout_before": s.holdout_before if s else None,
                             "holdout_after": s.holdout_after if s else None,
                             "hist_before": s.hist_before if s else None, "hist_after": s.hist_after if s else None})
    if mode == "dry-run" or not res.gate.passed:
        res.action = "dry-run" if mode == "dry-run" else "none"
        return res

    changed = changed_knobs(cand, res.current)
    parent = store.active_name()
    params = vparams.deep_merge(store.active_params(), knobs_override(cand, changed))
    fo = stats.objectives.get("fpg") or next(iter(stats.objectives.values()))
    metrics = {"holdout_before": fo.holdout_before, "holdout_after": fo.holdout_after,
               "hist_before": fo.hist_before, "hist_after": fo.hist_after, "n_live": stats.n_live,
               "objectives": res.objectives, "groups": chosen, "as_of": today.isoformat()}
    note = "; ".join(f"{k} {res.current[k]:g} -> {cand[k]:g}" for k in changed)
    who = by or ("auto" if mode == "auto" else "manual")
    rec = store.propose(params, changed, metrics, note=note, parent=parent, by=who)
    res.version, res.action = rec["version"], "proposed"
    if mode in ("apply", "auto"):
        tier_b = [g for g in chosen if tier(g) == "B"]
        if mode == "auto" and (tier_b or today < AUTO_APPLY_START):
            res.notes.append("auto-apply only for Tier A from " + AUTO_APPLY_START.isoformat())
        else:
            store.apply(rec["version"], by=who, note=note, as_of=today)
            res.action = "applied"
    return res


# --------------------------------------------------------------------------- champion / challenger

def shadow_check(ledger: Ledger, week: date, store: ParamsStore | None = None) -> dict[str, Any] | None:
    """Shadow-score the version the active one replaced on the snapshot whose 28-day window
    matured in the week before ``week`` (a Monday), recorded in the active version's
    ``shadow.weeks``. Two straight weeks won by the shadow roll the active version back to it.
    None when no promoted version with a shadow is active."""
    store = store or ParamsStore(ledger=ledger)
    active = store.active()
    if not active or not isinstance(active.get("shadow"), dict):
        return None
    sh = active["shadow"]
    shadow_name = sh.get("version") or PACKAGED
    weeks: dict[str, Any] = dict(sh.get("weeks") or {})
    wk = week - timedelta(days=week.weekday())
    since = date.fromisoformat(str(sh.get("since") or active.get("applied_at") or "1900-01-01")[:10])
    live, _ = replay_table(ledger, wk)
    lo, hi = wk - timedelta(days=35), wk - timedelta(days=29)
    rows = [o for o in live if o.real_fpg28 is not None and lo <= o.snap <= hi and o.snap >= since]
    result: dict[str, Any] = {"week": wk.isoformat(), "active": active["version"], "shadow": shadow_name, "n": len(rows)}
    if rows:
        packaged = vparams.load_packaged()
        p_active = vparams.deep_merge(packaged, active.get("params") or {})
        shadow_rec = store.get(shadow_name)
        p_shadow = vparams.deep_merge(packaged, (shadow_rec or {}).get("params") or {})
        ma, ms = mae(fpg_errors(rows, p_active)), mae(fpg_errors(rows, p_shadow))
        result.update({"active_mae": ma, "shadow_mae": ms,
                       "winner": "shadow" if ms is not None and ma is not None and ms < ma else "active"})
        weeks[wk.isoformat()] = {k: result[k] for k in ("n", "active_mae", "shadow_mae", "winner")}
        sh["weeks"] = weeks
        active["shadow"] = sh
        store.update(active)
    recent = [weeks[k]["winner"] for k in sorted(weeks)][-2:]
    if len(recent) == 2 and all(w == "shadow" for w in recent):
        note = (f"auto-rollback: shadow {shadow_name} beat {active['version']} two weeks running "
                f"({', '.join(sorted(weeks)[-2:])})")
        rb = store.rollback(to=shadow_name, by="auto-rollback", note=note)
        result["rolled_back"] = rb
        try:
            from .ingest import record_run
            record_run(ledger, "auto-rollback", None, wk, {"note": note, **rb})
        except Exception:  # noqa: BLE001
            pass
    return result


__all__ = ["AUTO_APPLY_START", "GROUPS", "GateResult", "GateStats", "KNOB", "KNOBS", "Knob", "MAX_GROUPS",
           "MIN_LIVE_OBS", "NOT_ELIGIBLE", "ObjectiveStats", "REFIT_START", "RefitResult", "ReplayObs", "TIER_A",
           "TIER_B", "TIER_B_APPLY_START", "allowed_groups", "changed_groups", "changed_knobs", "current_knobs",
           "daily_refit_mode", "gate", "historical_observations", "inseason_projection", "is_refit_day",
           "knob_grid", "knobs_override", "next_refit_day", "params_for", "recency_moves", "replay_table",
           "run_refit", "search_group", "shadow_check", "week_projection"]
