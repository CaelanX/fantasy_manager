"""Data health: which sources failed or went stale this run, in plain English.

Reads ``LeagueContext.sources`` (``providers.enrich.collect_sources``); a context without them
(enrichment crashed, or a loader that never enriched) falls back to classifying ``ctx.warnings``.

* failed  - severity "fail": the source (or the league login) did not deliver this run
* stale   - older than 3x its cache TTL (36 h for sources without a TTL), or flagged stale by its
  producer (the deployment ledger is judged by missed game days, not by age)
* warn    - partial failures (some teams / one season file missing): shown, but no alert

``overall`` is "failed" with any failure, "degraded" with any stale / partial source, else "ok".
``should_alert`` is true for any failure or a stale source that feeds valuation.
"""
from __future__ import annotations

import re
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any, Literal

from ..models import LeagueContext, SourceStatus

HOUR = 3600.0
TTL_FACTOR = 3.0
NO_TTL_STALE = 36 * HOUR
WARN_SIGN = "⚠"

_URL_RE = re.compile(r"https?://([^/\s)]+)[^\s)]*")
_STEP_WARN_RE = re.compile(r"^(?P<name>.+?) unavailable \((?P<err>.*)\)$")


# --------------------------------------------------------------------------- formatting

def ago(seconds: float | None) -> str:
    """"just now", "14 min ago", "5h ago", "yesterday", "3 days ago"."""
    if seconds is None:
        return "at an unknown time"
    if seconds < 90:
        return "just now"
    if seconds < 90 * 60:
        return f"{seconds / 60:.0f} min ago"
    if seconds < 36 * HOUR:
        return f"{seconds / HOUR:.0f}h ago"
    days = seconds / (24 * HOUR)
    return "yesterday" if days < 2 else f"{days:.0f} days ago"


def short_age(seconds: float | None) -> str:
    """Compact age for one-line footers: "12m", "19h", "3d"."""
    if seconds is None:
        return "?"
    if seconds < 90 * 60:
        return f"{max(seconds, 0) / 60:.0f}m"
    if seconds < 48 * HOUR:
        return f"{seconds / HOUR:.0f}h"
    return f"{seconds / (24 * HOUR):.0f}d"


def _span(seconds: float) -> str:
    if seconds < 2 * HOUR:
        return f"{seconds / 60:.0f} min"
    if seconds < 48 * HOUR:
        return f"{seconds / HOUR:.0f}h"
    return f"{seconds / (24 * HOUR):.0f} days"


def _clean(text: str | None) -> str:
    """Details without full URLs (host only) and collapsed whitespace."""
    return " ".join(_URL_RE.sub(r"\1", text or "").split())


# --------------------------------------------------------------------------- classification

def age_of(s: SourceStatus, now: datetime | None = None) -> float | None:
    """Age in seconds: from ``fetched_at`` when ``now`` is given, else the enrichment-time age."""
    if now is not None and s.fetched_at is not None:
        n = now if now.tzinfo else now.astimezone()
        f = s.fetched_at if s.fetched_at.tzinfo else s.fetched_at.replace(tzinfo=timezone.utc)
        return max(0.0, (n - f).total_seconds())
    return s.age_seconds


def is_stale(s: SourceStatus, now: datetime | None = None) -> bool:
    """Failed sources are never "stale" (they are failed). ``stale`` set by the producer wins."""
    if s.severity == "fail":
        return False
    if s.stale is not None:
        return bool(s.stale)
    age = age_of(s, now)
    if age is None:
        return False
    limit = TTL_FACTOR * s.ttl_seconds if s.ttl_seconds else NO_TTL_STALE
    return age > limit


def _is_login(s: SourceStatus) -> bool:
    return "login expired" in (s.detail or "")


def problem_line(s: SourceStatus, stale: bool, now: datetime | None = None) -> str:
    age = age_of(s, now)
    detail = _clean(s.detail)
    if s.severity == "fail":
        text = f"{s.name}: {detail or 'unavailable'}"
        if age is not None and not _is_login(s):
            text += f"; last good data {ago(age)}"
        return text
    if stale:
        if s.stale:                                  # producer's verdict: its detail says why
            return f"{s.name}: behind — {detail}" if detail else f"{s.name}: stale"
        every = f", normally refreshed every {_span(s.ttl_seconds)}" if s.ttl_seconds else ""
        return f"{s.name}: stale — fetched {ago(age)}{every}"
    return f"{s.name}: {detail or 'partial data'}"


def ok_line(s: SourceStatus, now: datetime | None = None) -> str:
    age = age_of(s, now)
    if age is not None:
        return f"{s.name}: fetched {ago(age)}"
    return f"{s.name}: {_clean(s.detail) or 'ok'}"


def _phrase(s: SourceStatus, stale: bool, now: datetime | None = None) -> str:
    """Two-to-four word problem label for the phone headline."""
    if s.severity == "fail":
        return f"{s.name} login expired" if _is_login(s) else f"{s.name} unavailable"
    if stale:
        age = age_of(s, now)
        return f"{s.name} behind" if s.stale else f"{s.name} stale ({short_age(age)})"
    return f"{s.name} partial"


@dataclass
class DataHealth:
    overall: Literal["ok", "degraded", "failed"]
    failed: list[SourceStatus]
    stale: list[SourceStatus]
    ok_count: int
    lines: list[str]                      # problems first (failed, stale, partial), then fresh sources
    warned: list[SourceStatus] = field(default_factory=list)      # partial failures (not stale)
    sources: list[SourceStatus] = field(default_factory=list)
    headline: str | None = None           # "⚠ Data problems: Fantrax login expired; ..." (None when ok)
    footer: str | None = None             # "All 11 sources fresh (oldest: NHL rosters, 19h)" when ok
    problem_lines: list[str] = field(default_factory=list)

    @property
    def total(self) -> int:
        return len(self.sources)

    @property
    def fresh(self) -> list[SourceStatus]:
        bad = {id(s) for s in (*self.failed, *self.stale, *self.warned)}
        return [s for s in self.sources if id(s) not in bad]


def warning_sources(ctx: LeagueContext) -> list[SourceStatus]:
    """Non-ok SourceStatus rows recoverable from ``ctx.warnings`` alone (the fallback when
    enrichment never listed its sources): provider login / fetch failures, a crashed enrichment
    and failed enrichment steps."""
    from ..providers.enrich import STEP_SOURCES, VALUATION_SOURCES, provider_status, step_source
    from ..providers.nhl import current_season, prior_season

    season = current_season(ctx.as_of)

    out: list[SourceStatus] = []
    prov = provider_status(ctx.provider, ctx.warnings)
    if prov.severity != "ok":
        out.append(prov)
    for w in ctx.warnings:
        w = str(w)
        if w.startswith("NHL enrichment failed"):
            out.append(SourceStatus(name="NHL data", ok=False, severity="fail", feeds_valuation=True,
                                    detail="enrichment failed (" + w.split(":", 1)[-1].strip()[:120] + ")"))
            continue
        m = _STEP_WARN_RE.match(w)
        label = m.group("name") if m else ""
        if m and (label in STEP_SOURCES or step_source(label) != label):
            name = step_source(label, season, prior_season(season))
            out.append(SourceStatus(name=name, ok=False, severity="fail", feeds_valuation=name in VALUATION_SOURCES,
                                    detail=f"unavailable ({m.group('err')[:120]})"))
    return out


def data_health(ctx: LeagueContext, now: datetime | None = None) -> DataHealth:
    """Classify ``ctx.sources`` (or, when enrichment never listed them, the non-ok rows
    recoverable from ``ctx.warnings``) into failed / stale / partial / fresh, with plain-English
    lines. ``now`` recomputes ages from ``fetched_at``; by default the ages measured at enrichment
    are used."""
    sources = list(ctx.sources or [])
    names = {s.name for s in sources}
    # enrichment lists every source it touched; without that list, recover what the warnings say
    extra = [] if sources else warning_sources(ctx)
    for s in extra:
        if s.name not in names:
            sources.append(s)
            names.add(s.name)
    failed = [s for s in sources if s.severity == "fail"]
    stale = [s for s in sources if is_stale(s, now)]
    stale_ids = {id(s) for s in stale}
    warned = [s for s in sources if s.severity == "warn" and id(s) not in stale_ids]
    bad = {id(s) for s in (*failed, *stale, *warned)}
    fresh = [s for s in sources if id(s) not in bad]
    # most important first: valuation inputs before context-only sources
    failed.sort(key=lambda s: not s.feeds_valuation)
    stale.sort(key=lambda s: (not s.feeds_valuation, -(age_of(s, now) or 0.0)))
    problems = [problem_line(s, False, now) for s in failed] + [problem_line(s, True, now) for s in stale] + \
               [problem_line(s, False, now) for s in warned]
    lines = problems + [ok_line(s, now) for s in fresh]
    overall: Literal["ok", "degraded", "failed"] = "failed" if failed else ("degraded" if stale or warned else "ok")
    headline = footer = None
    if overall != "ok":
        phrases = [_phrase(s, False, now) for s in failed] + [_phrase(s, True, now) for s in stale] + \
                  [_phrase(s, False, now) for s in warned]
        headline = f"{WARN_SIGN} Data problems: " + "; ".join(phrases)
    if fresh:
        aged = [s for s in fresh if age_of(s, now) is not None]
        oldest = max(aged, key=lambda s: age_of(s, now) or 0.0) if aged else None
        tail = f" (oldest: {oldest.name}, {short_age(age_of(oldest, now))})" if oldest else ""
        noun = "source" if len(fresh) == 1 else "sources"
        footer = (f"All {len(fresh)} {noun} fresh{tail}" if overall == "ok"
                  else f"{len(fresh)} other {noun} fresh{tail}")
    return DataHealth(overall=overall, failed=failed, stale=stale, ok_count=len(fresh), lines=lines,
                      warned=warned, sources=sources, headline=headline, footer=footer, problem_lines=problems)


def should_alert(health: DataHealth) -> bool:
    """Alert-worthy: any failed source, or a stale source that feeds valuation."""
    return bool(health.failed) or any(s.feeds_valuation for s in health.stale)


def footer_summary(ctx: Any) -> dict[str, str] | None:
    """Compact web-footer view of ``ctx.sources``: {"level": ok|warn|fail, "text": ...}, e.g.
    "12 fresh · 1 stale · 1 failed — worst: Fantrax (login expired ...)". None without sources."""
    if ctx is None or not getattr(ctx, "sources", None):
        return None
    h = data_health(ctx)
    if not h.sources:
        return None
    if h.overall == "ok":
        text = h.footer or f"All {h.total} sources fresh"
        return {"level": "ok", "text": text[0].lower() + text[1:]}
    counts = [f"{h.ok_count} fresh"]
    if h.stale:
        counts.append(f"{len(h.stale)} stale")
    if h.warned:
        counts.append(f"{len(h.warned)} partial")
    if h.failed:
        counts.append(f"{len(h.failed)} failed")
    worst = (h.failed or h.stale or h.warned)[0]
    stale = any(s is worst for s in h.stale)
    worst_line = problem_line(worst, stale).split(": ", 1)
    why = worst_line[1] if len(worst_line) > 1 else ""
    level = "fail" if h.failed or should_alert(h) else "warn"
    return {"level": level, "text": " · ".join(counts) + f" — worst: {worst.name}" + (f" ({why})" if why else "")}


__all__ = ["DataHealth", "age_of", "ago", "data_health", "footer_summary", "is_stale", "should_alert",
           "short_age", "warning_sources"]
