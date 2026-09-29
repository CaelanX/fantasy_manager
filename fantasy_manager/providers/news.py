"""Player news from RSS feeds (RotoWire player blurbs, ESPN NHL headlines)."""

from __future__ import annotations

import html
import logging
import re
from datetime import datetime, timedelta, timezone
from email.utils import parsedate_to_datetime
from typing import Callable

import feedparser
import httpx
from pydantic import BaseModel, Field

log = logging.getLogger(__name__)

FetchText = Callable[[str, "dict | None"], str]

ROTOWIRE_URL = "https://www.rotowire.com/rss/news.php?sport=NHL"
ESPN_NEWS_URL = "https://www.espn.com/espn/rss/nhl/news"

_TZ_OFFSETS = {
    "UTC": 0, "GMT": 0, "Z": 0,
    "EST": -5, "EDT": -4, "CST": -6, "CDT": -5, "MST": -7, "MDT": -6, "PST": -8, "PDT": -7,
    "AKST": -9, "AKDT": -8, "HST": -10,
}
_MONTHS = {m: i for i, m in enumerate(
    ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"], start=1)}
_DATE_RE = re.compile(
    r"(?:\w{3},\s*)?(\d{1,2})\s+(\w{3})\w*\s+(\d{4})\s+(\d{1,2}):(\d{2})(?::(\d{2}))?\s*([AaPp][Mm])?\s*"
    r"([A-Za-z]{1,5}|[+-]\d{4})?"
)
_ROTOWIRE_TRAILER = re.compile(r"\s*Visit RotoWire\.com for more analysis on this update\.?\s*$", re.I)
_TAG_RE = re.compile(r"<[^>]+>")

# tag -> regex (case-insensitive unless noted)
TAG_PATTERNS: dict[str, re.Pattern[str]] = {
    "injury": re.compile(
        r"injur|upper[- ]body|lower[- ]body|\((?:hip|knee|shoulder|ankle|groin|back|head|concussion|hand|"
        r"wrist|foot|leg|arm|illness|undisclosed|neck|face|elbow|finger|lower body|upper body)\)", re.I),
    "ir": re.compile(r"\bIR\b|injured reserve|\bLTIR\b"),
    "day-to-day": re.compile(r"day[- ]to[- ]day", re.I),
    "upper-body": re.compile(r"upper[- ]body", re.I),
    "lower-body": re.compile(r"lower[- ]body", re.I),
    "out": re.compile(r"\bout\b|\bsidelined\b", re.I),
    "return": re.compile(r"\breturn|\bactivated\b|\bback in (?:the )?lineup", re.I),
    "line": re.compile(r"\bline(?:s|mates?|up)?\b", re.I),
    "top-line": re.compile(r"\btop[- ](?:line|six|pairing)", re.I),
    "power-play": re.compile(r"power[- ]play|\bPP1\b|\bPP2\b", re.I),
    "scratch": re.compile(r"\bscratch", re.I),
    "waivers": re.compile(r"\bwaiv", re.I),
    "trade": re.compile(r"\btrad(?:e|ed|es|ing)\b|\bshipped\b|\bdealt\b", re.I),
    "acquired": re.compile(r"\bacquir", re.I),
    "signing": re.compile(r"\bsign(?:s|ed|ing)?\b|\bcontract\b|\bextension\b", re.I),
    "recall": re.compile(r"\brecall|\bcalled up\b|\bpromoted\b", re.I),
    "demotion": re.compile(r"\bdemot|\bassigned to\b|\bsent down\b|\breassigned\b", re.I),
    "goalie": re.compile(r"\bgoalie|\bgoaltend|\bnetminder|\bcrease\b", re.I),
    "start": re.compile(r"\bstart(?:s|ed|ing)?\b|\bget the nod\b|\bgets the nod\b", re.I),
}


class NewsItem(BaseModel):
    source: str                      # "rotowire" | "espn"
    id: str | None = None            # feed guid (for dedupe)
    player_name: str | None = None
    headline: str
    blurb: str = ""
    url: str | None = None
    published: datetime | None = None  # timezone-aware UTC
    tags: list[str] = Field(default_factory=list)


def default_fetch_text(url: str, params: dict | None = None) -> str:
    resp = httpx.get(url, params=params, follow_redirects=True, timeout=20.0,
                     headers={"User-Agent": "Mozilla/5.0 (fantasy-manager/0.1)"})
    resp.raise_for_status()
    return resp.text


def parse_rss_date(value: str | None) -> datetime | None:
    """Parse RSS pubDate, including RotoWire's 'Mon, 28 Sep 2026 2:38:00 PM PDT'. Returns UTC."""
    if not value:
        return None
    value = value.strip()
    if not re.search(r"\b[AaPp][Mm]\b", value):
        try:
            dt = parsedate_to_datetime(value)
            if dt is not None:
                if dt.tzinfo is None:
                    dt = dt.replace(tzinfo=timezone.utc)
                return dt.astimezone(timezone.utc)
        except (TypeError, ValueError, IndexError):
            pass
    m = _DATE_RE.search(value)
    if not m:
        return None
    day, mon, year, hh, mm, ss, ampm, tz = m.groups()
    month = _MONTHS.get(mon[:3].lower())
    if not month:
        return None
    hour = int(hh)
    if ampm:
        ampm = ampm.upper()
        if ampm == "PM" and hour != 12:
            hour += 12
        elif ampm == "AM" and hour == 12:
            hour = 0
    offset = timedelta(0)
    if tz:
        if tz[0] in "+-" and tz[1:].isdigit():
            sign = 1 if tz[0] == "+" else -1
            offset = sign * timedelta(hours=int(tz[1:3]), minutes=int(tz[3:5]))
        else:
            offset = timedelta(hours=_TZ_OFFSETS.get(tz.upper(), 0))
    try:
        local = datetime(int(year), month, int(day), hour, int(mm), int(ss or 0),
                         tzinfo=timezone(offset))
    except ValueError:
        return None
    return local.astimezone(timezone.utc)


def tag_text(*texts: str | None) -> list[str]:
    blob = " ".join(t for t in texts if t)
    return [tag for tag, pat in TAG_PATTERNS.items() if pat.search(blob)]


def _clean(text: str | None) -> str:
    if not text:
        return ""
    text = html.unescape(_TAG_RE.sub(" ", text))
    return re.sub(r"\s+", " ", text).strip()


def _entry_date(entry) -> datetime | None:
    raw = entry.get("published") or entry.get("updated")
    dt = parse_rss_date(raw)
    if dt is None and entry.get("published_parsed"):
        dt = datetime(*entry.published_parsed[:6], tzinfo=timezone.utc)
    return dt


def parse_rotowire(xml: str) -> list[NewsItem]:
    feed = feedparser.parse(xml)
    items = []
    for e in feed.entries:
        title = _clean(e.get("title"))
        if not title:
            continue
        player, headline = (title.split(": ", 1) + [""])[:2] if ": " in title else (None, title)
        blurb = _ROTOWIRE_TRAILER.sub("", _clean(e.get("summary") or e.get("description")))
        url = e.get("link")
        if url:
            url = re.sub(r"(?<!:)//+", "/", url)  # rotowire.com//hockey -> rotowire.com/hockey
        items.append(NewsItem(
            source="rotowire",
            id=e.get("id") or e.get("guid"),
            player_name=player.strip() if player else None,
            headline=(headline or title).strip(),
            blurb=blurb,
            url=url,
            published=_entry_date(e),
            tags=tag_text(headline, blurb),
        ))
    return items


def parse_espn_news(xml: str) -> list[NewsItem]:
    feed = feedparser.parse(xml)
    items = []
    for e in feed.entries:
        title = _clean(e.get("title"))
        if not title:
            continue
        blurb = _clean(e.get("summary") or e.get("description"))
        items.append(NewsItem(
            source="espn",
            id=e.get("id") or e.get("guid"),
            player_name=None,
            headline=title,
            blurb=blurb,
            url=e.get("link"),
            published=_entry_date(e),
            tags=tag_text(title, blurb),
        ))
    return items


def fetch_rotowire(fetch_text: FetchText | None = None) -> list[NewsItem]:
    return parse_rotowire((fetch_text or default_fetch_text)(ROTOWIRE_URL, None))


def fetch_espn_news(fetch_text: FetchText | None = None) -> list[NewsItem]:
    return parse_espn_news((fetch_text or default_fetch_text)(ESPN_NEWS_URL, None))


def fetch_all_news(fetch_text: FetchText | None = None) -> list[NewsItem]:
    """All sources, newest first. A failing source is logged and skipped."""
    items: list[NewsItem] = []
    for fn in (fetch_rotowire, fetch_espn_news):
        try:
            items.extend(fn(fetch_text))
        except Exception as exc:  # network / parse failure in one feed shouldn't sink the rest
            log.warning("news source %s failed: %s", fn.__name__, exc)
    epoch = datetime.min.replace(tzinfo=timezone.utc)
    items.sort(key=lambda n: n.published or epoch, reverse=True)
    return items
