"""Backfill the ledger from the JSON archive (v1 and v2), idempotently.

* ``projections-<league>-<day>.json`` -> ``projections`` (one row per player and day)
* ``recs-<league>-<day>.json`` -> ``recs`` + ``rec_players`` (the day's rows are replaced, so a
  re-archived day never leaves stale recs behind), then ``roll_episodes`` groups each
  ``rec_key``'s days into episodes: the same rec seen again within ``EPISODE_GAP_DAYS`` of its
  last sighting extends the episode (first_seen / last_seen / n_days), a longer gap starts a
  new one. Episodes are a pure function of the recs rows (ids are
  sha1(league|rec_key|first_seen)); match status columns are left untouched.

Files whose sha1 is unchanged since the last ingest are skipped (``archive_files``).
v1 recommendation files have no ``subjects``: IR targets are recovered from the title
("Move X to IR", "X now OUT (was ...)") and the same day's projections by name.
"""
from __future__ import annotations

import hashlib
import json
import re
from datetime import date
from pathlib import Path
from typing import Any, Iterable, Mapping

from ..backtest.archive import archive_dir, load_snapshot
from .ledger import Ledger, now_iso, rec_key

EPISODE_GAP_DAYS = 2      # seen again within 2 days (one missed archive run) = same episode
FILE_RE = re.compile(r"^(projections|recs)-(.+)-(\d{4}-\d{2}-\d{2})$")
_IR_TITLE_RES = (re.compile(r"\b[Mm]ove (.+?) to IR\b"), re.compile(r"^(.+?) now [A-Z]+ \(was "))


def _sha1(path: Path) -> str:
    return hashlib.sha1(path.read_bytes()).hexdigest()


def _int(v: Any) -> int | None:
    try:
        return int(v) if v not in (None, "") else None
    except (TypeError, ValueError):
        return None


def _j(v: Any) -> str | None:
    return None if v is None else json.dumps(v, sort_keys=True, default=str, separators=(",", ":"))


def archive_files(data_dir: Path | str, leagues: Iterable[str] | None = None) -> list[tuple[str, str, date, Path]]:
    """(kind, league, day, path) of every archive file, projections before recs per day."""
    wanted = set(leagues) if leagues else None
    out = []
    for path in archive_dir(data_dir).glob("*.json"):
        m = FILE_RE.match(path.stem)
        if not m:
            continue
        kind, league, day = m.group(1), m.group(2), date.fromisoformat(m.group(3))
        if wanted is None or league in wanted:
            out.append((kind, league, day, path))
    return sorted(out, key=lambda x: (x[2], x[1], 0 if x[0] == "projections" else 1))


# --------------------------------------------------------------------------- projections

def projection_rows(league: str, as_of: str, snap: Mapping[str, Any]) -> list[dict[str, Any]]:
    version = int(snap.get("version") or 1)
    rows = []
    for rec in snap.get("players") or []:
        fm = rec.get("fm") or {}
        inputs = rec.get("inputs")
        rows.append({
            "league": league, "as_of": as_of, "cid": rec.get("cid"), "name": rec.get("name"),
            "nhl_id": _int(rec.get("nhl_id")), "team": rec.get("team"),
            "positions": ",".join(rec.get("positions") or []), "status": rec.get("status"),
            "pct_owned": rec.get("pct_owned"),
            "fpg": fm.get("fpg"), "fpg_season": fm.get("fpg_season"), "fpg_week": fm.get("fpg_week"),
            "proj_week": fm.get("proj_week"), "vorp": fm.get("vorp"),
            "games_next7": (inputs or {}).get("games_next7"),
            "rates_json": _j(fm.get("rates")) if fm else None,
            "projected_json": _j(rec.get("projected")), "inputs_json": _j(inputs),
            "archive_version": version,
        })
    return [r for r in rows if r["cid"]]


def ingest_projections(ledger: Ledger, league: str, as_of: str, snap: Mapping[str, Any]) -> int:
    ledger.execute("DELETE FROM projections WHERE league=? AND as_of=?", (league, as_of))
    return ledger.upsert("projections", projection_rows(league, as_of, snap), ("league", "as_of", "cid"))


# --------------------------------------------------------------------------- recommendations

def _v1_subjects(ledger: Ledger, league: str, as_of: str, rec: Mapping[str, Any]) -> list[dict[str, Any]]:
    """IR target of a v1 rec (no ``subjects`` key) from its title + that day's projections."""
    if rec.get("kind") not in ("injury", "waiver"):
        return []
    title = rec.get("title") or ""
    for rx in _IR_TITLE_RES:
        m = rx.search(title)
        if not m:
            continue
        name = m.group(1).strip()
        hit = ledger.query("SELECT cid, name, nhl_id FROM projections WHERE league=? AND as_of=? AND name=?",
                           (league, as_of, name))
        if len(hit) == 1:
            return [{"cid": hit[0]["cid"], "name": hit[0]["name"], "nhl_id": hit[0]["nhl_id"]}]
        return [{"cid": f"name:{name}", "name": name, "nhl_id": None}]
    return []


def rec_rows(ledger: Ledger, league: str, as_of: str, snap: Mapping[str, Any]
             ) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    version = int(snap.get("version") or 1)
    recs: dict[str, dict[str, Any]] = {}
    players: dict[tuple, dict[str, Any]] = {}
    for rec in snap.get("recommendations") or []:
        adds = [p for p in rec.get("add") or [] if p.get("cid")]
        drops = [p for p in rec.get("drop") or [] if p.get("cid")]
        subjects = rec.get("subjects") if version >= 2 else None
        if subjects is None:
            subjects = _v1_subjects(ledger, league, as_of, rec)
        subjects = [p for p in subjects if p.get("cid")]
        key = rec_key(league, rec["kind"], (p["cid"] for p in adds), (p["cid"] for p in drops),
                      rec.get("counterparty"), (p["cid"] for p in subjects))
        if key in recs:
            continue   # the same rec twice in one day: keep the first (best-ranked)
        recs[key] = {
            "league": league, "as_of": as_of, "rec_key": key, "kind": rec["kind"], "title": rec.get("title"),
            "score": rec.get("score"), "strength": rec.get("strength"),
            "predicted_gain": rec.get("predicted_gain"), "gain_units": rec.get("gain_units"),
            "horizon_days": rec.get("horizon_days"), "counterparty": rec.get("counterparty"),
            "reasons_json": _j(rec.get("reasons") or []), "archive_version": version,
            "params_hash": snap.get("params_hash"), "code_hash": snap.get("code_hash"),
        }
        for side, plist in (("add", adds), ("drop", drops), ("subject", subjects)):
            for p in plist:
                players[(key, side, p["cid"])] = {"league": league, "as_of": as_of, "rec_key": key, "side": side,
                                                  "cid": p["cid"], "name": p.get("name"),
                                                  "nhl_id": _int(p.get("nhl_id"))}
    return list(recs.values()), list(players.values())


def ingest_recs(ledger: Ledger, league: str, as_of: str, snap: Mapping[str, Any]) -> int:
    recs, players = rec_rows(ledger, league, as_of, snap)
    ledger.execute("DELETE FROM recs WHERE league=? AND as_of=?", (league, as_of))
    ledger.execute("DELETE FROM rec_players WHERE league=? AND as_of=?", (league, as_of))
    ledger.upsert("recs", recs, ("league", "as_of", "rec_key"))
    ledger.upsert("rec_players", players, ("league", "as_of", "rec_key", "side", "cid"))
    ledger.commit()
    return len(recs)


# --------------------------------------------------------------------------- episodes

def episode_id(league: str, key: str, first_seen: str) -> str:
    return hashlib.sha1(f"{league}|{key}|{first_seen}".encode("utf-8")).hexdigest()[:20]


def group_days(days: Iterable[date], gap: int = EPISODE_GAP_DAYS) -> list[list[date]]:
    """Sorted sighting days split into runs where consecutive sightings are <= ``gap`` apart."""
    out: list[list[date]] = []
    for d in sorted(set(days)):
        if out and (d - out[-1][-1]).days <= gap:
            out[-1].append(d)
        else:
            out.append([d])
    return out


def roll_episodes(ledger: Ledger, leagues: Iterable[str] | None = None) -> int:
    """Recompute episodes from ``recs`` (keeps each episode's match status); returns count."""
    leagues = list(leagues) if leagues else [r["league"] for r in ledger.query("SELECT DISTINCT league FROM recs")]
    total = 0
    for league in leagues:
        rows = ledger.query("SELECT rec_key, as_of, kind, title, predicted_gain, gain_units, horizon_days, strength"
                            " FROM recs WHERE league=? ORDER BY rec_key, as_of", (league,))
        by_key: dict[str, list[dict[str, Any]]] = {}
        for r in rows:
            by_key.setdefault(r["rec_key"], []).append(r)
        episodes = []
        for key, rs in by_key.items():
            by_day = {date.fromisoformat(r["as_of"]): r for r in rs}
            for run in group_days(by_day):
                first, last = by_day[run[0]], by_day[run[-1]]
                episodes.append({
                    "episode_id": episode_id(league, key, run[0].isoformat()), "league": league, "rec_key": key,
                    "kind": first["kind"], "title": last["title"], "first_seen": run[0].isoformat(),
                    "last_seen": run[-1].isoformat(), "n_days": len(run),
                    "predicted_gain": first["predicted_gain"], "gain_units": first["gain_units"],
                    "horizon_days": first["horizon_days"], "strength": first["strength"], "updated_at": now_iso(),
                })
        keep = {e["episode_id"] for e in episodes}
        stale = [r["episode_id"] for r in ledger.query("SELECT episode_id FROM rec_episodes WHERE league=?", (league,))
                 if r["episode_id"] not in keep]
        for eid in stale:
            ledger.execute("DELETE FROM rec_episodes WHERE episode_id=?", (eid,))
        ledger.upsert("rec_episodes", episodes, ("episode_id",))
        total += len(episodes)
    ledger.commit()
    return total


# --------------------------------------------------------------------------- entry points

def ingest_archive(ledger: Ledger, data_dir: Path | str, leagues: Iterable[str] | None = None,
                   force: bool = False) -> dict[str, Any]:
    """Ingest every (new or changed) archive file, then roll episodes. Idempotent."""
    leagues = list(leagues) if leagues else None
    stats = {"files_seen": 0, "files_ingested": 0, "projections": 0, "recs": 0, "episodes": 0, "errors": []}
    known = {r["path"]: r["sha1"] for r in ledger.query("SELECT path, sha1 FROM archive_files")}
    touched: set[str] = set()
    for kind, league, day, path in archive_files(data_dir, leagues):
        stats["files_seen"] += 1
        try:
            sha = _sha1(path)
        except OSError as e:
            stats["errors"].append(f"{path.name}: {e}")
            continue
        if not force and known.get(path.name) == sha:
            continue
        try:
            snap = load_snapshot(path)
        except (OSError, ValueError) as e:
            stats["errors"].append(f"{path.name}: {e}")
            continue
        as_of = str(snap.get("as_of") or day.isoformat())
        if kind == "projections":
            stats["projections"] += ingest_projections(ledger, league, as_of, snap)
        else:
            stats["recs"] += ingest_recs(ledger, league, as_of, snap)
            touched.add(league)
        ledger.upsert("archive_files", [{"path": path.name, "sha1": sha, "kind": kind, "league": league,
                                         "as_of": as_of, "version": int(snap.get("version") or 1),
                                         "ingested_at": now_iso()}], ("path",))
        stats["files_ingested"] += 1
    if touched or force:
        roll_episodes(ledger, sorted(touched) or leagues)
    if leagues:
        stats["episodes"] = sum(ledger.count("rec_episodes", "WHERE league=?", (lg,)) for lg in leagues)
    else:
        stats["episodes"] = ledger.count("rec_episodes")
    return stats


def record_run(ledger: Ledger, command: str, league: str | None = None, as_of: date | str | None = None,
               summary: Mapping[str, Any] | None = None, status: str = "ok") -> int:
    """Log one finished harness run with the current params / code hashes."""
    from ..backtest.archive import _hashes

    h = _hashes()
    rid = ledger.start_run(command, league, str(as_of) if as_of else None, h["params_hash"], h["code_hash"])
    ledger.finish_run(rid, status, summary)
    return rid
