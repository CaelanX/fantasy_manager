import pytest

from fantasy_manager.cache import CacheMiss, CachedResponse, HttpCache, cache_key, install_espn_cache


def test_key_ignores_cookies_and_irrelevant_headers():
    a = cache_key("GET", "https://x/y", {"view": ["a", "b"]}, {"User-Agent": "1"})
    b = cache_key("GET", "https://x/y", {"view": ["a", "b"]}, {"User-Agent": "2"})
    assert a == b
    c = cache_key("GET", "https://x/y", {"view": ["a", "b"]}, {"X-Fantasy-Filter": "{}"})
    assert a != c
    assert cache_key("GET", "u", {"b": 1, "a": 2}) == cache_key("GET", "u", {"a": 2, "b": 1})


def test_offline_miss_and_hit(tmp_path):
    cache = HttpCache(tmp_path, offline=True)
    with pytest.raises(CacheMiss):
        cache.get_json("https://example.invalid/data")
    key = cache_key("GET", "https://example.invalid/data")
    cache._store(key, "https://example.invalid/data",
                 CachedResponse(200, '{"ok": true}', "https://example.invalid/data"))
    assert cache.get_json("https://example.invalid/data", ttl=0) == {"ok": True}  # offline ignores TTL
    cache.close()


def test_install_espn_cache_idempotent(tmp_path):
    from espn_api.requests import espn_requests

    original = espn_requests.requests
    try:
        c1, c2 = HttpCache(tmp_path / "a", offline=True), HttpCache(tmp_path / "b", offline=True)
        install_espn_cache(c1, ttl=60)
        shim = espn_requests.requests
        install_espn_cache(c2, ttl=30)
        assert espn_requests.requests is shim and shim.cache is c2 and shim.ttl == 30
        assert shim.codes is original.codes  # other attributes delegate to the real module
        with pytest.raises(CacheMiss):
            shim.get("https://example.invalid/espn", params={"view": "mTeam"}, cookies={"SWID": "x"})
        c1.close()
        c2.close()
    finally:
        espn_requests.requests = original
