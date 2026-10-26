"""Player news from RSS feeds (RotoWire player blurbs, ESPN NHL headlines).

The feeds only carry the latest few dozen items (a few hours of news), so every fetch goes
through :func:`fetch_news`, which merges the fresh items into a 30-day store
(``<fm_data_dir>/news_items.json``) and returns fresh + stored items. Every consumer (``fm news``,
the digest, LLM narration / ``fm ask``, the dashboard, rookie role signals) reads news this way.
"""

from __future__ import annotations

import html
import json
import logging
import re
from datetime import date, datetime, timedelta, timezone
from email.utils import parsedate_to_datetime
from pathlib import Path
from typing import Any, Callable, Iterable, Mapping

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
    return _newest_first(items)


# --------------------------------------------------------------------------- 30-day store

STORE_NAME = "news_items.json"
LEGACY_STORE_NAME = "news_role_items.json"   # rookie role-signal items only (before the shared store)
STORE_MAX_AGE_DAYS = 30
STORE_MAX_ITEMS = 5000                       # ~30 days of both feeds fetched several times a day
_EPOCH = datetime.min.replace(tzinfo=timezone.utc)


def _newest_first(items: Iterable[NewsItem]) -> list[NewsItem]:
    return sorted(items, key=lambda n: n.published or _EPOCH, reverse=True)


def item_key(it: NewsItem) -> str:
    """Dedupe key: the feed guid, else source + headline + publish time."""
    return it.id or f"{it.source}|{it.headline}|{it.published.isoformat() if it.published else ''}"


def _ref_time(as_of: date | datetime | None) -> datetime:
    """Reference time for the age cut-off: now, or the end of day ``as_of`` (UTC)."""
    if as_of is None:
        return datetime.now(timezone.utc)
    if isinstance(as_of, datetime):
        return as_of if as_of.tzinfo else as_of.replace(tzinfo=timezone.utc)
    return datetime(as_of.year, as_of.month, as_of.day, tzinfo=timezone.utc) + timedelta(days=1)


def _read_json(path: Path) -> dict[str, Any]:
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return raw if isinstance(raw, dict) else {}


def _write_store(store_dir: Path, raw: dict[str, Any]) -> bool:
    path = store_dir / STORE_NAME
    try:
        store_dir.mkdir(parents=True, exist_ok=True)
        tmp = path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(raw, indent=1, sort_keys=True), encoding="utf-8")
        tmp.replace(path)
        return True
    except OSError as e:
        log.info("news store not saved: %s", e)
        return False


def _load_raw(store_dir: Path) -> dict[str, Any]:
    """The store's {key: stored item}, with the old rookie-signal item file merged in once (it is
    deleted only after the merged store has been written, so nothing is lost)."""
    raw = _read_json(store_dir / STORE_NAME)
    legacy = store_dir / LEGACY_STORE_NAME
    if legacy.exists():
        for key, d in _read_json(legacy).items():
            raw.setdefault(key, d)
        if _write_store(store_dir, raw):
            try:
                legacy.unlink()
            except OSError:
                pass
    return raw


def _seen(d: Mapping[str, Any]) -> datetime | None:
    """When a stored item was published, else first stored (undated items age out from then)."""
    for k in ("published", "seen"):
        v = d.get(k)
        if v:
            try:
                dt = datetime.fromisoformat(str(v))
            except ValueError:
                continue
            return dt if dt.tzinfo else dt.replace(tzinfo=timezone.utc)
    return None


def _prune(raw: dict[str, Any], ref: datetime) -> dict[str, Any]:
    cutoff = ref - timedelta(days=STORE_MAX_AGE_DAYS)
    keep = [(k, d) for k, d in raw.items() if isinstance(d, dict) and (_seen(d) or ref) >= cutoff]
    keep.sort(key=lambda kv: _seen(kv[1]) or _EPOCH, reverse=True)
    return dict(keep[:STORE_MAX_ITEMS])


def _items(raw: Mapping[str, Any]) -> list[NewsItem]:
    out = []
    for d in raw.values():
        try:
            out.append(NewsItem.model_validate(d))
        except Exception:
            continue
    return out


def stored_news(store_dir: str | Path | None, as_of: date | datetime | None = None) -> list[NewsItem]:
    """Items in the store no older than STORE_MAX_AGE_DAYS before ``as_of`` (default now), newest first."""
    if store_dir is None:
        return []
    return _newest_first(_items(_prune(_load_raw(Path(store_dir)), _ref_time(as_of))))


def store_news(store_dir: str | Path | None, items: Iterable[NewsItem],
               as_of: date | datetime | None = None) -> list[NewsItem]:
    """Merge ``items`` into the store (a fresh copy replaces a stored one with the same key), prune
    it to the last STORE_MAX_AGE_DAYS / STORE_MAX_ITEMS, save it, and return every stored item no
    older than that before ``as_of``, newest first. Without ``store_dir``: just ``items``."""
    items = list(items)
    if store_dir is None:
        return _newest_first({item_key(it): it for it in items}.values())
    path = Path(store_dir)
    raw = _load_raw(path)
    now = datetime.now(timezone.utc).isoformat()
    for it in items:
        key = item_key(it)
        d = it.model_dump(mode="json")
        d["seen"] = (raw.get(key) or {}).get("seen") or now
        raw[key] = d
    raw = _prune(raw, datetime.now(timezone.utc))
    _write_store(path, raw)
    return _newest_first(_items(_prune(raw, _ref_time(as_of))))


def fetch_news(fetch_text: FetchText | None, store_dir: str | Path | None,
               as_of: date | datetime | None = None) -> tuple[list[NewsItem], list[tuple[str, Exception]]]:
    """Fetch every source, merge the fresh items into the 30-day store and return
    (fresh + stored items newest first, [(source label, error)] for sources that failed). A failed
    or offline source still leaves the stored items."""
    fresh: list[NewsItem] = []
    failed: list[tuple[str, Exception]] = []
    for label, fn in (("RotoWire news", fetch_rotowire), ("ESPN news", fetch_espn_news)):
        try:
            fresh.extend(fn(fetch_text))
        except Exception as e:  # one feed (or offline cache miss) must not sink the rest
            failed.append((label, e))
    return store_news(store_dir, fresh, as_of), failed
