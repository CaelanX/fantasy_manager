"""Dashboard Refresh also drops the cached starting-goalie and injury feeds."""
import pytest

pytest.importorskip("fastapi")
pytest.importorskip("httpx")

from fastapi.testclient import TestClient  # noqa: E402

from fantasy_manager.cache import CachedResponse, HttpCache  # noqa: E402
from fantasy_manager.providers import clear_status_cache  # noqa: E402
from fantasy_manager.providers.dailyfaceoff import goalies_url, team_url  # noqa: E402
from fantasy_manager.providers.injuries import INJURIES_URL  # noqa: E402

ESPN_LEAGUE = "https://lm-api-reads.fantasy.espn.com/apis/v3/games/fhl/seasons/2027/segments/0/leagues/1"
NHL = "https://api-web.nhle.com/v1/roster/EDM/current"


def _seed(cache: HttpCache) -> None:
    from datetime import date

    for url in (goalies_url(date(2026, 11, 4)), goalies_url(date(2026, 11, 3)), team_url("EDM"), INJURIES_URL,
                ESPN_LEAGUE, NHL):
        cache._store(url, url, CachedResponse(url=url, status_code=200, text="{}"))


def _urls(cache: HttpCache) -> set[str]:
    return {u for (u,) in cache._db.execute("SELECT url FROM http_cache").fetchall()}


def test_clear_status_cache_drops_only_goalies_and_injuries(tmp_path):
    cache = HttpCache(tmp_path)
    try:
        _seed(cache)
        assert clear_status_cache(cache) == 3
        assert _urls(cache) == {team_url("EDM"), ESPN_LEAGUE, NHL}       # lines, league, NHL stay
    finally:
        cache.close()


def test_web_refresh_clears_goalie_and_injury_cache(monkeypatch, tmp_path):
    from fantasy_manager.config import get_settings
    from fantasy_manager.web.app import create_app

    monkeypatch.setenv("FM_DATA_DIR", str(tmp_path))
    monkeypatch.delenv("FM_OFFLINE", raising=False)
    get_settings.cache_clear()
    try:
        cache = HttpCache(tmp_path)
        _seed(cache)
        cache.close()
        r = TestClient(create_app(lambda league: None)).post("/refresh?league=espn&next=/", follow_redirects=False)
        assert r.status_code == 303
        cache = HttpCache(tmp_path)
        try:
            assert _urls(cache) == {team_url("EDM"), NHL}      # ESPN league + goalies + injuries cleared
        finally:
            cache.close()
    finally:
        get_settings.cache_clear()


def test_web_refresh_offline_keeps_everything(monkeypatch, tmp_path):
    from fantasy_manager.config import get_settings
    from fantasy_manager.web.app import create_app

    monkeypatch.setenv("FM_DATA_DIR", str(tmp_path))
    monkeypatch.setenv("FM_OFFLINE", "1")
    get_settings.cache_clear()
    try:
        cache = HttpCache(tmp_path)
        _seed(cache)
        cache.close()
        TestClient(create_app(lambda league: None)).post("/refresh?league=espn&next=/", follow_redirects=False)
        cache = HttpCache(tmp_path)
        try:
            assert len(_urls(cache)) == 6                          # offline the cache is the only copy
        finally:
            cache.close()
    finally:
        get_settings.cache_clear()
