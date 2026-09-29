"""Name / team / position normalization used to match players across data sources."""

from __future__ import annotations

import re
import unicodedata

# Characters NFKD does not decompose into base letter + combining mark.
_SPECIAL = str.maketrans({
    "ø": "o", "Ø": "o", "æ": "ae", "Æ": "ae", "œ": "oe", "Œ": "oe", "ß": "ss",
    "ł": "l", "Ł": "l", "đ": "d", "Đ": "d", "ð": "d", "þ": "th", "ı": "i",
})

SUFFIXES = frozenset({"jr", "sr", "ii", "iii", "iv"})

# Only unambiguous short forms -> canonical first name (applied to the first token only).
NICKNAMES: dict[str, str] = {
    "alex": "alexander",
    "mitch": "mitchell",
    "matt": "matthew",
    "nick": "nicholas",
    "mike": "michael",
    "chris": "christopher",
    "zach": "zachary",
    "josh": "joshua",
    "will": "william",
}

_APOSTROPHES = re.compile(r"['’‘`´]")
_NON_ALNUM = re.compile(r"[^a-z0-9 ]+")


def strip_accents(text: str) -> str:
    text = text.translate(_SPECIAL)
    decomposed = unicodedata.normalize("NFKD", text)
    return "".join(ch for ch in decomposed if not unicodedata.combining(ch))


def normalize_name(name: str | None) -> str:
    """Canonical comparison key for a player name.

    'Martin Nečas' -> 'martin necas', 'T.J. Oshie' / 'T. J. Oshie' -> 'tj oshie',
    'Alex Ovechkin' -> 'alexander ovechkin', 'Jacob Bernard-Docker' -> 'jacob bernard docker'.
    """
    if not name:
        return ""
    s = strip_accents(name).lower()
    s = _APOSTROPHES.sub("", s)
    s = s.replace("-", " ").replace("‐", " ").replace("–", " ")
    # "t.j." -> "tj" but "j. smith" -> "j smith": drop periods, keeping token boundaries
    s = re.sub(r"\.(?=\S)", ". ", s)          # "t.j.oshie" edge case -> "t. j. oshie"
    s = s.replace(".", "")
    s = _NON_ALNUM.sub(" ", s)
    tokens = [t for t in s.split() if t]
    # merge a leading run of single letters: "t j oshie" -> "tj oshie"
    if len(tokens) >= 3 and len(tokens[0]) == 1:
        i = 0
        while i < len(tokens) - 1 and len(tokens[i]) == 1:
            i += 1
        if i >= 2:
            tokens = ["".join(tokens[:i])] + tokens[i:]
    tokens = [t for t in tokens if t not in SUFFIXES] or tokens
    if tokens and tokens[0] in NICKNAMES and len(tokens) > 1:
        tokens[0] = NICKNAMES[tokens[0]]
    return " ".join(tokens)


# Non-NHL abbreviations (ESPN, Fantrax, legacy) -> NHL abbreviations.
TEAM_ALIASES: dict[str, str] = {
    "NJ": "NJD", "SJ": "SJS", "TB": "TBL", "LA": "LAK", "LV": "VGK", "VEG": "VGK",
    "UTAH": "UTA", "UHC": "UTA", "WAS": "WSH", "MON": "MTL", "CLS": "CBJ", "CLB": "CBJ",
    "NAS": "NSH", "CAL": "CGY", "WIN": "WPG", "ANH": "ANA",
    "ARI": "UTA",  # Coyotes franchise relocated to Utah (2024)
}


def normalize_team(team: str | None) -> str | None:
    if not team:
        return None
    t = team.strip().upper()
    return TEAM_ALIASES.get(t, t)


_POS_ALIASES = {"L": "LW", "LW": "LW", "R": "RW", "RW": "RW", "C": "C", "D": "D", "G": "G",
                "F": "F", "W": "W"}
_FORWARDS = {"C", "LW", "RW"}


def normalize_positions(pos: str | None) -> set[str]:
    """'C/LW' -> {'C','LW'}; 'L' -> {'LW'}; 'F' -> {'C','LW','RW'}; 'W' -> {'LW','RW'}."""
    if not pos:
        return set()
    out: set[str] = set()
    for part in re.split(r"[/,\s]+", pos.strip().upper()):
        p = _POS_ALIASES.get(part)
        if p == "F":
            out |= _FORWARDS
        elif p == "W":
            out |= {"LW", "RW"}
        elif p:
            out.add(p)
    return out


def positions_agree(a: str | None, b: str | None) -> bool:
    """True if two position strings overlap. Forwards are treated loosely (C vs LW
    agree, since sites disagree on forward eligibility) but F/D/G groups must match."""
    pa, pb = normalize_positions(a), normalize_positions(b)
    if not pa or not pb:
        return False
    if pa & pb:
        return True
    return bool(pa & _FORWARDS) and bool(pb & _FORWARDS)
