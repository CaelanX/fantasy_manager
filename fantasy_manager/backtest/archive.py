"""Archive commercial projections and our recommendations so they can be graded later.

Files (one per provider per day; re-running the same day replaces that day's file, and an
identical snapshot is not rewritten):

* ``<fm_data_dir>/archive/projections-<provider>-YYYY-MM-DD.json`` - every player's provider
  ``projected`` StatLine, pct_owned, status and our own blended per-game rates (``fm``).
* ``<fm_data_dir>/archive/recs-<provider>-YYYY-MM-DD.json`` - the ranked recommendations.

Version 2 (``ARCHIVE_VERSION``) adds what is needed to refit the model later:

* file headers carry ``params_hash`` (valuation.params) and ``code_hash`` (sha1 over the
  valuation and recommend sources);
* each projection record adds an ``inputs`` block (season-to-date and last30/15/7 lines,
  the multi-season history baseline rates and GP, the provider projection, status, schedule
  counts, start share, birth date / age, pct_owned, positions) and ``fm`` adds fpg_season,
  fpg_week, proj_week and vorp; projection files are written compact (no indent);
* (added for the harness refit, M3) the projections header carries ``position_means`` (the
  positional mean rates a league projection is shrunk toward) and goalie inputs a ``share``
  block (``source`` projection / history and, for history, ``starts`` / ``team_games``), so
  ``harness.refit`` can replay the valuation with candidate parameters;
* recommendation records add ``predicted_gain`` / ``gain_units`` / ``horizon_days``,
  ``strength`` and ``subjects``.

Version 3 adds the new per-player signals to each projection record's ``inputs`` (flat keys,
only when the Player carries a value; see ``SIGNAL_FIELDS``): deployment (toi_per_game,
pp_toi_per_game, pp_share, toi_trend, pp_share_trend), market (pct_owned_change, pct_started,
adp, adp_change), lines / units / starts (line, pp_unit, pk_unit, confirmed_start) and luck
(ixg_per_game, goals_minus_ixg, onice_sh_pct, onice_xg_pct and ``xg_split``, the season they describe),
so the harness can test them later (and ``harness.refit`` replays the in-season xG goal shrink).

v1 and v2 files stay readable: every v2 key is optional for readers (``load_snapshot``), and a
v3 signal missing from ``inputs`` means "unknown" (``signal_inputs`` fills None for all of them).

``score_archive`` compares the provider projection, and ours, with actual season FPG.
"""
from __future__ import annotations

import hashlib
import json
from datetime import date, datetime
from pathlib import Path
from typing import Any, Iterable, Mapping

from ..models import LeagueContext, Player, Recommendation, ScoringConfig, StatLine, normalize_name
from .data import PlayerSeason
from .evaluate import metrics
from .scoring import fpg, scorer

ARCHIVE_VERSION = 3
# Player fields archived in ``inputs`` when set (v3). Filled by different enrichers; the archive
# captures whatever is there so the harness can grade them.
SIGNAL_FIELDS = ("toi_per_game", "pp_toi_per_game", "pp_share", "toi_trend", "pp_share_trend",
                 "pct_owned_change", "pct_started", "adp", "adp_change",
                 "line", "pp_unit", "pk_unit", "confirmed_start",
                 "ixg_per_game", "goals_minus_ixg", "onice_sh_pct", "onice_xg_pct", "xg_split")
PACKAGE_DIR = Path(__file__).resolve().parent.parent
CODE_HASH_DIRS = ("valuation", "recommend")


def code_hash(package_dir: Path | str | None = None) -> str:
    """sha1 over the sorted (relative path, contents) of valuation/*.py and recommend/*.py
    (line endings normalised), identifying the model code that produced a snapshot."""
    base = Path(package_dir) if package_dir else PACKAGE_DIR
    h = hashlib.sha1()
    files = sorted(f for d in CODE_HASH_DIRS for f in (base / d).glob("*.py"))
    for f in files:
        h.update(f.relative_to(base).as_posix().encode("utf-8") + b"\0")
        try:
            h.update(f.read_bytes().replace(b"\r\n", b"\n"))
        except OSError:
            continue
        h.update(b"\0")
    return h.hexdigest()


def _params_hash() -> str | None:
    try:
        from ..valuation.params import params_hash
        return params_hash()
    except Exception:
        return None


def _hashes() -> dict[str, str | None]:
    try:
        ch: str | None = code_hash()
    except Exception:
        ch = None
    return {"params_hash": _params_hash(), "code_hash": ch}


def archive_dir(data_dir: Path | str) -> Path:
    p = Path(data_dir) / "archive"
    p.mkdir(parents=True, exist_ok=True)
    return p


def _line(line: StatLine | None) -> dict[str, Any] | None:
    if line is None:
        return None
    return {"split": line.split, "gp": line.gp, "stats": dict(line.stats)}


def _player_ref(p: Player) -> dict[str, Any]:
    return {"cid": p.cid, "name": p.name, "nhl_id": p.nhl_id, "team": p.team, "positions": list(p.positions)}


def _write_dedup(path: Path, payload: dict[str, Any], content_key: str, indent: int | None = 1) -> str:
    """'created' / 'updated' / 'unchanged' (same content as today's existing file)."""
    if path.exists():
        try:
            old = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            old = None
        if old is not None and old.get(content_key) == json.loads(json.dumps(payload[content_key], default=str)):
            return "unchanged"
        status = "updated"
    else:
        status = "created"
    tmp = path.with_suffix(".tmp")
    text = (json.dumps(payload, indent=indent, default=str) if indent is not None
            else json.dumps(payload, separators=(",", ":"), default=str))
    tmp.write_text(text, encoding="utf-8")
    tmp.replace(path)
    return status


def _num(v: Any, nd: int = 4) -> float | None:
    try:
        return None if v is None else round(float(v), nd)
    except (TypeError, ValueError):
        return None


def _fm_block(pv: Any) -> dict[str, Any] | None:
    if pv is None:
        return None
    out: dict[str, Any] = {"fpg": round(float(pv.fpg), 4),
                           "rates": {k: round(float(v), 5) for k, v in (getattr(pv, "rates", None) or {}).items()}}
    for key in ("fpg_season", "fpg_week", "proj_week", "vorp"):
        v = _num(getattr(pv, key, None))
        if v is not None:
            out[key] = v
    return out


INPUT_SPLITS = ("season", "last30", "last15", "last7")


def player_inputs(p: Player, pv: Any, as_of: date, means: Mapping[str, Mapping[str, float]] | None
                  ) -> dict[str, Any]:
    """The v2 ``inputs`` block: everything the valuation read for this player on ``as_of``.

    Stat lines keep zero-valued stats (``zeros: true``): the valuation treats a stat missing
    from one side of a shrink as "borrow the other side", so a dropped zero would change the
    replayed rate. Files written before 2026-09-29 dropped zeros (no ``zeros`` key)."""
    from ..valuation.valuate import history_baseline, season_age

    age = season_age(p, as_of)
    inp: dict[str, Any] = {}
    for split in INPUT_SPLITS:
        ln = p.lines.get(split)
        if ln is not None:
            inp[split] = {"gp": ln.gp, "stats": dict(ln.stats), "zeros": True}
    hist = None
    if means is not None:
        try:
            rates, gp, _ = history_baseline(p, dict(means), age)
            if rates:
                hist = {"rates": {k: round(float(v), 5) for k, v in rates.items()}, "gp": gp}
        except Exception:
            hist = None
    inp["history"] = hist
    proj = p.lines.get("projected")
    inp["projection"] = None if proj is None else {"gp": proj.gp, "stats": dict(proj.stats)}
    inp.update({
        "status": p.status, "status_note": p.status_note,
        "games_next7": getattr(pv, "games_next7", None), "offnight_next7": getattr(pv, "offnight_next7", None),
        "start_share": _num(getattr(pv, "start_share", None)),
        "share": ({"source": getattr(pv, "share_source", None), "starts": _num(getattr(pv, "share_starts", None)),
                   "team_games": _num(getattr(pv, "share_team_games", None))}
                  if getattr(pv, "share_source", None) else None),
        "birth_date": p.birth_date.isoformat() if p.birth_date else None, "age": age,
        "pct_owned": p.pct_owned, "positions": list(p.positions),
    })
    inp.update(player_signals(p))
    return inp


def player_signals(p: Player) -> dict[str, Any]:
    """The v3 signals the Player carries (``SIGNAL_FIELDS`` that are not None; floats rounded)."""
    out: dict[str, Any] = {}
    for f in SIGNAL_FIELDS:
        v = getattr(p, f, None)
        if v is None:
            continue
        if isinstance(v, float):
            v = round(v, 5)
        out[f] = v
    return out


def signal_inputs(inputs: Mapping[str, Any] | None) -> dict[str, Any]:
    """Every ``SIGNAL_FIELDS`` value of an archived ``inputs`` block (any version; None when
    absent)."""
    return {f: (inputs or {}).get(f) for f in SIGNAL_FIELDS}


def _position_means(players: list[Player]) -> dict[str, dict[str, float]] | None:
    try:
        from ..valuation.valuate import positional_means
        return positional_means(players)
    except Exception:
        return None


def projection_records(ctx: LeagueContext, values: Mapping[str, Any] | None = None,
                       means: Mapping[str, Mapping[str, float]] | None = None) -> list[dict[str, Any]]:
    players = sorted(ctx.all_players(), key=lambda x: x.cid)
    if means is None:
        means = _position_means(players)
    day = ctx.as_of or date.today()
    out = []
    for p in players:
        pv = (values or {}).get(p.cid)
        rec = {**_player_ref(p), "ids": dict(p.ids), "status": p.status, "pct_owned": p.pct_owned,
               "projected": _line(p.lines.get("projected")), "fm": _fm_block(pv),
               "inputs": player_inputs(p, pv, day, means)}
        out.append(rec)
    return out


def archive_projections(ctx: LeagueContext, data_dir: Path | str, values: Mapping[str, Any] | None = None,
                        as_of: date | None = None) -> tuple[Path, str]:
    """Snapshot projections for ``ctx.provider`` on ``as_of`` (default ctx.as_of)."""
    day = as_of or ctx.as_of or date.today()
    path = archive_dir(data_dir) / f"projections-{ctx.provider}-{day.isoformat()}.json"
    means = _position_means(ctx.all_players())
    payload = {"version": ARCHIVE_VERSION, "provider": ctx.provider, "league_id": ctx.league_id,
               "season": ctx.season, "as_of": day.isoformat(),
               "archived_at": datetime.now().isoformat(timespec="seconds"), **_hashes(),
               "scoring": ctx.scoring.model_dump(),
               "position_means": {g: {k: round(float(v), 6) for k, v in m.items()} for g, m in (means or {}).items()},
               "players": projection_records(ctx, values, means)}
    return path, _write_dedup(path, payload, "players", indent=None)


def recommendation_records(recs: Iterable[Recommendation]) -> list[dict[str, Any]]:
    return [{"kind": r.kind, "score": round(float(r.score), 4), "title": r.title,
             "add": [_player_ref(p) for p in r.add], "drop": [_player_ref(p) for p in r.drop],
             "subjects": [_player_ref(p) for p in getattr(r, "subjects", None) or []],
             "counterparty": r.counterparty,
             "predicted_gain": _num(r.predicted_gain), "gain_units": r.gain_units, "horizon_days": r.horizon_days,
             "strength": _num(r.strength, 2),
             "reasons": [{"code": x.code, "text": x.text, "value": x.value, "baseline": x.baseline} for x in r.reasons]}
            for r in recs]


def archive_recommendations(recs: Iterable[Recommendation], provider: str, data_dir: Path | str,
                            as_of: date | None = None, league_id: str | None = None) -> tuple[Path, str]:
    day = as_of or date.today()
    path = archive_dir(data_dir) / f"recs-{provider}-{day.isoformat()}.json"
    payload = {"version": ARCHIVE_VERSION, "provider": provider, "league_id": league_id, "as_of": day.isoformat(),
               "archived_at": datetime.now().isoformat(timespec="seconds"), **_hashes(),
               "recommendations": recommendation_records(recs)}
    return path, _write_dedup(path, payload, "recommendations")


def load_snapshot(path: Path | str) -> dict[str, Any]:
    """Read an archive file (v1, v2 or v3); v2-only keys are filled with None / [] for v1 files.
    v3 signals are left out of older ``inputs`` blocks (read them with ``signal_inputs``)."""
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    data.setdefault("version", 1)
    data.setdefault("params_hash", None)
    data.setdefault("code_hash", None)
    if "players" in data:
        data.setdefault("position_means", None)
    for rec in data.get("players") or []:
        rec.setdefault("inputs", None)
    for rec in data.get("recommendations") or []:
        for key in ("predicted_gain", "gain_units", "horizon_days", "strength"):
            rec.setdefault(key, None)
        rec.setdefault("subjects", [])
    return data


# --------------------------------------------------------------------------- scoring the archive

def list_snapshots(data_dir: Path | str, provider: str, kind: str = "projections") -> list[Path]:
    return sorted(archive_dir(data_dir).glob(f"{kind}-{provider}-????-??-??.json"))


def _snapshot_date(path: Path) -> date:
    return date.fromisoformat(path.stem[-10:])


def pick_snapshot(data_dir: Path | str, provider: str, which: str = "first",
                  start: date | None = None, end: date | None = None) -> Path | None:
    """'first' / 'last' snapshot (optionally within [start, end]) or an ISO date."""
    snaps = [p for p in list_snapshots(data_dir, provider)
             if (start is None or _snapshot_date(p) >= start) and (end is None or _snapshot_date(p) <= end)]
    if not snaps:
        return None
    if which == "first":
        return snaps[0]
    if which == "last":
        return snaps[-1]
    match = [p for p in snaps if p.stem.endswith(which)]
    return match[0] if match else None


def _actuals_index(season_actuals: Iterable[PlayerSeason] | Mapping[int, PlayerSeason]
                   ) -> tuple[dict[int, PlayerSeason], dict[str, list[PlayerSeason]]]:
    rows = list(season_actuals.values()) if isinstance(season_actuals, Mapping) else list(season_actuals)
    by_id = {r.player_id: r for r in rows}
    by_name: dict[str, list[PlayerSeason]] = {}
    for r in rows:
        by_name.setdefault(normalize_name(r.name), []).append(r)
    return by_id, by_name


def score_snapshot(snapshot: Mapping[str, Any], season_actuals: Iterable[PlayerSeason] | Mapping[int, PlayerSeason],
                   scoring: ScoringConfig | None = None, min_gp: int = 20) -> dict[str, Any]:
    """Metrics of the provider projection and of ``fm`` vs actual FPG, on the players both cover."""
    cfg = scoring or ScoringConfig.model_validate(snapshot.get("scoring") or {"kind": "points"})
    sc = scorer(cfg)
    by_id, by_name = _actuals_index(season_actuals)
    pools: dict[str, dict[str, list[float]]] = {}
    unmatched = 0
    for rec in snapshot.get("players") or []:
        act = by_id.get(int(rec["nhl_id"])) if rec.get("nhl_id") not in (None, "") else None
        if act is None:
            cands = by_name.get(normalize_name(rec.get("name") or ""), [])
            act = cands[0] if len(cands) == 1 else None
        proj, mine = rec.get("projected"), rec.get("fm")
        if act is None:
            unmatched += 1
            continue
        if act.gp < min_gp or not proj or not proj.get("gp") or not mine or not mine.get("rates"):
            continue
        line = StatLine(split="projected", gp=int(proj["gp"]), stats=proj["stats"])
        pool = pools.setdefault("goalies" if act.group == "G" else "skaters",
                                {"provider": [], "fm": [], "actual": []})
        pool["provider"].append(fpg(line.per_game(), sc))
        pool["fm"].append(fpg(mine["rates"], sc))
        pool["actual"].append(fpg(act.per_game(), sc))
    out: dict[str, Any] = {"provider": snapshot.get("provider"), "as_of": snapshot.get("as_of"),
                           "unmatched": unmatched, "pools": {}}
    for pool, v in pools.items():
        top = pool == "skaters"
        out["pools"][pool] = {"provider": metrics(v["provider"], v["actual"], top).__dict__,
                              "fm": metrics(v["fm"], v["actual"], top).__dict__}
    return out


def score_archive(season_actuals: Iterable[PlayerSeason] | Mapping[int, PlayerSeason], data_dir: Path | str,
                  providers: Iterable[str] = ("espn", "fantrax"), which: str = "first",
                  scoring: ScoringConfig | None = None, min_gp: int = 20,
                  start: date | None = None, end: date | None = None) -> dict[str, Any]:
    """End-of-season grading: per provider, its archived projection vs ours vs actual FPG.

    ``which`` picks the snapshot ('first' = preseason, 'last', or an ISO date); pass
    ``scoring`` to grade every provider under one scoring (default: each league's own)."""
    out: dict[str, Any] = {}
    for prov in providers:
        path = pick_snapshot(data_dir, prov, which, start, end)
        if path is None:
            continue
        snap = json.loads(path.read_text(encoding="utf-8"))
        out[prov] = {"snapshot": path.name, **score_snapshot(snap, season_actuals, scoring, min_gp)}
    return out
