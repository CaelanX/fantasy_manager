"""Rookie / unproven-player model: value the players whose NHL stats cannot carry a projection.

A rookie's league projection (Fantrax / ESPN) is often the only number the plain model has, and
it is a guess. This module adds the other evidence a human would use and blends it, Bayesian
style, as votes on points per game (each vote carries a weight in "effective games"):

* **Baseline** (vote weight ``W_BASELINE`` = 40, + his NHL GP when the baseline includes NHL
  history): what ``valuate.baseline_rates`` produced - the league projection shrunk toward the
  positional mean, blended with any NHL history. Kept as one vote, never thrown away.
* **NHLe** (:func:`nhle_estimate`, weight up to ``W_NHLE`` = 40 x confidence): the most recent two
  seasons (>= 10 GP in leagues of ``valuation/nhle_factors.json``) weighted 2:1 and by GP; each
  league-season's PTS/GP x the league's NHL-equivalency factor x an age bump (+8% per year
  younger than the league's norm age, at most 3 years; NHL rows use the fitted year-over-year
  age factor instead). Confidence = min(1, GP / 60) x the leagues' reliability. The factors are
  APPROXIMATE practitioner midpoints (AHL 0.455, KHL 0.775, SHL 0.575, Liiga 0.475, NCAA 0.375,
  OHL / WHL 0.275, QMJHL 0.25, Czechia 0.5, NL(A) 0.45, ...), kept as data so they can be refit.
  NHL rows are skipped when the baseline already carries his NHL history (no double count).
* **Pedigree** (weight ``W_PEDIGREE`` = 15, fading with age 23 -> 25 and 200 career GP): a draft
  tier prior on PTS/GP, APPROXIMATE: picks 1-3 0.75, 4-10 0.60, 11-32 0.45, round 2 0.35, later
  0.30 (x0.7 for defensemen). Undrafted / unknown players get no pedigree vote.
* **Preseason**: the existing small vote (``blend.preseason_weight``: 3 GP -> ~6%), applied by
  ``valuate._rates_for`` after this blend, unchanged.

Posterior PTS/GP = sum(w_i x_i) / sum(w_i). With a baseline the offensive rates (G, A, PTS, PPG,
PPA, PPP, SHG, SHA, SHP, GWG, SOG) are scaled by posterior / baseline PTS and the rest (hits,
blocks, PIM, faceoffs...) kept; without one they are built from the prior (G = PTS x goal share,
SOG from goals at a positional shooting %, PPP = PTS x the positional PP share, other stats from the
positional mean). A baseline that is the only vote comes back unchanged.

Role signals then shift the estimate:

* Top line / first pair / PP1 confirmed by Daily Faceoff (``Player.line`` f1 / d1,
  ``Player.pp_unit`` pp1) or by recent news (``providers.news_roles``, <= 21 days, confidence >=
  0.6, >= 0.7 for LLM labels): x1.15 on the offensive rates (once, not stacked), GP expectation at least 0.95. PP2 / a
  second line / "top-six": x1.05. A depth line (DFO f4 / d3, or news "fourth line") without a
  positive signal: x0.92.
* Expected games share (``gp_expectation``, 0..1, relative to an established regular): 1.0 when
  he is confirmed in the NHL lineup (games this season, a DFO line or an NHL-roster news signal),
  0.9 on an NHL team as a top-10 pick, 0.75 on an NHL team otherwise, 0.3 without an NHL team;
  blended with the league projection's GP / 75 when there is one (50/50, or 75/25 once he is
  confirmed in the lineup: the roster doubt the projection priced in is then resolved). The latest roster-status
  news wins: "assigned to the AHL" -> 0.05, "returned to junior / loaned to Europe" -> 0.02 for the
  horizon (until a newer recall). A scratch x0.85. Games played this season pull it toward 1.0
  (fully at 20 GP) unless the latest news is a demotion.

``valuate.valuate_league`` multiplies an unproven player's season (and week) value by the games
share. Established players are never touched, and the model only engages when there is rookie
evidence (pedigree loaded, league history registered or a role signal), so a bare test player is
valued exactly as before. Every step is a Reason: NHLE, PEDIGREE_PRIOR, ROOKIE_BLEND (old vs new),
ROLE_DFO, ROLE_NEWS (with the quote), GP_EXPECTATION (PRESEASON comes from ``valuate``).
"""
from __future__ import annotations

from datetime import date, datetime, timezone
from typing import Any, Iterable, Mapping, Sequence

from pydantic import BaseModel, Field

from ..models import Player, Reason
from ..providers.nhl import NHLE_FACTORS, LeagueSeason
from ..providers.preseason_enrich import is_unproven as _preseason_is_unproven
from .params import age_factor

# vote weights, in effective games
W_BASELINE = 40.0
W_NHLE = 40.0
W_PEDIGREE = 15.0
NHLE_FULL_GP = 60            # GP over the two seasons for full NHLe confidence
NHLE_MIN_SEASON_GP = 10      # a season needs this many GP in table leagues to count
NHLE_SEASON_WEIGHTS = (2.0, 1.0)

# Draft-tier prior on rookie PTS/GP (APPROXIMATE; forwards; defensemen x PEDIGREE_D_MULT)
PEDIGREE_TIERS: tuple[tuple[int, float, str], ...] = ((3, 0.75, "top-3 pick"), (10, 0.60, "top-10 pick"),
                                                      (32, 0.45, "1st-round pick"), (64, 0.35, "2nd-round pick"))
PEDIGREE_LATER = 0.30
PEDIGREE_D_MULT = 0.7
PEDIGREE_MAX_AGE, PEDIGREE_FADE_AGE, PEDIGREE_PROVEN_GP = 23.0, 25.0, 200

# goal share of points, shooting %, PP share of points (positional defaults)
DEFAULT_G_SHARE = {"F": 0.40, "D": 0.22}
G_SHARE_K = 40.0             # points of shrinkage of the observed goal share
DEFAULT_SH_PCT = {"F": 0.11, "D": 0.05}
DEFAULT_PPP_SHARE = {"F": 0.28, "D": 0.35}

OFFENSIVE = ("G", "A", "PTS", "PPG", "PPA", "PPP", "SHG", "SHA", "SHP", "GWG", "SOG")

# role signals
SIGNAL_MAX_AGE_DAYS = 21
SIGNAL_MIN_CONF = 0.6
SIGNAL_MIN_CONF_LLM = 0.7        # LLM labels need more confidence (they come from the ambiguous blurbs)
ROLE_BOOST = 1.15
ROLE_MINOR_BOOST = 1.05
DEPTH_PENALTY = 0.92
GP_CONFIRMED, GP_TOP10_ON_TEAM, GP_ON_TEAM, GP_NO_TEAM = 1.0, 0.9, 0.75, 0.3
GP_TOP_ROLE = 0.95
GP_AHL, GP_JUNIOR = 0.05, 0.02
SCRATCH_MULT = 0.85
PROJ_FULL_GP = 75.0
PROJ_GP_WEIGHT = 0.5             # weight of the projection's GP share in the games share...
PROJ_GP_WEIGHT_CONFIRMED = 0.25  # ...once he is confirmed in the lineup (the roster doubt it priced is resolved)
GP_FADE_SEASON_GP = 20


def is_unproven(p: Player) -> bool:
    """Career NHL GP known and < 82, or no prior season (N-1..N-3) with >= 20 GP
    (the ``providers.preseason_enrich`` definition)."""
    return _preseason_is_unproven(p)


def position_group(p: Player) -> str:
    if "D" in p.positions and not set(p.positions) & {"C", "LW", "RW", "F"}:
        return "D"
    return "F"


class RookiePrior(BaseModel):
    """Evidence about an unproven player that does not come from his NHL stat lines."""
    pts_pg_nhle: float | None = None          # NHL-equivalent PTS/GP (age-adjusted), None without rows
    nhle_confidence: float = 0.0
    nhle_gp: int = 0
    nhle_rows: list[dict[str, Any]] = Field(default_factory=list)   # the league-seasons used
    g_share: float | None = None
    sog_pg_est: float | None = None
    pedigree_pts_pg: float | None = None
    pedigree_weight: float = 0.0
    pedigree_label: str | None = None
    gp_expectation: float = 1.0               # before role signals (see rookie_value)
    confidence: float = 0.0                   # 0..1 over the prior's votes
    reasons: list[Reason] = Field(default_factory=list)


class RookieEstimate(BaseModel):
    rates: dict[str, float] = Field(default_factory=dict)       # per game, before preseason
    gp_expectation: float = 1.0               # season games share (0..1)
    week_share: float = 1.0                   # near-term games share (roster status / scratches only)
    confidence: float = 0.0                   # 0..1: how much evidence stands behind the rates
    pts_pg: float | None = None               # posterior PTS/GP (before the role multiplier)
    role_mult: float = 1.0
    baseline_pts_pg: float | None = None
    votes: list[dict[str, Any]] = Field(default_factory=list)   # [{name, pts_pg, weight}]
    signals: list[dict[str, Any]] = Field(default_factory=list)  # role signals used (DFO + news)
    prior: RookiePrior | None = None
    history: list[dict[str, Any]] = Field(default_factory=list)  # league-season rows for display
    reasons: list[Reason] = Field(default_factory=list)


# --------------------------------------------------------------------------- NHLe

def _league(factors: Mapping[str, Any], league: str) -> Mapping[str, Any] | None:
    return (factors.get("leagues") or {}).get(league)


def age_multiplier(league: str, age: float | None, factors: Mapping[str, Any] | None = None,
                   group: str = "F") -> float:
    """Age bump of one league-season: +per_year for every year under the league class's norm age
    (at most max_years); NHL rows use the fitted year-over-year age factor. 1.0 when unknown."""
    factors = factors or NHLE_FACTORS
    info = _league(factors, league)
    if info is None or age is None:
        return 1.0
    if info.get("class") == "nhl":
        return age_factor(group, age)
    adj = factors.get("age_adjustment") or {}
    norm = (factors.get("norm_age") or {}).get(info.get("class"), 21)
    years = min(float(adj.get("max_years", 3)), max(0.0, float(norm) - float(age)))
    return 1.0 + float(adj.get("per_year", 0.08)) * years


def nhle_estimate(history: Sequence[LeagueSeason], factors: Mapping[str, Any] | None = None, *,
                  group: str = "F", include_nhl: bool = True, before_season: int | None = None
                  ) -> dict[str, Any] | None:
    """NHL-equivalent PTS/GP from league history (module docstring), or None without two usable
    seasons' worth of rows. ``before_season`` (e.g. 20262027) drops NHL rows of that season and
    later (the in-season model already has them)."""
    factors = factors or NHLE_FACTORS
    by_season: dict[int, list[tuple[LeagueSeason, Mapping[str, Any]]]] = {}
    for row in history:
        info = _league(factors, row.league)
        if info is None or row.gp <= 0:
            continue
        if row.league == "NHL" and (not include_nhl or (before_season is not None and row.season >= before_season)):
            continue
        by_season.setdefault(row.season, []).append((row, info))
    seasons = [s for s in sorted(by_season, reverse=True)
               if sum(r.gp for r, _ in by_season[s]) >= NHLE_MIN_SEASON_GP][:len(NHLE_SEASON_WEIGHTS)]
    if not seasons:
        return None
    num = den = gp_total = rel_num = 0.0
    g_raw = pts_raw = 0
    rows: list[dict[str, Any]] = []
    for sw, season in zip(NHLE_SEASON_WEIGHTS, seasons):
        for row, info in by_season[season]:
            f = float(info.get("factor", 0.0))
            am = age_multiplier(row.league, row.age_at_season, factors, group)
            nhle = row.pts_per_game * f * am
            w = sw * row.gp
            num += w * nhle
            den += w
            gp_total += row.gp
            rel_num += row.gp * float(info.get("reliability", 0.7))
            g_raw += row.g
            pts_raw += row.pts
            rows.append({"season": row.season, "league": row.league, "team": row.team, "gp": row.gp, "g": row.g,
                         "a": row.a, "pts": row.pts, "pts_pg": round(row.pts_per_game, 3), "factor": f,
                         "age": row.age_at_season, "age_mult": round(am, 3), "nhle_pts_pg": round(nhle, 3),
                         "weight": sw})
    if den <= 0:
        return None
    reliability = rel_num / gp_total if gp_total else 0.0
    conf = min(1.0, gp_total / NHLE_FULL_GP) * reliability
    return {"pts_pg": num / den, "confidence": conf, "gp": int(gp_total), "rows": rows,
            "g": g_raw, "pts": pts_raw}


# --------------------------------------------------------------------------- pedigree

def pedigree_prior(p: Player, age: float | None) -> tuple[float, float, str] | None:
    """(PTS/GP prior, vote weight, label) from the draft slot; None when undrafted / unknown or
    the prior has faded (age >= 25 or 200+ career NHL GP)."""
    ov = p.draft_overall
    if ov is None:
        return None
    pts, label = PEDIGREE_LATER, "later-round pick"
    for cut, val, lab in PEDIGREE_TIERS:
        if ov <= cut:
            pts, label = val, lab
            break
    if position_group(p) == "D":
        pts *= PEDIGREE_D_MULT
    f_age = 1.0
    if age is not None and age > PEDIGREE_MAX_AGE:
        f_age = max(0.0, (PEDIGREE_FADE_AGE - age) / (PEDIGREE_FADE_AGE - PEDIGREE_MAX_AGE))
    f_gp = max(0.0, 1.0 - (p.career_gp or 0) / PEDIGREE_PROVEN_GP)
    w = W_PEDIGREE * f_age * f_gp
    if w <= 0:
        return None
    year = f" {p.draft_year}" if p.draft_year else ""
    return pts, w, f"#{ov} overall{year} ({label})"


def rookie_prior(player: Player, history: Sequence[LeagueSeason] | None, factors: Mapping[str, Any] | None = None,
                 *, age: float | None = None, include_nhl: bool = True, before_season: int | None = None
                 ) -> RookiePrior:
    """NHLe + pedigree evidence for ``player`` (module docstring). ``age`` = age on Oct 1 of the
    season being projected (``valuate.season_age``)."""
    factors = factors or NHLE_FACTORS
    group = position_group(player)
    prior = RookiePrior()
    est = nhle_estimate(history or [], factors, group=group, include_nhl=include_nhl, before_season=before_season)
    if est is not None:
        prior.pts_pg_nhle = est["pts_pg"]
        prior.nhle_confidence = est["confidence"]
        prior.nhle_gp = est["gp"]
        prior.nhle_rows = est["rows"]
        # observed goal share of the rows used, shrunk toward the positional default (k = 40 points)
        prior.g_share = (est["g"] + DEFAULT_G_SHARE[group] * G_SHARE_K) / (est["pts"] + G_SHARE_K)
        prior.sog_pg_est = prior.pts_pg_nhle * prior.g_share / DEFAULT_SH_PCT[group]
        seasons = sorted({r["season"] for r in est["rows"]}, reverse=True)
        parts = []
        for s in seasons:
            rs = [r for r in est["rows"] if r["season"] == s]
            parts.append(f"{s // 10000}-{str(s % 10000)[2:]} " + " + ".join(
                f"{r['league']} {r['gp']} GP {r['pts']} PTS x{r['factor']:g}"
                + (f" x{r['age_mult']:.2f} age" if abs(r["age_mult"] - 1.0) > 1e-3 else "") for r in rs))
        prior.reasons.append(Reason(
            code="NHLE",
            text=f"NHL equivalency {prior.pts_pg_nhle:.2f} PTS/GP from {'; '.join(parts)} (seasons weighted 2:1 by "
                 f"GP; approximate league factors, {prior.nhle_confidence:.0%} confidence)",
            value=round(prior.pts_pg_nhle, 4), baseline=round(prior.nhle_confidence, 3)))
    ped = pedigree_prior(player, age)
    if ped is not None:
        prior.pedigree_pts_pg, prior.pedigree_weight, prior.pedigree_label = ped
        prior.reasons.append(Reason(
            code="PEDIGREE_PRIOR",
            text=f"Draft pedigree {ped[2]}: prior {ped[0]:.2f} PTS/GP (approximate draft-tier rookie rates), "
                 f"vote weight {ped[1]:.0f} games",
            value=round(ped[0], 4), baseline=round(ped[1], 2)))
    w = (W_NHLE * prior.nhle_confidence if prior.pts_pg_nhle is not None else 0.0) + prior.pedigree_weight
    prior.confidence = round(w / (w + W_BASELINE), 3) if w > 0 else 0.0
    return prior


# --------------------------------------------------------------------------- role signals

def _as_dt(d: Any) -> datetime | None:
    if d is None:
        return None
    if isinstance(d, datetime):
        return d if d.tzinfo else d.replace(tzinfo=timezone.utc)
    if isinstance(d, date):
        return datetime(d.year, d.month, d.day, tzinfo=timezone.utc)
    return None


def dfo_signals(p: Player) -> list[dict[str, Any]]:
    """Role signals from the Daily Faceoff lineup fields on the Player."""
    out: list[dict[str, Any]] = []
    line = (p.line or "").lower()
    pp = (p.pp_unit or "").lower()
    src = {"source": "dfo", "origin": "dfo", "published": None, "confidence": 0.9}
    if line:
        out.append({**src, "kind": "nhl_roster", "direction": 1, "quote": f"Daily Faceoff lineup: {line.upper()}",
                    "confidence": 0.8})
    if line == "f1":
        out.append({**src, "kind": "top_line", "direction": 1, "quote": "Daily Faceoff: first line (F1)"})
    elif line == "d1":
        out.append({**src, "kind": "first_line_pairing", "direction": 1, "quote": "Daily Faceoff: top pair (D1)"})
    elif line in ("f2", "d2"):
        out.append({**src, "kind": "extended_role", "direction": 1, "quote": f"Daily Faceoff: {line.upper()}",
                    "confidence": 0.7})
    elif line in ("f4", "d3", "d4"):
        out.append({**src, "kind": "top_line", "direction": -1, "quote": f"Daily Faceoff: {line.upper()}",
                    "confidence": 0.7})
    if pp == "pp1":
        out.append({**src, "kind": "pp1", "direction": 1, "quote": "Daily Faceoff: first power-play unit (PP1)"})
    elif pp == "pp2":
        out.append({**src, "kind": "pp2", "direction": 1, "quote": "Daily Faceoff: second power-play unit (PP2)",
                    "confidence": 0.8})
    if (p.line_change or "").lower() == "scratched":
        out.append({**src, "kind": "scratched", "direction": -1, "quote": "Daily Faceoff: dropped out of the lineup",
                    "confidence": 0.7})
    return out


def _signal_dict(s: Any) -> dict[str, Any]:
    if isinstance(s, Mapping):
        return dict(s)
    return {"kind": s.kind, "direction": s.direction, "confidence": s.confidence, "quote": s.quote,
            "published": s.published, "source": s.source, "origin": getattr(s, "origin", "rules"),
            "ambiguous": getattr(s, "ambiguous", False)}


def usable_news(signals: Iterable[Any], as_of: date | None, max_age_days: int = SIGNAL_MAX_AGE_DAYS
                ) -> list[dict[str, Any]]:
    """News signals recent enough (<= ``max_age_days`` before ``as_of``) and confident enough
    (>= 0.6, >= 0.7 for LLM labels; not ambiguous, not direction 0) to move a valuation; newest first."""
    out = []
    ref = _as_dt(as_of) if as_of else None
    for s in signals or []:
        d = _signal_dict(s)
        floor = SIGNAL_MIN_CONF_LLM if d.get("origin") == "llm" else SIGNAL_MIN_CONF
        if d.get("ambiguous") or float(d.get("confidence") or 0) < floor or d.get("direction") == 0:
            continue
        pub = _as_dt(d.get("published"))
        if ref is not None and pub is not None:
            age = (ref - pub).total_seconds() / 86400.0
            if age > max_age_days + 1 or age < -2:
                continue
        out.append(d)
    epoch = datetime.min.replace(tzinfo=timezone.utc)
    out.sort(key=lambda d: _as_dt(d.get("published")) or epoch, reverse=True)
    return out


def _fmt_signal(d: Mapping[str, Any]) -> str:
    pub = _as_dt(d.get("published"))
    when = f", {pub.strftime('%Y-%m-%d')}" if pub else ""
    return f"\"{d.get('quote')}\" ({d.get('source') or 'news'}{when})"


# --------------------------------------------------------------------------- the blend

def _pts(rates: Mapping[str, float]) -> float:
    if "PTS" in rates:
        return float(rates["PTS"])
    return float(rates.get("G", 0.0)) + float(rates.get("A", 0.0))


MAX_SANE_PTS_PG = 2.0


def comparable_baseline(rates: Mapping[str, float]) -> bool:
    """True when a baseline can be put on the priors' PTS/GP scale: it carries points (PTS, or
    both G and A) at a plausible NHL rate (< 2 PTS/GP). Otherwise the votes are not blended and
    the baseline is kept as is (only the role multiplier and the games share apply)."""
    if not rates or not ("PTS" in rates or ("G" in rates and "A" in rates)):
        return False
    return 0.0 < _pts(rates) < MAX_SANE_PTS_PG


def _from_prior(pts: float, prior: RookiePrior | None, group: str, mean: Mapping[str, float]) -> dict[str, float]:
    """Per-game rates built from a PTS/GP estimate (no baseline to scale)."""
    g_share = prior.g_share if prior and prior.g_share is not None else DEFAULT_G_SHARE[group]
    out = {k: float(v) for k, v in mean.items() if k not in OFFENSIVE}
    g = pts * g_share
    mean_pts = _pts(mean) if mean else 0.0
    ppp_share = (float(mean["PPP"]) / mean_pts) if mean and "PPP" in mean and mean_pts > 0 else DEFAULT_PPP_SHARE[group]
    ppp = pts * ppp_share
    out.update({"PTS": pts, "G": g, "A": pts - g, "PPP": ppp, "PPG": ppp * g_share, "PPA": ppp * (1 - g_share),
                "SOG": g / DEFAULT_SH_PCT[group]})
    if mean and mean_pts > 0:
        for k in ("SHG", "SHA", "SHP", "GWG"):
            if k in mean:
                out[k] = float(mean[k]) * pts / mean_pts
    return out


def _scale(rates: Mapping[str, float], s: float) -> dict[str, float]:
    return {k: (float(v) * s if k in OFFENSIVE else float(v)) for k, v in rates.items()}


def rookie_value(player: Player, prior: RookiePrior | None, provider_projection: Mapping[str, float] | None,
                 preseason: Any = None, deployment: Any = None, news_signals: Iterable[Any] | None = None, *,
                 baseline_gp: float = 0.0, projection_gp: int | None = None, mean: Mapping[str, float] | None = None,
                 season_gp: int = 0, as_of: date | None = None, scoring: Any = None) -> RookieEstimate:
    """Blend the votes (module docstring) into per-game rates and a games share.

    ``provider_projection``: the baseline rates (``valuate.baseline_rates``; {} / None without one).
    ``baseline_gp``: NHL history GP inside that baseline (adds to its vote weight). ``projection_gp``:
    the league projection's GP (games-share evidence). ``preseason`` (a PreseasonLine) is only
    reported here: ``valuate`` blends it after this function, exactly as for any unproven player.
    ``deployment``: the Player (Daily Faceoff fields) or None. ``news_signals``: RoleSignals /
    dicts for this player. ``scoring`` (optional) prints FPG in the ROOKIE_BLEND reason."""
    group = position_group(player)
    base = dict(provider_projection or {})
    mean = dict(mean or {})
    est = RookieEstimate(prior=prior)
    reasons: list[Reason] = list(prior.reasons) if prior else []
    votes: list[dict[str, Any]] = []
    base_pts = _pts(base) if base else 0.0
    if base and not comparable_baseline(base):
        if prior is not None and (prior.pts_pg_nhle is not None or prior.pedigree_pts_pg is not None):
            reasons = [Reason(code="ROOKIE_BLEND", text="Rookie priors not blended: the baseline carries no "
                                                        "comparable PTS/GP (needs PTS, or G and A, below 2.0)")]
        prior = None                              # the priors cannot be put on this baseline's scale
    if base and base_pts > 0:
        votes.append({"name": "baseline", "pts_pg": base_pts, "weight": W_BASELINE + max(0.0, baseline_gp)})
    if prior is not None and prior.pts_pg_nhle is not None and prior.nhle_confidence > 0:
        votes.append({"name": "NHLe", "pts_pg": prior.pts_pg_nhle, "weight": W_NHLE * prior.nhle_confidence})
    if prior is not None and prior.pedigree_pts_pg is not None and prior.pedigree_weight > 0:
        votes.append({"name": "pedigree", "pts_pg": prior.pedigree_pts_pg, "weight": prior.pedigree_weight})
    est.votes = [{**v, "pts_pg": round(v["pts_pg"], 4), "weight": round(v["weight"], 2)} for v in votes]
    est.baseline_pts_pg = base_pts if base_pts > 0 else None
    total_w = sum(v["weight"] for v in votes)
    if not votes:
        rates = dict(base)
        post = base_pts if base_pts > 0 else None
    elif len(votes) == 1 and votes[0]["name"] == "baseline":
        rates, post = dict(base), base_pts
    else:
        post = sum(v["weight"] * v["pts_pg"] for v in votes) / total_w
        rates = _scale(base, post / base_pts) if base and base_pts > 0 else _from_prior(post, prior, group, mean)
        vtxt = " + ".join(f"{v['name']} {v['pts_pg']:.2f} (w {v['weight']:.0f})" for v in votes)
        old = f"{base_pts:.2f}" if base_pts > 0 else "none"
        fpg = ""
        if scoring is not None:
            try:
                fpg = f"; {scoring.value(base):.2f} -> {scoring.value(rates):.2f} FPG" if base else \
                      f"; {scoring.value(rates):.2f} FPG"
            except Exception:
                fpg = ""
        reasons.append(Reason(code="ROOKIE_BLEND",
                              text=f"Rookie blend: PTS/GP {old} -> {post:.2f} from {vtxt}{fpg}",
                              value=round(post, 4), baseline=round(base_pts, 4) if base_pts > 0 else None))
    est.pts_pg = post
    est.confidence = round(total_w / (total_w + W_BASELINE), 3) if total_w > 0 else 0.0

    # ---- role signals: Daily Faceoff + news
    dfo = dfo_signals(deployment) if isinstance(deployment, Player) else []
    news = usable_news(news_signals or [], as_of)
    used = dfo + news
    est.signals = [{k: (v.isoformat() if isinstance(v, (date, datetime)) else v) for k, v in d.items()} for d in used]
    pos_top = [d for d in used if d["kind"] in ("top_line", "pp1", "first_line_pairing") and d["direction"] > 0]
    pos_minor = [d for d in used if d["kind"] in ("pp2", "extended_role") and d["direction"] > 0]
    depth = [d for d in used if d["kind"] == "top_line" and d["direction"] < 0]
    mult = 1.0
    if pos_top:
        mult = ROLE_BOOST
    elif pos_minor:
        mult = ROLE_MINOR_BOOST
    elif depth:
        mult = DEPTH_PENALTY
    for d in dfo:
        reasons.append(Reason(code="ROLE_DFO", text=f"{d['quote']} ({d['kind'].replace('_', ' ')}, "
                                                    f"{'+' if d['direction'] > 0 else '-'})",
                              value=float(d["direction"]), baseline=float(d["confidence"])))
    for d in news:
        reasons.append(Reason(code="ROLE_NEWS", text=f"{d['kind'].replace('_', ' ')} "
                                                     f"{'+' if d['direction'] > 0 else '-'}: {_fmt_signal(d)}",
                              value=float(d["direction"]), baseline=float(d["confidence"])))
    if mult != 1.0 and rates:
        rates = _scale(rates, mult)
        why = pos_top or pos_minor or depth
        reasons.append(Reason(code="ROLE_BOOST" if mult > 1 else "ROLE_DEPTH",
                              text=f"Role x{mult:.2f} on scoring rates ({why[0]['kind'].replace('_', ' ')} from "
                                   f"{why[0].get('source') or 'news'})", value=mult, baseline=1.0))
    est.role_mult = mult

    # ---- expected games share
    confirmed = season_gp > 0 or any(d["kind"] == "nhl_roster" and d["direction"] > 0 for d in used)
    if confirmed:
        gp_exp, why = GP_CONFIRMED, "confirmed in the NHL lineup" if season_gp <= 0 else f"{season_gp} GP this season"
    elif player.team:
        top10 = player.draft_overall is not None and player.draft_overall <= 10
        gp_exp = GP_TOP10_ON_TEAM if top10 else GP_ON_TEAM
        why = f"on {player.team}" + (" as a top-10 pick" if top10 else "") + ", roster spot not confirmed"
    else:
        gp_exp, why = GP_NO_TEAM, "no NHL team"
    parts = [f"{why} {gp_exp:.0%}"]
    if projection_gp:
        share = min(1.0, projection_gp / PROJ_FULL_GP)
        wp = PROJ_GP_WEIGHT_CONFIRMED if confirmed else PROJ_GP_WEIGHT
        gp_exp = (1.0 - wp) * gp_exp + wp * share
        parts.append(f"{wp:.0%} weight on the projection's {projection_gp} GP ({share:.0%})")
    week = 1.0
    status = [d for d in news + dfo if d["kind"] in ("nhl_roster", "ahl_demotion", "junior_return")]
    latest = status[0] if status else None       # news first (dated, newest first), then DFO
    demoted = latest is not None and latest["kind"] in ("ahl_demotion", "junior_return") and latest["direction"] < 0
    if demoted:
        gp_exp = GP_JUNIOR if latest["kind"] == "junior_return" else GP_AHL
        week = gp_exp
        parts.append(f"latest news {_fmt_signal(latest)} -> {gp_exp:.0%}")
    else:
        if pos_top and gp_exp < GP_TOP_ROLE:
            gp_exp = GP_TOP_ROLE
            parts.append(f"top role -> {gp_exp:.0%}")
        scratch = [d for d in used if d["kind"] == "scratched"]
        if scratch and scratch[0]["direction"] < 0:
            gp_exp *= SCRATCH_MULT
            week *= SCRATCH_MULT
            parts.append(f"scratched x{SCRATCH_MULT:g}")
        if season_gp > 0 and gp_exp < 1.0:
            gp_exp += (1.0 - gp_exp) * min(1.0, season_gp / GP_FADE_SEASON_GP)
    est.gp_expectation = round(max(0.0, min(1.0, gp_exp)), 4)
    est.week_share = round(max(0.0, min(1.0, week)), 4)
    reasons.append(Reason(code="GP_EXPECTATION",
                          text=f"Expected games share {est.gp_expectation:.0%}: " + "; ".join(parts),
                          value=est.gp_expectation, baseline=1.0))
    est.rates = rates
    est.reasons = reasons
    return est


def has_rookie_evidence(p: Player, history: Sequence[LeagueSeason] | None, signals: Sequence[Any] | None) -> bool:
    """The model engages only with evidence beyond the plain baseline: pedigree loaded (draft slot
    or career GP), league history, a Daily Faceoff lineup spot or a news role signal."""
    return bool(history) or bool(signals) or p.draft_overall is not None or p.career_gp is not None \
        or bool(p.line or p.pp_unit)


__all__ = ["RookiePrior", "RookieEstimate", "is_unproven", "rookie_prior", "rookie_value", "nhle_estimate",
           "age_multiplier", "pedigree_prior", "dfo_signals", "usable_news", "has_rookie_evidence"]
