"""Data health: SourceStatus population (providers.enrich), classification (report.health) and
the web footer."""
from datetime import datetime, timezone

import pytest

from fantasy_manager.harness.ledger import Ledger
from fantasy_manager.models import SourceStatus
from fantasy_manager.providers import enrich
from fantasy_manager.providers.enrich import (SourceTracker, classify_provider_warning, enrich_context,
                                              provider_status)
from fantasy_manager.report.health import data_health, footer_summary, is_stale, should_alert

from .test_digest import make_league
from .test_integration_sources import DeadNhl, FakeDfo, FakeMp, S, enrich_ctx

HOUR = 3600.0
NOW = 1_000_000.0


# --------------------------------------------------------------------------- population

class FakeCache:
    """HttpCache stand-in: URLs containing "bad" raise; every URL has a cached row fetched at
    ``fetched`` (the cache keeps expired rows, so a failure still has a last good copy)."""

    def __init__(self, fetched):
        self.fetched = fetched

    def get_json(self, url, params=None, ttl=0):
        if "bad" in url:
            raise RuntimeError("HTTP 503 for " + url)
        return {"ok": True}

    get_text = get_json

    def _lookup(self, key):
        return ("u", 200, "{}", self.fetched)


def test_tracker_statuses_ok_partial_and_failed():
    t = SourceTracker(FakeCache(NOW - 2 * HOUR))
    t.fetch_json("https://api-web.nhle.com/v1/roster/EDM/20262027")
    with pytest.raises(RuntimeError):
        t.fetch_json("https://api-web.nhle.com/v1/roster/bad/20262027")
    with pytest.raises(RuntimeError):
        t.fetch_json(enrich.INJURIES_URL + "?bad=1")
    t.fetch_json("https://api-web.nhle.com/v1/player/8478402/landing")
    by = {s.name: s for s in t.statuses(now=NOW)}
    assert set(by) == {"NHL rosters", "Injuries", "NHL player pages"}
    ros = by["NHL rosters"]
    assert (ros.severity, ros.ok, ros.requests, ros.ttl_seconds) == ("warn", True, 2, enrich.TTL_ROSTER)
    assert ros.detail.startswith("1 of 2 requests failed (RuntimeError: HTTP 503")
    assert ros.age_seconds == pytest.approx(2 * HOUR) and ros.feeds_valuation
    assert ros.fetched_at == datetime.fromtimestamp(NOW - 2 * HOUR, tz=timezone.utc)
    inj = by["Injuries"]
    assert (inj.severity, inj.ok, inj.ttl_seconds) == ("fail", False, enrich.TTL_INJURIES)
    assert inj.detail.startswith("unavailable (RuntimeError") and inj.age_seconds == pytest.approx(2 * HOUR)
    pages = by["NHL player pages"]
    assert pages.severity == "ok" and pages.detail is None and not pages.feeds_valuation
    # the free-text notes are unchanged (successful requests only)
    assert "NHL rosters: 1 requests, data 2h ago" in t.notes(now=NOW)


def test_enrich_populates_sources_and_keeps_free_text(tmp_path):
    ctx = enrich_ctx()
    ctx.warnings.append("Fantrax session refreshed by login")     # not this provider: ignored
    led = Ledger(tmp_path / "led")
    try:
        enrich_context(ctx, S(tmp_path), None, nhl=DeadNhl(), injuries_fetch=DeadNhl().x, teams=["EDM"],
                       ledger=led, lines_client=FakeDfo(), xg_client=FakeMp())
    finally:
        led.close()
    names = [s.name for s in ctx.sources]
    assert names[0] == "Test"                                        # the league provider first
    by = {s.name: s for s in ctx.sources}
    assert by["Test"].severity == "ok"
    for name in ("NHL schedules", "NHL stats", "NHL stats (past seasons)", "Injuries", "NHL rosters"):
        assert by[name].severity == "fail" and not by[name].ok, name
    assert by["NHL rosters"].detail.startswith("1 of 1 teams unavailable")
    assert by["Daily Faceoff lines"].severity == "ok" and by["Daily Faceoff lines"].detail == "1 teams"
    assert by["Daily Faceoff goalies"].detail == "1 starters named for 2026-10-20"
    assert by["MoneyPuck"].severity == "ok" and by["MoneyPuck"].ttl_seconds == 12 * HOUR
    dep = by["Deployment ledger"]
    assert dep.severity == "warn" and dep.stale is False and "no games pulled yet" in dep.detail
    # backwards compatible: the free-text warnings / notes are still there
    assert any(w.startswith("NHL team schedules unavailable") for w in ctx.warnings)
    assert any("Daily Faceoff: lines for 1 teams" in n for n in ctx.source_notes)
    h = data_health(ctx)
    assert h.overall == "failed" and should_alert(h)
    assert "Injuries: unavailable (RuntimeError: offline)" in h.lines


def test_enrich_lines_and_xg_failures_become_statuses(tmp_path):
    class DeadDfo(FakeDfo):
        def all_lines(self, teams=None):
            raise RuntimeError("site changed")

        def starting_goalies(self, day):
            raise RuntimeError("site changed")

    class HalfMp(FakeMp):
        def skaters(self, year, situation="all", with_pp=False):
            if year != 2025:
                raise RuntimeError("HTTP 404")
            return super().skaters(year, situation, with_pp)

    ctx = enrich_ctx()
    enrich_context(ctx, S(tmp_path), None, nhl=DeadNhl(), injuries_fetch=DeadNhl().x, teams=["EDM"],
                   deployment=False, lines_client=DeadDfo(), xg_client=HalfMp())
    by = {s.name: s for s in ctx.sources}
    assert by["Daily Faceoff lines"].severity == "fail"
    assert by["Daily Faceoff lines"].detail == "unavailable (site changed)"
    assert by["Daily Faceoff goalies"].severity == "fail"
    mp = by["MoneyPuck"]
    assert mp.severity == "warn" and mp.detail.startswith("2026-27 file unavailable (") and "using 2025-26" in mp.detail
    assert "Deployment ledger" not in by


@pytest.mark.parametrize("provider,warning,want", [
    ("fantrax", "Standings unavailable: Fantrax says you are not logged in: the saved session or FANTRAX_COOKIE is "
                "missing", "fail"),
    ("fantrax", "Fantrax session refreshed by login", "ok"),
    ("fantrax", "Some Fantrax free-agent queries failed: HTTP 500", "fail"),
    ("fantrax", "Standings could not be parsed: KeyError", "warn"),
    ("fantrax", "Fantrax activity unavailable: timeout", "warn"),
    ("fantrax", "FANTRAX_POINTS is not set: using Fantrax's own FP/G.", None),
    ("espn", "ESPN trending players unavailable: HTTPError: 500", "warn"),
    ("espn", "ESPN scoring period unavailable: 401 Unauthorized", "fail"),
    ("espn", "Fantrax activity unavailable: x", None),               # another provider's text
])
def test_provider_warning_classification(provider, warning, want):
    got = classify_provider_warning(provider, warning)
    assert (got[0] if got else None) == want


def test_provider_login_failure_status():
    st = provider_status("fantrax", ["Fantrax session refreshed by login",
                                     "Standings unavailable: Fantrax says you are not logged in: ..."])
    assert (st.name, st.severity, st.ok, st.feeds_valuation) == ("Fantrax", "fail", False, True)
    assert st.detail.startswith("login expired — refresh FANTRAX_COOKIE")
    assert provider_status("espn", []).detail == "league data loaded"


# --------------------------------------------------------------------------- classification

def src(name, severity="ok", age=None, ttl=None, detail=None, valuation=False, stale=None):
    return SourceStatus(name=name, ok=severity != "fail", severity=severity, age_seconds=age, ttl_seconds=ttl,
                        detail=detail, feeds_valuation=valuation, stale=stale)


def with_sources(*sources):
    ctx, _ = make_league()
    ctx.sources = list(sources)
    return ctx


def test_health_all_fresh():
    ctx = with_sources(src("ESPN", detail="league data loaded", valuation=True),
                       src("NHL rosters", age=19 * HOUR, ttl=30 * 24 * HOUR, valuation=True),
                       src("Daily Faceoff goalies", age=14 * 60, ttl=3 * HOUR, valuation=True))
    h = data_health(ctx)
    assert (h.overall, h.ok_count, h.failed, h.stale, h.headline) == ("ok", 3, [], [], None)
    assert h.footer == "All 3 sources fresh (oldest: NHL rosters, 19h)"
    assert "Daily Faceoff goalies: fetched 14 min ago" in h.lines and "ESPN: league data loaded" in h.lines
    assert not should_alert(h)


def test_health_stale_rules():
    inj = src("Injuries", age=5 * HOUR, ttl=HOUR, valuation=True)                   # > 3x TTL
    news = src("News feeds", age=1.4 * HOUR, ttl=0.5 * HOUR)                       # stale, not valuation
    ledger = src("Deployment ledger", age=40 * HOUR, stale=False, valuation=True)   # producer says fine
    odds = src("Odds archive", age=40 * HOUR)                                      # no TTL: 36h rule
    fresh = src("NHL stats", age=2 * HOUR, ttl=6 * HOUR, valuation=True)
    assert [is_stale(s) for s in (inj, news, ledger, odds, fresh)] == [True, False, False, True, False]
    h = data_health(with_sources(fresh, news, odds))
    assert h.overall == "degraded" and [s.name for s in h.stale] == ["Odds archive"] and not should_alert(h)
    assert h.headline == "⚠ Data problems: Odds archive stale (40h)"
    assert h.footer == "2 other sources fresh (oldest: NHL stats, 2h)"
    h = data_health(with_sources(fresh, inj))
    assert should_alert(h) and h.lines[0] == "Injuries: stale — fetched 5h ago, normally refreshed every 60 min"
    behind = src("Deployment ledger", age=3 * 24 * HOUR, stale=True, valuation=True,
                 detail="through 2026-10-02; 3 game day(s) not pulled since (run `fm harness daily`)")
    h = data_health(with_sources(behind))
    assert should_alert(h) and h.lines[0].startswith("Deployment ledger: behind — through 2026-10-02")


def test_health_failed_and_partial():
    ctx = with_sources(
        src("Fantrax", "fail", detail="login expired — refresh FANTRAX_COOKIE", valuation=True),
        src("MoneyPuck", "fail", age=40 * HOUR, ttl=12 * HOUR,
            detail="unavailable (HttpError: HTTP 503 for https://moneypuck.com/x.csv)", valuation=True),
        src("NHL rosters", "warn", age=HOUR, ttl=30 * 24 * HOUR, detail="1 of 32 requests failed", valuation=True),
        src("NHL stats", age=HOUR, ttl=6 * HOUR, valuation=True))
    h = data_health(ctx)
    assert h.overall == "failed" and should_alert(h) and h.ok_count == 1
    assert [s.name for s in h.failed] == ["Fantrax", "MoneyPuck"] and [s.name for s in h.warned] == ["NHL rosters"]
    assert h.headline == "⚠ Data problems: Fantrax login expired; MoneyPuck unavailable; NHL rosters partial"
    assert h.lines[:3] == ["Fantrax: login expired — refresh FANTRAX_COOKIE",
                           "MoneyPuck: unavailable (HttpError: HTTP 503 for moneypuck.com); last good data yesterday",
                           "NHL rosters: 1 of 32 requests failed"]
    assert h.lines[3] == "NHL stats: fetched 60 min ago"


def test_health_falls_back_to_warnings_without_sources():
    ctx, _ = make_league()
    assert data_health(ctx).overall == "ok" and data_health(ctx).footer is None     # nothing known
    ctx.warnings += ["NHL enrichment failed: boom", "ESPN scoring period unavailable: 401 Unauthorized",
                     "Injury feed unavailable (RuntimeError: offline)", "Dynasty valuation failed: x"]
    h = data_health(ctx)
    assert [s.name for s in h.failed] == ["ESPN", "NHL data", "Injuries"]
    assert h.lines[0] == "ESPN: login expired — refresh ESPN_S2 / ESPN_SWID"


# --------------------------------------------------------------------------- web footer

def test_footer_summary_text():
    assert footer_summary(with_sources()) is None
    ok = footer_summary(with_sources(src("NHL stats", age=HOUR, ttl=6 * HOUR)))
    assert ok == {"level": "ok", "text": "all 1 source fresh (oldest: NHL stats, 60m)"}
    bad = footer_summary(with_sources(src("NHL stats", age=HOUR, ttl=6 * HOUR),
                                      src("Injuries", age=5 * HOUR, ttl=HOUR, valuation=True),
                                      src("Fantrax", "fail", detail="login expired — refresh FANTRAX_COOKIE")))
    assert bad["level"] == "fail"
    assert bad["text"] == ("1 fresh · 1 stale · 1 failed — worst: Fantrax "
                           "(login expired — refresh FANTRAX_COOKIE)")


def test_web_footer_renders_sources():
    pytest.importorskip("fastapi")
    pytest.importorskip("jinja2")
    from fastapi.testclient import TestClient

    from fantasy_manager.web.app import create_app

    from .test_web import make_result

    res = make_result()
    res.ctx.source_notes = ["NHL rosters: 32 requests, data 19h ago"]
    plain = TestClient(create_app(lambda league: res)).get("/").text
    assert "sources-foot" not in plain and "Data sources (1)" in plain       # no sources: free text only
    res.ctx.sources = [src("NHL rosters", age=19 * HOUR, ttl=30 * 24 * HOUR),
                       src("Fantrax", "fail", detail="login expired — refresh FANTRAX_COOKIE")]
    h = TestClient(create_app(lambda league: res)).get("/").text
    assert 'class="sources-foot sources-fail"' in h and "var(--bad)" in h
    assert "Data: 1 fresh · 1 failed — worst: Fantrax (login expired" in h
    assert "<details><summary>Data sources (1)</summary>" in h              # free text kept in details
