"""Availability adjustments for injury / suspension status.

The flat multipliers come from ``params.availability`` at call time (a harness version may
override them); ``AVAILABILITY`` is a read-only import-time snapshot kept for display and old
imports.

Season horizon, proportional to the games missed (:func:`season_availability`): a flat 0.5 for
every suspension or 0.4 for every IR stint values a 7-game suspension like a half-season one. When
the absence can be sized, the season multiplier is instead ``max(0.05, 1 - games_missed /
games_remaining)``, games counted on the player's NHL team schedule (``ctx.schedule``; without
one, 82 games over a 186-day season). :func:`expected_return` reads the ``status_note``, first
match wins:

1. an explicit estimated return date, "est. return 2026-10-17" (the injury feed's field);
2. a game count: "six-game suspension", "suspended 3 games", "miss Florida's first 14
   regular-season games", "miss the first two games of the year";
3. a calendar date: "return Nov. 2", "back by Dec. 15";
4. "the first week / first two weeks / first month" of the season (counted from opening night);
5. a duration: "4-6 months" (midpoint), "another 2-3 months", "out about three weeks", "for 10
   days" (counted from ``as_of``, or from opening night when that is later; from the injury month
   when the note says so: "4-6 months after having shoulder surgery in June");
6. a month: "until at least early November" (Nov 5; mid = 15th, late = 25th, "until / at least /
   before" a bare month = the 1st, otherwise the 15th), "sometime in November", "return in
   December" (never a bare "in January": that is usually when the injury happened);
7. "month-to-month" 45 days, "week-to-week" 21 days, "day-to-day" 3 days.

Without anything in the note, the status decides: dtd = day-to-day (3 days), IR = 30 days, LTIR =
the rest of the season, suspended = 5 games; "out" (and anything else) keeps the flat multiplier.
The week horizon is unchanged (``availability_multiplier``: 0 while out).
"""
from __future__ import annotations

import re
from dataclasses import dataclass
from datetime import date, timedelta
from typing import Any, Iterable, Literal, Mapping

from . import params as _params

Horizon = Literal["week", "season"]

# status -> (week multiplier, season multiplier); import-time snapshot, live code uses the accessor
AVAILABILITY: dict[str, tuple[float, float]] = _params.availability_table()

SEASON_GAMES = 82
SEASON_DAYS = 186                    # opening night to the last regular-season game
FLOOR = 0.05
DAY_TO_DAY_DAYS = 3
WEEK_TO_WEEK_DAYS = 21
MONTH_TO_MONTH_DAYS = 45
IR_DEFAULT_DAYS = 30
SUSPENDED_DEFAULT_GAMES = 5
MONTH_DAYS = 30.4
OUT_STATUSES = ("dtd", "out", "ir", "ltir", "suspended")


def availability_multiplier(status: str, horizon: Horizon = "season",
                            params: Mapping[str, Any] | None = None) -> float:
    return _params.availability(status, horizon, params)


# --------------------------------------------------------------------------- note parsing

_NUMS = {"a": 1, "an": 1, "one": 1, "two": 2, "three": 3, "four": 4, "five": 5, "six": 6, "seven": 7, "eight": 8,
         "nine": 9, "ten": 10, "eleven": 11, "twelve": 12, "thirteen": 13, "fourteen": 14, "fifteen": 15,
         "sixteen": 16, "seventeen": 17, "eighteen": 18, "nineteen": 19, "twenty": 20, "couple": 2, "few": 3}
_NUM = r"(\d{1,3}|" + "|".join(sorted(_NUMS, key=len, reverse=True)) + r")"
_MONTHS = {m: i for i, m in enumerate(["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct",
                                        "nov", "dec"], start=1)}
_MONTH = r"(Jan(?:uary)?|Feb(?:ruary)?|Mar(?:ch)?|Apr(?:il)?|May|June?|July?|Aug(?:ust)?|Sept?(?:ember)?|" \
         r"Oct(?:ober)?|Nov(?:ember)?|Dec(?:ember)?)"
_UNIT_DAYS = {"day": 1.0, "week": 7.0, "month": MONTH_DAYS}

_EST_RETURN = re.compile(r"est\.?\s*return\s+(\d{4}-\d{2}-\d{2})", re.I)
_GAMES = (
    re.compile(rf"\b{_NUM}[- ]game\s+suspension\b", re.I),
    re.compile(rf"\bsuspended\s+(?:for\s+)?{_NUM}\s+games\b", re.I),
    re.compile(rf"\b(?:miss|misses|missing|sit\s+out|serve|out\s+for|sidelined\s+for)\s+(?:at\s+least\s+)?"
               rf"(?:the\s+|his\s+|[A-Z][\w.]*(?:'s|’s)\s+)?(?:first\s+|next\s+|opening\s+)?{_NUM}\s+"
               rf"(?:regular[- ]season\s+)?games\b", re.I),
)
_DATE = re.compile(rf"\b(?:return|returns|returning|back|activated|available|rejoin)\b[^.;]{{0,25}}?\b(?:on|by|for)?\s*"
                   rf"{_MONTH}\.?\s+(\d{{1,2}})\b", re.I)
_FIRST = re.compile(rf"\bfirst\s+(?:{_NUM}\s+)?(week|month)s?\b(?:\s+of\s+(?:the\s+)?(?:\d{{4}}-\d{{2}}\s+)?"
                    rf"(?:regular\s+)?(?:season|campaign|year))?", re.I)
_RANGE = re.compile(rf"\b{_NUM}\s*(?:-|–|to)\s*{_NUM}\s+(day|week|month)s?\b", re.I)
_SINGLE = re.compile(rf"\b(?:another|about|approximately|roughly|around|out|miss|sidelined|for|at\s+least)\s+"
                     rf"(?:for\s+)?(?:about\s+|at\s+least\s+)?{_NUM}\s+(day|week|month)s?\b", re.I)
_MONTH_PHRASE = re.compile(rf"\b(until|by|sometime\s+in|at\s+least|around|before|(?:return|back|rejoin\w*)"
                           rf"(?:\s+\w+){{0,3}}?\s+in)\s+"
                           rf"(?:at\s+least\s+)?(?:the\s+)?(?:(early|mid|late)[- ])?{_MONTH}\b(?!\.?\s+\d)", re.I)
# "4-6 months after having shoulder surgery in June": a duration counts from the (past) injury month
_INJURED_IN = re.compile(rf"\b(?:surgery|procedure|injur\w*|hurt|operation)\b[^.;]{{0,20}}?\b(?:in|on)\s+"
                         rf"(?:(early|mid|late)[- ])?{_MONTH}\b", re.I)
_TO_PHRASES = ((re.compile(r"\bmonth[- ]to[- ]month\b", re.I), MONTH_TO_MONTH_DAYS, "month-to-month"),
               (re.compile(r"\bweek[- ]to[- ]week\b", re.I), WEEK_TO_WEEK_DAYS, "week-to-week"),
               (re.compile(r"\bday[- ]to[- ]day\b", re.I), DAY_TO_DAY_DAYS, "day-to-day"))


def _num(s: str) -> int:
    s = s.lower()
    return int(s) if s.isdigit() else _NUMS.get(s, 0)


def _season_year(as_of: date) -> int:
    return as_of.year if as_of.month >= 7 else as_of.year - 1


def _month_date(month: str, day: int, as_of: date) -> date | None:
    m = _MONTHS.get(month[:3].lower())
    if m is None:
        return None
    y = _season_year(as_of)
    try:
        return date(y if m >= 7 else y + 1, m, day)
    except ValueError:
        return None


@dataclass
class ReturnEstimate:
    """How long a player is expected to be out (``games_missed`` of ``games_remaining``)."""
    return_date: date | None
    games_missed: float
    games_remaining: float
    multiplier: float
    source: str                      # what was read, e.g. 'note: "est. return 2026-10-17"'

    def text(self) -> str:
        when = f"back ~{self.return_date.isoformat()}, " if self.return_date else ""
        return (f"Expected return: {when}~{self.games_missed:.0f} of {self.games_remaining:.0f} remaining games "
                f"missed ({self.source}): season availability x{self.multiplier:.2f}")


def _parse(note: str, as_of: date, season_start: date | None) -> tuple[str, Any, str] | None:
    """(kind, value, matched text) from the note; kind in date / games / days_from_start / days."""
    m = _EST_RETURN.search(note)
    if m:
        try:
            return "date", date.fromisoformat(m.group(1)), m.group(0)
        except ValueError:
            pass
    for pat in _GAMES:
        m = pat.search(note)
        if m and _num(m.group(1)) > 0:
            return "games", _num(m.group(1)), m.group(0)
    m = _DATE.search(note)
    if m:
        d = _month_date(m.group(1), int(m.group(2)), as_of)
        if d is not None:
            return "date", d, m.group(0)
    m = _FIRST.search(note)
    if m and ("season" in m.group(0).lower() or "campaign" in m.group(0).lower() or "year" in m.group(0).lower()
              or re.search(r"\bmiss\b", note, re.I)):
        n = _num(m.group(1)) if m.group(1) else 1
        return "days_from_start", n * _UNIT_DAYS[m.group(2).lower()], m.group(0)
    m = _RANGE.search(note)
    if m:
        a, b = _num(m.group(1)), _num(m.group(2))
        if a > 0 and b >= a:
            return "days", (a + b) / 2 * _UNIT_DAYS[m.group(3).lower()], m.group(0)
    m = _SINGLE.search(note)
    if m and _num(m.group(1)) > 0:
        return "days", _num(m.group(1)) * _UNIT_DAYS[m.group(2).lower()], m.group(0)
    m = _MONTH_PHRASE.search(note)
    if m:
        lead, part, month = m.group(1).lower(), (m.group(2) or "").lower(), m.group(3)
        day = {"early": 5, "mid": 15, "late": 25}.get(part) or (1 if lead in ("until", "at least", "before") else 15)
        d = _month_date(month, day, as_of)
        if d is not None:
            return "date", d, m.group(0)
    for pat, days, label in _TO_PHRASES:
        m = pat.search(note)
        if m:
            return "days", float(days), m.group(0)
    return None


def _count_games(team_dates: Iterable[date] | None, start: date, end: date | None) -> int | None:
    if not team_dates:
        return None
    ds = set(team_dates)
    return sum(1 for d in ds if d >= start and (end is None or d < end))


def _games_between(start: date, end: date, team_dates: Iterable[date] | None, season_start: date | None) -> float:
    n = _count_games(team_dates, start, end)
    if n is not None:
        return float(n)
    first = max(start, season_start) if season_start else start
    return max(0.0, (end - first).days) * SEASON_GAMES / SEASON_DAYS


def _games_remaining(as_of: date, team_dates: Iterable[date] | None, season_start: date | None) -> float:
    n = _count_games(team_dates, as_of, None)
    if n:
        return float(n)
    if season_start is None or as_of <= season_start:
        return float(SEASON_GAMES)
    return max(1.0, SEASON_GAMES * (1.0 - (as_of - season_start).days / SEASON_DAYS))


def _date_after_games(n: float, as_of: date, team_dates: Iterable[date] | None) -> date | None:
    if not team_dates:
        return None
    ahead = sorted(d for d in set(team_dates) if d >= as_of)
    k = int(round(n))
    return ahead[k] if k < len(ahead) else None


def return_estimate(status: str, note: str | None, as_of: date, team_dates: Iterable[date] | None = None,
                    season_start: date | None = None) -> ReturnEstimate | None:
    """The sized absence of a player with ``status`` (module docstring); None when healthy / unknown
    or when nothing can be inferred (the flat multiplier applies)."""
    if status not in OUT_STATUSES:
        return None
    team_dates = list(team_dates or [])
    remaining = _games_remaining(as_of, team_dates, season_start)
    anchor = max(as_of, season_start) if season_start else as_of
    got = _parse(note or "", as_of, season_start)
    ret: date | None = None
    if got is not None:
        kind, val, text = got
        src = f'note: "{text.strip()}"'
        if kind == "games":
            missed = float(val)
            ret = _date_after_games(missed, as_of, team_dates)
        else:
            if kind == "date":
                ret = val
            elif kind == "days_from_start":
                ret = anchor + timedelta(days=round(val))
            else:
                start = anchor
                inj = _INJURED_IN.search(note or "")
                if inj:
                    part = (inj.group(1) or "").lower()
                    d = _month_date(inj.group(2), {"early": 5, "mid": 15, "late": 25}.get(part, 15), as_of)
                    if d is not None and d > as_of:        # "in June" said in September: last June
                        d = d.replace(year=d.year - 1)
                    if d is not None and d < as_of:
                        start, src = d, f'{src}, from "{inj.group(0).strip()}"'
                ret = start + timedelta(days=round(val))
            missed = _games_between(as_of, ret, team_dates, season_start) if ret > as_of else 0.0
    elif status == "dtd":
        ret, src = anchor + timedelta(days=DAY_TO_DAY_DAYS), f"day-to-day, ~{DAY_TO_DAY_DAYS} days assumed"
        missed = _games_between(as_of, ret, team_dates, season_start)
    elif status == "ir":
        ret, src = anchor + timedelta(days=IR_DEFAULT_DAYS), f"IR without a timetable, {IR_DEFAULT_DAYS} days assumed"
        missed = _games_between(as_of, ret, team_dates, season_start)
    elif status == "ltir":
        ret, src, missed = None, "LTIR without a timetable: rest of the season assumed", remaining
    elif status == "suspended":
        missed, src = float(SUSPENDED_DEFAULT_GAMES), f"suspension length unknown, {SUSPENDED_DEFAULT_GAMES} games assumed"
        ret = _date_after_games(missed, as_of, team_dates)
    else:
        return None
    missed = min(missed, remaining)
    mult = max(FLOOR, 1.0 - missed / remaining) if remaining > 0 else FLOOR
    return ReturnEstimate(return_date=ret, games_missed=missed, games_remaining=remaining, multiplier=mult,
                          source=src)


def expected_return(status: str, note: str | None, as_of: date, team_dates: Iterable[date] | None = None,
                    season_start: date | None = None) -> tuple[date | None, float | None]:
    """(estimated return date or None, estimated games missed or None when nothing can be inferred)."""
    est = return_estimate(status, note, as_of, team_dates, season_start)
    return (None, None) if est is None else (est.return_date, est.games_missed)


def season_availability(status: str, note: str | None, as_of: date, team_dates: Iterable[date] | None = None,
                        season_start: date | None = None, params: Mapping[str, Any] | None = None
                        ) -> tuple[float, ReturnEstimate | None]:
    """(season-horizon availability multiplier, the estimate behind it or None for the flat value)."""
    est = return_estimate(status, note, as_of, team_dates, season_start)
    if est is None:
        return availability_multiplier(status, "season", params), None
    return est.multiplier, est
