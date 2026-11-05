#!/usr/bin/env python
"""Dump frozen parity fixtures for the Rust port (see docs/ARCHITECTURE.md "Parity harness").

For each league it writes ``parity/fixtures/<league>/``:

  snapshot.json        everything the pure computation needs, no network: the enriched
                       LeagueContext (deployment_details included), the module-level enrichment
                       registries (preseason / rookie / xG / line meta), the merged valuation params,
                       the provider side data (ages, rules, standings, trade blocks, ESPN league meta),
                       and the pinned clock ("today", "now").
  raw_ctx.json         provider.load() output before enrichment (dynasty mode resolved) - the
                       provider-parsing parity target.
  values.json          valuate_league: cid -> PlayerValue (excluded fields included).
  dynasty.json         apply_dynasty: cid -> DynastyValue (null when not a dynasty league).
  picks.json           draft pick assets by holder (dynasty leagues).
  engines/<name>.json  each recommender called directly on the snapshot (the inputs advise() uses).
  cli/<command>.json   `fm --league <l> --json <command>` output, with the loader patched to return
                       a deep copy of the snapshot (so every command sees the same inputs).
  tape.json.gz         every HTTP GET (HttpCache) and Fantrax fxpa POST answered during the run,
                       keyed by the Python cache key - lets the Rust providers/enrichment replay the
                       exact responses without network or cache.db. Large: gitignored.

and once: ``fixtures/_datadir/`` = the data-dir state the run started from (prefs.json, plans/,
lines/, status_history.db, crosswalk.db, news json, harness.db, fantrax_cache/ minus nothing the
commands need from the network). Rust CLI parity tests copy it to a temp dir and point
FM_DATA_DIR there. harness.db and fantrax_cache/ are gitignored (size).

The clock is frozen (``--now``, default: start time) inside every fantasy_manager module and the
script re-executes itself with PYTHONHASHSEED=0, so outputs are a function of the inputs only.

Usage (Windows, from anywhere):
  C:/code/fantasy_manager/.venv/Scripts/python parity/dump_fixtures.py [--leagues espn,fantrax]
      [--live] [--no-tape] [--out DIR] [--now ISO]

Default is offline (FM_OFFLINE=1, served from <repo>/data/cache.db). The script imports
``fantasy_manager`` from THIS worktree (not the installed package) and runs with cwd = the main
checkout (FM_REPO, default C:/code/fantasy_manager) so its .env is used. The data dir is a temp
copy of <repo>/data (minus cache.db and bulky archives) so commands that write state (plan
archive, status history) never touch the real one; the HTTP cache is opened read-only from the
real data dir.
"""
from __future__ import annotations

import argparse
import copy
import dataclasses
import datetime as dt
import gzip
import json
import math
import os
import shutil
import sys
import tempfile
import traceback
from pathlib import Path
from typing import Any

WORKTREE = Path(os.environ.get("FM_LEGACY_ROOT") or Path(__file__).resolve().parents[2] / "fm-legacy")  # legacy-python checkout
REPO = Path(os.environ.get("FM_REPO", r"C:\code\fantasy_manager"))
SCHEMA = 1
SKIP_DATA = {"cache.db", "archive", "backtest", "reports", "pregame"}


# --------------------------------------------------------------------------- json helpers

def jsonable(x: Any) -> Any:
    """Plain JSON value for pydantic models, dataclasses, dates, sets, tuple-keyed dicts."""
    if hasattr(x, "model_dump"):
        return x.model_dump(mode="json")
    if dataclasses.is_dataclass(x) and not isinstance(x, type):
        return {f.name: jsonable(getattr(x, f.name)) for f in dataclasses.fields(x)}
    if isinstance(x, dict):
        return {(k if isinstance(k, str) else ":".join(map(str, k)) if isinstance(k, tuple) else str(k)
                 if not isinstance(k, (dt.date,)) else k.isoformat()): jsonable(v) for k, v in x.items()}
    if isinstance(x, (list, tuple)):
        return [jsonable(v) for v in x]
    if isinstance(x, (set, frozenset)):
        return sorted(jsonable(v) for v in x)
    if isinstance(x, (dt.datetime, dt.date)):
        return x.isoformat()
    if isinstance(x, float) and not math.isfinite(x):
        return None
    if isinstance(x, (str, int, float, bool)) or x is None:
        return x
    return str(x)


def write_json(path: Path, data: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    # Compact (the fixture set is ~25 MB); pretty-print with `python -m json.tool` when debugging.
    path.write_text(json.dumps(data, ensure_ascii=False, allow_nan=False, default=str, separators=(",", ":"))
                    + "\n", encoding="utf-8")


def pv_json(pv: Any) -> dict[str, Any]:
    """PlayerValue with the dump-excluded fields (share_*, rookie) included; player by cid."""
    d = pv.model_dump(mode="json", exclude={"player", "rookie"})
    d["cid"] = pv.player.cid
    for k in ("share_source", "share_starts", "share_team_games"):
        d[k] = getattr(pv, k, None)
    d["rookie"] = jsonable(pv.rookie) if pv.rookie is not None else None
    return d


def dyn_json(dv: Any) -> dict[str, Any]:
    d = dv.model_dump(mode="json", exclude={"player"})
    d["cid"] = dv.player.cid
    return d


# --------------------------------------------------------------------------- recording tape

class Tape:
    def __init__(self) -> None:
        self.http: dict[str, Any] = {}
        self.fxpa: dict[str, Any] = {}

    def install(self) -> None:
        from fantasy_manager import cache as cache_mod
        from fantasy_manager.providers import fantrax as fx

        tape = self
        orig_fetch = cache_mod.HttpCache.fetch

        def fetch(self_, url, params=None, headers=None, cookies=None, ttl=3600):  # noqa: ANN001
            resp = orig_fetch(self_, url, params=params, headers=headers, cookies=cookies, ttl=ttl)
            key = cache_mod.cache_key("GET", url, params, headers)
            tape.http[key] = {"url": url, "params": jsonable(params), "key_headers": {
                k: v for k, v in (headers or {}).items() if k.lower() in cache_mod.KEY_HEADERS},
                "status": resp.status_code, "final_url": resp.url, "body": resp.text}
            return resp

        cache_mod.HttpCache.fetch = fetch

        orig_post = fx.FxpaClient._cached_post

        def cached_post(self_, msgs, fresh=False):  # noqa: ANN001
            body = orig_post(self_, msgs, fresh=fresh)
            tape.fxpa[self_._key(msgs)] = {"league": self_.league_id, "msgs": msgs, "body": body}
            return body

        fx.FxpaClient._cached_post = cached_post

    def dump(self, path: Path) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        with gzip.open(path, "wt", encoding="utf-8") as f:
            json.dump({"schema": SCHEMA, "http": self.http, "fxpa": self.fxpa}, f, ensure_ascii=False)


# --------------------------------------------------------------------------- snapshot pieces

def registries() -> dict[str, Any]:
    from fantasy_manager.providers import lines_enrich, preseason_enrich, rookie_enrich, xg_enrich

    return {
        "preseason_lines": [jsonable(v) for v in preseason_enrich._REGISTRY.values()],
        "rookie_history": {str(k): [jsonable(r) for r in v] for k, v in rookie_enrich._HISTORY.items()},
        "rookie_signals": {k: [jsonable(s) for s in v] for k, v in rookie_enrich._SIGNALS.items()},
        "gsax": [{"season_start_year": k[0], **jsonable(v)} for k, v in xg_enrich._GSAX.items()],
        "xg_norms": {str(k): v for k, v in xg_enrich._NORMS.items()},
        "line_team_meta": {k.isoformat(): jsonable(v) for k, v in lines_enrich._TEAM_META.items()},
    }


def params_info() -> dict[str, Any]:
    from fantasy_manager.valuation import params

    return {"merged": jsonable(params.load_params()), "active": jsonable(params.active_version()),
            "source": params.source(), "hash": getattr(params, "params_hash", lambda: None)()}


def env_flags() -> dict[str, Any]:
    keys = ("FM_OPP_ADJUST", "FM_ROOKIE_MODEL", "FM_PLAYOFF_START", "FM_NEWS_ROLES_LLM", "FM_PARAMS_OVERRIDE",
            "FANTRAX_KEEPERS", "FANTRAX_MODE", "FM_TZ", "TZ", "FM_OFFLINE")
    return {k: os.environ.get(k) for k in keys}


def provider_extras(provider: Any) -> dict[str, Any]:
    out: dict[str, Any] = {"kind": type(provider).__name__}
    for attr in ("warnings", "ages", "standings", "trade_blocks", "pending_trades", "transactions",
                 "scoring_source", "weights_mae", "weights_checked", "config_mae", "config_checked",
                 "draft_picks", "team_draft_picks", "acquisition_settings", "waiver_rank", "fantasy_points"):
        if hasattr(provider, attr):
            out[attr] = jsonable(getattr(provider, attr))
    rules = getattr(provider, "rules", None)
    out["rules"] = jsonable(rules) if rules is not None else None
    for prop in ("lineup_lock", "rules_dynasty"):
        try:
            out[prop] = jsonable(getattr(provider, prop))
        except Exception as e:  # noqa: BLE001
            out[prop] = None
            out.setdefault("errors", []).append(f"{prop}: {e}")
    league = getattr(provider, "_league_obj", None)
    if league is not None:
        st = getattr(league, "settings", None)
        out["espn_league"] = jsonable({
            "current_week": getattr(league, "current_week", None),
            "currentMatchupPeriod": getattr(league, "currentMatchupPeriod", None),
            "finalScoringPeriod": getattr(league, "finalScoringPeriod", None),
            "scoringPeriodId": getattr(league, "scoringPeriodId", None),
            "settings": {k: getattr(st, k, None) for k in (
                "matchup_periods", "reg_season_count", "playoff_team_count", "trade_deadline", "team_count",
                "playoff_matchup_period_length", "name")},
        })
    return out


# --------------------------------------------------------------------------- engines

def run_engines(d: Any, out_dir: Path, now: dt.datetime) -> dict[str, str]:
    """Each recommender on a deep copy of the snapshot; returns {engine: error} for failures."""
    from fantasy_manager import cli
    from fantasy_manager.recommend import advise as adv
    from fantasy_manager.recommend.injuries import StatusHistory

    errors: dict[str, str] = {}

    def fresh() -> tuple[Any, Any, Any]:
        return copy.deepcopy((d.lc, d.values, d.dyn))

    def save(name: str, fn) -> None:  # noqa: ANN001
        try:
            res = fn()
            write_json(out_dir / "engines" / f"{name}.json", jsonable(res))
        except Exception as e:  # noqa: BLE001
            errors[name] = f"{type(e).__name__}: {e}"
            write_json(out_dir / "engines" / f"{name}.error.json", {"error": errors[name],
                                                                     "trace": traceback.format_exc()})

    for name in adv.expand_include(("lineup", "waivers", "trades", "flags", "injuries", "alerts")):
        fn = adv._resolve(name)
        if fn is None:
            errors[name] = "unresolved"
            continue

        def call(fn=fn):  # noqa: ANN001
            lc, values, dyn = fresh()
            hist = StatusHistory(os.environ["FM_DATA_DIR"])
            try:
                return adv._call(fn, lc, values, dynasty_values=dyn, history=hist)
            finally:
                hist.close()
        save(name, call)

    def advise_all():
        lc, values, dyn = fresh()
        hist = StatusHistory(os.environ["FM_DATA_DIR"])
        try:
            loaded = cli.Loaded(lc, values, dyn, d.provider, d.cache)
            return cli._run_advise(loaded, hist, [])
        finally:
            hist.close()
    save("advise_merged", advise_all)

    def trades_with_blocks():
        from fantasy_manager.recommend.trades import recommend_trades
        lc, values, dyn = fresh()
        return recommend_trades(lc, values, dynasty_values=dyn, wants=cli._trade_wants(lc, d.provider),
                                offered=cli._trade_offered(d.provider))
    save("trades_with_blocks", trades_with_blocks)

    def need_trades():
        from fantasy_manager.recommend.trades import recommend_need_trades
        lc, values, dyn = fresh()
        return recommend_need_trades(lc, values, dynasty_values=dyn)
    save("need_trades", need_trades)
    return errors


# --------------------------------------------------------------------------- CLI

CLI_COMMANDS: list[tuple[str, list[str]]] = [
    ("settings", ["settings"]), ("picks", ["picks"]), ("roster", ["roster"]), ("waivers", ["waivers"]),
    ("lineup", ["lineup"]), ("injuries", ["injuries", "--no-record"]), ("trades", ["trades"]), ("trades_need", ["trades", "--need"]),
    ("flags", ["flags"]), ("advise", ["advise"]), ("alerts", ["alerts"]), ("trending", ["trending"]),
    ("lines", ["lines"]), ("goalies", ["goalies"]), ("schedule", ["schedule"]), ("playoffs", ["playoffs"]),
    ("outlook", ["outlook"]), ("matchup", ["matchup", "--seed", "7"]),
    ("plan", ["plan", "--no-archive"]), ("claims", ["claims"]),
]


def run_cli(league: str, d: Any, raw: tuple[Any, Any], out_dir: Path, extra: list[tuple[str, list[str]]]
            ) -> dict[str, str]:
    from typer.testing import CliRunner

    from fantasy_manager import cli

    def load_all(league_name, deep=False, cache=None, mode=None):  # noqa: ANN001
        lc, values, dyn = copy.deepcopy((d.lc, d.values, d.dyn))
        return cli.Loaded(lc, values, dyn, d.provider, d.cache)

    def load_context(league_name, cache=None, mode=None):  # noqa: ANN001
        return copy.deepcopy(raw[0]), raw[1]

    orig = (cli._load_all, cli._load_context)
    cli._load_all = load_all
    cli._load_context = load_context
    # The provider object is shared by every command; warnings one command raises after load (e.g.
    # `trending` -> "ESPN trending players unavailable") must not leak into the next one's meta.
    providers = [p for p in {id(d.provider): d.provider, id(raw[1]): raw[1]}.values() if hasattr(p, "warnings")]
    saved = [list(p.warnings) for p in providers]

    def reset() -> None:
        for p, w in zip(providers, saved):
            p.warnings[:] = w
    try:
        return _invoke_all(league, out_dir, extra, reset)
    finally:
        cli._load_all, cli._load_context = orig


def _invoke_all(league: str, out_dir: Path, extra: list[tuple[str, list[str]]], reset=lambda: None
                ) -> dict[str, str]:  # noqa: ANN001
    from typer.testing import CliRunner

    from fantasy_manager import cli

    runner = CliRunner()
    errors: dict[str, str] = {}
    for name, args in CLI_COMMANDS + extra:
        reset()
        res = runner.invoke(cli.app, ["--league", league, "--json", *args], catch_exceptions=True)
        path = out_dir / "cli" / f"{name}.json"
        try:
            payload = json.loads(res.stdout)
            write_json(path, {"args": args, "exit_code": res.exit_code, "output": payload})
        except Exception:  # noqa: BLE001
            errors[name] = f"exit {res.exit_code}: {(res.stdout or '')[-300:]!r} {res.exception!r}"
            write_json(out_dir / "cli" / f"{name}.error.json", {
                "args": args, "exit_code": res.exit_code, "stdout": res.stdout[-5000:],
                "exception": repr(res.exception),
                "trace": "".join(traceback.format_exception(*res.exc_info)) if res.exc_info else None})
    return errors


def trade_eval_args(d: Any) -> list[tuple[str, list[str]]]:
    """A deterministic offer: my best-valued player for the best player of the first other team."""
    mine = d.lc.my_team
    other = next((t for t in d.lc.teams if not t.owner_is_me), None)
    if other is None:
        return []

    def best(team: Any) -> Any:
        ps = [p for p in team.players if p.cid in d.values]
        return max(ps, key=lambda p: (d.values[p.cid].fpg_season, p.cid), default=None)
    give, get = best(mine), best(other)
    if give is None or get is None:
        return []
    return [("trade_eval", ["trade-eval", "--give", give.name, "--get", get.name])]


# --------------------------------------------------------------------------- main

def prepare_env(args: argparse.Namespace) -> Path:
    os.chdir(REPO)
    tmp = Path(tempfile.mkdtemp(prefix="fm_parity_"))
    src = REPO / "data"
    for item in src.iterdir():
        if item.name in SKIP_DATA:
            continue
        dst = tmp / item.name
        if item.is_dir():
            shutil.copytree(item, dst)
        else:
            shutil.copy2(item, dst)
    os.environ["FM_DATA_DIR"] = str(tmp)
    os.environ.setdefault("FANTRAX_KEEPERS", "3")
    os.environ["FM_NEWS_ROLES_LLM"] = "0"          # no LLM calls while dumping
    if not args.live:
        os.environ["FM_OFFLINE"] = "1"
    sys.path.insert(0, str(WORKTREE))
    for m in [m for m in sys.modules if m == "fantasy_manager" or m.startswith("fantasy_manager.")]:
        del sys.modules[m]
    import fantasy_manager

    assert Path(fantasy_manager.__file__).resolve().is_relative_to(WORKTREE), fantasy_manager.__file__
    freeze_clock(FROZEN_UTC)
    return tmp


# --------------------------------------------------------------------------- frozen clock

FROZEN_UTC: dt.datetime = dt.datetime.now(dt.timezone.utc).replace(microsecond=0)


def freeze_clock(now_utc: dt.datetime) -> None:
    """Pin datetime.now / date.today / time.time inside every fantasy_manager module to
    ``now_utc`` (the snapshot's "now"), so every output is a function of the snapshot alone.
    Module-level names are swapped after import (pydantic annotations already hold the real
    classes); the frozen classes construct REAL date/datetime instances."""
    import importlib
    import pkgutil
    import time as _time
    import types

    import fantasy_manager

    for info in pkgutil.walk_packages(fantasy_manager.__path__, "fantasy_manager."):
        try:
            importlib.import_module(info.name)
        except Exception:  # noqa: BLE001  (optional deps)
            pass
    real_dt, real_date = dt.datetime, dt.date
    local_naive = now_utc.astimezone().replace(tzinfo=None)

    class _Meta(type):
        def __instancecheck__(cls, obj: Any) -> bool:
            return isinstance(obj, cls.__mro__[1])

        def __subclasscheck__(cls, sub: Any) -> bool:
            return issubclass(sub, cls.__mro__[1])

    class FrozenDateTime(real_dt, metaclass=_Meta):
        def __new__(cls, *a: Any, **k: Any) -> Any:  # noqa: D102
            return real_dt(*a, **k)

        @classmethod
        def now(cls, tz: Any = None) -> Any:  # noqa: D102
            return local_naive if tz is None else now_utc.astimezone(tz)

        @classmethod
        def utcnow(cls) -> Any:  # noqa: D102
            return now_utc.replace(tzinfo=None)

        @classmethod
        def today(cls) -> Any:  # noqa: D102
            return local_naive

    class FrozenDate(real_date, metaclass=_Meta):
        def __new__(cls, *a: Any, **k: Any) -> Any:  # noqa: D102
            return real_date(*a, **k)

        @classmethod
        def today(cls) -> Any:  # noqa: D102
            return local_naive.date()

    dt_shim = types.ModuleType("datetime")
    dt_shim.__dict__.update({k: v for k, v in vars(dt).items() if not k.startswith("__")})
    dt_shim.datetime, dt_shim.date = FrozenDateTime, FrozenDate
    time_shim = types.ModuleType("time")
    time_shim.__dict__.update({k: v for k, v in vars(_time).items() if not k.startswith("__")})
    time_shim.time = lambda: now_utc.timestamp()
    for name, mod in list(sys.modules.items()):
        if not (name == "fantasy_manager" or name.startswith("fantasy_manager.")) or mod is None:
            continue
        for attr, val in list(vars(mod).items()):
            if val is real_dt:
                setattr(mod, attr, FrozenDateTime)
            elif val is real_date:
                setattr(mod, attr, FrozenDate)
            elif val is dt:
                setattr(mod, attr, dt_shim)
            elif val is _time:
                setattr(mod, attr, time_shim)


def dump_league(league: str, out_root: Path, tape: Tape | None) -> dict[str, Any]:
    from fantasy_manager import cli
    from fantasy_manager.cache import HttpCache
    from fantasy_manager.config import get_settings
    from fantasy_manager.providers import lines_enrich, preseason_enrich, rookie_enrich, xg_enrich
    from fantasy_manager.providers.enrich import enrich_context

    for mod in (preseason_enrich, rookie_enrich, xg_enrich):
        mod.clear_registry()
    lines_enrich._TEAM_META.clear()
    out_dir = out_root / league
    if out_dir.exists():
        shutil.rmtree(out_dir)
    if tape is not None:
        tape.http.clear()
        tape.fxpa.clear()
    now = FROZEN_UTC
    today = now.astimezone().date()
    cache = HttpCache(REPO / "data", offline=get_settings().fm_offline)
    cli._cache = lambda: cache  # read the real cache.db; the temp data dir holds everything else

    lc, provider = cli._load_context(league, cache)
    raw_ctx = copy.deepcopy(lc)
    write_json(out_dir / "raw_ctx.json", raw_ctx.model_dump(mode="json"))
    try:
        enrich_context(lc, get_settings(), cache)
    except Exception as e:  # noqa: BLE001  (mirror cli._load_all)
        lc.warnings.append(f"NHL enrichment failed: {e}")
    values = cli._valuate(lc)
    dyn = cli._dynasty(lc, values, provider)
    d = cli.Loaded(lc, values, dyn, provider, cache)

    ctx_json = lc.model_dump(mode="json")
    ctx_json["deployment_details"] = jsonable(lc.deployment_details)
    snapshot = {
        "schema": SCHEMA, "league": league, "generated_at": now.isoformat(), "now": now.isoformat(),
        "today": today.isoformat(), "local_now": now.astimezone().replace(tzinfo=None).isoformat(),
        "local_utc_offset_secs": int(now.astimezone().utcoffset().total_seconds()),
        "worktree_head": _git_head(), "ctx": ctx_json, "extras": registries(),
        "params": params_info(), "env": env_flags(), "provider": provider_extras(provider),
        "trade_wants": jsonable(cli._trade_wants(lc, provider)),
        "trade_offered": jsonable(cli._trade_offered(provider)),
    }
    write_json(out_dir / "snapshot.json", snapshot)
    write_json(out_dir / "values.json", {cid: pv_json(pv) for cid, pv in values.items()})
    write_json(out_dir / "dynasty.json", None if dyn is None else {cid: dyn_json(v) for cid, v in dyn.items()})
    try:
        picks = cli._picks_by_holder(lc, values, dyn)
        write_json(out_dir / "picks.json", {k: [cli._pick_json(a) for a in v] for k, v in (picks or {}).items()})
    except Exception as e:  # noqa: BLE001
        write_json(out_dir / "picks.error.json", {"error": repr(e), "trace": traceback.format_exc()})

    engine_errors = run_engines(d, out_dir, now)
    cli_errors = run_cli(league, d, (raw_ctx, provider), out_dir, trade_eval_args(d))
    if tape is not None:
        tape.dump(out_dir / "tape.json.gz")
    summary = {"league": league, "players": len(lc.all_players()), "values": len(values),
               "dynasty": dyn is not None, "engine_errors": engine_errors, "cli_errors": cli_errors,
               "tape": None if tape is None else {"http": len(tape.http), "fxpa": len(tape.fxpa)}}
    write_json(out_dir / "summary.json", summary)
    return summary


def _git_head() -> str | None:
    try:
        import subprocess
        return subprocess.run(["git", "-C", str(WORKTREE), "rev-parse", "HEAD"], capture_output=True, text=True,
                              check=True).stdout.strip()
    except Exception:  # noqa: BLE001
        return None


def main() -> None:
    # Set iteration order (hash-randomized for str) leaks into a few outputs (plan day lines, the
    # plan's win-probability input order, find_needs): pin it so fixtures are reproducible.
    if os.environ.get("PYTHONHASHSEED") is None:
        import subprocess
        env = dict(os.environ, PYTHONHASHSEED="0")
        raise SystemExit(subprocess.call([sys.executable, *sys.argv], env=env))
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--leagues", default="espn,fantrax")
    ap.add_argument("--out", default=str(Path(__file__).resolve().parent / "fixtures"))
    ap.add_argument("--live", action="store_true", help="allow network (default: offline from cache.db)")
    ap.add_argument("--no-tape", action="store_true")
    ap.add_argument("--now", help="pin the clock to this ISO time (default: now, to the second)")
    args = ap.parse_args()
    if args.now:
        global FROZEN_UTC
        t = dt.datetime.fromisoformat(args.now)
        FROZEN_UTC = (t if t.tzinfo else t.astimezone()).astimezone(dt.timezone.utc)
    tmp = prepare_env(args)
    tape = None if args.no_tape else Tape()
    if tape is not None:
        tape.install()
    out_root = Path(args.out)
    datadir_copy = out_root / "_datadir"
    if datadir_copy.exists():
        shutil.rmtree(datadir_copy)
    shutil.copytree(tmp, datadir_copy)
    try:
        for league in [x.strip() for x in args.leagues.split(",") if x.strip()]:
            s = dump_league(league, out_root, tape)
            print(json.dumps(s, indent=1))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
