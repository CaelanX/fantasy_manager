"""Grade recommendation episodes and my own moves against realized NHL results (M2).

``grade_outcomes(ledger, league, as_of)`` writes one ``outcomes`` row per (episode or decision,
basis, window) whose window has started by ``as_of``; realized points come from
``realized_daily`` scored with the league's own ScoringConfig (``scoring_config``).

Windows (``window_start`` .. ``window_end`` inclusive):

* ``7d`` / ``28d``  the 7 / 28 days after the basis day (the rec's ``first_seen``, the day I
                    acted, or my move's day); ``ros`` from the day after to the end of the
                    regular season (graded so far; complete only once the season is over).
* lineup recs       one ``7d`` window only. ESPN (daily locks): the rec's own day + 6, counting
                    only days on which both an ``in`` and an ``out`` player had a game. Fantrax
                    (weekly locks): the locked week, Monday (``first_lockable_day``) to Sunday.

A row is ``complete`` only when the whole window is before ``as_of`` and every day of it was
pulled (``realized_pulls``, days without games included); otherwise it is stored with
``complete=0`` (``partial=1``, graded over the pulled days so far) and regraded on the next run.
A window none of whose days has been pulled yet is not stored. When nobody involved played a
game in the window (preseason, a break) the realized gain is None: not judgeable, not a miss. Rows are idempotent upserts keyed by
``outcome_id``; rows whose episode / decision no longer exists are removed.

Realized gain (fantasy points over the window unless noted):

* waiver / injury (and my own adds)  pts(add) - pts(drop); an IR-only move has no gain.
* trade   pts(get) - pts(give) - (n_get - n_give) * replacement, where the replacement is the
          realized points of the ~replacement-level players that day (the ``REPLACEMENT_N``
          players with |vorp| closest to 0): a 2-for-1 frees (or costs) a roster spot.
* lineup  pts(in) - pts(out) on the days described above (pts(in) when the rec fills an
          empty slot; a "bench X" rec without an ``in`` player has no gain).
* sell_high / buy_low  directional, ``28d`` only: realized FPG over the window minus the
          player's L15 FPG at flag time (from the archived inputs). Sell-high hits when it is
          negative, buy-low when it is positive (players with >= 1 GP in the window).

``hit`` is realized > 0 for every non-flag kind. ``predicted_pts`` converts the rec's
``predicted_gain`` into points over the window (``gain_pts_h``): ``week_pts`` scale by
days / 7, per-game units (``season_fpg`` / ``lineup_fpg`` / ``dynasty``) multiply by the
expected games (the add's ``games_next7`` / 7 per day that day, else ``GAMES_PER_DAY``).
Episode origins: ``followed`` / ``partial`` / ``proposed``, ``ignored`` (expired) and ``open``;
my moves without a rec are ``user_only``, with the model's own view of the move that day
(fpg(add) - fpg(drop) from that day's projections) kept for the counterfactual.
"""
from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass, field
from datetime import date, timedelta
from pathlib import Path
from typing import Any, Iterable, Mapping

from ..models import ScoringConfig
from .ledger import Ledger, now_iso
from .match import first_lockable_day, load_episodes

WINDOWS: dict[str, int] = {"7d": 7, "28d": 28}
ROS = "ros"
GAMES_PER_DAY = 82 / 186          # an NHL team's regular season: 82 games in ~186 days
REPLACEMENT_N = 5
FLAG_KINDS = ("sell_high", "buy_low")
ORIGIN_OF_STATUS = {"expired": "ignored"}
SEASON_END = (4, 30)              # regular season over by April 30 (ros windows complete after)


# --------------------------------------------------------------------------- scoring config

def _meta(ledger: Ledger, key: str) -> str | None:
    rows = ledger.query("SELECT value FROM meta WHERE key=?", (key,))
    return rows[0]["value"] if rows else None


def _set_meta(ledger: Ledger, key: str, value: str) -> None:
    ledger.execute("INSERT INTO meta(key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                   (key, value))
    ledger.commit()


def store_scoring(ledger: Ledger, league: str, cfg: ScoringConfig) -> None:
    """Remember the league's ScoringConfig (``meta['scoring:<league>']``, set at daily-run time)."""
    _set_meta(ledger, f"scoring:{league}", json.dumps(cfg.model_dump(), sort_keys=True))


def _archive_scoring(data_dir: Path, league: str) -> dict[str, Any] | None:
    d = Path(data_dir) / "archive"
    if not d.is_dir():
        return None
    for path in sorted(d.glob(f"projections-{league}-????-??-??.json"), reverse=True):
        try:
            snap = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        if isinstance(snap.get("scoring"), dict) and snap["scoring"].get("kind"):
            return snap["scoring"]
    return None


def scoring_config(ledger: Ledger, league: str) -> tuple[ScoringConfig | None, str]:
    """(config, source): ``meta`` first, else the newest archived projections header (then stored
    in ``meta``), else the backtest preset of the same name."""
    raw = _meta(ledger, f"scoring:{league}")
    if raw:
        try:
            return ScoringConfig.model_validate(json.loads(raw)), "meta"
        except ValueError:
            pass
    arch = _archive_scoring(ledger.data_dir, league)
    if arch is not None:
        try:
            cfg = ScoringConfig.model_validate(arch)
            store_scoring(ledger, league, cfg)
            return cfg, "archive"
        except ValueError:
            pass
    from ..backtest.scoring import PRESETS
    if league in PRESETS:
        return PRESETS[league], "preset"
    return None, "none"


def points_scorer(cfg: ScoringConfig | None):
    """PointsScoring for a points league, None otherwise (categories leagues are not graded)."""
    if cfg is None or cfg.kind != "points":
        return None
    from ..backtest.scoring import scorer
    return scorer(cfg)


# --------------------------------------------------------------------------- realized results

class Realized:
    """Realized fantasy points per player per game date under one scoring."""

    def __init__(self, ledger: Ledger, sc: Any, start: date | None = None, end: date | None = None):
        where, params = [], []
        if start is not None:
            where.append("game_date >= ?")
            params.append(start.isoformat())
        if end is not None:
            where.append("game_date <= ?")
            params.append(end.isoformat())
        clause = (" WHERE " + " AND ".join(where)) if where else ""
        self.pulled: set[date] = {date.fromisoformat(r["game_date"])
                                  for r in ledger.query(f"SELECT game_date FROM realized_pulls{clause}", params)}
        self.by_player: dict[int, dict[date, tuple[int, float]]] = {}
        for r in ledger.query(f"SELECT nhl_id, game_date, gp, stats_json FROM realized_daily{clause}", params):
            try:
                stats = json.loads(r["stats_json"] or "{}")
            except ValueError:
                continue
            pts = float(sc.value(stats)) if stats else 0.0
            self.by_player.setdefault(int(r["nhl_id"]), {})[date.fromisoformat(r["game_date"])] = (int(r["gp"]), pts)

    @classmethod
    def from_rows(cls, pulled: Iterable[date], rows: Mapping[int, Mapping[date, tuple[int, float]]]) -> "Realized":
        obj = cls.__new__(cls)
        obj.pulled = set(pulled)
        obj.by_player = {int(k): dict(v) for k, v in rows.items()}
        return obj

    def day(self, nhl_id: int, d: date) -> tuple[int, float]:
        return self.by_player.get(int(nhl_id), {}).get(d, (0, 0.0))

    def window(self, nhl_id: int, days: Iterable[date]) -> tuple[int, float]:
        rows = self.by_player.get(int(nhl_id), {})
        gp = 0
        pts = 0.0
        for d in days:
            g, p = rows.get(d, (0, 0.0))
            gp += g
            pts += p
        return gp, pts

    def covered(self, days: Iterable[date]) -> bool:
        return all(d in self.pulled for d in days)


def days_between(start: date, end: date) -> list[date]:
    return [start + timedelta(days=i) for i in range((end - start).days + 1)] if end >= start else []


def season_end(day: date) -> date:
    y = day.year if day.month >= 9 else day.year - 1
    return date(y + 1, *SEASON_END)


@dataclass
class Window:
    name: str
    start: date
    end: date                   # planned last day
    graded: list[date]          # pulled days graded so far (start .. min(end, as_of - 1))
    complete: bool

    @property
    def n_days(self) -> int:
        return (self.end - self.start).days + 1


def make_window(name: str, start: date, end: date, as_of: date, realized: Realized) -> Window | None:
    """The window if it has started before ``as_of`` and at least one of its days was pulled
    (None otherwise); only pulled days are graded."""
    last = min(end, as_of - timedelta(days=1))
    if last < start:
        return None
    graded = [d for d in days_between(start, last) if d in realized.pulled]
    if not graded:
        return None
    complete = end < as_of and realized.covered(days_between(start, end))
    return Window(name, start, end, graded, complete)


def standard_windows(basis: date, as_of: date, realized: Realized, names: Iterable[str] = ("7d", "28d", ROS)
                     ) -> list[Window]:
    out = []
    start = basis + timedelta(days=1)
    for name in names:
        end = season_end(basis) if name == ROS else basis + timedelta(days=WINDOWS[name])
        w = make_window(name, start, end, as_of, realized)
        if w is not None:
            out.append(w)
    return out


# --------------------------------------------------------------------------- per-kind gains

@dataclass
class Players:
    """Everything grading needs to know about a league's players (identity, projections)."""
    nhl: dict[str, int] = field(default_factory=dict)                          # cid -> nhl_id
    proj: dict[str, dict[str, dict[str, Any]]] = field(default_factory=dict)   # as_of -> cid -> row
    days: list[str] = field(default_factory=list)                              # projection days, sorted

    def nhl_id(self, cid: str) -> int | None:
        return self.nhl.get(cid)

    def proj_on(self, day: date) -> dict[str, dict[str, Any]]:
        """The projections of ``day`` or of the latest earlier projection day ({} if none)."""
        key = day.isoformat()
        best = None
        for d in self.days:
            if d <= key:
                best = d
            else:
                break
        return self.proj.get(best, {}) if best else {}


def load_players(ledger: Ledger, league: str) -> Players:
    pl = Players()
    for sql in ("SELECT cid, nhl_id FROM transactions WHERE league=? AND nhl_id IS NOT NULL",
                "SELECT cid, nhl_id FROM rec_players WHERE league=? AND nhl_id IS NOT NULL",
                "SELECT cid, nhl_id FROM projections WHERE league=? AND nhl_id IS NOT NULL ORDER BY as_of"):
        for r in ledger.query(sql, (league,)):
            pl.nhl[r["cid"]] = int(r["nhl_id"])
    for r in ledger.query("SELECT as_of, cid, nhl_id, fpg, vorp, games_next7, positions, inputs_json"
                          " FROM projections WHERE league=?", (league,)):
        pl.proj.setdefault(r["as_of"], {})[r["cid"]] = r
    pl.days = sorted(pl.proj)
    return pl


def _pts(players: Players, realized: Realized, cids: Iterable[str], days: list[date]
         ) -> tuple[float, int, list[str]]:
    """(points, games, unmatched cids) of ``cids`` over ``days``."""
    pts, gp, missing = 0.0, 0, []
    for c in cids:
        nid = players.nhl_id(c)
        if nid is None:
            missing.append(c)
            continue
        g, p = realized.window(nid, days)
        pts += p
        gp += g
    return pts, gp, missing


def replacement_pts(players: Players, realized: Realized, basis: date, days: list[date],
                    cache: dict | None = None, exclude: Iterable[str] = ()) -> float | None:
    """Mean realized points over ``days`` of the ``REPLACEMENT_N`` players with |vorp| closest
    to 0 in the basis day's projections, other than the traded ones (None without v2 vorp)."""
    skip = frozenset(exclude)
    key = (basis, tuple(days), skip)
    if cache is not None and key in cache:
        return cache[key]
    rows = [r for r in players.proj_on(basis).values()
            if r.get("vorp") is not None and r.get("nhl_id") and r["cid"] not in skip]
    rows.sort(key=lambda r: (abs(float(r["vorp"])), r["cid"]))
    pick = rows[:REPLACEMENT_N]
    val = (sum(realized.window(int(r["nhl_id"]), days)[1] for r in pick) / len(pick)) if pick else None
    if cache is not None:
        cache[key] = val
    return val


def games_rate(players: Players, basis: date, adds: Iterable[str]) -> float:
    """Expected games per day of the added player(s): games_next7 / 7 that day, else the NHL mean."""
    proj = players.proj_on(basis)
    rates = [float(proj[c]["games_next7"]) / 7.0 for c in adds
             if c in proj and proj[c].get("games_next7") is not None]
    return sum(rates) / len(rates) if rates else GAMES_PER_DAY


def predicted_pts(gain: float | None, units: str | None, n_days: int, rate: float = GAMES_PER_DAY) -> float | None:
    """``predicted_gain`` as points over an ``n_days`` window (``gain_pts_h``)."""
    if gain is None or units is None:
        return None
    if units == "week_pts":
        return float(gain) * n_days / 7.0
    if units in ("season_fpg", "lineup_fpg", "dynasty"):
        return float(gain) * rate * n_days
    return None


def _fpg_of(line: Mapping[str, Any] | None, sc: Any) -> float | None:
    if not line or not line.get("gp"):
        return None
    return float(sc.value(dict(line.get("stats") or {}))) / float(line["gp"])


def l15_fpg(players: Players, cid: str, basis: date, sc: Any) -> float | None:
    row = players.proj_on(basis).get(cid)
    if not row or not row.get("inputs_json"):
        return None
    try:
        inputs = json.loads(row["inputs_json"])
    except ValueError:
        return None
    return _fpg_of(inputs.get("last15"), sc)


def model_fpg_delta(players: Players, day: date, adds: Iterable[str], drops: Iterable[str]) -> float | None:
    """The model's own view of a move that day: fpg(adds) - fpg(drops) from that day's projections."""
    proj = players.proj_on(day)
    adds, drops = list(adds), list(drops)
    if not adds and not drops:
        return None
    vals = []
    for c, sign in [(c, 1.0) for c in adds] + [(c, -1.0) for c in drops]:
        r = proj.get(c)
        if r is None or r.get("fpg") is None:
            return None
        vals.append(sign * float(r["fpg"]))
    return sum(vals)


# --------------------------------------------------------------------------- grading one subject

@dataclass
class Subject:
    """One thing to grade: an episode on one basis, or one of my user-only moves."""
    league: str
    kind: str
    origin: str
    basis: str                   # first_seen | acted_on | decision
    day: date
    adds: list[str]
    drops: list[str]
    episode_id: str | None = None
    decision_id: str | None = None
    predicted_gain: float | None = None
    gain_units: str | None = None
    horizon_days: int | None = None
    title: str | None = None
    strength: float | None = None
    last_seen: date | None = None


def outcome_id(s: Subject, window: str) -> str:
    ref = f"ep:{s.episode_id}" if s.episode_id and s.basis != "decision" else f"dec:{s.decision_id}"
    return hashlib.sha1(f"{s.league}|{ref}|{s.basis}|{window}".encode("utf-8")).hexdigest()[:20]


def _row(s: Subject, w: Window, realized_gain: float | None, hit: int | None, pred_pts: float | None,
         detail: dict[str, Any], predicted_gain: float | None = None, units: str | None = None) -> dict[str, Any]:
    return {
        "outcome_id": outcome_id(s, w.name), "league": s.league, "episode_id": s.episode_id,
        "decision_id": s.decision_id, "window": w.name, "basis": s.basis,
        "predicted_gain": s.predicted_gain if predicted_gain is None else predicted_gain,
        "realized_gain": None if realized_gain is None else round(realized_gain, 4),
        "gain_units": units or s.gain_units, "hit": hit, "partial": 0 if w.complete else 1,
        "graded_at": now_iso(), "kind": s.kind, "origin": s.origin, "complete": int(w.complete),
        "window_start": w.start.isoformat(), "window_end": w.end.isoformat(),
        "predicted_pts": None if pred_pts is None else round(pred_pts, 4),
        "detail_json": json.dumps({**detail, "graded_through": w.graded[-1].isoformat(), "days": len(w.graded),
                                   "title": s.title, "day": s.day.isoformat(), "strength": s.strength},
                                  sort_keys=True, default=str),
    }


def _hit(v: float | None) -> int | None:
    return None if v is None else int(v > 0)


def grade_subject(s: Subject, players: Players, realized: Realized, as_of: date, sc: Any, provider: str,
                  repl_cache: dict | None = None) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    if s.kind == "lineup":
        if provider == "fantrax":
            start = first_lockable_day("fantrax", s.day)
        else:
            start = s.day
        w = make_window("7d", start, start + timedelta(days=6), as_of, realized)
        if w is None:
            return out
        unmatched = [c for c in s.adds + s.drops if players.nhl_id(c) is None]
        if unmatched or not s.adds:        # "Bench X (out)": nothing realized to compare
            gain, shared = None, 0
        elif not s.drops:                  # an empty starting slot filled: his points vs nothing
            shared = sum(1 for d in w.graded if any(realized.day(players.nhl[c], d)[0] for c in s.adds))
            gain = _pts(players, realized, s.adds, w.graded)[0] if shared else None
        elif provider == "fantrax":        # weekly lock: the whole locked week counts
            p_in, g_in, _ = _pts(players, realized, s.adds, w.graded)
            p_out, g_out, _ = _pts(players, realized, s.drops, w.graded)
            gain = p_in - p_out if g_in + g_out else None
            shared = len(w.graded)
        else:                              # ESPN daily: only days both sides played
            gain, shared = 0.0, 0
            for d in w.graded:
                ins = [realized.day(players.nhl[c], d) for c in s.adds]
                outs = [realized.day(players.nhl[c], d) for c in s.drops]
                if any(g for g, _ in ins) and any(g for g, _ in outs):
                    gain += sum(p for _, p in ins) - sum(p for _, p in outs)
                    shared += 1
            if shared == 0:
                gain = None
        pp = predicted_pts(s.predicted_gain, s.gain_units, w.n_days)
        out.append(_row(s, w, gain, _hit(gain), pp,
                        {"shared_days": shared, "unmatched": unmatched, "lock": provider == "fantrax"}))
        return out

    if s.kind in FLAG_KINDS:
        target = s.drops if s.kind == "sell_high" else s.adds
        for w in standard_windows(s.day, as_of, realized, ("28d",)):
            detail: dict[str, Any] = {"players": target}
            gain = None
            gp = 0
            if len(target) == 1 and players.nhl_id(target[0]) is not None:
                base = l15_fpg(players, target[0], s.day, sc)
                gp, pts = realized.window(players.nhl[target[0]], w.graded)
                detail.update({"l15_fpg": base, "gp": gp, "fpg_window": pts / gp if gp else None})
                if base is not None and gp > 0:
                    gain = pts / gp - base
            else:
                detail["unmatched"] = [c for c in target if players.nhl_id(c) is None]
            hit = None if gain is None else int(gain < 0 if s.kind == "sell_high" else gain > 0)
            out.append(_row(s, w, gain, hit, None, detail, units="season_fpg"))
        return out

    if not s.adds and not s.drops:
        return out                         # an IR-only move: nothing to grade
    rate = games_rate(players, s.day, s.adds)
    for w in standard_windows(s.day, as_of, realized):
        p_in, g_in, m_in = _pts(players, realized, s.adds, w.graded)
        p_out, g_out, m_out = _pts(players, realized, s.drops, w.graded)
        unmatched = m_in + m_out
        detail = {"pts_in": round(p_in, 3), "pts_out": round(p_out, 3), "gp_in": g_in, "gp_out": g_out,
                  "unmatched": unmatched}
        # nobody on either side played (preseason, a break): nothing to judge, not a miss
        gain: float | None = None if unmatched or g_in + g_out == 0 else p_in - p_out
        if s.kind == "trade" and gain is not None:
            n_diff = len(s.adds) - len(s.drops)
            if n_diff:
                repl = replacement_pts(players, realized, s.day, w.graded, repl_cache, s.adds + s.drops)
                detail["replacement_pts"] = None if repl is None else round(repl, 3)
                if repl is not None:
                    gain -= n_diff * repl
        if s.kind == "trade" and w.name == ROS:
            detail["label"] = "partial" if not w.complete else "season"
        pp = predicted_pts(s.predicted_gain, s.gain_units, w.n_days if w.name != ROS else len(w.graded),
                           rate if s.gain_units == "season_fpg" else GAMES_PER_DAY)
        if s.origin == "user_only":
            m = model_fpg_delta(players, s.day, s.adds, s.drops)
            detail["model_fpg_delta"] = None if m is None else round(m, 4)
            pp = predicted_pts(m, "season_fpg", w.n_days if w.name != ROS else len(w.graded), rate)
            out.append(_row(s, w, gain, _hit(gain), pp, detail, predicted_gain=m, units="season_fpg"))
        else:
            out.append(_row(s, w, gain, _hit(gain), pp, detail))
    return out


# --------------------------------------------------------------------------- subjects of a league

def _json_list(v: str | None) -> list[str]:
    try:
        return sorted(json.loads(v)) if v else []
    except ValueError:
        return []


def load_subjects(ledger: Ledger, league: str) -> list[Subject]:
    eps = {e.episode_id: e for e in load_episodes(ledger, league)}
    rows = ledger.query("SELECT * FROM rec_episodes WHERE league=? ORDER BY first_seen", (league,))
    decisions = {d["episode_id"]: d for d in ledger.query(
        "SELECT * FROM decisions WHERE league=? AND episode_id IS NOT NULL", (league,))}
    out: list[Subject] = []
    for r in rows:
        e = eps.get(r["episode_id"])
        if e is None:
            continue
        origin = ORIGIN_OF_STATUS.get(r["status"], r["status"])
        common = dict(league=league, kind=r["kind"], origin=origin, episode_id=r["episode_id"],
                      predicted_gain=r["predicted_gain"], gain_units=r["gain_units"],
                      horizon_days=r["horizon_days"], title=r["title"], strength=r["strength"],
                      last_seen=e.last_seen)
        out.append(Subject(basis="first_seen", day=e.first_seen, adds=sorted(e.adds), drops=sorted(e.drops),
                           **common))
        dec = decisions.get(r["episode_id"])
        if r["acted_on"] and r["status"] in ("followed", "partial") and r["kind"] != "lineup":
            adds, drops = sorted(e.adds), sorted(e.drops)
            try:
                m = json.loads(r["match_json"] or "{}")
            except ValueError:
                m = {}
            if r["kind"] == "waiver" and m.get("added"):
                adds, drops = sorted(m["added"]), sorted(m.get("dropped") or [])
            elif r["kind"] == "trade" and m.get("got"):
                adds, drops = sorted(m["got"]), sorted(m.get("gave") or [])
            out.append(Subject(basis="acted_on", day=date.fromisoformat(r["acted_on"]), adds=adds, drops=drops,
                               decision_id=dec["decision_id"] if dec else None, **common))
    for d in ledger.query("SELECT * FROM decisions WHERE league=? AND origin='user_only'", (league,)):
        out.append(Subject(league=league, kind=d["kind"] or "waiver", origin="user_only", basis="decision",
                           day=date.fromisoformat(d["day"]), adds=_json_list(d["adds_json"]),
                           drops=_json_list(d["drops_json"]), decision_id=d["decision_id"]))
    return out


def model_alternative(s: Subject, subjects: Iterable[Subject]) -> Subject | None:
    """The model's own pick of the same kind that day: the rec episode (first_seen basis) live on
    my move's day with the highest predicted gain (then strength)."""
    best = None
    for e in subjects:
        if e.basis != "first_seen" or e.kind != s.kind or e.episode_id is None or not e.adds:
            continue
        last = e.last_seen or e.day
        if not (e.day <= s.day <= last):
            continue
        key = (e.predicted_gain if e.predicted_gain is not None else float("-inf"),
               e.strength if e.strength is not None else float("-inf"))
        if best is None or key > best[0]:
            best = (key, e)
    return best[1] if best else None


def _attach_alternative(s: Subject, rows: list[dict[str, Any]], subjects: list[Subject], players: Players,
                        realized: Realized, repl_cache: dict) -> None:
    """Counterfactual: the model's pick that day, realized over the same windows as my move."""
    alt = model_alternative(s, subjects)
    for r in rows:
        detail = json.loads(r["detail_json"])
        if alt is None:
            detail["alt"] = None
        else:
            start = date.fromisoformat(r["window_start"])
            days = days_between(start, date.fromisoformat(detail["graded_through"]))
            p_in, _, m_in = _pts(players, realized, alt.adds, days)
            p_out, _, m_out = _pts(players, realized, alt.drops, days)
            _, g_in, _ = _pts(players, realized, alt.adds, days)
            _, g_out, _ = _pts(players, realized, alt.drops, days)
            gain = None if (m_in or m_out or not g_in + g_out) else p_in - p_out
            if gain is not None and s.kind == "trade" and len(alt.adds) != len(alt.drops):
                repl = replacement_pts(players, realized, s.day, days, repl_cache, alt.adds + alt.drops)
                gain = gain - (len(alt.adds) - len(alt.drops)) * repl if repl is not None else gain
            detail["alt"] = {"episode_id": alt.episode_id, "title": alt.title,
                             "gain": None if gain is None else round(gain, 4)}
        r["detail_json"] = json.dumps(detail, sort_keys=True, default=str)


@dataclass
class GradeSummary:
    league: str
    scoring: str = "none"
    graded: int = 0
    complete: int = 0
    partial: int = 0
    ungradable: int = 0
    unmatched: int = 0
    by_kind: dict[str, int] = field(default_factory=dict)
    note: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return dict(self.__dict__)

    def line(self) -> str:
        if self.note:
            return f"{self.league}: {self.note}"
        kinds = ", ".join(f"{k} {v}" for k, v in sorted(self.by_kind.items())) or "none"
        return (f"{self.league}: {self.graded} outcome rows ({self.complete} complete, {self.partial} partial window; "
                f"{self.ungradable} without a realized gain, {self.unmatched} with unmatched players); {kinds}")


def grade_outcomes(ledger: Ledger, league: str, as_of: date | None = None) -> GradeSummary:
    """Grade every episode and user-only move of ``league`` whose window has started by ``as_of``
    (default today); idempotent."""
    as_of = as_of or date.today()
    summary = GradeSummary(league=league)
    cfg, src = scoring_config(ledger, league)
    summary.scoring = src
    sc = points_scorer(cfg)
    if sc is None:
        summary.note = "not a points league (or no scoring known): outcomes not graded"
        return summary
    subjects = load_subjects(ledger, league)
    players = load_players(ledger, league)
    first = min((s.day for s in subjects), default=as_of)
    realized = Realized(ledger, sc, first, as_of)
    repl_cache: dict = {}
    rows: list[dict[str, Any]] = []
    for s in subjects:
        graded = grade_subject(s, players, realized, as_of, sc, league, repl_cache)
        if s.origin == "user_only" and graded:
            _attach_alternative(s, graded, subjects, players, realized, repl_cache)
        rows.extend(graded)
    keep = {r["outcome_id"] for r in rows}
    for r in ledger.query("SELECT outcome_id FROM outcomes WHERE league=?", (league,)):
        if r["outcome_id"] not in keep:
            ledger.execute("DELETE FROM outcomes WHERE outcome_id=?", (r["outcome_id"],))
    ledger.commit()
    ledger.upsert("outcomes", rows, ("outcome_id",))
    summary.graded = len(rows)
    summary.complete = sum(r["complete"] for r in rows)
    summary.partial = summary.graded - summary.complete
    summary.ungradable = sum(r["realized_gain"] is None for r in rows)
    summary.unmatched = sum(bool(json.loads(r["detail_json"]).get("unmatched")) for r in rows)
    for r in rows:
        summary.by_kind[r["kind"]] = summary.by_kind.get(r["kind"], 0) + 1
    return summary


__all__ = ["FLAG_KINDS", "GAMES_PER_DAY", "GradeSummary", "Players", "ROS", "Realized", "Subject", "WINDOWS",
           "Window", "grade_outcomes", "grade_subject", "load_players", "load_subjects", "make_window",
           "predicted_pts", "replacement_pts", "scoring_config", "season_end", "standard_windows",
           "store_scoring"]
