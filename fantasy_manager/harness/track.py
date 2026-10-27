"""Track record shown where decisions are made: how moves like this one turned out.

Read-only over the graded ``outcomes`` (``harness.outcomes``): complete outcomes of archived
recommendations (any origin except my own ``user_only`` moves, graded from ``first_seen`` on
the kind's primary window, ``metrics.primary_window``) whose window ended in the last
``TRACK_DAYS`` days. A hit is a positive realized gain (points, or the FPG change for sell-high
/ buy-low flags).

Groups: the recommendation ``kind`` and a ``subkind`` from the title / gain units
(``subkind_of``): waiver ``stream`` (this-matchup pickups) vs ``season``, lineup ``ir`` (IR
swaps / activations) vs ``swap``, injury ``ir`` vs ``hold_cut``. A subkind with too little
history falls back to its kind. Below ``MIN_N`` graded moves (the Health page's "hidden"
threshold, ``metrics.HIT_PROVISIONAL``) the record says so instead of showing a rate.

``TrackBook.load(data_dir, league)`` reads the ledger once (nothing is created when there is no
``harness.db`` yet); ``book.record(kind, subkind)`` -> {n, hits, hit_rate, avg_gain, ci, ...};
``track_text(rec)`` is the one-line card / CLI text. ``track_record(kind, subkind, data_dir=...,
league=...)`` is the one-call form for other features (the weekly plan).
"""
from __future__ import annotations

import re
from dataclasses import dataclass, field
from datetime import date, timedelta
from pathlib import Path
from typing import Any, Mapping

from .ledger import DB_NAME, Ledger
from .metrics import HIT_KINDS, HIT_PROVISIONAL, primary_window, wilson
from .outcomes import FLAG_KINDS

TRACK_DAYS = 60
MIN_N = HIT_PROVISIONAL
SUBKIND_LABEL = {
    ("waiver", "stream"): "this-matchup pickups", ("waiver", "season"): "season pickups",
    ("lineup", "ir"): "IR swaps and activations", ("lineup", "swap"): "start / sit changes",
    ("injury", "ir"): "IR moves", ("injury", "hold_cut"): "hold-or-cut calls",
}
KIND_NOUN = {"waiver": "pickups", "lineup": "lineup changes", "injury": "injury moves", "sell_high": "sell-high calls",
             "buy_low": "buy-low calls", "alert": "followed alerts"}


def subkind_of(kind: str, title: str | None, gain_units: str | None = None) -> str | None:
    """Sub-group of a recommendation (see the module docstring); None when the kind has none."""
    t = (title or "").lower()
    if kind == "waiver":
        return "stream" if gain_units == "matchup_pts" or t.endswith("for this matchup") else "season"
    if kind == "lineup":
        return "ir" if re.search(r"\bir\b", t) else "swap"
    if kind == "injury":
        return "hold_cut" if t.startswith(("hold ", "cut ")) else "ir"
    return None


def _stats(rows: list[Mapping[str, Any]]) -> dict[str, Any]:
    n = len(rows)
    hits = sum(int(r["hit"]) for r in rows)
    gains = [float(r["realized_gain"]) for r in rows if r["realized_gain"] is not None]
    lo, hi = wilson(hits, n)
    return {"n": n, "hits": hits, "hit_rate": hits / n if n else None,
            "avg_gain": sum(gains) / len(gains) if gains else None, "ci": (lo, hi)}


@dataclass
class TrackBook:
    """Graded outcomes of one league, grouped by (kind, subkind)."""
    league: str
    days: int = TRACK_DAYS
    groups: dict[tuple[str, str | None], list[Mapping[str, Any]]] = field(default_factory=dict)
    available: bool = False        # False: no harness ledger yet

    @classmethod
    def load(cls, data_dir: str | Path, league: str, today: date | None = None,
             days: int = TRACK_DAYS) -> "TrackBook":
        book = cls(league=league, days=days)
        if not (Path(data_dir) / DB_NAME).exists():
            return book
        since = ((today or date.today()) - timedelta(days=days)).isoformat()
        with Ledger(data_dir) as ledger:
            rows = ledger.query(
                "SELECT o.kind, o.gain_units, o.window, o.hit, o.realized_gain, o.origin, e.title "
                "FROM outcomes o LEFT JOIN rec_episodes e ON e.episode_id = o.episode_id "
                "WHERE o.league=? AND o.complete=1 AND o.hit IS NOT NULL AND o.window_end >= ? "
                "AND COALESCE(o.basis, 'first_seen') != 'acted_on' AND COALESCE(o.origin, '') != 'user_only'",
                (league, since))
        book.available = True
        for r in rows:
            kind = r["kind"]
            if kind not in HIT_KINDS or r["window"] != primary_window(kind, r["gain_units"]):
                continue
            book.groups.setdefault((kind, None), []).append(r)
            sub = subkind_of(kind, r["title"], r["gain_units"])
            if sub is not None:
                book.groups.setdefault((kind, sub), []).append(r)
        return book

    def record(self, kind: str, subkind: str | None = None) -> dict[str, Any]:
        """{n, hits, hit_rate, avg_gain, ci, kind, subkind, enough, units, days, label}. A subkind
        below MIN_N falls back to the whole kind when that has enough history."""
        sub = self.groups.get((kind, subkind), []) if subkind else []
        whole = self.groups.get((kind, None), [])
        use_sub = subkind is not None and (len(sub) >= MIN_N or len(whole) < MIN_N)
        rows = sub if use_sub else whole
        out = _stats(rows)
        out.update(kind=kind, subkind=subkind if use_sub else None, enough=out["n"] >= MIN_N, min_n=MIN_N,
                   days=self.days, units="fpg change" if kind in FLAG_KINDS else "pts",
                   label=SUBKIND_LABEL.get((kind, subkind)) if use_sub else KIND_NOUN.get(kind, kind),
                   available=self.available)
        return out


def record_text(tr: Mapping[str, Any]) -> str:
    """One line: "Similar moves: 62% gained points (n=34, last 60 days)", or why there is no rate yet."""
    if tr.get("kind") not in HIT_KINDS:
        return "Track record: trades are judged case by case (Health page)"
    if not tr.get("available"):
        return "Track record: starts once the daily harness run has graded a few weeks of moves"
    if not tr["enough"]:
        return (f"Track record: not enough history yet (n={tr['n']} graded {tr['label']}, "
                f"shown from {tr['min_n']})")
    verb = "played out" if tr["units"] == "fpg change" else "gained points"
    lo, hi = tr["ci"]
    ci = f", 95% CI {lo * 100:.0f}-{hi * 100:.0f}%" if lo is not None else ""
    return (f"Similar moves ({tr['label']}): {tr['hit_rate'] * 100:.0f}% {verb} "
            f"(n={tr['n']}, last {tr['days']} days{ci})")


def rec_record(book: TrackBook, rec: Any) -> dict[str, Any]:
    """The record for a Recommendation (or anything with kind / title / gain_units)."""
    kind = getattr(rec, "kind", None) or ""
    return book.record(kind, subkind_of(kind, getattr(rec, "title", None), getattr(rec, "gain_units", None)))


def track_text(book: TrackBook, rec: Any) -> str:
    return record_text(rec_record(book, rec))


def strip(book: TrackBook, kinds: tuple[str, ...] = ("waiver", "lineup", "injury")) -> list[dict[str, Any]]:
    """Compact per-kind summary for the dashboard header: [{kind, label, text, enough, n, hit_rate}]."""
    out = []
    for k in kinds:
        tr = book.record(k)
        out.append({"kind": k, "label": KIND_NOUN.get(k, k), "n": tr["n"], "enough": tr["enough"],
                    "hit_rate": tr["hit_rate"], "min_n": tr["min_n"],
                    "text": (f"{tr['hit_rate'] * 100:.0f}% gained (n={tr['n']})" if tr["enough"]
                             else f"{tr['n']}/{tr['min_n']} graded")})
    return out


def track_record(kind: str, subkind: str | None = None, *, data_dir: str | Path, league: str,
                 today: date | None = None) -> dict[str, Any]:
    """{n, hit_rate, avg_gain, ci, ...} for one kind / subkind (``TrackBook.record``)."""
    return TrackBook.load(data_dir, league, today).record(kind, subkind)


def as_json(book: TrackBook, kinds: tuple[str, ...] = HIT_KINDS) -> dict[str, Any]:
    return {k: {**book.record(k), "ci": list(book.record(k)["ci"])} for k in kinds}


__all__ = ["TRACK_DAYS", "MIN_N", "TrackBook", "subkind_of", "record_text", "rec_record", "track_text", "strip",
           "track_record", "as_json"]
