"""Attach news items to league players.

* Items with ``player_name`` (RotoWire "Player: headline") go through ``matcher.match_player``;
  only ``exact``/``high`` confidence matches are kept.
* Items without one (ESPN headlines) are scanned for players' normalized full names appearing as
  whole-word substrings of the normalized headline + blurb. Names shared by several players in
  the pool are skipped as ambiguous.
"""
from __future__ import annotations

import re
from collections import defaultdict
from datetime import datetime, timezone

from ..matching.matcher import Candidate, PlayerIndex
from ..matching.normalize import NICKNAMES, SUFFIXES, normalize_name, strip_accents
from ..models import Player
from ..providers.news import NewsItem

_APOS = re.compile(r"['’‘`´]")
_NON_ALNUM = re.compile(r"[^a-z0-9]+")
_SHORT_FORMS: dict[str, list[str]] = defaultdict(list)
for _short, _full in NICKNAMES.items():
    _SHORT_FORMS[_full].append(_short)


def normalize_text(text: str | None) -> str:
    """Accent/punctuation-insensitive token string for substring scans (no nickname rewriting)."""
    if not text:
        return ""
    s = _APOS.sub("", strip_accents(text).lower())
    s = s.replace(".", "")
    return " ".join(_NON_ALNUM.sub(" ", s).split())


def name_keys(name: str) -> set[str]:
    """Full-name variants to look for in free text (accents stripped, with/without suffix,
    canonical and short first-name forms). Single-token names produce no keys."""
    keys: set[str] = set()
    raw = normalize_text(name).split()
    canon = normalize_name(name).split()
    for toks in (raw, [t for t in raw if t not in SUFFIXES], canon):
        if len(toks) >= 2:
            keys.add(" ".join(toks))
            for short in _SHORT_FORMS.get(toks[0], []):
                keys.add(" ".join([short, *toks[1:]]))
    return keys


def _candidate(p: Player) -> Candidate:
    return Candidate(key=p.cid, name=p.name, team=p.team, position="/".join(p.positions) or None)


def match_news_to_players(news: list[NewsItem], players: list[Player]) -> dict[str, list[NewsItem]]:
    """Map player cid -> news items about that player (only exact/high confidence matches).

    Each cid's list is de-duplicated and sorted newest first.
    """
    uniq: dict[str, Player] = {}
    for p in players:
        uniq.setdefault(p.cid, p)
    if not news or not uniq:
        return {}
    index = PlayerIndex(_candidate(p) for p in uniq.values())

    key_owners: dict[str, set[str]] = defaultdict(set)
    for p in uniq.values():
        for k in name_keys(p.name):
            key_owners[k].add(p.cid)
    unique_keys = {k: next(iter(cids)) for k, cids in key_owners.items() if len(cids) == 1}
    # group keys by token count so the scan is a handful of n-gram set lookups per item
    lengths = sorted({len(k.split()) for k in unique_keys})

    out: dict[str, list[NewsItem]] = defaultdict(list)
    seen: dict[str, set[str]] = defaultdict(set)

    def add(cid: str, item: NewsItem) -> None:
        ident = item.id or f"{item.source}|{item.headline}|{item.published}"
        if ident not in seen[cid]:
            seen[cid].add(ident)
            out[cid].append(item)

    for item in news:
        if item.player_name:
            res = index.match(item.player_name)
            if res.matched:
                add(res.key, item)
            continue
        tokens = normalize_text(f"{item.headline} {item.blurb}").split()
        hits: set[str] = set()
        for n in lengths:
            for i in range(len(tokens) - n + 1):
                cid = unique_keys.get(" ".join(tokens[i:i + n]))
                if cid:
                    hits.add(cid)
        for cid in sorted(hits):
            add(cid, item)

    epoch = datetime.min.replace(tzinfo=timezone.utc)
    return {cid: sorted(items, key=lambda n: n.published or epoch, reverse=True)
            for cid, items in out.items()}


def player_news_summary(items: list[NewsItem], limit: int = 3, blurb_chars: int = 200) -> list[str]:
    """Short one-line summaries, newest first: '2026-09-27 (rotowire): headline -- blurb'."""
    epoch = datetime.min.replace(tzinfo=timezone.utc)
    lines: list[str] = []
    for n in sorted(items, key=lambda n: n.published or epoch, reverse=True)[:limit]:
        when = n.published.strftime("%Y-%m-%d") if n.published else "undated"
        blurb = " ".join((n.blurb or "").split())
        if len(blurb) > blurb_chars:
            blurb = blurb[: blurb_chars - 1].rstrip() + "…"
        head = " ".join(n.headline.split())
        lines.append(f"{when} ({n.source}): {head}" + (f" -- {blurb}" if blurb else ""))
    return lines


__all__ = ["match_news_to_players", "player_news_summary", "normalize_text", "name_keys"]
