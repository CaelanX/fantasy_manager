"""Lineup freshness: short TTL for lineup requests, and refresh clears every cache holding league state."""
import json
import time
from types import SimpleNamespace

from fantasy_manager.cache import CachedResponse, HttpCache
from fantasy_manager.providers import clear_league_cache
from fantasy_manager.providers import fantrax as fx


class FakeSession:
    def __init__(self):
        self.calls = 0

    def post(self, url, params=None, json=None, timeout=None):
        self.calls += 1
        n = self.calls
        return SimpleNamespace(status_code=200,
                               json=lambda: {"responses": [{"data": {"n": n}} for _ in json["msgs"]]})


def _age(cache_dir, seconds):
    for f in cache_dir.glob("*.json"):
        entry = json.loads(f.read_text(encoding="utf-8"))
        entry["fetched_at"] = time.time() - seconds
        f.write_text(json.dumps(entry), encoding="utf-8")


def test_lineup_requests_expire_after_two_minutes(tmp_path):
    sess = FakeSession()
    c = fx.FxpaClient("lg", sess, tmp_path / fx.CACHE_DIR_NAME)
    roster = ("getTeamRosterInfo", {"view": "STATS"})
    info = ("getFantasyLeagueInfo", {})
    c.call(roster)
    c.call(info)
    assert sess.calls == 2
    _age(tmp_path / fx.CACHE_DIR_NAME, fx.LINEUP_TTL + 10)      # 2+ minutes old, under 15
    assert c.call(info)[0]["n"] == 2 and sess.calls == 2        # league info: still cached
    assert c.call(roster)[0]["n"] == 3 and sess.calls == 3      # lineup: fetched again


def test_clear_league_cache_fantrax_drops_file_cache(tmp_path):
    sess = FakeSession()
    c = fx.FxpaClient("lg", sess, tmp_path / fx.CACHE_DIR_NAME)
    c.call(("getTeamRosterInfo", {"view": "STATS"}))
    s = SimpleNamespace(fm_data_dir=str(tmp_path))
    assert clear_league_cache("fantrax", s, None) == 1
    c.call(("getTeamRosterInfo", {"view": "STATS"}))
    assert sess.calls == 2                                      # read live after the clear


def test_clear_league_cache_espn_keeps_nhl_rows(tmp_path):
    cache = HttpCache(tmp_path)
    try:
        for url in ("https://lm-api-reads.fantasy.espn.com/apis/v3/games/fhl/seasons/2027/segments/0/leagues/1",
                    "https://api-web.nhle.com/v1/roster/EDM/current"):
            cache._store(url, url, CachedResponse(url=url, status_code=200, text="{}"))
        assert clear_league_cache("espn", SimpleNamespace(fm_data_dir=str(tmp_path)), cache) == 1
        assert cache._db.execute("SELECT url FROM http_cache").fetchall() == [
            ("https://api-web.nhle.com/v1/roster/EDM/current",)]
    finally:
        cache.close()


def test_fantrax_requests_send_the_local_zone_not_utc(monkeypatch, tmp_path):
    # Fantrax answers "today" in the sent zone: at 22:00 in Edmonton it is already tomorrow in UTC
    monkeypatch.setenv("FM_TZ", "America/Edmonton")
    seen = []

    class Sess(FakeSession):
        def post(self, url, params=None, json=None, timeout=None):
            seen.append(json["tz"])
            return super().post(url, params=params, json=json, timeout=timeout)

    fx.FxpaClient("lg", Sess(), tmp_path / fx.CACHE_DIR_NAME).call(("getTeamRosterInfo", {"view": "STATS"}))
    assert seen == ["America/Edmonton"]


def test_local_tz_name_falls_back_to_the_current_offset(monkeypatch):
    from datetime import datetime, timedelta, timezone

    monkeypatch.delenv("FM_TZ", raising=False)
    monkeypatch.delenv("TZ", raising=False)
    monkeypatch.setattr(fx.Path, "read_text", lambda self, encoding=None: (_ for _ in ()).throw(OSError()))
    monkeypatch.setattr(fx.Path, "resolve", lambda self, strict=False: self)
    late = datetime(2026, 10, 26, 22, 0, tzinfo=timezone(timedelta(hours=-6)))   # 04:00 Oct 27 in UTC
    name = fx.local_tz_name(late)
    assert name == "Etc/GMT+6"                    # POSIX sign: 6 hours behind UTC
    assert (late.astimezone(timezone.utc) - timedelta(hours=6)).date() == late.date()   # still Oct 26 there
    assert fx.local_tz_name(datetime(2026, 1, 1, tzinfo=timezone(timedelta(hours=5, minutes=30)))) == "UTC"


def test_web_refresh_clears_league_cache(monkeypatch, tmp_path):
    from fastapi.testclient import TestClient

    from fantasy_manager import providers
    from fantasy_manager.config import get_settings
    from fantasy_manager.web.app import create_app

    monkeypatch.setenv("FM_DATA_DIR", str(tmp_path))
    monkeypatch.delenv("FM_OFFLINE", raising=False)
    get_settings.cache_clear()
    seen = []
    monkeypatch.setattr(providers, "clear_league_cache", lambda name, s, cache: seen.append(name) or 0)
    app = create_app(lambda league: None)
    r = TestClient(app).post("/refresh?league=fantrax&next=/", follow_redirects=False)
    assert r.status_code == 303 and seen == ["fantrax"]
    get_settings.cache_clear()
