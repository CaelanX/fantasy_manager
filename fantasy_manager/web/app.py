"""FastAPI dashboard: overview, roster, recommendations, news and player pages.

Everything is computed by the same pipeline as the CLI (provider -> NHL enrichment -> valuation
-> dynasty -> ``advise``) and cached in memory per league for ``cache_ttl`` seconds. Creating the
app never touches the network; data is loaded lazily on the first request for a league.

The dynasty mode (contend / balanced / rebuild) comes from ``prefs.py`` (prefs.json set by the
header toggle or ``fm mode`` > FANTRAX_MODE > balanced). ``POST /mode`` saves it and drops the
league's cached result; ``PipelineLoader`` keeps the mode-independent half of the pipeline, so the
next page load only reruns dynasty valuation and ``advise``.

Access control lives in ``auth.py``: with ``FM_WEB_PASSWORD`` set every page and API needs a
login; without it the server answers loopback clients only (unless ``FM_WEB_ALLOW_INSECURE=1``).
``create_app(auth=None)`` (tests) installs no checks.

Run with ``fm web`` or ``python -m uvicorn fantasy_manager.web.app:app --port 8765``.
"""
from __future__ import annotations

import hashlib
import logging
import re
import threading
import time
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any, Callable, Mapping
from urllib.parse import parse_qsl, quote, urlencode, urlsplit, urlunsplit

from fastapi import FastAPI, Query, Request
from fastapi.responses import HTMLResponse, JSONResponse, RedirectResponse, Response
from fastapi.staticfiles import StaticFiles
from fastapi.templating import Jinja2Templates
from markupsafe import Markup, escape

from ..models import LeagueContext, Player, Recommendation
from ..providers.base import ProviderError
from . import auth as web_auth
from . import views
from .views import is_prospect, player_age

log = logging.getLogger(__name__)

HERE = Path(__file__).parent
LEAGUES = ("espn", "fantrax")
LEAGUE_PATTERN = "^(espn|fantrax)$"
CACHE_TTL = 600.0

SLOT_ORDER = {s: i for i, s in enumerate(("C", "LW", "RW", "F", "D", "UTIL", "G", "BN", "IR"))}
ALERT_STATUSES = ("dtd", "out", "ir", "ltir", "suspended")
STATUS_LABEL = {"healthy": "Healthy", "dtd": "Day-to-day", "out": "Out", "ir": "IR", "ltir": "LTIR",
                "suspended": "Suspended", "unknown": "Unknown"}
STATUS_CLASS = {"healthy": "ok", "dtd": "dtd", "out": "out", "ir": "ir", "ltir": "ir",
                "suspended": "susp", "unknown": "unk"}
KIND_LABEL = {"lineup": "Lineup", "waiver": "Waiver", "trade": "Trade", "sell_high": "Sell high",
              "buy_low": "Buy low", "injury": "Injury", "alert": "Alert"}
# (label for rec.add, label for rec.drop) per kind
MOVE_LABEL = {"waiver": ("Add", "Drop"), "injury": ("Add", "Drop"), "trade": ("Get", "Give"),
              "lineup": ("Start", "Sit"), "sell_high": ("Buy", "Sell"), "buy_low": ("Buy", "Sell"),
              "alert": ("Add", "Drop")}
# filter key -> (tab label, rec kinds), in display priority order
KIND_FILTERS: dict[str, tuple[str, tuple[str, ...]]] = {
    "injury": ("Injury", ("injury",)),
    "lineup": ("Lineup", ("lineup",)),
    "waiver": ("Waiver", ("waiver",)),
    "trade": ("Trade", ("trade",)),
    "flags": ("Flags", ("sell_high", "buy_low")),
    "alert": ("Alerts", ("alert",)),
}
OVERVIEW_MOVES = 5          # moves listed on the overview
REASONS_SHOWN = 3           # reasons shown per move before the "more reasons" disclosure
HIDDEN_REASONS = {"RAW_SCORE"}

# (env var, Settings attribute, requirement note) shown on the setup page
SETUP_VARS: dict[str, list[tuple[str, str, str]]] = {
    "espn": [
        ("ESPN_LEAGUE_ID", "espn_league_id", "required - the leagueId=... number in your league URL"),
        ("ESPN_S2", "espn_s2", "private leagues - espn_s2 cookie from a logged-in browser"),
        ("ESPN_SWID", "espn_swid", "private leagues - SWID cookie, including the braces"),
        ("ESPN_YEAR", "espn_year", "optional - season as ESPN labels it (2026-27 is 2027)"),
        ("ESPN_TEAM", "espn_team", "optional - your team id or part of its name"),
    ],
    "fantrax": [
        ("FANTRAX_LEAGUE_ID", "fantrax_league_id", "required - the id in your league URL"),
        ("FANTRAX_USERNAME", "fantrax_username", "recommended - your Fantrax login (fm logs in itself)"),
        ("FANTRAX_PASSWORD", "fantrax_password", "recommended - stored only in the local .env"),
        ("FANTRAX_COOKIE", "fantrax_cookie", "alternative to a login - Cookie request header"),
        ("FANTRAX_COOKIE_FILE", "fantrax_cookie_file", "alternative to FANTRAX_COOKIE - path to a cookie file"),
        ("FANTRAX_TEAM", "fantrax_team", "optional - your team id or part of its name"),
        ("FANTRAX_POINTS", "fantrax_points", "recommended - your point values, e.g. G=3,A=2,SOG=0.4"),
    ],
}


# --------------------------------------------------------------------------- data loading

@dataclass
class LoadResult:
    ctx: LeagueContext
    values: Mapping[str, Any]
    dynasty: Mapping[str, Any] | None
    recs: list[Recommendation]
    news_by_cid: Mapping[str, list[Any]] = field(default_factory=dict)
    loaded_at: datetime = field(default_factory=datetime.now)
    warnings: list[str] = field(default_factory=list)
    # Dynasty mode these values were computed with, and where it came from ("prefs", "env",
    # "default"). None for loaders that do not resolve it (the page then shows the current mode).
    mode: str | None = None
    mode_source: str | None = None
    # Seconds spent computing this result, and whether only the mode-dependent half was rerun.
    compute_seconds: float | None = None
    recalc_only: bool = False
    # cid -> recorded status changes (injury history), oldest first; empty when unavailable.
    status_log: Mapping[str, list[Any]] = field(default_factory=dict)
    # rec_key -> {"text", "model"} or {"error"}: on-demand LLM explanations (POST /explain),
    # kept until the result is dropped (refresh / TTL / mode change).
    explanations: dict[str, dict[str, Any]] = field(default_factory=dict, repr=False, compare=False)
    # Memo for derived view models (lineups, standings, ...), keyed by view name.
    memo: dict[str, Any] = field(default_factory=dict, repr=False, compare=False)


Loader = Callable[[str], LoadResult]


def current_mode() -> tuple[str, str]:
    """(effective dynasty mode, source): prefs.json > FANTRAX_MODE > default; never raises."""
    from ..prefs import DEFAULT_DYNASTY_MODE, dynasty_mode_info

    try:
        from ..config import get_settings

        return dynasty_mode_info(get_settings())
    except Exception:  # a bad .env still lets the page render
        return DEFAULT_DYNASTY_MODE, "default"


def _dynasty(ctx: LeagueContext, values: Mapping[str, Any], provider: Any) -> dict[str, Any] | None:
    if not ctx.dynasty:
        return None
    try:
        from ..valuation.dynasty import apply_dynasty
    except (ImportError, AttributeError, SyntaxError):
        ctx.warnings.append("Dynasty valuation is not available yet.")
        return None
    ages = getattr(provider, "ages", None)
    try:
        return apply_dynasty(values, ctx, ages=ages if isinstance(ages, Mapping) and ages else None)
    except Exception as e:  # never let an optional column sink the page
        ctx.warnings.append(f"Dynasty valuation failed: {e}")
        return None


def _news(ctx: LeagueContext, cache: Any) -> dict[str, list[Any]]:
    from ..providers.enrich import make_fetch_text
    from ..providers.news import fetch_all_news
    from ..report.news_match import match_news_to_players

    items = fetch_all_news(make_fetch_text(cache))
    return match_news_to_players(items, ctx.all_players())


@dataclass
class BaseLoad:
    """The mode-independent (expensive) half of a load: provider + NHL enrichment + valuation
    + news. ``finish`` reruns only dynasty valuation and ``advise`` on top of it."""
    ctx: LeagueContext
    values: Mapping[str, Any]
    provider: Any
    news_by_cid: Mapping[str, list[Any]]
    loaded_at: datetime
    seconds: float
    # the provider's HTTP cache, kept open while the base is cached: the /matchup preview and the
    # overview's matchup strip call the provider (ESPN scoreboard) after the load
    cache: Any = None


def load_base(league: str) -> BaseLoad:
    """Provider load -> NHL enrichment -> valuation -> news (no dynasty, no recommendations)."""
    from ..cache import CacheMiss, HttpCache
    from ..config import get_settings
    from ..providers import get_provider
    from ..providers.enrich import enrich_context
    from ..scoring import fit_to_context, from_config
    from ..valuation.valuate import valuate_league

    t0 = time.perf_counter()
    settings = get_settings()
    cache = HttpCache(settings.fm_data_dir, offline=settings.fm_offline)
    ok = False
    try:
        provider = get_provider(league, settings, cache)
        try:
            ctx = provider.load()
        except CacheMiss as e:
            raise ProviderError(f"Offline mode (FM_OFFLINE=1) and league data is not cached: {e}") from e
        ctx.warnings.extend(str(w) for w in getattr(provider, "warnings", None) or [])
        try:
            enrich_context(ctx, settings, cache)
        except Exception as e:  # enrichment is best effort
            ctx.warnings.append(f"NHL enrichment failed: {e}")
        scoring = fit_to_context(from_config(ctx.scoring), ctx)
        values = valuate_league(ctx, scoring)
        try:
            news = _news(ctx, cache)
        except Exception as e:
            ctx.warnings.append(f"News feeds failed: {e}")
            news = {}
        ok = True
    finally:
        if not ok:
            cache.close()
    return BaseLoad(ctx=ctx, values=values, provider=provider, news_by_cid=news, loaded_at=datetime.now(),
                    seconds=time.perf_counter() - t0, cache=cache)


def finish(base: BaseLoad, mode: str, source: str, recalc_only: bool = False) -> LoadResult:
    """Apply a dynasty mode to a BaseLoad: dynasty values + recommendations. Works on a shallow
    copy of the context, so the base can be reused for another mode."""
    from ..config import get_settings
    from ..recommend.advise import advise
    from ..recommend.injuries import StatusHistory

    t0 = time.perf_counter()
    ctx = base.ctx.model_copy(update={"dynasty_mode": mode, "dynasty_mode_source": source,
                                      "warnings": list(base.ctx.warnings),
                                      "source_notes": list(base.ctx.source_notes)})
    dyn = _dynasty(ctx, base.values, base.provider)
    history = StatusHistory(get_settings().fm_data_dir)
    try:  # read-only: `fm injuries` owns recording new snapshots
        recs = advise(ctx, base.values, dynasty_values=dyn, history=history)
        try:
            status_log = history.by_player()
        except Exception as e:  # the injury history is display-only
            ctx.warnings.append(f"Injury history unavailable: {e}")
            status_log = {}
    finally:
        history.close()
    secs = time.perf_counter() - t0 + (0.0 if recalc_only else base.seconds)
    return LoadResult(ctx=ctx, values=base.values, dynasty=dyn, recs=recs, news_by_cid=base.news_by_cid,
                      loaded_at=base.loaded_at, warnings=list(ctx.warnings), mode=mode, mode_source=source,
                      compute_seconds=secs, recalc_only=recalc_only, status_log=status_log)


def default_loader(league: str) -> LoadResult:
    """The CLI pipeline (``fm advise``) without importing the typer app, using the effective
    dynasty mode (prefs.json > FANTRAX_MODE > default)."""
    return finish(load_base(league), *current_mode())


class PipelineLoader:
    """``default_loader`` that keeps the mode-independent BaseLoad per league for ``ttl``
    seconds, so a dynasty-mode change only reruns ``apply_dynasty`` + ``advise`` (about a
    second) instead of the provider/NHL/news pipeline. ``invalidate`` drops the base (used by
    /refresh)."""

    def __init__(self, ttl: float = CACHE_TTL, clock: Callable[[], float] = time.monotonic,
                 base_loader: Callable[[str], BaseLoad] = load_base):
        self.ttl, self.clock, self.base_loader = ttl, clock, base_loader
        self._bases: dict[str, tuple[float, BaseLoad]] = {}
        self._guard = threading.Lock()

    def __call__(self, league: str) -> LoadResult:
        mode, source = current_mode()
        with self._guard:
            hit = self._bases.get(league)
        if hit is not None and self.clock() - hit[0] < self.ttl:
            return finish(hit[1], mode, source, recalc_only=True)
        base = self.base_loader(league)
        with self._guard:
            old = self._bases.get(league)
            self._bases[league] = (self.clock(), base)
        old_cache = getattr(old[1], "cache", None) if old is not None else None
        if old_cache is not None and old_cache is not getattr(base, "cache", None):
            try:  # the replaced base's provider is no longer reachable from the pages
                old_cache.close()
            except Exception:  # noqa: BLE001
                pass
        return finish(base, mode, source)

    def invalidate(self, league: str | None = None) -> None:
        with self._guard:
            if league is None:
                self._bases.clear()
            else:
                self._bases.pop(league, None)


class ResultCache:
    """Per-league LoadResult cache with a TTL; one load at a time per league."""

    def __init__(self, loader: Loader, ttl: float = CACHE_TTL, clock: Callable[[], float] = time.monotonic):
        self.loader, self.ttl, self.clock = loader, ttl, clock
        self._items: dict[str, tuple[float, LoadResult]] = {}
        self._locks: dict[str, threading.Lock] = {}
        self._guard = threading.Lock()

    def _lock(self, league: str) -> threading.Lock:
        with self._guard:
            return self._locks.setdefault(league, threading.Lock())

    def get(self, league: str) -> LoadResult:
        with self._lock(league):
            hit = self._items.get(league)
            if hit is not None and self.clock() - hit[0] < self.ttl:
                return hit[1]
            result = self.loader(league)
            self._items[league] = (self.clock(), result)
            return result

    def age(self, league: str) -> float | None:
        hit = self._items.get(league)
        return None if hit is None else self.clock() - hit[0]

    def peek(self, league: str) -> LoadResult | None:
        """The cached result (even if expired) without loading."""
        hit = self._items.get(league)
        return None if hit is None else hit[1]

    def clear(self, league: str | None = None) -> None:
        with self._guard:
            if league is None:
                self._items.clear()
            else:
                self._items.pop(league, None)


# --------------------------------------------------------------------------- view helpers

def _sources_footer(ctx: LeagueContext | None) -> dict[str, str] | None:
    """Compact data-freshness line for the footer (report.health.footer_summary); never raises."""
    try:
        from ..report.health import footer_summary

        return footer_summary(ctx)
    except Exception:
        return None


def _moves_footer(ctx: LeagueContext) -> str | None:
    """Footer league line: "moves: 2 per matchup period (1 used, 1 left, resets Mon Oct 5)";
    None when the context carries no budget information."""
    try:
        from ..recommend.base import moves_text

        if ctx.moves_limit_per_period is None and ctx.moves_used_this_period is None \
                and ctx.moves_limit_season is None:
            return None
        return moves_text(ctx)
    except Exception:  # never let a footer line sink a page
        return None


def fmt_num(v: Any, digits: int = 2, signed: bool = False) -> str:
    if not isinstance(v, (int, float)):
        return "-"
    return f"{v:+.{digits}f}" if signed else f"{v:.{digits}f}"


def fmt1(v: Any, signed: bool = False) -> str:
    """One decimal (two when a non-zero value would otherwise print as 0.0)."""
    if not isinstance(v, (int, float)) or isinstance(v, bool):
        return "-"
    d = 2 if v and abs(v) < 0.1 else 1
    return f"{v:+.{d}f}" if signed else f"{v:.{d}f}"


def fmt_rate(v: Any, stat: str = "") -> str:
    """Per-game box-score rates: SV% 3 decimals, everything else 2 (0.35 G/GP needs both)."""
    if not isinstance(v, (int, float)) or isinstance(v, bool):
        return "-"
    return f"{v:.3f}" if stat == "SVPCT" else f"{v:.2f}"


def rec_key(league: str, r: Recommendation) -> str:
    """Stable id of a move across reloads: sha1 of league|kind|sorted add cids|sorted drop
    cids|counterparty (the same key the harness ledger uses)."""
    raw = "|".join((league, r.kind, ",".join(sorted(p.cid for p in r.add)),
                    ",".join(sorted(p.cid for p in r.drop)), r.counterparty or ""))
    return hashlib.sha1(raw.encode("utf-8")).hexdigest()


def pos_str(p: Player) -> str:
    return "/".join(x for x in p.positions if x != "F") or "/".join(p.positions) or "-"


def ago(when: datetime | None, now: datetime | None = None) -> str:
    if when is None:
        return "never"
    secs = ((now or datetime.now()) - when).total_seconds()
    if secs < 60:
        return "just now"
    if secs < 3600:
        return f"{secs // 60:.0f} min ago"
    if secs < 86400:
        return f"{secs // 3600:.0f} h ago"
    return f"{secs // 86400:.0f} d ago"


def reason_label(code: str) -> str:
    return code.replace("_", " ").lower()


def score_pct(score: float) -> int:
    return max(2, min(100, round(float(score) * 10)))


def kind_group(kind: str) -> str:
    return "flags" if kind in ("sell_high", "buy_low") else kind


def val(values: Mapping[str, Any], cid: str, attr: str) -> Any:
    v = values.get(cid)
    return getattr(v, attr, None) if v is not None else None


def dyn_value(dynasty: Mapping[str, Any] | None, cid: str) -> float | None:
    d = (dynasty or {}).get(cid)
    v = getattr(d, "value", None)
    return float(v) if isinstance(v, (int, float)) else None


def safe_url(url: str | None) -> str | None:
    return url if url and url.lower().startswith(("https://", "http://")) else None


def _ranked(recs: list[Recommendation]) -> list[Recommendation]:
    return sorted(recs, key=lambda r: -r.score)


def _filter_recs(recs: list[Recommendation], kind: str | None) -> list[Recommendation]:
    if not kind or kind == "all":
        return _ranked(recs)
    kinds = KIND_FILTERS[kind][1] if kind in KIND_FILTERS else (kind,)
    return _ranked([r for r in recs if r.kind in kinds])


def kind_counts(recs: list[Recommendation]) -> dict[str, int]:
    return {k: len(_filter_recs(recs, k)) for k in KIND_FILTERS}


def normalize_kind(kind: str | None) -> str:
    return kind if kind and (kind in KIND_FILTERS or kind in KIND_LABEL) else "all"


# --------------------------------------------------------------------------- move wording

NAME_SUFFIXES = {"jr", "jr.", "sr", "sr.", "ii", "iii", "iv"}
NUMBER_WORDS = ("No", "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine", "Ten")
LEAD_VERBS = {"move", "add", "start", "trade", "activate", "bench", "sell", "buy"}


def last_name(name: str) -> str:
    parts = name.split()
    while len(parts) > 1 and parts[-1].lower() in NAME_SUFFIXES:
        parts.pop()
    return parts[-1] if parts else name


def _short_names(players: list[Player], others: list[Player] = ()) -> list[str]:
    """Last names, or full names when two players in the same move share a last name."""
    lasts = [last_name(p.name) for p in (*players, *others)]
    return [last_name(p.name) if lasts.count(last_name(p.name)) == 1 else p.name for p in players]


def action_line(r: Recommendation) -> str:
    """One imperative "what to do" line per move, e.g. "Start Lafreniere over Stenberg" or
    "Propose: Hughes for Thompson + Weegar". Only text: the dashboard never makes moves."""
    title = r.title.strip()
    add, drop = list(r.add), list(r.drop)
    if r.kind == "lineup":
        if title.startswith("Bench ") and drop:
            return f"Bench {_short_names(drop)[0]}"
        if title.startswith("Activate ") and add:
            return f"Activate {_short_names(add)[0]} from IR"
        if add:
            ins, outs = _short_names(add, drop), _short_names(drop, add)
            return f"Start {' + '.join(ins)}" + (f" over {' + '.join(outs)}" if outs else "")
    if r.kind == "trade" and add and drop:
        return f"Propose: {' + '.join(_short_names(drop, add))} for {' + '.join(_short_names(add, drop))}"
    if r.kind == "injury":
        if title.startswith("Move "):
            return title.replace(" and add ", ", add ")
        if " now " in title and not title.startswith(("Activate ", "Add ")):
            name, _, rest = title.partition(" now ")
            return f"Check on {name}: now {rest}"
        if title.startswith("Activate ") and drop:
            return f"{title}, drop {drop[0].name}"
        return title
    if r.kind == "waiver":
        return title.replace(" (open roster spot)", "")
    if r.kind == "sell_high" and drop:
        return f"Shop {_short_names(drop)[0]} while his value is high"
    if r.kind == "buy_low" and add:
        cp = (r.counterparty or "").strip()
        return f"Ask about {_short_names(add)[0]}" + (f" ({cp})" if cp else "") + " while his value is low"
    return title


def action_note(r: Recommendation) -> str | None:
    """Context the action line drops from the title (the Fantrax weekly lineup lock, an open
    roster spot); None when the title adds nothing (names and counterparty are shown anyway)."""
    title = r.title.strip()
    if r.kind == "lineup" and title.endswith("(locks Monday)"):
        return "For this week's lineup: Fantrax locks it on Monday."
    if r.kind == "lineup" and "; move " in title:
        # chain: "Start A at UTIL for B; move B to C over D (out)" - the line shows A over D only
        rest = title.split("; ", 1)[1].strip()
        return f"Then {rest}" + ("" if rest.endswith(".") else ".")
    if r.kind == "lineup" and ("(empty slot)" in title or "(open starting slot)" in title):
        return "Fills an empty starting slot."
    if r.kind == "lineup" and title.startswith("Bench ") and ":" in title:
        return title.split(":", 1)[1].strip().capitalize() + "."
    if r.kind == "waiver" and "(open roster spot)" in title:
        return "Uses an open roster spot, no drop needed."
    return None


def _number_word(n: int, capital: bool = True) -> str:
    word = NUMBER_WORDS[n] if 0 <= n < len(NUMBER_WORDS) else str(n)
    return word if capital else word.lower()


def _status_phrase(status: str) -> str:
    if status in ("ir", "ltir"):
        return f"is on {STATUS_LABEL[status]}"
    return {"dtd": "is day-to-day", "out": "is out", "suspended": "is suspended"}.get(status, f"is {status}")


def _join(items: list[Markup]) -> Markup:
    if len(items) <= 2:
        return Markup(" and ").join(items)
    return Markup(", ").join(items[:-1]) + Markup(", and ") + items[-1]


def overview_summary(recs: list[Recommendation], alerts: list[tuple[str, Player]],
                     prospects: int = 0) -> Markup:
    """The plain-English lede on the overview: how many moves, the strongest one, and who is
    hurt (plus prospects in dynasty leagues). Built here, not in the template, so it is
    testable; every name is HTML-escaped."""
    parts: list[Markup] = []
    ranked = _ranked(recs)
    n = len(ranked)
    if n == 0:
        parts.append(Markup("No moves are recommended right now."))
    else:
        parts.append(Markup("<b>{}</b> {} on the board.").format(
            f"{n} move{'s' if n != 1 else ''}", "is" if n == 1 else "are"))
        title = ranked[0].title.strip().rstrip(".")
        lead = "The strongest" if n > 1 else "It"
        if title.split(" ", 1)[0].lower() in LEAD_VERBS:
            parts.append(Markup("{} is to {}.").format(lead, title[:1].lower() + title[1:]))
        else:
            parts.append(Markup("{}: {}.").format(lead, title))
    k = len(alerts)
    if k == 0:
        parts.append(Markup("Nobody on your roster is hurt."))
    elif k == 1:
        p = alerts[0][1]
        parts.append(Markup("One player needs attention: <b>{}</b> {}.").format(p.name, _status_phrase(p.status)))
    else:
        who = [Markup("<b>{}</b> ({})").format(p.name, STATUS_LABEL.get(p.status, p.status)) for _, p in alerts]
        parts.append(Markup("{} players need attention: {}.").format(_number_word(k), _join(who)))
    if prospects:
        parts.append(Markup("You carry {}.").format(
            "one prospect" if prospects == 1 else f"{_number_word(prospects, False)} prospects"))
    return Markup(" ").join(parts)


def vorp_bar(v: Any, scale: float) -> dict[str, Any] | None:
    """Bar geometry for a signed VORP: negative grows left of the centre line, positive right."""
    if not isinstance(v, (int, float)):
        return None
    width = min(50.0, abs(v) / scale * 50.0) if scale > 0 else 0.0
    return {"side": "pos" if v >= 0 else "neg", "width": round(width, 1)}


def _my_team(ctx: LeagueContext):
    try:
        return ctx.my_team
    except LookupError:
        return ctx.teams[0] if ctx.teams else None


def _where(ctx: LeagueContext, cid: str) -> tuple[Any, str | None]:
    for t in ctx.teams:
        for s in t.slots:
            if s.player is not None and s.player.cid == cid:
                return t, s.slot
    return None, None


def _meta(res: LoadResult) -> dict[str, Any]:
    lc = res.ctx
    return {"provider": lc.provider, "league_id": lc.league_id, "league_name": lc.name,
            "as_of": lc.as_of.isoformat(), "loaded_at": res.loaded_at.isoformat(timespec="seconds"),
            "season_start": lc.season_start.isoformat() if lc.season_start else None,
            "sources": list(lc.source_notes), "warnings": list(res.warnings or lc.warnings)}


def _setup_rows(league: str) -> list[dict[str, Any]]:
    try:
        from ..config import get_settings

        s = get_settings()
    except Exception:
        s = None
    rows = []
    for env, attr, note in SETUP_VARS.get(league, []):
        value = getattr(s, attr, None) if s is not None else None
        rows.append({"env": env, "note": note, "set": bool(value) if s is not None else None})
    return rows


def safe_next(next_url: str | None, league: str, **extra: str) -> str:
    """A same-site redirect target: a path starting with a single "/" (anything else -> "/"),
    with ``league`` added when missing, any old ``msg`` dropped and ``extra`` params appended."""
    target = next_url or "/"
    if (not target.startswith("/") or target.startswith("//") or "\\" in target
            or any(ord(c) < 32 or ord(c) == 127 for c in target)):
        target = "/"
    parts = urlsplit(target)
    if parts.scheme or parts.netloc:
        parts = urlsplit("/")
    query = [(k, v) for k, v in parse_qsl(parts.query, keep_blank_values=True) if k not in ("msg", *extra)]
    if not any(k == "league" for k, _ in query):
        query.append(("league", league))
    query.extend(extra.items())
    return urlunsplit(("", "", parts.path or "/", urlencode(query), ""))


def _mode_flash(msg: str | None, res: LoadResult | None) -> str | None:
    """Text for ``?msg=mode-<mode>`` (set by POST /mode); anything else shows nothing."""
    from ..prefs import normalize_mode

    if not msg or not msg.startswith("mode-"):
        return None
    mode = normalize_mode(msg[len("mode-"):])
    if mode is None:
        return None
    text = f"Recalculating with {mode} mode… done"
    if res is not None and res.compute_seconds is not None and (res.mode in (None, mode)):
        text += f" in {res.compute_seconds:.1f}s" + (" (cached league data reused)" if res.recalc_only else "")
    return text + f". Dynasty values and recommendations now use {mode} weights."


# --------------------------------------------------------------------------- app factory

def _data_dir() -> Path:
    from ..config import get_settings

    return Path(get_settings().fm_data_dir)


def _params_without_ledger(data_dir: Path) -> dict[str, Any]:
    try:
        from ..harness.params_store import ParamsStore
        from ..valuation.params import params_hash, source

        st = ParamsStore(data_dir=data_dir)
        return {"version": st.active_name(), "hash": params_hash(), "source": source(), "versions": len(st.versions())}
    except Exception as e:  # noqa: BLE001
        return {"version": "packaged", "hash": None, "source": f"unavailable ({type(e).__name__})", "versions": 0}


def health_report(league: str | None = None) -> dict[str, Any]:
    """The harness bar (``harness.metrics.status_report``) from ``<FM_DATA_DIR>/harness.db`` plus
    the M4 surfaces (``harness.health.full_report``: chart series, scorecard, data capture,
    params changelog, refit calendar). Without a ledger (it is never created from here) the
    leagues are empty and only the params / calendar are filled in. ``league``: also build that
    league's report when the ledger does not know it yet (the page always shows one league)."""
    from ..harness.health import calendar, changelog, extend_league, full_report
    from ..harness.ledger import DB_NAME, Ledger
    from ..harness.metrics import empty_report, league_report
    from ..harness.params_store import ParamsStore

    data_dir = _data_dir()
    if not (data_dir / DB_NAME).exists():
        rep = empty_report()
        rep["params"] = _params_without_ledger(data_dir)
        rep["changelog"] = changelog(ParamsStore(data_dir=data_dir))
        rep["calendar"] = calendar(data_dir=data_dir)
        return rep
    with Ledger(data_dir) as led:
        rep = full_report(led, data_dir=data_dir)
        if league and league not in rep["leagues"]:
            rep["leagues"][league] = extend_league(led, league, league_report(led, league))
        return rep


def refit_dry_run(force: bool = False) -> dict[str, Any]:
    """``fm harness refit --dry-run`` for the /health page (writes nothing; ``force`` bypasses the
    calendar lock only). ``{"error": ...}`` when there is no ledger yet."""
    from datetime import date

    from ..harness.ledger import DB_NAME, Ledger
    from ..harness.refit import run_refit

    data_dir = _data_dir()
    if not (data_dir / DB_NAME).exists():
        return {"error": "no harness ledger yet: run `fm harness daily` first"}
    with Ledger(data_dir) as led:
        return run_refit(led, date.today(), mode="dry-run", force=force).to_dict()


def params_rollback(version: str | None) -> dict[str, Any]:
    """Roll the active params version back (``fm harness rollback --to``); raises ValueError."""
    from datetime import date

    from ..harness.ledger import DB_NAME, Ledger
    from ..harness.params_store import ParamsStore

    data_dir = _data_dir()
    if not (data_dir / DB_NAME).exists():
        return ParamsStore(data_dir=data_dir).rollback(to=version, by="dashboard")
    with Ledger(data_dir) as led:
        out = ParamsStore(ledger=led).rollback(to=version, by="dashboard")
        try:
            from ..harness.ingest import record_run

            record_run(led, "rollback", None, date.today(), {**out, "via": "dashboard"})
        except Exception:  # noqa: BLE001 - the rollback itself already happened
            log.exception("recording the rollback run failed")
        return out


VERSION_RE = re.compile(r"^(v\d{4,}|packaged)$")
ROLLBACK_MSG = re.compile(r"^rollback-ok-(v\d{4,}|packaged)-(v\d{4,}|packaged)$")
ROLLBACK_FLASH = {
    "rollback-unconfirmed": "Nothing changed: tick the confirmation box to roll the parameters back.",
    "rollback-none": "Nothing to roll back: the packaged parameters are active.",
    "rollback-unknown": "Rollback failed: unknown or already active version. Nothing changed.",
}


def _rollback_flash(msg: str | None) -> tuple[str | None, bool]:
    """(text, is_warning) for ``?msg=rollback-...`` (set by POST /params/rollback)."""
    if not msg or not msg.startswith("rollback-"):
        return None, False
    m = ROLLBACK_MSG.match(msg)
    if m:
        return (f"Rolled back the valuation parameters from {m.group(1)} to {m.group(2)}. "
                "Every page recalculates with them on its next load."), False
    text = ROLLBACK_FLASH.get(msg)
    return (text, True) if text else (None, False)


def default_llm() -> Any:
    """The OpenRouter client used by POST /explain (``available`` is False without a key)."""
    from ..config import get_settings
    from ..llm.openrouter import LLMClient

    return LLMClient(get_settings())


EXPLAIN_FAILED = ("Explanation unavailable right now (free model rate-limited); "
                  "numbers above are the full basis.")


def create_app(loader: Loader | None = None, *, cache_ttl: float = CACHE_TTL,
               clock: Callable[[], float] = time.monotonic, default_league: str = "espn",
               llm: Callable[[], Any] | None = None, auth: web_auth.AuthConfig | None = None) -> FastAPI:
    """Build the dashboard app. ``loader(league) -> LoadResult`` defaults to the live pipeline;
    ``llm()`` returns the client for on-demand explanations (defaults to ``default_llm``);
    ``auth`` is the access policy (``auth.auth_from_settings()`` for the real server; None = no
    checks, as in the tests)."""
    from ..prefs import DYNASTY_MODE_KEY, DYNASTY_MODES, mode_source_label, normalize_mode, set_pref

    app = FastAPI(title="Fantasy Manager", docs_url=None, redoc_url=None)
    loader = loader or PipelineLoader(cache_ttl, clock)
    cache = ResultCache(loader, cache_ttl, clock)
    app.state.cache = cache
    app.state.loader = loader
    templates = Jinja2Templates(directory=str(HERE / "templates"))
    env = templates.env
    env.filters.update(num=fmt_num, pos=pos_str, ago=ago, reason_label=reason_label, pct=score_pct,
                       kind_group=kind_group, safe_url=safe_url, q=lambda s: quote(str(s), safe=":"),
                       action=action_line, action_note=action_note, n1=fmt1, rate=fmt_rate,
                       last=last_name)
    env.globals.update(STATUS_LABEL=STATUS_LABEL, STATUS_CLASS=STATUS_CLASS, KIND_LABEL=KIND_LABEL, MOVE_LABEL=MOVE_LABEL,
                       KIND_FILTERS=KIND_FILTERS, LEAGUES=LEAGUES, HIDDEN_REASONS=HIDDEN_REASONS,
                       REASONS_SHOWN=REASONS_SHOWN, val=val, dyn_value=dyn_value, MODES=DYNASTY_MODES,
                       mode_source_label=mode_source_label, compare=views.compare, gain_info=views.gain_info,
                       strength_info=views.strength_info, counterparty=views.counterparty, rec_key=rec_key,
                       SKATER_RATES=views.SKATER_RATES, GOALIE_RATES=views.GOALIE_RATES,
                       RATE_LABEL=views.RATE_LABEL, POSITION_FILTERS=views.POSITION_FILTERS,
                       EXPLAIN_FAILED=EXPLAIN_FAILED, auth_enabled=False, credits=views.credits,
                       limits_text=views.limits_text, exploits=views.exploits)
    env.globals.update(moves_text=_moves_footer)
    env.globals.update(sources_text=_sources_footer)
    if auth is not None:
        web_auth.install(app, auth, templates)
    make_llm = llm or default_llm

    def llm_available() -> bool:
        try:
            return bool(getattr(make_llm(), "available", False))
        except Exception:
            return False
    app.mount("/static", StaticFiles(directory=str(HERE / "static")), name="static")

    def params_source() -> str:
        try:
            from ..valuation.params import source

            return source()
        except Exception:
            return "unavailable"

    def render(request: Request, name: str, league: str, status_code: int = 200, **ctx: Any) -> HTMLResponse:
        path = request.url.path
        switch_path = path if path in ("/", "/roster", "/recommendations", "/news", "/health",
                                         "/schedule", "/matchup") else "/"
        query = [(k, v) for k, v in request.query_params.multi_items() if k != "msg"]
        here = path + (f"?{urlencode(query)}" if query else "")
        cur_mode, cur_source = current_mode()
        base = {"league": league, "path": path, "switch_path": switch_path, "here": here,
                "now": datetime.now(), "res": None, "cache_age": cache.age(league),
                "cur_mode": cur_mode, "cur_mode_source": cur_source, "params_source": params_source()}
        base.update(ctx)
        # points leagues show fantasy points per game; categories/roto show a per-game z-score sum
        _res = base.get("res")
        _kind = getattr(getattr(getattr(_res, "ctx", None), "scoring", None), "kind", "points") if _res is not None else "points"
        base.setdefault("vlabel", "FPG" if _kind == "points" else "Val/G")
        base.setdefault("vlabel_title", "fantasy points per game" if _kind == "points" else "per-game z-score value summed over your categories (fitted on rostered players), not points")
        res = base.get("res")
        base["show_mode"] = (league == "fantrax" or bool(res is not None and res.ctx.dynasty)) \
            and not ctx.get("no_mode")
        base["data_mode"] = (res.mode if res is not None and res.mode else cur_mode)
        base["data_mode_source"] = (res.mode_source if res is not None and res.mode else cur_source)
        base["flash"] = _mode_flash(request.query_params.get("msg"), res)
        if base["flash"] is None:
            base["flash"], base["flash_warn"] = _rollback_flash(request.query_params.get("msg"))
        if "llm_ok" not in base:
            base["llm_ok"] = llm_available() if name in ("overview.html", "recommendations.html",
                                                         "player.html") else False
        return templates.TemplateResponse(request, name, base, status_code=status_code)

    def get_result(league: str) -> LoadResult:
        """Cached result; recomputed when the effective mode changed since it was computed
        (e.g. `fm mode rebuild` in a terminal while the dashboard is running)."""
        res = cache.get(league)
        if res.mode is not None and res.mode != current_mode()[0]:
            cache.clear(league)
            res = cache.get(league)
        return res

    def load_or_error(request: Request, league: str) -> LoadResult | HTMLResponse:
        try:
            return get_result(league)
        except ProviderError as e:
            return render(request, "setup.html", league, error=str(e), rows=_setup_rows(league),
                          title="Setup needed")
        except Exception as e:
            log.exception("loading %s failed", league)
            return render(request, "error.html", league, status_code=500, error=f"{type(e).__name__}: {e}",
                          title="Something went wrong")

    def json_load(league: str) -> LoadResult | JSONResponse:
        try:
            return get_result(league)
        except ProviderError as e:
            return JSONResponse({"error": str(e), "setup": _setup_rows(league)}, status_code=503)
        except Exception as e:
            log.exception("loading %s failed", league)
            return JSONResponse({"error": f"{type(e).__name__}: {e}"}, status_code=500)

    if default_league not in LEAGUES:
        raise ValueError(f"unknown league {default_league!r}; expected one of {', '.join(LEAGUES)}")
    LeagueQ = Query(default_league, pattern=LEAGUE_PATTERN, description="espn or fantrax")

    @app.get("/healthz")
    def healthz() -> dict[str, Any]:
        return {"status": "ok", "cached": {lg: cache.age(lg) is not None for lg in LEAGUES}}

    @app.get("/", response_class=HTMLResponse)
    def overview(request: Request, league: str = LeagueQ, kind: str = "all"):
        res = load_or_error(request, league)
        if isinstance(res, Response):
            return res
        ctx = res.ctx
        team = _my_team(ctx)
        kind = normalize_kind(kind)
        shown = _filter_recs(res.recs, kind)
        alerts = []
        prospects = 0
        news: list[tuple[Player, Any]] = []
        if team is not None:
            for s in team.slots:
                p = s.player
                if p is None:
                    continue
                if p.status in ALERT_STATUSES:
                    alerts.append((s.slot, p))
                if ctx.dynasty and is_prospect(p, player_age(p, ctx.as_of, res.dynasty)):
                    prospects += 1
                news.extend((p, n) for n in res.news_by_cid.get(p.cid, []))
        epoch = datetime.min
        news.sort(key=lambda t: t[1].published.replace(tzinfo=None) if t[1].published else epoch, reverse=True)
        return render(request, "overview.html", league, res=res, team=team, kind=kind,
                      moves=shown[:OVERVIEW_MOVES], shown_total=len(shown), counts=kind_counts(res.recs),
                      summary=overview_summary(res.recs, alerts, prospects), alerts=alerts,
                      news=news[:4], total=len(res.recs), week=views.week_panel(res, team),
                      standings=views.standings(res), free_agents=views.free_agents(res),
                      league_injuries=views.league_injuries(res),
                      matchup=views.matchup_strip(res, views.provider_for(loader, league)), title="Overview")

    @app.get("/roster", response_class=HTMLResponse)
    def roster(request: Request, league: str = LeagueQ, team: str | None = None, view: str = "fantasy"):
        res = load_or_error(request, league)
        if isinstance(res, Response):
            return res
        ctx = res.ctx
        view = view if view in ("fantasy", "stats") else "fantasy"
        chosen = next((t for t in ctx.teams if t.team_id == team), None) if team else None
        chosen = chosen or _my_team(ctx)
        slots = sorted((s for s in (chosen.slots if chosen else []) if s.player),
                       key=lambda s: (SLOT_ORDER.get(s.slot, 99), s.player.name))
        has_proj = any(val(res.values, s.player.cid, "proj_week") is not None for s in slots)
        vorps = [v for s in slots if isinstance(v := val(res.values, s.player.cid, "vorp"), (int, float))]
        scale = max((abs(v) for v in vorps), default=0.0)
        week = views.week_panel(res, chosen) if chosen else None
        changes = {r["cid"]: r for r in (week["rows"] if week else [])}
        rows = []
        for s in slots:
            row = views.player_row(res, s.player, slot=s.slot)
            ch = changes.get(s.player.cid, {})
            row.update(starting=s.starting, bar=vorp_bar(row["vorp"], scale), change=ch.get("change"),
                       change_gain=ch.get("change_gain"))
            rows.append(row)
        groups = [(label, [r for r in rows if test(r)]) for label, test in (
            ("Starting", lambda r: r["starting"] and r["slot"] != "IR"),
            ("Bench", lambda r: not r["starting"] and r["slot"] != "IR"),
            ("Injured reserve", lambda r: r["slot"] == "IR"))]
        return render(request, "roster.html", league, res=res, team=chosen, slots=slots, view=view,
                      groups=[g for g in groups if g[1]], any_prospect=any(r["prospect"] for r in rows),
                      skaters=[r for r in rows if not r["goalie"]], goalies=[r for r in rows if r["goalie"]],
                      week=week, has_proj=has_proj, has_dyn=res.dynasty is not None,
                      has_pct=any(r["pct"] is not None for r in rows), title="Roster")

    @app.get("/recommendations", response_class=HTMLResponse)
    def recommendations(request: Request, league: str = LeagueQ, kind: str = "all",
                        min_gain: str | None = None, pos: str | None = None):
        res = load_or_error(request, league)
        if isinstance(res, Response):
            return res
        kind = normalize_kind(kind)
        try:
            floor = float(min_gain) if min_gain not in (None, "") else None
        except ValueError:
            floor = None
        pos = pos if pos in views.POSITION_FILTERS else None
        recs = _filter_recs(res.recs, kind)
        if pos:
            recs = [r for r in recs if views.involves_position(r, pos)]
        if floor is not None:
            recs = [r for r in recs if (g := views.gain_info(r, res)) is not None and g["value"] >= floor]
        extra = urlencode([(k, v) for k, v in (("min_gain", min_gain if floor is not None else None),
                                                 ("pos", pos)) if v])
        return render(request, "recommendations.html", league, res=res, kind=kind, recs=recs,
                      counts=kind_counts(res.recs), total=len(res.recs), min_gain=floor, pos=pos,
                      filter_qs=("&" + extra) if extra else "", filtered=bool(pos or floor is not None),
                      title="Moves")

    @app.get("/news", response_class=HTMLResponse)
    def news(request: Request, league: str = LeagueQ):
        res = load_or_error(request, league)
        if isinstance(res, Response):
            return res
        ctx = res.ctx
        team = _my_team(ctx)
        mine = {p.cid for p in team.players} if team else set()
        by_cid = {p.cid: p for p in ctx.all_players()}
        entries: dict[str, dict[str, Any]] = {}
        for cid, items in res.news_by_cid.items():
            p = by_cid.get(cid)
            if p is None:
                continue
            for n in items:
                ident = n.id or f"{n.source}|{n.headline}|{n.published}"
                e = entries.setdefault(ident, {"item": n, "players": [], "mine": False})
                e["players"].append(p)
                e["mine"] = e["mine"] or cid in mine
        epoch = datetime.min

        def key(e: dict[str, Any]) -> datetime:
            pub = e["item"].published
            return pub.replace(tzinfo=None) if pub else epoch

        ordered = sorted(entries.values(), key=key, reverse=True)
        return render(request, "news.html", league, res=res,
                      mine=[e for e in ordered if e["mine"]][:50],
                      other=[e for e in ordered if not e["mine"]][:50], title="News")

    @app.get("/player/{cid:path}", response_class=HTMLResponse)
    def player(request: Request, cid: str, league: str = LeagueQ):
        res = load_or_error(request, league)
        if isinstance(res, Response):
            return res
        ctx = res.ctx
        p = next((x for x in ctx.all_players() if x.cid == cid), None)
        if p is None:
            return render(request, "error.html", league, status_code=404, res=res, title="Player not found",
                          error=f"No player with id {cid!r} in this league.")
        team, slot = _where(ctx, cid)
        pv = res.values.get(cid)
        dyn = (res.dynasty or {}).get(cid)
        weights = ctx.scoring.weights if ctx.scoring.kind == "points" else None
        cols, rows = views.split_table(p, weights)
        rates = getattr(pv, "rates", None) or {}
        recs = [r for r in _ranked(res.recs)
                if any(x.cid == cid for x in (*r.add, *r.drop, *(getattr(r, "subjects", None) or [])))]
        start, _, preseason = views.week_start(ctx)
        return render(request, "player.html", league, res=res, p=p, pv=pv, dyn=dyn, team=team, slot=slot,
                      cols=cols, rows=rows, rates=rates, recs=recs, row=views.player_row(res, p, slot=slot),
                      schedule=views.team_games(ctx, p.team, start, 14), schedule_start=start,
                      preseason=preseason, history=views.status_history(res, cid),
                      breakdown=views.dynasty_breakdown(dyn), weights=weights,
                      news=list(res.news_by_cid.get(cid, []))[:20], title=p.name)

    @app.get("/schedule", response_class=HTMLResponse)
    def schedule(request: Request, league: str = LeagueQ, week: str | None = None):
        res = load_or_error(request, league)
        if isinstance(res, Response):
            return res
        page = views.schedule_page(res, week, views.provider_for(loader, league))
        return render(request, "schedule.html", league, res=res, title="Schedule", **page)

    @app.get("/matchup", response_class=HTMLResponse)
    def matchup(request: Request, league: str = LeagueQ):
        res = load_or_error(request, league)
        if isinstance(res, Response):
            return res
        try:
            page = views.matchup_page(res, views.provider_for(loader, league))
        except Exception as e:  # the preview is best effort; the rest of the dashboard still works
            log.exception("matchup preview for %s failed", league)
            return render(request, "error.html", league, status_code=500, res=res, title="Matchup unavailable",
                          error=f"{type(e).__name__}: {e}")
        return render(request, "matchup.html", league, res=res, title="Matchup", **page)

    @app.post("/explain")
    async def explain(request: Request):
        """On-demand LLM narrative for one move (form fields: league, rec_key, next). Cached on
        the LoadResult by rec_key; failures leave an inline note, never an error page."""
        fields = dict(request.query_params)
        body = await request.body()
        if body:
            fields.update(parse_qsl(body.decode("utf-8", "replace"), keep_blank_values=True))
        league = fields.get("league") or default_league
        if league not in LEAGUES:
            return JSONResponse({"detail": f"league must be one of {', '.join(LEAGUES)}"}, status_code=422)
        key = (fields.get("rec_key") or "").strip().lower()
        target = safe_next(fields.get("next"), league)
        try:
            res = get_result(league)
        except Exception:
            return RedirectResponse(target, status_code=303)
        rec = next((r for r in res.recs if rec_key(league, r) == key), None)
        if rec is None:
            return RedirectResponse(target, status_code=303)
        try:
            client = make_llm()
        except Exception as e:
            log.warning("explain: no LLM client: %s", e)
            client = None
        if client is None or not getattr(client, "available", False):
            res.explanations[key] = {"error": "Explanations need an OpenRouter key (set OPENROUTER_API_KEY)."}
            return RedirectResponse(f"{target}#rec-{key}", status_code=303)
        from ..llm.openrouter import narrate

        one = rec.model_copy(update={"narrative": None})
        try:
            narrate([one], res.ctx, res.news_by_cid, client)
        except Exception as e:  # narrate never raises, but a stub or a future change might
            log.warning("explain failed: %s", e)
        if one.narrative:
            res.explanations[key] = {"text": one.narrative, "model": getattr(client, "model", None)}
        else:
            res.explanations[key] = {"error": EXPLAIN_FAILED}
        return RedirectResponse(f"{target}#rec-{key}", status_code=303)

    @app.get("/api/recs.json")
    def api_recs(league: str = LeagueQ, kind: str = "all"):
        res = json_load(league)
        if isinstance(res, Response):
            return res
        recs = _filter_recs(res.recs, normalize_kind(kind))
        return {"meta": _meta(res), "recommendations": [r.model_dump(mode="json") for r in recs]}

    @app.get("/api/roster.json")
    def api_roster(league: str = LeagueQ, team: str | None = None):
        res = json_load(league)
        if isinstance(res, Response):
            return res
        ctx = res.ctx
        chosen = (next((t for t in ctx.teams if t.team_id == team), None) if team else None) or _my_team(ctx)
        slots = []
        for s in (chosen.slots if chosen else []):
            if s.player is None:
                continue
            pv = res.values.get(s.player.cid)
            d = (res.dynasty or {}).get(s.player.cid)
            slots.append({
                "slot": s.slot, "starting": s.starting,
                "player": s.player.model_dump(mode="json", exclude={"lines"}),
                "value": pv.model_dump(mode="json", exclude={"player"}) if hasattr(pv, "model_dump") else None,
                "dynasty": d.model_dump(mode="json", exclude={"player"}) if hasattr(d, "model_dump") else None,
            })
        return {"meta": _meta(res), "team": {"team_id": chosen.team_id, "name": chosen.name,
                                             "record": chosen.record} if chosen else None, "slots": slots}

    def clear_settings() -> None:
        try:  # pick up .env edits made while the server was running
            from ..config import get_settings

            get_settings.cache_clear()
        except Exception:
            pass

    @app.post("/refresh")
    def refresh(league: str = LeagueQ, next: str = "/"):
        try:  # pick up a promoted / rolled-back harness params version (fm harness refit / rollback)
            from ..valuation import params as vparams

            vparams.reload()
        except Exception:
            pass
        cache.clear(league)
        invalidate = getattr(loader, "invalidate", None)
        if callable(invalidate):
            invalidate(league)
        clear_settings()
        return RedirectResponse(safe_next(next, league), status_code=303)

    @app.post("/mode")
    async def set_mode(request: Request):
        """Save the dynasty mode (form fields or query params: mode, league, next) and redirect
        back; the next page load recalculates dynasty values and recommendations."""
        fields = dict(request.query_params)
        body = await request.body()
        if body:
            fields.update(parse_qsl(body.decode("utf-8", "replace"), keep_blank_values=True))
        mode = normalize_mode(fields.get("mode"))
        league = fields.get("league") or default_league
        if mode is None:
            return JSONResponse({"detail": f"mode must be one of {', '.join(DYNASTY_MODES)}"}, status_code=422)
        if league not in LEAGUES:
            return JSONResponse({"detail": f"league must be one of {', '.join(LEAGUES)}"}, status_code=422)
        clear_settings()
        try:
            set_pref(DYNASTY_MODE_KEY, mode)
        except OSError as e:
            log.exception("saving the dynasty mode failed")
            return JSONResponse({"detail": f"could not save the mode: {e}"}, status_code=500)
        cache.clear(league)  # the mode-independent base stays cached in the loader: cheap recompute
        log.info("dynasty mode set to %s from the dashboard", mode)
        return RedirectResponse(safe_next(fields.get("next"), league, msg=f"mode-{mode}"), status_code=303)

    @app.get("/api/health.json")
    def api_health():
        try:
            return health_report()
        except Exception as e:
            log.exception("health report failed")
            return JSONResponse({"error": f"{type(e).__name__}: {e}"}, status_code=500)

    @app.get("/health", response_class=HTMLResponse)
    def health(request: Request, league: str = LeagueQ, refit: str | None = None, force: str | None = None):
        from . import health_view

        forced = (force or "").lower() in ("1", "on", "true", "yes")
        try:
            report = health_report(league)
        except Exception as e:
            log.exception("health report failed")
            return render(request, "error.html", league, status_code=500, error=f"{type(e).__name__}: {e}",
                          title="Something went wrong")
        proposal = None
        if refit == "dry-run":
            try:
                proposal = refit_dry_run(force=forced)
            except Exception as e:  # noqa: BLE001 - show the failure inline, keep the page
                log.exception("refit dry run failed")
                proposal = {"error": f"{type(e).__name__}: {e}"}
        L = (report.get("leagues") or {}).get(league)
        return render(request, "health.html", league, report=report, L=L, no_mode=True, title="Model health",
                      charts=health_view.health_charts((L or {}).get("series"), league), hv=health_view,
                      proposal=proposal, proposal_rows=health_view.refit_rows(proposal), forced=forced)

    @app.post("/params/rollback")
    async def rollback_params(request: Request):
        """Roll the params back (form fields: version, confirm, league, next) and redirect back
        (same-site only, like POST /mode). The ``confirm`` checkbox is required."""
        fields = dict(request.query_params)
        body = await request.body()
        if body:
            fields.update(parse_qsl(body.decode("utf-8", "replace"), keep_blank_values=True))
        league = fields.get("league") or default_league
        if league not in LEAGUES:
            return JSONResponse({"detail": f"league must be one of {', '.join(LEAGUES)}"}, status_code=422)
        nxt = fields.get("next") or f"/health?league={league}"
        if (fields.get("confirm") or "").strip().lower() not in ("1", "on", "yes", "true"):
            return RedirectResponse(safe_next(nxt, league, msg="rollback-unconfirmed"), status_code=303)
        version = (fields.get("version") or "").strip() or None
        if version is not None and not VERSION_RE.match(version):
            return RedirectResponse(safe_next(nxt, league, msg="rollback-unknown"), status_code=303)
        try:
            out = params_rollback(version)
        except ValueError as e:
            code = "rollback-none" if "nothing to roll back" in str(e) else "rollback-unknown"
            return RedirectResponse(safe_next(nxt, league, msg=code), status_code=303)
        except OSError as e:
            log.exception("params rollback failed")
            return JSONResponse({"detail": f"could not roll back: {e}"}, status_code=500)
        log.info("params rolled back %s -> %s from the dashboard", out["from"], out["to"])
        try:
            from ..valuation import params as vparams

            vparams.reload()
        except Exception:
            pass
        cache.clear()                                   # every league's values depend on the params
        invalidate = getattr(loader, "invalidate", None)
        if callable(invalidate):
            invalidate()
        return RedirectResponse(safe_next(nxt, league, msg=f"rollback-ok-{out['from']}-{out['to']}"),
                                status_code=303)

    @app.get("/api/mode.json")
    def api_mode():
        mode, source = current_mode()
        computed = {}
        for lg in LEAGUES:
            res = cache.peek(lg)
            computed[lg] = None if res is None else {
                "mode": res.mode or res.ctx.dynasty_mode, "dynasty": res.ctx.dynasty,
                "stale": res.mode is not None and res.mode != mode,
                "compute_seconds": res.compute_seconds, "recalc_only": res.recalc_only}
        return {"mode": mode, "source": source, "source_label": mode_source_label(source),
                "modes": list(DYNASTY_MODES), "computed": computed}

    return app


def run(host: str = "127.0.0.1", port: int = 8765, league: str = "espn") -> None:
    """Serve the dashboard with uvicorn (blocking). ``league`` is the default when a URL has none.

    Raises ``auth.InsecureBindError`` for a non-loopback ``host`` without ``FM_WEB_PASSWORD``
    (unless ``FM_WEB_ALLOW_INSECURE=1``)."""
    import uvicorn

    policy = web_auth.auth_from_settings()
    web_auth.check_bind(host, bool(policy.password), policy.allow_insecure)
    web = create_app(default_league=league, auth=policy)
    print(f"Fantasy Manager dashboard: http://{host}:{port}/?league={league}"
          + ("  (password required)" if policy.password else "") + "  (Ctrl+C to stop)")
    uvicorn.run(web, host=host, port=port, log_level="info", proxy_headers=True,
                forwarded_allow_ips=",".join(sorted(policy.trusted_proxies)))


def _server_policy() -> web_auth.AuthConfig:
    try:
        return web_auth.auth_from_settings()
    except Exception as e:  # noqa: BLE001 - a bad .env must not open the dashboard: fail closed
        log.error("could not read the dashboard settings (%s); serving loopback clients only", type(e).__name__)
        return web_auth.AuthConfig()


# Module-level app for `uvicorn fantasy_manager.web.app:app` (the systemd service), with the
# access policy from .env / the environment. Building it never touches the network or writes files.
app = create_app(auth=_server_policy())

__all__ = ["BaseLoad", "LoadResult", "PipelineLoader", "ResultCache", "action_line", "app", "create_app",
           "current_mode", "default_loader", "finish", "load_base", "overview_summary", "run", "safe_next"]
