import json
from datetime import datetime, timedelta, timezone
from pathlib import Path
from types import SimpleNamespace

import pytest

from fantasy_manager.providers import news
from fantasy_manager.providers.news import (
    ESPN_NEWS_URL, ROTOWIRE_URL, fetch_all_news, fetch_espn_news, fetch_rotowire, parse_rss_date, tag_text,
)

FIX = Path(__file__).parent / "fixtures" / "news"
FEEDS = {
    ROTOWIRE_URL: (FIX / "rotowire.xml").read_text(encoding="utf-8"),
    ESPN_NEWS_URL: (FIX / "espn.xml").read_text(encoding="utf-8"),
}


def fake_fetch_text(url, params=None):
    return FEEDS[url]


def test_rotowire_parsing():
    items = fetch_rotowire(fake_fetch_text)
    assert len(items) == 5
    faber = items[0]
    assert faber.source == "rotowire"
    assert faber.player_name == "Brock Faber"
    assert faber.headline == "Begins season on IR"
    assert faber.blurb.startswith("Faber (upper body) will begin")
    assert "Visit RotoWire.com" not in faber.blurb
    assert faber.url == "https://www.rotowire.com/hockey/player/brock-faber-6317"
    # "Mon, 28 Sep 2026 2:38:00 PM PDT" -> 21:38 UTC
    assert faber.published == datetime(2026, 9, 28, 21, 38, tzinfo=timezone.utc)
    assert {"injury", "ir", "upper-body"} <= set(faber.tags)
    assert faber.id == "nhl594131"


def test_rotowire_trade_tagging():
    items = {i.player_name: i for i in fetch_rotowire(fake_fetch_text)}
    assert "trade" in items["Matthew Knies"].tags
    assert "trade" in items["Elvis Merzlikins"].tags


def test_espn_parsing():
    items = fetch_espn_news(fake_fetch_text)
    assert items
    first = items[0]
    assert first.source == "espn" and first.player_name is None
    assert first.headline.startswith("NHL season preview")
    assert first.url.startswith("https://www.espn.com/")
    assert first.published == datetime(2026, 9, 28, 14, 42, 14, tzinfo=timezone.utc)  # 09:42:14 EST


def test_fetch_all_sorted_and_resilient():
    items = fetch_all_news(fake_fetch_text)
    assert {i.source for i in items} == {"rotowire", "espn"}
    dates = [i.published for i in items if i.published]
    assert dates == sorted(dates, reverse=True)

    def broken_espn(url, params=None):
        if url == ESPN_NEWS_URL:
            raise RuntimeError("boom")
        return FEEDS[url]

    only_roto = fetch_all_news(broken_espn)
    assert only_roto and all(i.source == "rotowire" for i in only_roto)


@pytest.mark.parametrize("raw,expected", [
    ("Mon, 28 Sep 2026 2:38:00 PM PDT", datetime(2026, 9, 28, 21, 38, tzinfo=timezone.utc)),
    ("Mon, 28 Sep 2026 12:05:00 AM EST", datetime(2026, 9, 28, 5, 5, tzinfo=timezone.utc)),
    ("Mon, 28 Sep 2026 12:05:00 PM CST", datetime(2026, 9, 28, 18, 5, tzinfo=timezone.utc)),
    ("Mon, 28 Sep 2026 09:42:14 EST", datetime(2026, 9, 28, 14, 42, 14, tzinfo=timezone.utc)),
    ("Mon, 28 Sep 2026 22:21:13 GMT", datetime(2026, 9, 28, 22, 21, 13, tzinfo=timezone.utc)),
    ("Tue, 29 Sep 2026 01:00:00 -0600", datetime(2026, 9, 29, 7, 0, tzinfo=timezone.utc)),
    ("Mon, 28 Sep 2026 7:15 PM MDT", datetime(2026, 9, 29, 1, 15, tzinfo=timezone.utc)),
    ("garbage", None),
    (None, None),
])
def test_parse_rss_date(raw, expected):
    assert parse_rss_date(raw) == expected


def test_tagging_keywords():
    tags = tag_text("Recalled from AHL", "Smith was recalled and will skate on the top line and first power-play unit.")
    assert {"recall", "top-line", "power-play", "line"} <= set(tags)
    assert "scratch" in tag_text("Healthy scratch Saturday", None)
    assert "waivers" in tag_text("Placed on waivers", None)
    assert "goalie" in tag_text(None, "The goaltender will start Tuesday")
    assert "start" in tag_text(None, "The goaltender will start Tuesday")
    assert "demotion" in tag_text("Sent down to minors", "He was assigned to AHL Bakersfield.")
    assert "signing" in tag_text("Signs three-year deal", None)
    # IR must be a standalone uppercase token, not part of a word
    assert "ir" not in tag_text("Their first win", "their")
    assert tag_text("", None) == []


def _item(i, hours_ago=1, **kw):
    published = datetime.now(timezone.utc) - timedelta(hours=hours_ago) if hours_ago is not None else None
    base = dict(source="rotowire", id=f"n{i}", player_name=f"Player {i}", headline=f"Headline {i}",
                published=published)
    base.update(kw)
    return news.NewsItem(**base)


def test_store_keeps_items_across_fetches_and_dedupes(tmp_path):
    first = news.store_news(tmp_path, [_item(1, hours_ago=1), _item(2, hours_ago=2)])
    assert [i.id for i in first] == ["n1", "n2"]
    # a later fetch no longer carries n1 / n2 (the feed moved on) but they stay in the store;
    # an item seen again (same id, edited blurb) replaces the stored copy instead of duplicating it
    second = news.store_news(tmp_path, [_item(3, hours_ago=0), _item(2, hours_ago=2, blurb="updated")])
    assert [i.id for i in second] == ["n3", "n1", "n2"]
    assert next(i for i in second if i.id == "n2").blurb == "updated"
    # items without a guid dedupe on source + headline + publish time
    same = _item(4, id=None)
    assert len([i for i in news.store_news(tmp_path, [same, same.model_copy()]) if i.id is None]) == 1
    assert {i.id for i in news.stored_news(tmp_path)} == {"n1", "n2", "n3", None}


def test_store_prunes_after_30_days_and_caps(tmp_path, monkeypatch):
    old = _item(1, hours_ago=24 * (news.STORE_MAX_AGE_DAYS + 1))
    kept = news.store_news(tmp_path, [old, _item(2)])
    assert [i.id for i in kept] == ["n2"]
    assert "n1" not in json.loads((tmp_path / news.STORE_NAME).read_text(encoding="utf-8"))
    monkeypatch.setattr(news, "STORE_MAX_ITEMS", 3)
    news.store_news(tmp_path, [_item(i, hours_ago=i) for i in range(10, 20)])
    assert [i.id for i in news.stored_news(tmp_path)] == ["n2", "n10", "n11"]     # newest 3


def test_undated_items_age_out_from_first_seen(tmp_path):
    news.store_news(tmp_path, [_item(1, hours_ago=None)])
    raw = json.loads((tmp_path / news.STORE_NAME).read_text(encoding="utf-8"))
    raw["n1"]["seen"] = (datetime.now(timezone.utc) - timedelta(days=news.STORE_MAX_AGE_DAYS + 2)).isoformat()
    (tmp_path / news.STORE_NAME).write_text(json.dumps(raw), encoding="utf-8")
    assert news.store_news(tmp_path, []) == []


def test_legacy_role_item_file_is_migrated(tmp_path):
    legacy = {"old1": _item("old1", id="old1").model_dump(mode="json")}
    (tmp_path / news.LEGACY_STORE_NAME).write_text(json.dumps(legacy), encoding="utf-8")
    items = news.stored_news(tmp_path)
    assert [i.id for i in items] == ["old1"]
    assert not (tmp_path / news.LEGACY_STORE_NAME).exists()
    assert "old1" in json.loads((tmp_path / news.STORE_NAME).read_text(encoding="utf-8"))


def _fake_feeds(monkeypatch):
    monkeypatch.setattr(news, "fetch_rotowire", lambda fetch=None: [_item(10, hours_ago=0)])
    monkeypatch.setattr(news, "fetch_espn_news", lambda fetch=None: [_item(11, hours_ago=0.5, source="espn",
                                                                             player_name=None)])


def _offline(url, params=None):
    raise RuntimeError("offline")


def test_fetch_news_returns_fresh_plus_stored_and_survives_failures(tmp_path, monkeypatch):
    news.store_news(tmp_path, [_item(1, hours_ago=48)])
    with monkeypatch.context() as m:
        _fake_feeds(m)
        items, failed = news.fetch_news(None, tmp_path)
    assert failed == [] and [i.id for i in items] == ["n10", "n11", "n1"]
    items, failed = news.fetch_news(_offline, tmp_path)
    assert [label for label, _ in failed] == ["RotoWire news", "ESPN news"]
    assert [i.id for i in items] == ["n10", "n11", "n1"]     # stored news still comes back offline
    assert news.fetch_news(_offline, None)[0] == []          # no store: nothing to fall back on


def test_rookie_enrich_and_cli_read_the_store(tmp_path, monkeypatch):
    from fantasy_manager import cli
    from fantasy_manager.providers import rookie_enrich

    news.store_news(tmp_path, [_item(1, headline="Skating on top line")])
    warnings: list[str] = []
    got = rookie_enrich.fetch_news(_offline, warnings, store_dir=tmp_path)
    assert [i.id for i in got] == ["n1"] and len(warnings) == 2
    monkeypatch.setattr(cli, "get_settings", lambda: SimpleNamespace(fm_data_dir=tmp_path))
    monkeypatch.setattr("fantasy_manager.providers.enrich.make_fetch_text", lambda cache: None)
    _fake_feeds(monkeypatch)
    errors: list[str] = []
    items = cli._fetch_news(None, errors)
    assert errors == [] and [i.id for i in items] == ["n10", "n11", "n1"]


def test_title_without_colon():
    xml = """<?xml version="1.0"?><rss version="2.0"><channel><item>
    <title>General update without player</title><description>Something.</description>
    <link>https://example.com/x</link><pubDate>Mon, 28 Sep 2026 2:38:00 PM PDT</pubDate>
    </item></channel></rss>"""
    item = news.parse_rotowire(xml)[0]
    assert item.player_name is None and item.headline == "General update without player"
