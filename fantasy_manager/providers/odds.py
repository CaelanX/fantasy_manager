"""Betting-market context for NHL games from The Odds API (v4): moneylines, game totals and the
implied team totals derived from them.

Purpose: a logged, per-day signal ("how many goals does the market expect this team to score
tonight?") that the harness can later test as a weekly-projection feature. Nothing in the
valuation reads it yet; ``team_context`` is the hook for that.

Request: ``GET /v4/sports/icehockey_nhl/odds?regions=us&markets=h2h,totals&oddsFormat=american``.
Cost = markets x regions = 2 credits per call; the free tier has 500 credits a month, so the
client never calls more than once per 20 hours (``odds_cache.json`` in the data dir) and the
daily archive (``<fm_data_dir>/archive/odds-YYYY-MM-DD.json``) is written once per day. Quota
comes from the ``x-requests-remaining`` / ``x-requests-used`` / ``x-requests-last`` headers.

The API key travels only in the request's query string. It is never stored (not in the cache
file, the archive or error messages) and never logged.

Math (all approximations; see the function docstrings):

* American odds -> implied probability: ``-A -> A / (A + 100)``, ``+B -> 100 / (B + 100)``.
* No-vig (proportional / multiplicative method): ``p_home = q_home / (q_home + q_away)``.
* Expected game goals: the Poisson mean ``lam`` whose no-vig P(over the line) matches the market
  (a 6.5 line with a juiced Under means fewer than ~6.6 expected goals).
* Team split: home and away goals are independent Poisson with means ``s * lam`` and
  ``(1 - s) * lam``; ``s`` is solved so that P(home wins) = P(H > A) + s * P(H = A) equals the
  no-vig home win probability (ties go to overtime / shootout, won in proportion to the scoring
  rates). Implied team totals are ``s * lam`` and ``(1 - s) * lam``.
* Consensus across bookmakers = the median (of implied probabilities for prices, of lines for
  the total, and of the per-book win probability / expected total for the derived numbers).

Hockey goals are over-dispersed relative to Poisson (empty-net goals, score effects) and the
listed total includes overtime / shootout goals, so the implied totals are an approximation
good to roughly a tenth of a goal, which is what a projection feature needs.
"""
from __future__ import annotations

import json
import logging
import math
import re
import statistics
import unicodedata
from dataclasses import asdict, dataclass, field
from datetime import date, datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Callable, Iterable, Mapping

import httpx

log = logging.getLogger(__name__)

ODDS_URL = "https://api.the-odds-api.com/v4/sports/icehockey_nhl/odds"
MARKETS = "h2h,totals"
CACHE_FILE = "odds_cache.json"
CACHE_TTL_HOURS = 20.0
ARCHIVE_VERSION = 1
LOOKBACK_DAYS = 7

# fetch_json(url, params) -> payload, or (payload, response headers)
FetchJson = Callable[[str, "dict | None"], Any]

# The Odds API team names -> NHL abbreviations (providers/nhl.py NHL_TEAMS).
TEAM_ABBREVS: dict[str, str] = {
    "Anaheim Ducks": "ANA", "Boston Bruins": "BOS", "Buffalo Sabres": "BUF", "Calgary Flames": "CGY",
    "Carolina Hurricanes": "CAR", "Chicago Blackhawks": "CHI", "Colorado Avalanche": "COL",
    "Columbus Blue Jackets": "CBJ", "Dallas Stars": "DAL", "Detroit Red Wings": "DET",
    "Edmonton Oilers": "EDM", "Florida Panthers": "FLA", "Los Angeles Kings": "LAK", "Minnesota Wild": "MIN",
    "Montreal Canadiens": "MTL", "Nashville Predators": "NSH", "New Jersey Devils": "NJD",
    "New York Islanders": "NYI", "New York Rangers": "NYR", "Ottawa Senators": "OTT",
    "Philadelphia Flyers": "PHI", "Pittsburgh Penguins": "PIT", "San Jose Sharks": "SJS",
    "Seattle Kraken": "SEA", "St. Louis Blues": "STL", "Tampa Bay Lightning": "TBL",
    "Toronto Maple Leafs": "TOR", "Utah Mammoth": "UTA", "Vancouver Canucks": "VAN",
    "Vegas Golden Knights": "VGK", "Washington Capitals": "WSH", "Winnipeg Jets": "WPG",
}
# Spellings seen across books / seasons (accents and punctuation are normalised away anyway).
TEAM_ALIASES: dict[str, str] = {
    "Saint Louis Blues": "STL", "Utah Hockey Club": "UTA", "Utah HC": "UTA",
}


def _norm_name(name: str) -> str:
    s = unicodedata.normalize("NFKD", str(name)).encode("ascii", "ignore").decode("ascii")
    return re.sub(r"[^a-z0-9]", "", s.lower())


_NAME_INDEX = {_norm_name(k): v for k, v in {**TEAM_ABBREVS, **TEAM_ALIASES}.items()}


def team_abbrev(name: str | None) -> str | None:
    """Full team name as The Odds API writes it ("Montréal Canadiens", "St Louis Blues") -> "MTL"."""
    if not name:
        return None
    return _NAME_INDEX.get(_norm_name(name))


class OddsError(Exception):
    """An Odds API failure; the message never contains the API key."""

    def __init__(self, message: str, status_code: int | None = None, headers: Mapping[str, str] | None = None):
        super().__init__(message)
        self.status_code = status_code
        self.headers = dict(headers or {})


# --------------------------------------------------------------------------- odds math

def american_to_prob(price: float) -> float:
    """Implied probability of an American price, vig included: -150 -> 0.6, +150 -> 0.4."""
    price = float(price)
    if price == 0 or -100 < price < 100:
        raise ValueError(f"not an American price: {price}")
    return -price / (-price + 100.0) if price < 0 else 100.0 / (price + 100.0)


def prob_to_american(p: float) -> int:
    """Inverse of american_to_prob (rounded): 0.6 -> -150, 0.4 -> +150, 0.5 -> +100."""
    if not 0.0 < p < 1.0:
        raise ValueError(f"probability out of range: {p}")
    if p > 0.5:
        return int(round(-100.0 * p / (1.0 - p)))
    return int(round(100.0 * (1.0 - p) / p))


def no_vig(q_a: float, q_b: float) -> tuple[float, float]:
    """Remove the bookmaker margin proportionally: (0.6, 0.45) -> (0.5714, 0.4286)."""
    s = q_a + q_b
    if s <= 0:
        raise ValueError("probabilities must be positive")
    return q_a / s, q_b / s


def _pmf(lam: float, n: int) -> list[float]:
    out = [math.exp(-lam)]
    for k in range(1, n + 1):
        out.append(out[-1] * lam / k)
    return out


MAX_GOALS = 25


def win_probability(lam_home: float, lam_away: float) -> float:
    """P(home wins) with independent Poisson goals; a regulation tie is won by the home team with
    probability lam_home / (lam_home + lam_away) (overtime / shootout)."""
    ph, pa = _pmf(lam_home, MAX_GOALS), _pmf(lam_away, MAX_GOALS)
    cdf_a, acc = [], 0.0
    for x in pa:
        acc += x
        cdf_a.append(acc)
    p_win = sum(ph[i] * cdf_a[i - 1] for i in range(1, MAX_GOALS + 1))
    p_tie = sum(ph[i] * pa[i] for i in range(MAX_GOALS + 1))
    return p_win + p_tie * lam_home / (lam_home + lam_away)


def split_total(total: float, p_home: float) -> tuple[float, float]:
    """Split an expected game total into (home, away) implied team totals.

    Poisson-split: find the home share ``s`` with ``win_probability(s * total, (1 - s) * total)
    == p_home`` (bisection; win_probability rises with s), return ``(s * total, (1 - s) * total)``.
    p_home = 0.5 gives an even split. An approximation (independent Poisson goals); the win
    probability should be no-vig. Shares are clamped to [0.1, 0.9]."""
    if total <= 0:
        raise ValueError("total must be positive")
    lo, hi = 0.1, 0.9
    if win_probability(lo * total, (1 - lo) * total) >= p_home:
        s = lo
    elif win_probability(hi * total, (1 - hi) * total) <= p_home:
        s = hi
    else:
        for _ in range(60):
            mid = (lo + hi) / 2
            if win_probability(mid * total, (1 - mid) * total) < p_home:
                lo = mid
            else:
                hi = mid
        s = (lo + hi) / 2
    return s * total, (1 - s) * total


def _p_over(lam: float, point: float) -> float:
    """P(over ``point`` | no push) for Poisson(lam) goals."""
    pmf = _pmf(lam, MAX_GOALS + 10)
    k = math.floor(point)
    under = sum(pmf[: k + (0 if float(point).is_integer() else 1)])
    push = pmf[k] if float(point).is_integer() else 0.0
    over = max(0.0, 1.0 - under - push)
    return over / (over + under) if over + under > 0 else 0.5


def expected_total(point: float, p_over: float | None) -> float:
    """Expected game goals implied by a total line and its no-vig P(over): the Poisson mean whose
    P(over | no push) matches. Without prices, the line itself. Clamped to [0.5, 15]."""
    if p_over is None:
        return float(point)
    lo, hi = 0.5, 15.0
    if _p_over(lo, point) >= p_over:
        return lo
    if _p_over(hi, point) <= p_over:
        return hi
    for _ in range(60):
        mid = (lo + hi) / 2
        if _p_over(mid, point) < p_over:
            lo = mid
        else:
            hi = mid
    return (lo + hi) / 2


# --------------------------------------------------------------------------- times

def _parse_utc(s: str | None) -> datetime | None:
    if not s:
        return None
    try:
        dt = datetime.fromisoformat(str(s).replace("Z", "+00:00"))
    except ValueError:
        return None
    return dt if dt.tzinfo else dt.replace(tzinfo=timezone.utc)


def _nth_sunday(year: int, month: int, n: int) -> date:
    d = date(year, month, 1)
    d += timedelta(days=(6 - d.weekday()) % 7)
    return d + timedelta(weeks=n - 1)


def eastern_date(dt: datetime) -> date:
    """The NHL game date (US Eastern) of a UTC instant. US DST rules are computed directly
    (second Sunday of March 2:00 to first Sunday of November 2:00) because Windows Pythons often
    lack the tz database."""
    dt = dt.astimezone(timezone.utc)
    y = dt.year
    dst_start = datetime.combine(_nth_sunday(y, 3, 2), datetime.min.time(), timezone.utc) + timedelta(hours=7)
    dst_end = datetime.combine(_nth_sunday(y, 11, 1), datetime.min.time(), timezone.utc) + timedelta(hours=6)
    offset = -4 if dst_start <= dt < dst_end else -5
    return (dt + timedelta(hours=offset)).date()


def _iso_z(dt: datetime) -> str:
    return dt.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


# --------------------------------------------------------------------------- parsing

@dataclass
class GameOdds:
    game_date: date                 # US Eastern date of puck drop (the NHL schedule's date)
    commence_time_utc: str          # ISO 8601, "Z"
    home: str                       # NHL abbreviations
    away: str
    home_ml: int | None             # consensus American moneylines (median implied prob, vig in)
    away_ml: int | None
    total: float | None             # consensus game total line (median point)
    over_price: int | None
    under_price: int | None
    implied_home_total: float | None
    implied_away_total: float | None
    bookmaker_count: int
    home_win_prob: float | None = None   # no-vig, median across books
    expected_total: float | None = None  # expected goals behind the line (median across books)
    event_id: str | None = None
    books: list[dict[str, Any]] = field(default_factory=list)  # per-book rows (compact)

    def to_dict(self, with_books: bool = True) -> dict[str, Any]:
        d = asdict(self)
        d["game_date"] = self.game_date.isoformat()
        if not with_books:
            d.pop("books")
        return d

    @classmethod
    def from_dict(cls, d: Mapping[str, Any]) -> "GameOdds":
        kw = {k: d.get(k) for k in cls.__dataclass_fields__ if k in d}
        kw["game_date"] = date.fromisoformat(str(d["game_date"]))
        kw.setdefault("books", [])
        kw["books"] = kw["books"] or []
        return cls(**kw)


def _book_row(bm: Mapping[str, Any], home_name: str, away_name: str) -> dict[str, Any] | None:
    row: dict[str, Any] = {"book": bm.get("key") or bm.get("title")}
    for m in bm.get("markets") or []:
        outcomes = m.get("outcomes") or []
        if m.get("key") == "h2h":
            by = {o.get("name"): o.get("price") for o in outcomes}
            if by.get(home_name) is not None and by.get(away_name) is not None:
                row["home_ml"], row["away_ml"] = by[home_name], by[away_name]
        elif m.get("key") == "totals":
            over = next((o for o in outcomes if str(o.get("name")).lower() == "over"), None)
            under = next((o for o in outcomes if str(o.get("name")).lower() == "under"), None)
            if over and under and over.get("point") is not None and over.get("point") == under.get("point"):
                row["point"], row["over"], row["under"] = float(over["point"]), over.get("price"), under.get("price")
    return row if len(row) > 1 else None


def _median(xs: Iterable[float | None]) -> float | None:
    v = [float(x) for x in xs if x is not None]
    return statistics.median(v) if v else None


def _safe_prob(price: Any) -> float | None:
    try:
        return american_to_prob(float(price))
    except (TypeError, ValueError):
        return None


def consensus(books: list[dict[str, Any]]) -> dict[str, Any]:
    """Median consensus over per-book rows (``home_ml``/``away_ml``, ``point``/``over``/``under``)."""
    out: dict[str, Any] = {"home_ml": None, "away_ml": None, "total": None, "over_price": None,
                           "under_price": None, "home_win_prob": None, "expected_total": None}
    qh, qa, pw = [], [], []
    for b in books:
        h, a = _safe_prob(b.get("home_ml")), _safe_prob(b.get("away_ml"))
        if h is None or a is None:
            continue
        qh.append(h)
        qa.append(a)
        pw.append(no_vig(h, a)[0])
    if pw:
        out["home_ml"], out["away_ml"] = prob_to_american(_median(qh)), prob_to_american(_median(qa))
        out["home_win_prob"] = round(_median(pw), 4)
    tot = [b for b in books if b.get("point") is not None]
    if tot:
        line = _median(b["point"] for b in tot)
        out["total"] = line
        nearest = min(abs(b["point"] - line) for b in tot)
        at_line = [b for b in tot if abs(b["point"] - line) == nearest]
        qo = [q for q in (_safe_prob(b.get("over")) for b in at_line) if q is not None]
        qu = [q for q in (_safe_prob(b.get("under")) for b in at_line) if q is not None]
        if qo and qu:
            out["over_price"], out["under_price"] = prob_to_american(_median(qo)), prob_to_american(_median(qu))
        lams = []
        for b in tot:
            o, u = _safe_prob(b.get("over")), _safe_prob(b.get("under"))
            lams.append(expected_total(b["point"], no_vig(o, u)[0] if o is not None and u is not None else None))
        out["expected_total"] = round(_median(lams), 3)
    return out


def parse_events(events: Iterable[Mapping[str, Any]], now: datetime | None = None
                 ) -> tuple[list[GameOdds], list[str]]:
    """The /odds response -> GameOdds (games already started at ``now`` are left out: in-play
    prices are not pre-game context). Returns (games sorted by puck drop, warnings)."""
    games: list[GameOdds] = []
    warnings: list[str] = []
    for ev in events or []:
        start = _parse_utc(ev.get("commence_time"))
        home, away = team_abbrev(ev.get("home_team")), team_abbrev(ev.get("away_team"))
        if start is None:
            warnings.append(f"odds: event {ev.get('id')} without a commence_time")
            continue
        if home is None or away is None:
            warnings.append(f"odds: unknown team name {ev.get('home_team')!r} / {ev.get('away_team')!r}")
            continue
        if now is not None and start <= now:
            continue
        books = [r for r in (_book_row(bm, ev["home_team"], ev["away_team"]) for bm in ev.get("bookmakers") or [])
                 if r is not None]
        c = consensus(books)
        ih = ia = None
        if c["expected_total"] and c["home_win_prob"] is not None:
            h, a = split_total(c["expected_total"], c["home_win_prob"])
            ih, ia = round(h, 2), round(a, 2)
        games.append(GameOdds(
            game_date=eastern_date(start), commence_time_utc=_iso_z(start), home=home, away=away,
            home_ml=c["home_ml"], away_ml=c["away_ml"], total=c["total"], over_price=c["over_price"],
            under_price=c["under_price"], implied_home_total=ih, implied_away_total=ia,
            bookmaker_count=len(books), home_win_prob=c["home_win_prob"], expected_total=c["expected_total"],
            event_id=ev.get("id"), books=books))
    games.sort(key=lambda g: (g.commence_time_utc, g.home))
    return games, warnings


def parse_quota(headers: Mapping[str, Any] | None) -> dict[str, int] | None:
    """``x-requests-remaining`` / ``-used`` / ``-last`` response headers -> {remaining, used, last}."""
    if not headers:
        return None
    h = {str(k).lower(): v for k, v in headers.items()}
    out: dict[str, int] = {}
    for name in ("remaining", "used", "last"):
        v = h.get(f"x-requests-{name}")
        if v is None:
            continue
        try:
            out[name] = int(float(str(v).strip()))
        except ValueError:
            continue
    return out or None


# --------------------------------------------------------------------------- client

_no_key_logged = False


def _redact(text: str, secret: str | None) -> str:
    return text.replace(secret, "***") if secret else text


def default_fetch_json(url: str, params: dict | None = None) -> tuple[Any, dict[str, str]]:
    """GET -> (json, headers). Raises OddsError with a key-free message."""
    secret = (params or {}).get("apiKey")
    try:
        r = httpx.get(url, params=params, timeout=20.0, headers={"User-Agent": "fantasy-manager/0.1"})
    except httpx.HTTPError as e:
        raise OddsError(_redact(f"{type(e).__name__}: {e}", secret)) from None
    headers = dict(r.headers)
    if not 200 <= r.status_code < 300:
        detail = ""
        try:
            body = r.json()
            detail = str(body.get("message") or body.get("error_code") or "") if isinstance(body, dict) else ""
        except ValueError:
            pass
        hint = {401: " (invalid ODDS_API_KEY?)", 429: " (quota or rate limit reached)"}.get(r.status_code, "")
        msg = f"The Odds API returned HTTP {r.status_code}{hint}" + (f": {detail[:200]}" if detail else "")
        raise OddsError(_redact(msg, secret), status_code=r.status_code, headers=headers)
    return r.json(), headers


class OddsClient:
    """NHL odds from The Odds API. Without a key ``available`` is False and every call is a no-op.

    With ``data_dir`` the raw response is cached in ``<data_dir>/odds_cache.json`` (without the
    key) and reused for ``ttl_hours`` (20h), so at most one 2-credit call happens per day.
    ``offline`` serves only that cache."""

    def __init__(self, api_key: str | None, fetch_json: FetchJson | None = None, region: str = "us",
                 data_dir: Path | str | None = None, offline: bool = False, ttl_hours: float = CACHE_TTL_HOURS,
                 clock: Callable[[], datetime] | None = None):
        self._key = (api_key or "").strip() or None
        self.fetch_json = fetch_json or default_fetch_json
        self.region = (region or "us").strip() or "us"
        self.data_dir = Path(data_dir) if data_dir is not None else None
        self.offline = offline
        self.ttl = timedelta(hours=ttl_hours)
        self.clock = clock or (lambda: datetime.now(timezone.utc))
        self.quota: dict[str, int] | None = None
        self.fetched_at: datetime | None = None
        self.from_cache = False
        self.calls = 0
        self.warnings: list[str] = []
        self._mem: dict[str, Any] | None = None

    def __repr__(self) -> str:  # never show the key
        return f"OddsClient(available={self.available}, region={self.region!r})"

    @property
    def available(self) -> bool:
        return self._key is not None

    @classmethod
    def from_settings(cls, settings: Any, fetch_json: FetchJson | None = None) -> "OddsClient":
        from ..config import secret_value
        return cls(secret_value(getattr(settings, "odds_api_key", None)) or None, fetch_json=fetch_json,
                   region=getattr(settings, "odds_region", "us") or "us",
                   data_dir=getattr(settings, "fm_data_dir", None), offline=bool(getattr(settings, "fm_offline", False)))

    # -- cache ---------------------------------------------------------------
    @property
    def cache_path(self) -> Path | None:
        return self.data_dir / CACHE_FILE if self.data_dir is not None else None

    def _read_cache(self) -> dict[str, Any] | None:
        if self._mem is not None:
            return self._mem
        p = self.cache_path
        if p is None or not p.exists():
            return None
        try:
            d = json.loads(p.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return None
        return d if isinstance(d, dict) and d.get("region") == self.region else None

    def _write_cache(self, payload: dict[str, Any]) -> None:
        self._mem = payload
        p = self.cache_path
        if p is None:
            return
        p.parent.mkdir(parents=True, exist_ok=True)
        tmp = p.with_suffix(".tmp")
        tmp.write_text(json.dumps(payload, separators=(",", ":")), encoding="utf-8")
        tmp.replace(p)

    def raw_events(self) -> list[dict[str, Any]]:
        """The raw /odds events: from the cache when fresh (or offline), else one API call."""
        global _no_key_logged
        if not self.available:
            if not _no_key_logged:
                log.info("ODDS_API_KEY not set: betting odds are disabled")
                _no_key_logged = True
            return []
        now = self.clock()
        cached = self._read_cache()
        if cached is not None:
            at = _parse_utc(cached.get("fetched_at"))
            if at is not None and (self.offline or now - at < self.ttl):
                self.from_cache, self.fetched_at = True, at
                self.quota = cached.get("quota")
                return list(cached.get("events") or [])
        if self.offline:
            raise OddsError("offline and no cached odds")
        params = {"apiKey": self._key, "regions": self.region, "markets": MARKETS, "oddsFormat": "american",
                  "dateFormat": "iso"}
        self.calls += 1
        try:
            res = self.fetch_json(ODDS_URL, params)
        except OddsError as e:
            self.quota = parse_quota(e.headers) or self.quota
            raise
        except Exception as e:  # noqa: BLE001 - re-raise without anything that could carry the key
            raise OddsError(_redact(f"{type(e).__name__}: {e}", self._key)) from None
        body, headers = res if isinstance(res, tuple) else (res, {})
        if not isinstance(body, list):
            raise OddsError("unexpected odds response (expected a list of events)")
        self.quota = parse_quota(headers)
        self.fetched_at, self.from_cache = now, False
        self._write_cache({"fetched_at": _iso_z(now), "region": self.region, "markets": MARKETS,
                           "quota": self.quota, "events": body})
        return body

    def nhl_odds(self) -> list[GameOdds]:
        """Upcoming NHL games with consensus prices and implied team totals ([] without a key)."""
        events = self.raw_events()
        if not events:
            return []
        games, warnings = parse_events(events, now=self.clock())
        self.warnings.extend(warnings)
        return games


# --------------------------------------------------------------------------- archive

def odds_archive_path(data_dir: Path | str, day: date) -> Path:
    return Path(data_dir) / "archive" / f"odds-{day.isoformat()}.json"


def archive_odds(games: list[GameOdds], data_dir: Path | str, day: date, *, fetched_at: datetime | None = None,
                 quota: Mapping[str, int] | None = None, region: str = "us", force: bool = False
                 ) -> tuple[Path, str]:
    """Write ``<data_dir>/archive/odds-<day>.json`` once per day. Returns (path, "written" |
    "exists"); an existing file is kept unless ``force``."""
    path = odds_archive_path(data_dir, day)
    if path.exists() and not force:
        return path, "exists"
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = {"kind": "odds", "version": ARCHIVE_VERSION, "date": day.isoformat(), "source": "the-odds-api",
               "region": region, "markets": MARKETS,
               "fetched_at": _iso_z(fetched_at) if fetched_at else None, "quota": dict(quota) if quota else None,
               "games": [g.to_dict() for g in games]}
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps(payload, separators=(",", ":")), encoding="utf-8")
    tmp.replace(path)
    return path, "written"


def load_odds(data_dir: Path | str, day: date) -> dict[str, Any] | None:
    """The odds archive for ``day`` (the header keys plus ``games`` as GameOdds), or None."""
    path = odds_archive_path(data_dir, day)
    if not path.exists():
        return None
    try:
        d = json.loads(path.read_text(encoding="utf-8"))
        d["games"] = [GameOdds.from_dict(g) for g in d.get("games") or []]
    except (OSError, ValueError, KeyError, TypeError):
        return None
    return d


def team_context(day: date, data_dir: Path | str | None = None, lookback_days: int = LOOKBACK_DAYS
                 ) -> dict[str, dict[str, Any]]:
    """Market context for the teams playing on ``day`` (US Eastern game date):
    ``{abbrev: {implied_total, win_prob, opponent, home, game_total, opp_implied_total, as_of}}``.

    Uses the most recent archive on or before ``day`` (up to ``lookback_days`` back) that lists
    that day's games, since each daily snapshot also carries the next few days. {} when nothing is
    archived. ``data_dir`` defaults to FM_DATA_DIR."""
    if data_dir is None:
        from ..config import get_settings
        data_dir = get_settings().fm_data_dir
    for back in range(lookback_days + 1):
        snap_day = day - timedelta(days=back)
        snap = load_odds(data_dir, snap_day)
        if not snap:
            continue
        games = [g for g in snap["games"] if g.game_date == day]
        if not games:
            continue
        out: dict[str, dict[str, Any]] = {}
        for g in games:
            p = g.home_win_prob
            for team, opp, home in ((g.home, g.away, True), (g.away, g.home, False)):
                out[team] = {
                    "implied_total": g.implied_home_total if home else g.implied_away_total,
                    "opp_implied_total": g.implied_away_total if home else g.implied_home_total,
                    "win_prob": None if p is None else (p if home else round(1 - p, 4)),
                    "opponent": opp, "home": home, "game_total": g.total, "as_of": snap_day.isoformat()}
        return out
    return {}
