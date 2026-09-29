from datetime import datetime, timezone
from pathlib import Path

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


def test_title_without_colon():
    xml = """<?xml version="1.0"?><rss version="2.0"><channel><item>
    <title>General update without player</title><description>Something.</description>
    <link>https://example.com/x</link><pubDate>Mon, 28 Sep 2026 2:38:00 PM PDT</pubDate>
    </item></channel></rss>"""
    item = news.parse_rotowire(xml)[0]
    assert item.player_name is None and item.headline == "General update without player"
