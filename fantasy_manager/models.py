"""Provider-neutral domain models (pydantic v2)."""
from __future__ import annotations

import re
import unicodedata
import datetime as _dt
from datetime import date
from typing import Any, Literal

from pydantic import BaseModel, Field

SKATER_STATS = ("G", "A", "PTS", "PM", "PIM", "PPG", "PPA", "PPP", "SHG", "SHA", "SHP", "GWG",
                "FOW", "FOL", "SOG", "HIT", "BLK", "HAT", "GP", "DEF", "STP", "ENG", "FT")
GOALIE_STATS = ("GS", "W", "L", "OTL", "SA", "GA", "SV", "SO", "GAA", "SVPCT")
CANONICAL_STATS = frozenset(SKATER_STATS + GOALIE_STATS)
# Stats that are already rates and must not be divided by games played.
RATE_STATS = frozenset({"GAA", "SVPCT"})

POSITIONS = ("C", "LW", "RW", "F", "D", "G")
SLOTS = POSITIONS + ("UTIL", "BN", "IR")

Split = Literal["season", "prior", "prior2", "prior3", "last7", "last15", "last30", "projected"]
# prior = season N-1, prior2 = N-2, prior3 = N-3 (N = the current / upcoming season)
Status = Literal["healthy", "dtd", "out", "ir", "ltir", "suspended", "unknown"]
# Dynasty priority: win this season, balance now vs later, or build for later seasons.
DynastyMode = Literal["contend", "balanced", "rebuild"]


def normalize_name(name: str) -> str:
    """Basic accent/punctuation-insensitive key ("Tim Stützle" -> "tim stutzle")."""
    s = unicodedata.normalize("NFKD", name)
    s = "".join(c for c in s if not unicodedata.combining(c)).lower()
    s = re.sub(r"[^a-z0-9 ]+", " ", s.replace("'", "").replace(".", ""))
    return re.sub(r"\s+", " ", s).strip()


class StatLine(BaseModel):
    split: Split
    gp: int
    stats: dict[str, float]

    def per_game(self) -> dict[str, float]:
        """Per-game rates; rate stats (GAA, SV%) pass through; empty if gp == 0."""
        if self.gp <= 0:
            return {}
        out: dict[str, float] = {}
        for k, v in self.stats.items():
            if k == "GP":
                continue
            out[k] = float(v) if k in RATE_STATS else float(v) / self.gp
        return out


class Player(BaseModel):
    cid: str
    name: str
    name_norm: str
    ids: dict[str, str]
    team: str | None
    positions: list[str]
    # The platform's single default position (ESPN defaultPositionId), which is what ESPN's
    # per-position roster maximums count; None when the provider has no such notion.
    primary_position: str | None = None
    birth_date: date | None = None
    status: Status = "unknown"
    status_note: str | None = None
    lines: dict[str, StatLine] = Field(default_factory=dict)
    pct_owned: float | None = None
    # NHL pedigree (player landing page, filled by providers.enrich for young / unproven players)
    draft_overall: int | None = None
    draft_round: int | None = None
    draft_year: int | None = None
    career_gp: int | None = None      # NHL regular-season games played, career
    # Market / ownership trend signals (provider-specific scale, percent of leagues)
    pct_owned_change: float | None = None   # change in % rostered over the provider's trend window
    pct_started: float | None = None
    adp: float | None = None
    adp_change: float | None = None
    # Deployment (from NHL per-game TOI/PP reports; per game, minutes)
    toi_per_game: float | None = None
    pp_toi_per_game: float | None = None
    pp_share: float | None = None           # share of team PP time, 0..1
    toi_trend: float | None = None          # last-5/10 GP minus season baseline, minutes
    pp_share_trend: float | None = None
    # Lines / units / goalie starts (Daily Faceoff snapshot)
    line: str | None = None                 # f1..f4, d1..d3, g
    pp_unit: str | None = None              # pp1, pp2 or None
    pk_unit: str | None = None
    line_change: str | None = None          # human-readable change vs previous snapshot, e.g. "PP2 -> PP1"
    confirmed_start: bool | None = None     # goalies: confirmed/likely starter today (None = unknown)
    start_source: str | None = None         # who confirmed it and when
    # Luck / regression inputs (MoneyPuck)
    ixg_per_game: float | None = None       # individual expected goals per game (season)
    goals_minus_ixg: float | None = None    # season total goals minus ixG
    onice_sh_pct: float | None = None
    onice_xg_pct: float | None = None
    # which season the MoneyPuck fields describe: "season" (this season, >= 5 GP) or "prior"
    # (providers.xg_enrich; None when unknown)
    xg_split: str | None = None

    @property
    def is_goalie(self) -> bool:
        return "G" in self.positions

    @property
    def nhl_id(self) -> int | None:
        """Canonical NHL player id once resolved by the crosswalk (``ids["nhl"]``)."""
        raw = self.ids.get("nhl")
        try:
            return int(raw) if raw not in (None, "") else None
        except (TypeError, ValueError):
            return None

    def gp(self, split: str = "season") -> int:
        line = self.lines.get(split)
        return line.gp if line else 0


class RosterSlot(BaseModel):
    slot: str
    player: Player | None
    starting: bool


class FantasyTeam(BaseModel):
    team_id: str
    name: str
    owner_is_me: bool
    slots: list[RosterSlot]
    record: tuple[int, int, int] | None = None

    @property
    def players(self) -> list[Player]:
        return [s.player for s in self.slots if s.player is not None]


class ScoringConfig(BaseModel):
    kind: Literal["points", "categories", "roto"]
    weights: dict[str, float] = Field(default_factory=dict)
    categories: list[str] = Field(default_factory=list)
    # Points leagues with a separate goalie scoring group: overrides applied to goalie lines.
    goalie_weights: dict[str, float] = Field(default_factory=dict)


SourceSeverity = Literal["ok", "warn", "fail"]


class SourceStatus(BaseModel):
    """Structured freshness / failure state of one data source for this run (filled by
    ``providers.enrich``; read by ``report.health``). ``fetched_at`` is when the data in use (or,
    for a failed source, the last good cached copy) was fetched; ``age_seconds`` is measured at
    enrichment time. ``stale`` overrides the age / TTL rule when the producer knows better (e.g.
    the deployment ledger is judged by missed game days, not by age); None = decide from age."""
    name: str
    ok: bool = True
    severity: SourceSeverity = "ok"
    fetched_at: _dt.datetime | None = None
    age_seconds: float | None = None
    ttl_seconds: float | None = None
    detail: str | None = None
    requests: int | None = None
    feeds_valuation: bool = False
    stale: bool | None = None


class LeagueContext(BaseModel):
    provider: str
    league_id: str
    season: int
    name: str
    scoring: ScoringConfig
    roster_shape: dict[str, int]
    teams: list[FantasyTeam]
    # Per-position roster maximums (C/LW/RW/F/D/G -> max players at that position on the whole
    # roster, IR included; see recommend.base.limit_positions for how a player is counted) and the
    # maximum number of rostered players outside IR / minors slots. Empty / None = not known.
    # ESPN: rosterSettings.positionLimits (keyed by defaultPositionId, <= 0 = no limit) and the
    # non-IR lineupSlotCounts; Fantrax: the Rules page "Total Max" column and "Maximum Total Players".
    position_limits: dict[str, int] = Field(default_factory=dict)
    max_roster_size: int | None = None
    free_agents: list[Player]
    matchup_period: int | None
    dynasty: bool = False
    keeper_horizon_years: int = 3
    dynasty_mode: DynastyMode = "contend"
    # Where dynasty_mode came from: "prefs" (dashboard / `fm mode`), "env" (FANTRAX_MODE), "default"
    # or "option" (`--mode`, this run only); None when a loader did not resolve it.
    dynasty_mode_source: str | None = None
    as_of: date
    # Filled by providers.enrich (NHL schedule): team -> regular-season game dates,
    # league-wide games per date, and the regular-season opening date.
    schedule: dict[str, list[date]] = Field(default_factory=dict)
    games_per_day: dict[date, int] = Field(default_factory=dict)
    # team -> game date -> opponent label ("BOS" at home, "@BOS" away), same source as schedule
    opponents: dict[str, dict[date, str]] = Field(default_factory=dict)
    season_start: date | None = None
    # How lineup changes take effect: "daily" or "weekly" (Fantrax: parsed once from the Rules page
    # by ``FantraxProvider.lineup_lock``; ESPN is daily). None when the provider did not say.
    lineup_lock: Literal["daily", "weekly"] | None = None
    # From the harness ledger (providers.enrich deployment step; empty when the ledger has fewer
    # than 5 games for the team): goalie cid -> (starts, team games) this season, and NHL team ->
    # (P(#1 starts the second night of a back-to-back), back-to-backs seen). Opt-in inputs of
    # valuation.valuate (START_ACTUAL / B2B reasons).
    goalie_actual_starts: dict[str, tuple[int, int]] = Field(default_factory=dict)
    b2b_second_night: dict[str, tuple[float, int]] = Field(default_factory=dict)
    # NHL team -> {NHL id: name} of the goalies on the club's current NHL roster (providers.enrich
    # step 2, from the cached roster pulls; empty when unavailable). Teammate-aware goalie start
    # shares (valuation.schedule.teammate_shares) use it to skip minor-league goalies and to count
    # roster goalies missing from the league pool.
    nhl_goalies: dict[str, dict[int, str]] = Field(default_factory=dict)
    # cid -> deployment summary (harness.deployment.summarize_rows) for exact role-alert numbers
    deployment_details: dict[str, dict[str, Any]] = Field(default_factory=dict, exclude=True)
    # Transaction budget (recommend.base.moves_left / move_scarcity_threshold). "Moves" are
    # acquisitions (adds / waiver claims); drops and IR moves are free. ``moves_limit_per_period``
    # is the league's limit per ``moves_period_label`` (ESPN: per matchup period,
    # settings.acquisitionSettings.matchupAcquisitionLimit; Fantrax: "Max # of claims per week");
    # None = unlimited or unknown. ``moves_used_this_period``: my adds since ``period_start``
    # (``period_start``..``period_end`` = the current matchup / scoring period, inclusive).
    # Season limits (ESPN acquisitionLimit, Fantrax "Max # of claims per season") cap it further.
    # ``recent_adds``: cid -> date I added him, last 14 days (churn guard).
    moves_limit_per_period: int | None = None
    moves_used_this_period: int | None = None
    moves_period_label: str | None = None
    moves_limit_season: int | None = None
    moves_used_season: int | None = None
    period_start: date | None = None
    period_end: date | None = None
    recent_adds: dict[str, date] = Field(default_factory=dict)
    faab_remaining: float | None = None
    # Human-readable data freshness notes and non-fatal warnings per source.
    source_notes: list[str] = Field(default_factory=list)
    warnings: list[str] = Field(default_factory=list)
    # Structured per-source freshness / failure state (providers.enrich; report.health reads it).
    sources: list[SourceStatus] = Field(default_factory=list)

    @property
    def my_team(self) -> FantasyTeam:
        for t in self.teams:
            if t.owner_is_me:
                return t
        raise LookupError("no team flagged as mine")

    def all_players(self) -> list[Player]:
        seen: dict[str, Player] = {}
        for t in self.teams:
            for p in t.players:
                seen.setdefault(p.cid, p)
        for p in self.free_agents:
            seen.setdefault(p.cid, p)
        return list(seen.values())


class Reason(BaseModel):
    code: str
    text: str
    value: float | None = None
    baseline: float | None = None


class Recommendation(BaseModel):
    kind: Literal["lineup", "waiver", "trade", "sell_high", "buy_low", "injury", "alert"]
    score: float
    title: str
    add: list[Player] = Field(default_factory=list)
    drop: list[Player] = Field(default_factory=list)
    counterparty: str | None = None
    reasons: list[Reason] = Field(default_factory=list)
    narrative: str | None = None
    # Explicit predicted gain (graded later by the harness). Units:
    #   week_pts   - projected fantasy points over the next ``horizon_days`` (7)
    #   season_fpg - fantasy points per game (rest of season when horizon_days is None)
    #   lineup_fpg - change in my optimal starting lineup's summed FPG (trades)
    #   dynasty    - dynasty value (FPG-equivalent)
    predicted_gain: float | None = None
    gain_units: GainUnits | None = None
    horizon_days: int | None = None
    # Players the rec acts on without adding or dropping them (IR moves, status alerts).
    subjects: list[Player] = Field(default_factory=list)
    # Absolute 0-10 strength (recommend.strength), comparable across kinds and days, unlike
    # ``score`` (advise() rescales it by rank within the kind). rank_in_kind / kind_total:
    # "1 of 4 waivers".
    strength: float | None = None
    rank_in_kind: int | None = None
    kind_total: int | None = None


GainUnits = Literal["week_pts", "season_fpg", "lineup_fpg", "dynasty"]
ActivityAction = Literal["ADD", "DROP", "TRADE_IN", "TRADE_OUT", "IR", "ACTIVATE", "PROPOSED"]


class ActivityItem(BaseModel):
    """One player move from a provider's league activity feed (provider-neutral).

    Items of one transaction (an add/drop pair, both sides of a trade, a trade proposal)
    share ``group_id``. ``counterparty_id`` is the other team of a trade / proposal: for
    TRADE_IN / PROPOSED the team the player comes from, for TRADE_OUT the team he goes to."""
    source: str
    tx_id: str
    ts: _dt.datetime
    team_id: str | None
    team_name: str | None = None
    action: ActivityAction
    cid: str | None
    player_name: str | None = None
    nhl_id: int | None = None
    group_id: str | None = None
    counterparty_id: str | None = None


class LineupDay(BaseModel):
    """One rostered player's lineup slot on one scoring day (and provider points if known)."""
    team_id: str
    date: _dt.date
    cid: str
    slot: str
    starting: bool
    provider_pts: float | None = None
