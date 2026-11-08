//! Rust port of `fantasy_manager/providers/espn.py`: the ESPN fantasy hockey provider, built on
//! [`api`] (the used subset of the `espn_api` 0.46 package). All HTTP goes through
//! [`fm_http::HttpCache`] (TTL [`ESPN_TTL`] for the league API, [`TRENDING_TTL`] for the
//! league-independent trending pool), with the same URLs / params / `x-fantasy-filter` headers as
//! Python, so the shared `cache.db` and the parity tapes hit.
//! Owner: work package `W3-espn`.
//!
//! Deviations (documented):
//! * Clock: `date.today()` is the provider's `today` field (default: local today, set it with
//!   [`EspnProvider::with_today`]); `datetime.fromtimestamp` uses `chrono::Local`.
//! * Python's `_warn` appends to `provider.warnings` and to the loaded context's `warnings`. Here
//!   warnings raised during [`EspnProvider::load`] after the context exists go to both
//!   (`ctx.warnings` and `extras().warnings`); warnings from the remote calls made after load
//!   (`activity`, `box_scores`, `trending`, ...) take `&self` and are only visible through
//!   [`EspnProvider::warnings_now`] (the returned context is owned by the caller).
//! * `type(e).__name__` in warning texts comes from [`api::exc_name`] (Python built-in names for
//!   parse errors, `CacheMiss`, `HttpError`, `ESPN*` errors; `Exception` otherwise).

pub mod api;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use fm_core::activity::{count_adds, recent_adds_from, RECENT_ADDS_WINDOW};
use fm_core::calendar::{calendar_weeks, monday};
use fm_core::config::Settings;
use fm_core::models::{
    is_canonical_stat, normalize_name, ActivityAction, ActivityItem, DynastyMode, FantasyTeam, LeagueContext,
    LineupDay, Player, RosterSlot, ScoringConfig, ScoringKind, Split, StatLine, Status,
};
use fm_core::provider::LeagueProvider;
use fm_core::snapshot::{EspnLeagueMeta, EspnLeagueSettings, ProviderExtras};
use fm_core::{Map, ProviderError, PyDateTime};
use fm_http::ParamValue;
use fm_http::{CacheMiss, HttpCache, Params};
use serde_json::Value;

use self::api::{
    as_int_key, exc_name, filter_header, get_lenient, py_float, py_int, py_str, truthy, truthy_opt, BoxScore,
    EspnError, EspnFantasyRequests, League, PyErr,
};

// League API (lineups, rosters, free agents): short, so a lineup set on espn.com shows up on the next run
pub const ESPN_TTL: f64 = 2.0 * 60.0;
/// every league API URL (lm-api-reads.fantasy.espn.com) contains it
pub const ESPN_HOST: &str = "fantasy.espn.com";
pub const TRENDING_TTL: f64 = 6.0 * 3600.0;
/// League-independent ("leaguedefaults") player pool: ESPN-wide % rostered and its change, no
/// cookies needed. `{year}` is the season (ESPN_YEAR).
pub fn trending_url(year: i64) -> String {
    format!("https://lm-api-reads.fantasy.espn.com/apis/v3/games/fhl/seasons/{year}/segments/0/leaguedefaults/1")
}
pub const TRENDING_POOL: i64 = 300;

/// espn_api STATS_MAP abbreviations that differ from our canonical keys.
pub fn stat_alias(k: &str) -> &str {
    match k {
        "+/-" => "PM",
        "SV%" => "SVPCT",
        other => other,
    }
}

/// ESPN lineup slot names (espn_api POSITION_MAP) -> canonical.
pub fn slot_name(s: &str) -> Option<&'static str> {
    Some(match s {
        "Center" => "C",
        "Left Wing" => "LW",
        "Right Wing" => "RW",
        "Forward" => "F",
        "Defense" => "D",
        "Goalie" => "G",
        "Util" => "UTIL",
        "Bench" => "BN",
        "IR" => "IR",
        _ => return None,
    })
}

/// ESPN lineup slot ids -> canonical.
pub fn slot_id(id: i64) -> Option<&'static str> {
    Some(match id {
        0 => "C",
        1 => "LW",
        2 => "RW",
        3 => "F",
        4 => "D",
        5 => "G",
        6 => "UTIL",
        7 => "BN",
        8 => "IR",
        _ => return None,
    })
}

pub const FA_POSITIONS: [&str; 5] = ["Center", "Left Wing", "Right Wing", "Defense", "Goalie"];

fn injury_status(s: &str) -> Option<Status> {
    Some(match s {
        "ACTIVE" | "NORMAL" => Status::Healthy,
        "DAY_TO_DAY" => Status::Dtd,
        "OUT" => Status::Out,
        "INJURY_RESERVE" => Status::Ir,
        "SUSPENSION" => Status::Suspended,
        _ => return None,
    })
}

const PRO_TEAMS: &[(&str, &str)] = &[
    ("boston bruins", "BOS"), ("buffalo sabres", "BUF"), ("calgary flames", "CGY"),
    ("chicago blackhawks", "CHI"), ("detroit red wings", "DET"), ("edmonton oilers", "EDM"),
    ("carolina hurricanes", "CAR"), ("los angeles kings", "LAK"), ("dallas stars", "DAL"),
    ("new jersey devils", "NJD"), ("new york islanders", "NYI"), ("new york rangers", "NYR"),
    ("ottawa senators", "OTT"), ("philadelphia flyers", "PHI"), ("pittsburgh penguins", "PIT"),
    ("colorado avalanche", "COL"), ("san jose sharks", "SJS"), ("st. louis blues", "STL"),
    ("st louis blues", "STL"), ("tampa bay lightning", "TBL"), ("toronto maple leafs", "TOR"),
    ("vancouver canucks", "VAN"), ("washington capitals", "WSH"), ("arizona coyotes", "ARI"),
    ("anaheim ducks", "ANA"), ("florida panthers", "FLA"), ("nashville predators", "NSH"),
    ("winnipeg jets", "WPG"), ("columbus blue jackets", "CBJ"), ("minnesota wild", "MIN"),
    ("vegas golden knights", "VGK"), ("seattle kraken", "SEA"), ("utah hockey club", "UTA"),
    ("utah mammoth", "UTA"),
];

// -- pure parsing helpers (unit-tested without network) ---------------------

/// ESPN full team name -> NHL 3-letter code (None for free agents/unknown).
pub fn pro_team_abbrev(full_name: Option<&str>) -> Option<&'static str> {
    let full_name = full_name.filter(|s| !s.is_empty() && *s != "Unknown Team")?;
    let key = full_name.trim().to_lowercase();
    if let Some((_, a)) = PRO_TEAMS.iter().find(|(k, _)| *k == key) {
        return Some(a);
    }
    if key.contains("canadiens") {
        // constant spells it with an accent (or mojibake)
        return Some("MTL");
    }
    if key.starts_with("utah") {
        return Some("UTA");
    }
    None
}

/// Map an espn_api stats 'total' dict to canonical keys, dropping placeholders.
pub fn canonical_stats(total: Option<&Map<String, Value>>) -> Map<String, f64> {
    let mut out: Map<String, f64> = Map::new();
    for (k, v) in total.into_iter().flatten() {
        let key = stat_alias(k);
        if is_canonical_stat(key) && !v.is_null() {
            if let Some(x) = py_float(v) {
                out.insert(key.to_string(), x);
            }
        }
    }
    if !out.contains_key("PTS") && (out.contains_key("G") || out.contains_key("A")) {
        let pts = out.get("G").copied().unwrap_or(0.0) + out.get("A").copied().unwrap_or(0.0);
        out.insert("PTS".into(), pts);
    }
    out
}

fn gp(stats: &Map<String, f64>) -> i64 {
    for k in ["GP", "GS"] {
        if let Some(&x) = stats.get(k) {
            if x != 0.0 {
                return fm_pycompat::num::py_round_i64(x);
            }
        }
    }
    0
}

/// Convert espn_api Player.stats ({'Total 2027': {...total...}, ...}) to StatLines.
pub fn parse_stat_lines(stats: &Map<String, Map<String, Value>>, year: i64) -> Map<String, StatLine> {
    let wanted = [
        (format!("Total {year}"), Split::Season),
        (format!("Total {}", year - 1), Split::Prior),
        (format!("Last 7 {year}"), Split::Last7),
        (format!("Last 15 {year}"), Split::Last15),
        (format!("Last 30 {year}"), Split::Last30),
        (format!("Projected {year}"), Split::Projected),
    ];
    let mut lines = Map::new();
    for (key, split) in wanted {
        let Some(total) = stats.get(&key).filter(|t| !t.is_empty()) else { continue };
        let cs = canonical_stats(Some(total));
        lines.insert(split.as_str().to_string(), StatLine { split, gp: gp(&cs), stats: cs });
    }
    lines
}

/// `map_injury_status(status, injured)`: a non-string status (espn_api's `[]` when absent) is
/// healthy, or unknown when ESPN flags the player injured.
pub fn map_injury_status(status: &Value, injured: bool) -> Status {
    match status {
        Value::String(s) if !s.is_empty() => injury_status(&s.to_uppercase()).unwrap_or(Status::Unknown),
        _ => {
            if injured {
                Status::Unknown
            } else {
                Status::Healthy
            }
        }
    }
}

pub fn positions_from_espn(eligible_slots: &[String], default_position: Option<&str>) -> Vec<String> {
    const ORDER: [&str; 6] = ["C", "LW", "RW", "F", "D", "G"];
    let found: HashSet<&str> = eligible_slots.iter().filter_map(|s| slot_name(s)).collect();
    let mut pos: Vec<String> = ORDER.iter().filter(|p| found.contains(*p)).map(|p| p.to_string()).collect();
    if pos.is_empty() {
        if let Some(d) = default_position.filter(|d| !d.is_empty()) {
            if let Some(mapped) = slot_name(d).filter(|m| ORDER.contains(m)) {
                pos = vec![mapped.to_string()];
            }
        }
    }
    pos
}

/// ESPN `player.ownership` block -> Player market fields (`ownership_fields`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Ownership {
    pub pct_owned: Option<f64>,
    pub pct_owned_change: Option<f64>,
    pub pct_started: Option<f64>,
    pub adp: Option<f64>,
    pub adp_change: Option<f64>,
}

impl Ownership {
    fn any(&self) -> bool {
        self.pct_owned.is_some()
            || self.pct_owned_change.is_some()
            || self.pct_started.is_some()
            || self.adp.is_some()
            || self.adp_change.is_some()
    }

    /// The dict form (`{"pct_owned": .., "pct_owned_change": .., "pct_started": .., "adp": ..,
    /// "adp_change": ..}`).
    pub fn to_map(&self) -> Map<String, Value> {
        let f = |x: Option<f64>| x.map(Value::from).unwrap_or(Value::Null);
        let mut m = Map::new();
        m.insert("pct_owned".into(), f(self.pct_owned));
        m.insert("pct_owned_change".into(), f(self.pct_owned_change));
        m.insert("pct_started".into(), f(self.pct_started));
        m.insert("adp".into(), f(self.adp));
        m.insert("adp_change".into(), f(self.adp_change));
        m
    }
}

/// `_float(val)`
fn float_or_none(v: Option<&Value>) -> Option<f64> {
    v.filter(|v| !v.is_null()).and_then(py_float)
}

/// percentOwned -> pct_owned, percentChange -> pct_owned_change (ESPN's weekly "+/-" of %
/// rostered, in points), percentStarted -> pct_started, averageDraftPosition -> adp (None when
/// 0 / absent), averageDraftPositionPercentChange -> adp_change.
pub fn ownership_fields(raw_player: Option<&Value>) -> Ownership {
    let o = raw_player.and_then(|p| get_lenient(p, "ownership")).filter(|o| truthy(o));
    let g = |k: &str| float_or_none(o.and_then(|o| get_lenient(o, k)));
    let adp = g("averageDraftPosition");
    Ownership {
        pct_owned: g("percentOwned"),
        pct_owned_change: g("percentChange"),
        pct_started: g("percentStarted"),
        adp: adp.filter(|&a| a != 0.0 && a > 0.0),
        adp_change: g("averageDraftPositionPercentChange"),
    }
}

/// Build a Player from an espn_api hockey Player. `ownership` fills the market fields; its
/// pct_owned is used when `pct_owned` is None.
pub fn player_from_espn(
    p: &api::Player,
    year: i64,
    pct_owned: Option<f64>,
    ownership: Option<&Ownership>,
) -> anyhow::Result<Player> {
    let own = ownership.copied().unwrap_or_default();
    let pct_owned = pct_owned.or(own.pct_owned);
    let status_raw = &p.injury_status;
    let note = match status_raw {
        Value::String(s) if !s.is_empty() && s != "ACTIVE" && s != "NORMAL" => Some(s.clone()),
        _ => None,
    };
    let primary = slot_name(&p.position).filter(|x| ["C", "LW", "RW", "D", "G"].contains(x));
    let name = match &p.name {
        Value::String(s) => s.clone(),
        other => {
            return Err(PyErr::value(format!("name: Input should be a valid string, got {}", api::py_repr(other))).into())
        }
    };
    let id = py_str(&p.player_id);
    let mut ids = Map::new();
    ids.insert("espn".to_string(), id.clone());
    Ok(Player {
        cid: format!("espn:{id}"),
        name_norm: normalize_name(&name),
        name,
        ids,
        team: pro_team_abbrev(Some(&p.pro_team)).map(str::to_string),
        positions: positions_from_espn(&p.eligible_slots, Some(&p.position)),
        primary_position: primary.map(str::to_string),
        birth_date: None,
        status: map_injury_status(status_raw, p.injured),
        status_note: note,
        waiver_until: None,
        lines: parse_stat_lines(&p.stats, year),
        pct_owned,
        draft_overall: None,
        draft_round: None,
        draft_year: None,
        career_gp: None,
        pct_owned_change: own.pct_owned_change,
        pct_started: own.pct_started,
        adp: own.adp,
        adp_change: own.adp_change,
        toi_per_game: None,
        pp_toi_per_game: None,
        pp_share: None,
        toi_trend: None,
        pp_share_trend: None,
        line: None,
        pp_unit: None,
        pk_unit: None,
        line_change: None,
        usual_line: None,
        usual_pp_unit: None,
        confirmed_start: None,
        start_source: None,
        ixg_per_game: None,
        goals_minus_ixg: None,
        onice_sh_pct: None,
        onice_xg_pct: None,
        xg_split: None,
    })
}

fn item_key(item: &Value) -> Option<String> {
    let id = py_str(get_lenient(item, "statId").unwrap_or(&Value::Null));
    let abbrev = api::stats_map(&id).unwrap_or("");
    let key = stat_alias(abbrev);
    is_canonical_stat(key).then(|| key.to_string())
}

/// Build ScoringConfig from ESPN scoringType + scoringItems.
pub fn scoring_from_settings(scoring_type: Option<&str>, scoring_items: &[Value]) -> anyhow::Result<ScoringConfig> {
    let st = scoring_type.unwrap_or("").to_uppercase();
    let kind = if st.contains("CATEGOR") {
        ScoringKind::Categories
    } else if st.contains("ROTO") {
        ScoringKind::Roto
    } else {
        ScoringKind::Points
    };
    let mut weights: Map<String, f64> = Map::new();
    let mut cats: Vec<String> = Vec::new();
    for item in scoring_items {
        let Some(key) = item_key(item) else { continue };
        if kind == ScoringKind::Points {
            let pts = match get_lenient(item, "points") {
                Some(v) if truthy(v) => py_float(v)
                    .ok_or_else(|| PyErr::value(format!("could not convert to float: {}", api::py_repr(v))))?,
                _ => 0.0,
            };
            let cur = weights.get(&key).copied().unwrap_or(0.0);
            weights.insert(key, cur + pts);
        } else if !cats.contains(&key) {
            cats.push(key.clone());
            // categories: +1 higher-is-better, -1 for reverse items (GAA, L, GA...)
            weights.insert(key, if truthy_opt(get_lenient(item, "isReverseItem")) { -1.0 } else { 1.0 });
        }
    }
    Ok(ScoringConfig { kind, weights, categories: cats, goalie_weights: Map::new() })
}

/// `int(sid)` of a dict key (Python `int(str)`).
fn int_key(s: &str) -> Option<i64> {
    py_int(&Value::String(s.to_string()))
}

pub fn roster_shape_from_counts(counts: Option<&Value>) -> anyhow::Result<Map<String, i64>> {
    let mut shape = Map::new();
    if let Some(Value::Object(m)) = counts {
        for (sid, n) in m {
            let id = int_key(sid).ok_or_else(|| PyErr::value(format!("invalid literal for int() with base 10: '{sid}'")))?;
            let n = py_int(n).ok_or_else(|| PyErr::type_err("int() argument must be a string or a real number"))?;
            if let Some(slot) = slot_id(id) {
                if n > 0 {
                    shape.insert(slot.to_string(), n);
                }
            }
        }
    }
    Ok(shape)
}

/// rosterSettings.positionLimits is keyed by the player's ESPN defaultPositionId (not the lineup
/// slot id): 1 C, 2 LW, 3 RW, 4 D, 5 G; key 0 is unused. Values <= 0 mean no limit.
pub fn position_limit_id(id: i64) -> Option<&'static str> {
    Some(match id {
        1 => "C",
        2 => "LW",
        3 => "RW",
        4 => "D",
        5 => "G",
        _ => return None,
    })
}

/// ESPN `rosterSettings.positionLimits` -> {C/LW/RW/D/G: max rostered at that default
/// position}, only for positions that have a limit.
pub fn position_limits_from_settings(roster_settings: Option<&Value>) -> Map<String, i64> {
    let mut out = Map::new();
    let limits = roster_settings.and_then(|r| get_lenient(r, "positionLimits")).filter(|v| truthy(v));
    if let Some(Value::Object(m)) = limits {
        for (pid, n) in m {
            let (Some(pid), Some(cap)) = (int_key(pid), py_int(n)) else { continue };
            if let Some(pos) = position_limit_id(pid) {
                if cap > 0 {
                    out.insert(pos.to_string(), cap);
                }
            }
        }
    }
    out
}

/// ESPN `rosterSettings.lineupSlotStatLimits` -> one text per limited slot, e.g.
/// "G: limit 82 (stat 32)".
pub fn stat_limits_from_settings(roster_settings: Option<&Value>) -> Vec<String> {
    let mut out = Vec::new();
    let limits = roster_settings.and_then(|r| get_lenient(r, "lineupSlotStatLimits")).filter(|v| truthy(v));
    if let Some(Value::Object(m)) = limits {
        for (sid, v) in m {
            let slot = match int_key(sid) {
                Some(i) => slot_id(i).map(str::to_string).unwrap_or_else(|| format!("slot {sid}")),
                None => format!("slot {sid}"),
            };
            if let Value::Object(o) = v {
                let n = o.get("limitValue").unwrap_or(&Value::Null);
                let stat = o.get("statId").filter(|s| !s.is_null());
                let mut line = format!("{slot}: limit {}", py_str(n));
                if let Some(s) = stat {
                    line.push_str(&format!(" (stat {})", py_str(s)));
                }
                out.push(line);
            } else {
                out.push(format!("{slot}: {}", py_str(v)));
            }
        }
    }
    out
}

/// Maximum rostered players outside IR: the sum of the non-IR lineupSlotCounts.
pub fn max_roster_from_counts(counts: Option<&Value>) -> anyhow::Result<Option<i64>> {
    let shape = roster_shape_from_counts(counts)?;
    let n: i64 = shape.iter().filter(|(k, _)| k.as_str() != "IR").map(|(_, v)| *v).sum();
    Ok(if n != 0 { Some(n) } else { None })
}

// -- transaction budget: settings.acquisitionSettings ----------------------------------

pub const WEEK_DAYS: i64 = 7;

/// `datetime.fromtimestamp(secs)` (naive local; microseconds rounded half-even like CPython).
pub fn fromtimestamp_local(secs: f64) -> Option<NaiveDateTime> {
    let (t, us) = split_timestamp(secs)?;
    Local.timestamp_opt(t, us * 1000).single().map(|d| d.naive_local())
}

/// `datetime.fromtimestamp(secs, tz=timezone.utc)` as a naive UTC wall clock.
pub fn fromtimestamp_utc(secs: f64) -> Option<NaiveDateTime> {
    let (t, us) = split_timestamp(secs)?;
    Utc.timestamp_opt(t, us * 1000).single().map(|d| d.naive_utc())
}

fn split_timestamp(secs: f64) -> Option<(i64, u32)> {
    if !secs.is_finite() || secs.abs() > 1e14 {
        return None;
    }
    let mut t = secs.trunc();
    let mut us = ((secs - t) * 1e6).round_ties_even();
    if us >= 1e6 {
        t += 1.0;
        us -= 1e6;
    } else if us < 0.0 {
        t -= 1.0;
        us += 1e6;
    }
    Some((t as i64, us as u32))
}

/// Local date a player on waivers (`status` WAIVERS) clears: `waiverProcessDate` (epoch ms).
pub fn waiver_until_from_raw(raw: &Value) -> Option<NaiveDate> {
    if get_lenient(raw, "status").and_then(Value::as_str) != Some("WAIVERS") {
        return None;
    }
    let ms = get_lenient(raw, "waiverProcessDate")?.as_f64()?;
    if ms > 0.0 {
        fromtimestamp_local(ms / 1000.0).map(|d| d.date())
    } else {
        None
    }
}

/// ESPN `settings.acquisitionSettings` decoded (`acquisition_limits`). `per_period` None =
/// unlimited.
#[derive(Clone, Debug, PartialEq)]
pub struct AcquisitionLimits {
    pub per_period: Option<i64>,
    pub season: Option<i64>,
    pub per_scoring_period: Option<f64>,
    pub faab_budget: Option<f64>,
    pub waiver_hours: Option<i64>,
    pub kind: Option<Value>,
}

/// `period_days` = scoring periods in the current matchup (default 7) for a
/// per-scoring-period limit.
pub fn acquisition_limits(acq: Option<&Value>, period_days: Option<i64>) -> AcquisitionLimits {
    let g = |k: &str| acq.and_then(|a| get_lenient(a, k));
    let raw = float_or_none(g("matchupAcquisitionLimit"));
    let per_sp = truthy_opt(g("matchupLimitPerScoringPeriod"));
    let mut per_period = None;
    if let Some(r) = raw.filter(|&r| r > 0.0) {
        let days = period_days.filter(|&d| d != 0).unwrap_or(WEEK_DAYS);
        per_period = Some(if per_sp {
            fm_pycompat::num::py_round_i64(r * days as f64)
        } else {
            fm_pycompat::num::py_round_i64(r)
        });
    }
    let season_raw = float_or_none(g("acquisitionLimit"));
    let season = season_raw.filter(|&s| s >= 0.0).map(|s| s.trunc() as i64);
    let budget = if truthy_opt(g("isUsingAcquisitionBudget")) { float_or_none(g("acquisitionBudget")) } else { None };
    let hours = float_or_none(g("waiverHours"));
    AcquisitionLimits {
        per_period,
        season,
        per_scoring_period: raw.filter(|&r| per_sp && r > 0.0),
        faab_budget: budget,
        waiver_hours: hours.map(|h| h.trunc() as i64),
        kind: g("acquisitionType").cloned(),
    }
}

/// (acquisitions this matchup period, acquisitions this season, FAAB spent) from a raw league
/// team's `transactionCounter`; Nones when the counter is absent.
pub fn transaction_counter(team_json: Option<&Value>, matchup_period: Option<i64>) -> (Option<i64>, Option<i64>, Option<f64>) {
    let Some(tc) = team_json.and_then(|t| get_lenient(t, "transactionCounter")).filter(|t| t.is_object()) else {
        return (None, None, None);
    };
    let empty = Value::Object(Default::default());
    // totals = tc.get("matchupAcquisitionTotals") or {}
    let totals = get_lenient(tc, "matchupAcquisitionTotals").filter(|t| truthy(t)).unwrap_or(&empty);
    let mut period = None;
    if let (Some(mp), Value::Object(m)) = (matchup_period, totals) {
        // JSON keys are strings: totals.get(matchup_period) (an int key) never matches
        let v = m.get(&mp.to_string()).filter(|v| truthy(v));
        period = Some(v.and_then(py_int).unwrap_or(0));
    }
    let season = get_lenient(tc, "acquisitions").filter(|v| v.is_number()).and_then(py_int);
    (period, season, float_or_none(get_lenient(tc, "acquisitionBudgetSpent")))
}

/// game start (UTC) -> US Eastern calendar date
pub const ET_OFFSET_HOURS: i64 = 5;

/// `proTeamSchedules_wl` JSON -> ({scoring period: date}, {date: NHL games}). A scoring
/// period's date is the Eastern date of its earliest game.
pub fn espn_scoring_dates(pro_schedule: Option<&Value>) -> (Map<i64, NaiveDate>, Map<NaiveDate, i64>) {
    let mut first: Map<i64, NaiveDateTime> = Map::new();
    let mut games: BTreeMap<NaiveDate, HashSet<String>> = BTreeMap::new();
    let teams = pro_schedule
        .and_then(|p| get_lenient(p, "settings"))
        .and_then(|s| get_lenient(s, "proTeams"))
        .and_then(Value::as_array);
    for team in teams.into_iter().flatten() {
        let Some(Value::Object(by_sp)) = get_lenient(team, "proGamesByScoringPeriod") else { continue };
        for (sp, rows) in by_sp {
            for g in rows.as_array().into_iter().flatten() {
                let Some(ms) = get_lenient(g, "date").and_then(py_float) else { continue };
                let Some(utc) = fromtimestamp_utc(ms / 1000.0) else { continue };
                let when = utc - Duration::hours(ET_OFFSET_HOURS);
                let Some(k) = int_key(sp) else { continue };
                if first.get(&k).is_none_or(|&f| when < f) {
                    first.insert(k, when);
                }
                let id = get_lenient(g, "id").filter(|v| truthy(v));
                let key = match id {
                    Some(v) => format!("id:{}", api::py_repr(v)),
                    None => format!(
                        "t:{}:{}",
                        api::py_repr(get_lenient(g, "homeProTeamId").unwrap_or(&Value::Null)),
                        when
                    ),
                };
                games.entry(when.date()).or_default().insert(key);
            }
        }
    }
    (
        first.into_iter().map(|(k, v)| (k, v.date())).collect(),
        games.into_iter().map(|(d, ids)| (d, ids.len() as i64)).collect(),
    )
}

/// A league-context stub / base with every optional field at its Python default.
#[allow(clippy::too_many_arguments)]
pub fn new_context(
    league_id: String,
    season: i64,
    name: String,
    scoring: ScoringConfig,
    roster_shape: Map<String, i64>,
    teams: Vec<FantasyTeam>,
    free_agents: Vec<Player>,
    matchup_period: Option<i64>,
    as_of: NaiveDate,
) -> LeagueContext {
    LeagueContext {
        provider: "espn".into(),
        league_id,
        season,
        name,
        scoring,
        roster_shape,
        teams,
        position_limits: Map::new(),
        max_roster_size: None,
        stat_limits: None,
        free_agents,
        matchup_period,
        dynasty: false,
        keeper_horizon_years: 3,
        keepers_per_team: None,
        dynasty_mode: DynastyMode::Contend,
        dynasty_mode_source: None,
        as_of,
        schedule: Map::new(),
        games_per_day: Map::new(),
        opponents: Map::new(),
        game_starts: Map::new(),
        season_start: None,
        lineup_lock: None,
        lock_per_game: true,
        goalie_actual_starts: Map::new(),
        b2b_second_night: Map::new(),
        nhl_goalies: Map::new(),
        deployment_details: Map::new(),
        moves_limit_per_period: None,
        moves_used_this_period: None,
        moves_period_label: None,
        moves_limit_season: None,
        moves_used_season: None,
        period_start: None,
        period_end: None,
        recent_adds: Map::new(),
        faab_remaining: None,
        draft_picks: Map::new(),
        source_notes: Vec::new(),
        warnings: Vec::new(),
        sources: Vec::new(),
        enrich: Default::default(),
    }
}

/// Dates of the matchup period containing `today` (the first one before the season):
/// Monday-Sunday weeks from opening night, the All-Star-break week merged into the next when
/// `games_per_day` shows it, then the first two merged while there are more weeks than
/// `n_periods`. When ESPN's `current_period` number disagrees with that calendar the plain
/// Monday-Sunday week containing `today` is used.
pub fn espn_period_bounds(
    opening: NaiveDate,
    final_scoring_period: Option<i64>,
    n_periods: Option<i64>,
    today: NaiveDate,
    games_per_day: Option<&Map<NaiveDate, i64>>,
    current_period: Option<i64>,
) -> Option<(NaiveDate, NaiveDate)> {
    let days = final_scoring_period.filter(|&f| f != 0).unwrap_or(200);
    let last = opening + Duration::days(days - 1);
    let mut stub = new_context(
        String::new(),
        0,
        String::new(),
        ScoringConfig { kind: ScoringKind::Points, weights: Map::new(), categories: vec![], goalie_weights: Map::new() },
        Map::new(),
        vec![],
        vec![],
        None,
        today,
    );
    stub.games_per_day = games_per_day.cloned().unwrap_or_default();
    let weeks = calendar_weeks(&stub, n_periods, Some(opening), Some(last));
    let first = *weeks.first()?;
    if today < first.0 {
        return Some(first);
    }
    let (idx, w) = weeks.iter().enumerate().find(|(_, w)| w.0 <= today && today <= w.1).map(|(i, w)| (i + 1, *w))?;
    match current_period {
        None => Some(w),
        Some(c) if c == idx as i64 => Some(w),
        Some(_) => {
            stub.games_per_day = Map::new();
            let plain = calendar_weeks(&stub, None, Some(opening), Some(last));
            Some(plain.into_iter().find(|p| p.0 <= today && today <= p.1).unwrap_or(w))
        }
    }
}

fn norm_swid(s: Option<&Value>) -> String {
    let s = match s {
        Some(v) if truthy(v) => py_str(v),
        _ => String::new(),
    };
    s.trim().trim_matches(|c| c == '{' || c == '}').to_lowercase()
}

/// Index of my espn_api Team: ESPN_TEAM (id or name substring) first, then SWID owner match.
pub fn find_my_team(teams: &[api::Team], swid: Option<&str>, team_hint: Option<&str>) -> Result<usize, ProviderError> {
    if let Some(hint) = team_hint.filter(|h| !h.is_empty()) {
        let hint = hint.trim();
        let is_digit = !hint.is_empty() && hint.chars().all(|c| c.is_ascii_digit());
        if is_digit {
            if let Ok(n) = hint.parse::<i64>() {
                if let Some(i) = teams.iter().position(|t| t.team_id == n) {
                    return Ok(i);
                }
            }
        }
        let lower = hint.to_lowercase();
        if let Some(i) = teams
            .iter()
            .position(|t| t.team_name.to_lowercase().contains(&lower) || lower == t.team_abbrev.to_lowercase())
        {
            return Ok(i);
        }
    }
    if let Some(swid) = swid.filter(|s| !s.is_empty()) {
        let target = norm_swid(Some(&Value::String(swid.to_string())));
        for (i, t) in teams.iter().enumerate() {
            for o in &t.owners {
                if o.is_object() && norm_swid(o.get("id")) == target {
                    return Ok(i);
                }
            }
        }
    }
    let listing = teams.iter().map(|t| format!("{}={}", t.team_id, t.team_name)).collect::<Vec<_>>().join(", ");
    Err(ProviderError::Message(format!("Could not identify your ESPN team. Set ESPN_TEAM to one of: {listing}")))
}

/// Raw league JSON (teams with rosters) -> {player id: ownership_fields} per rostered player.
fn ownership_from_league(raw: &Value) -> HashMap<String, Ownership> {
    let mut out = HashMap::new();
    for team in get_lenient(raw, "teams").and_then(Value::as_array).into_iter().flatten() {
        let entries = get_lenient(team, "roster").and_then(|r| get_lenient(r, "entries")).and_then(Value::as_array);
        for entry in entries.into_iter().flatten() {
            let Some(pl) = get_lenient(entry, "playerPoolEntry").and_then(|p| get_lenient(p, "player")) else { continue };
            let own = ownership_fields(Some(pl));
            if let Some(id) = get_lenient(pl, "id").filter(|v| !v.is_null()) {
                if own.any() {
                    out.insert(py_str(id), own);
                }
            }
        }
    }
    out
}

/// X-Fantasy-Filter for the leaguedefaults pool sorted by % rostered change (risers first, or
/// fallers first with `fallers`). Stats are limited to the season total.
pub fn trending_filter(year: i64, limit: i64, fallers: bool) -> Value {
    serde_json::json!({"players": {"sortPercChanged": {"sortPriority": 1, "sortAsc": fallers},
                                   "limit": limit,
                                   "filterStatsForTopScoringPeriodIds": {"value": 1, "additionalValue": [format!("00{year}")]}}})
}

/// leaguedefaults `kona_player_info` JSON -> player-lite rows, largest % rostered rise first
/// (only rises > 0), or largest fall first with `fallers` (only falls < 0).
///
/// Each row: cid, espn_id, name, team, positions, status, pct_owned, pct_owned_change,
/// pct_started, adp, adp_change.
pub fn parse_trending(data: &Value, year: i64, limit: i64, fallers: bool) -> Vec<Map<String, Value>> {
    let entries: &[Value] = match data {
        Value::Object(m) => m.get("players").and_then(Value::as_array).map(|a| a.as_slice()).unwrap_or(&[]),
        Value::Array(a) => a,
        _ => &[],
    };
    let mut out: Vec<(f64, Map<String, Value>)> = Vec::new();
    for raw in entries {
        let empty = Value::Object(Default::default());
        let pl = get_lenient(raw, "player").filter(|p| truthy(p)).unwrap_or(&empty);
        let own = ownership_fields(Some(pl));
        let Some(chg) = own.pct_owned_change else { continue };
        if get_lenient(pl, "id").is_none_or(Value::is_null) || (if fallers { chg >= 0.0 } else { chg <= 0.0 }) {
            continue;
        }
        let Ok(ep) = api::Player::new(raw) else { continue };
        let Ok(p) = player_from_espn(&ep, year, None, Some(&own)) else { continue };
        let mut row = Map::new();
        row.insert("cid".into(), Value::from(p.cid.clone()));
        row.insert("espn_id".into(), Value::from(p.ids["espn"].clone()));
        row.insert("name".into(), Value::from(p.name.clone()));
        row.insert("team".into(), p.team.clone().map(Value::from).unwrap_or(Value::Null));
        row.insert("positions".into(), Value::from(p.positions.clone()));
        row.insert("status".into(), Value::from(p.status.as_str()));
        row.extend(own.to_map());
        out.push((chg, row));
    }
    if fallers {
        out.sort_by(|a, b| fm_pycompat::order::cmp_f64(a.0, b.0));
    } else {
        fm_pycompat::order::sort_desc_by(&mut out, |a, b| fm_pycompat::order::cmp_f64(a.0, b.0));
    }
    out.into_iter().take(limit.max(0) as usize).map(|(_, r)| r).collect()
}

// -- league activity and daily lineups (harness capture) ----------------------

/// kona_league_communication message types (espn_api ACTIVITY_MAP): adds, drops, trades.
pub const ESPN_ACTIVITY_TYPES: [i64; 6] = [178, 180, 179, 239, 181, 244];
pub const ESPN_ADD_TYPES: [i64; 2] = [178, 180];
pub const ESPN_DROP_TYPES: [i64; 3] = [179, 181, 239];
pub const ESPN_TRADE_TYPE: i64 = 244;
pub const ESPN_PAGE_SIZE: i64 = 25;
pub const ESPN_MAX_PAGES: i64 = 10;

fn espn_ts(ms: Option<&Value>) -> Option<NaiveDateTime> {
    let ms = py_float(ms?)?;
    fromtimestamp_local(ms / 1000.0)
}

/// `msg.get(key, default)`: the default only when the key is absent.
fn get_or<'a>(msg: &'a Value, key: &str, default: Option<&'a Value>) -> Option<&'a Value> {
    match msg.get(key) {
        Some(v) => Some(v),
        None => default,
    }
}

/// Raw `kona_league_communication` topics -> ActivityItems.
///
/// Team of a message (as espn_api reads it): `for` for type 239, `from` (sender) and `to`
/// (receiver) for trades (244), `to` otherwise. A trade message yields a TRADE_OUT for the
/// sender and a TRADE_IN for the receiver; all items of one topic share its id as `group_id`.
/// Unknown message types are skipped. `player_names`: NHL player id -> name.
pub fn parse_espn_activity(
    topics: &[Value],
    team_names: &HashMap<String, String>,
    player_names: &HashMap<i64, String>,
) -> anyhow::Result<Vec<ActivityItem>> {
    let mut out = Vec::new();
    for topic in topics {
        if !topic.is_object() {
            return Err(PyErr::type_err(format!("'{}' object has no attribute 'get'", api::type_name(topic))).into());
        }
        let Some(ts) = espn_ts(topic.get("date")) else { continue };
        let gid = match topic.get("id").filter(|v| truthy(v)) {
            Some(v) => py_str(v),
            None => py_str(topic.get("date").unwrap_or(&Value::Null)),
        };
        let msgs = topic.get("messages").filter(|v| truthy(v));
        for msg in msgs.and_then(Value::as_array).into_iter().flatten() {
            if !msg.is_object() {
                return Err(PyErr::type_err("message is not a dict").into());
            }
            let mt = msg.get("messageTypeId").and_then(as_int_key);
            let pid = msg.get("targetId").filter(|v| !v.is_null());
            let cid = pid.map(|p| format!("espn:{}", py_str(p)));
            let pname = pid
                .and_then(as_int_key)
                .and_then(|i| player_names.get(&i))
                .filter(|n| !n.is_empty())
                .cloned();
            let item = |action: ActivityAction, team: Option<&Value>, other: Option<&Value>| -> ActivityItem {
                let tid = team.filter(|t| !t.is_null()).map(py_str);
                ActivityItem {
                    source: "espn".into(),
                    tx_id: gid.clone(),
                    ts: PyDateTime::naive(ts),
                    team_name: team_names.get(tid.as_deref().unwrap_or("")).cloned(),
                    team_id: tid,
                    action,
                    cid: cid.clone(),
                    player_name: pname.clone(),
                    nhl_id: None,
                    group_id: Some(gid.clone()),
                    counterparty_id: other.filter(|o| !o.is_null()).map(py_str),
                }
            };
            let Some(mt) = mt else { continue };
            if mt == ESPN_TRADE_TYPE {
                let src = msg.get("from").filter(|v| !v.is_null());
                let dst = msg.get("to").filter(|v| !v.is_null());
                if src.is_some() {
                    out.push(item(ActivityAction::TradeOut, src, dst));
                }
                if dst.is_some() {
                    out.push(item(ActivityAction::TradeIn, dst, src));
                }
            } else if ESPN_ADD_TYPES.contains(&mt) {
                out.push(item(ActivityAction::Add, get_or(msg, "to", msg.get("for")), None));
            } else if ESPN_DROP_TYPES.contains(&mt) {
                let team = if mt == 239 { msg.get("for") } else { get_or(msg, "to", msg.get("for")) };
                let team = match team.filter(|t| !t.is_null()) {
                    Some(t) => Some(t),
                    None => msg.get("from"),
                };
                out.push(item(ActivityAction::Drop, team, None));
            }
        }
    }
    Ok(out)
}

/// espn_api BoxScore objects (one scoring period) -> LineupDay rows for both teams. `known`:
/// the league's team ids (a BoxScore side whose id matched a league team holds the Team object
/// in Python, which is never "empty").
pub fn parse_espn_box_lineups(box_scores: &[BoxScore], day: NaiveDate, known: &[i64]) -> Vec<LineupDay> {
    let mut out = Vec::new();
    for bs in box_scores {
        for (team, lineup) in [(&bs.home_team, &bs.home_lineup), (&bs.away_team, &bs.away_lineup)] {
            let is_team = as_int_key(team).is_some_and(|i| known.contains(&i));
            let empty = match team {
                Value::Null => true,
                Value::String(s) => s.is_empty(),
                Value::Number(n) => n.as_f64() == Some(0.0),
                Value::Bool(b) => !*b,
                _ => false,
            };
            if empty && !is_team {
                continue;
            }
            let tid = py_str(team);
            for bp in lineup {
                if bp.player.player_id.is_null() {
                    continue;
                }
                let slot = slot_name(&bp.slot_position).unwrap_or("BN");
                out.push(LineupDay {
                    team_id: tid.clone(),
                    date: day,
                    cid: format!("espn:{}", py_str(&bp.player.player_id)),
                    slot: slot.to_string(),
                    starting: slot != "BN" && slot != "IR",
                    provider_pts: Some(bp.points),
                });
            }
        }
    }
    out
}

/// Turn `[(key, value)]` query pairs into [`Params`] (a repeated key becomes a list param).
pub fn params_from_pairs(pairs: &[(String, String)]) -> Params {
    let mut out: Params = Vec::new();
    for (k, v) in pairs {
        match out.iter_mut().find(|(kk, _)| kk == k) {
            Some((_, ParamValue::List(l))) => l.push(v.clone()),
            Some((_, slot)) => {
                let prev = match std::mem::replace(slot, ParamValue::None) {
                    ParamValue::Str(s) => s,
                    _ => String::new(),
                };
                *slot = ParamValue::List(vec![prev, v.clone()]);
            }
            None => out.push((k.clone(), ParamValue::Str(v.clone()))),
        }
    }
    out
}

// ------------------------------------------------------------------ the provider

/// (raw league JSON, raw mSettings JSON, free agents with their raw rows)
type LoadedJson = (Value, Value, Vec<(api::Player, Value)>);

pub struct EspnProvider {
    pub settings: Settings,
    pub cache: Arc<HttpCache>,
    pub fa_size: i64,
    /// Python `date.today()`
    pub today: NaiveDate,
    /// settings.acquisitionSettings (claims planner)
    pub acquisition_settings: Map<String, Value>,
    /// my team's waiverRank (1 = first claim)
    pub waiver_rank: Option<i64>,
    extras: ProviderExtras,
    warnings: Mutex<Vec<String>>,
    league: Mutex<Option<Arc<League>>>,
    /// (free-agent cids, rostered cids) of the loaded context (`trending`'s `in_league`)
    ctx_cids: Option<(HashSet<String>, HashSet<String>)>,
}

impl EspnProvider {
    pub fn new(settings: Settings, cache: Arc<HttpCache>, fa_size: i64) -> EspnProvider {
        EspnProvider {
            settings,
            cache,
            fa_size,
            today: Local::now().date_naive(),
            acquisition_settings: Map::new(),
            waiver_rank: None,
            extras: ProviderExtras { kind: "EspnProvider".into(), ..Default::default() },
            warnings: Mutex::new(Vec::new()),
            league: Mutex::new(None),
            ctx_cids: None,
        }
    }

    /// Pin the provider's `date.today()`.
    pub fn with_today(mut self, today: NaiveDate) -> EspnProvider {
        self.today = today;
        self
    }

    /// Use an already built league (Python tests set `provider._league_obj`).
    pub fn with_league(self, league: League) -> EspnProvider {
        *self.league.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(league));
        self
    }

    /// `provider.warnings` as of now (load-time warnings plus later remote-call warnings).
    pub fn warnings_now(&self) -> Vec<String> {
        self.warnings.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The loaded espn_api league, if any (`provider._league_obj`).
    pub fn league_obj(&self) -> Option<Arc<League>> {
        self.league.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn warn(&self, msg: String) {
        self.warnings.lock().unwrap_or_else(|e| e.into_inner()).push(msg);
    }

    fn league_cached(&self) -> anyhow::Result<Arc<League>> {
        if let Some(l) = self.league_obj() {
            return Ok(l);
        }
        let l = Arc::new(self.league_new()?);
        *self.league.lock().unwrap_or_else(|e| e.into_inner()) = Some(l.clone());
        Ok(l)
    }

    fn activity_page(league: &League, size: i64, offset: i64) -> anyhow::Result<Vec<Value>> {
        let filters = serde_json::json!({"topics": {"filterType": {"value": ["ACTIVITY_TRANSACTIONS"]}, "limit": size,
                                                    "limitPerMessageSet": {"value": 25}, "offset": offset,
                                                    "sortMessageDate": {"sortPriority": 1, "sortAsc": false},
                                                    "sortFor": {"sortPriority": 2, "sortAsc": false},
                                                    "filterIncludeMessageTypeIds": {"value": ESPN_ACTIVITY_TYPES}}});
        let data = league.espn_request.league_get(
            &api::view_param("kona_league_communication"),
            &filter_header(&filters),
            "/communication/",
        )?;
        Ok(match get_lenient(&data, "topics") {
            Some(Value::Array(a)) => a.clone(),
            Some(v) if truthy(v) => {
                return Err(PyErr::type_err(format!("'{}' object is not iterable", api::type_name(v))).into())
            }
            _ => vec![],
        })
    }

    /// League adds / drops / trades, newest first, paging `offset` until items are older than
    /// `since` (at most ESPN_MAX_PAGES pages of ESPN_PAGE_SIZE). Best effort.
    pub fn activity(&self, since: Option<NaiveDate>) -> Vec<ActivityItem> {
        let league = match self.league_cached() {
            Ok(l) => l,
            Err(e) => {
                self.warn(format!("ESPN activity unavailable: {e}"));
                return vec![];
            }
        };
        let team_names: HashMap<String, String> =
            league.teams.iter().map(|t| (t.team_id.to_string(), t.team_name.clone())).collect();
        let mut items = Vec::new();
        let mut seen: HashSet<(String, ActivityAction, Option<String>, Option<String>)> = HashSet::new();
        for page in 0..ESPN_MAX_PAGES {
            let res = Self::activity_page(&league, ESPN_PAGE_SIZE, page * ESPN_PAGE_SIZE)
                .and_then(|topics| parse_espn_activity(&topics, &team_names, &league.player_map).map(|p| (topics, p)));
            let (topics, parsed) = match res {
                Ok(x) => x,
                Err(e) => {
                    self.warn(format!("ESPN activity page {} unavailable: {}: {e}", page + 1, exc_name(&e)));
                    break;
                }
            };
            for it in parsed {
                let key = (it.tx_id.clone(), it.action, it.cid.clone(), it.team_id.clone());
                if since.is_some_and(|s| it.ts.date() < s) || seen.contains(&key) {
                    continue;
                }
                seen.insert(key);
                items.push(it);
            }
            if (topics.len() as i64) < ESPN_PAGE_SIZE {
                break;
            }
            let oldest = topics.iter().filter_map(|t| espn_ts(get_lenient(t, "date"))).min();
            if let (Some(s), Some(o)) = (since, oldest) {
                if o.date() < s {
                    break;
                }
            }
        }
        items
    }

    /// ESPN hockey scoring period (one per day) of `day`, from the league's current one.
    pub fn scoring_period_for(&self, day: NaiveDate, today: Option<NaiveDate>) -> Option<i64> {
        let cur = match self.league_cached() {
            Ok(l) => l.current_week,
            Err(e) => {
                self.warn(format!("ESPN scoring period unavailable: {e}"));
                return None;
            }
        };
        Some(cur - (today.unwrap_or(self.today) - day).num_days())
    }

    /// Every team's lineup (slot, starting, ESPN points) for one scoring period (a day). `day`
    /// labels the rows (default: derived from the current scoring period). Best effort.
    pub fn box_scores(&self, scoring_period: i64, day: Option<NaiveDate>) -> Vec<LineupDay> {
        if scoring_period < 1 {
            return vec![];
        }
        let res = (|| -> anyhow::Result<Vec<LineupDay>> {
            let league = self.league_cached()?;
            let boxes = league.box_scores(None, Some(scoring_period), false)?;
            let day = day.unwrap_or_else(|| self.today - Duration::days(league.current_week - scoring_period));
            let known: Vec<i64> = league.teams.iter().map(|t| t.team_id).collect();
            Ok(parse_espn_box_lineups(&boxes, day, &known))
        })();
        match res {
            Ok(rows) => rows,
            Err(e) => {
                self.warn(format!(
                    "ESPN box scores unavailable (period {scoring_period}): {}: {e}",
                    exc_name(&e)
                ));
                vec![]
            }
        }
    }

    /// ESPN-wide % rostered risers (or fallers) as player-lite rows (`parse_trending`) from the
    /// public leaguedefaults pool, through the HTTP cache (TTL 6h, no cookies). Once a league is
    /// loaded each row also gets `in_league`: "fa", "rostered" or None. Best effort.
    pub fn trending(&self, limit: i64, fallers: bool) -> Vec<Map<String, Value>> {
        let year = self.settings.espn_year.filter(|&y| y != 0).unwrap_or(self.today.year() as i64 + 1);
        let res = (|| -> anyhow::Result<Vec<Map<String, Value>>> {
            let headers = filter_header(&trending_filter(year, TRENDING_POOL, fallers));
            let data = self.cache.get_json(&trending_url(year), &api::view_param("kona_player_info"), &headers, &[], TRENDING_TTL)?;
            Ok(parse_trending(&data, year, limit, fallers))
        })();
        let mut out = match res {
            Ok(o) => o,
            Err(e) => {
                self.warn(format!("ESPN trending players unavailable: {}: {e}", exc_name(&e)));
                return vec![];
            }
        };
        if let Some((fa, rostered)) = &self.ctx_cids {
            for d in &mut out {
                let cid = d.get("cid").and_then(Value::as_str).unwrap_or("").to_string();
                let v = if fa.contains(&cid) {
                    Value::from("fa")
                } else if rostered.contains(&cid) {
                    Value::from("rostered")
                } else {
                    Value::Null
                };
                d.insert("in_league".into(), v);
            }
        }
        out
    }

    fn league_new(&self) -> anyhow::Result<League> {
        let s = &self.settings;
        let Some(league_id) = s.espn_league_id.filter(|&i| i != 0) else {
            return Err(ProviderError::Message(
                "ESPN_LEAGUE_ID is not set. Copy .env.example to .env and fill it in.".into(),
            )
            .into());
        };
        let year = s.espn_year.ok_or_else(|| PyErr::type_err("int() argument must be a string or a real number, not 'NoneType'"))?;
        let s2 = s.espn_s2.as_ref().map(|x| x.expose());
        let swid = s.espn_swid.as_ref().map(|x| x.expose());
        let req = EspnFantasyRequests::new(year, league_id, s2, swid, self.cache.clone(), ESPN_TTL);
        League::fetch(req).map_err(|e| {
            if let Some(x) = e.downcast_ref::<EspnError>() {
                let msg = match x {
                    EspnError::AccessDenied(m) => {
                        let both = s2.is_some_and(|v| !v.is_empty()) && swid.is_some_and(|v| !v.is_empty());
                        let hint = if both { "" } else { " ESPN_S2 and ESPN_SWID are required for private leagues." };
                        format!("ESPN denied access: {m}.{hint} See README for how to copy the cookies.")
                    }
                    EspnError::InvalidLeague(m) => format!("{m} (season {year}). Check ESPN_LEAGUE_ID / ESPN_YEAR."),
                    EspnError::UnknownError(m) => format!("ESPN request failed: {m}"),
                };
                return ProviderError::Message(msg).into();
            }
            if let Some(m) = e.downcast_ref::<CacheMiss>() {
                return ProviderError::Message(format!("Offline mode and ESPN data not cached: {m}")).into();
            }
            e
        })
    }

    /// Mirror of League.free_agents that also keeps the raw JSON (for ownership %).
    fn free_agents_raw(&self, league: &League, position: &str) -> anyhow::Result<Vec<(api::Player, Value)>> {
        let slot = api::position_id(position).ok_or_else(|| PyErr::key(position))?;
        let filters = serde_json::json!({"players": {"filterStatus": {"value": ["FREEAGENT", "WAIVERS"]},
                                                     "filterSlotIds": {"value": [slot]},
                                                     "limit": self.fa_size,
                                                     "sortPercOwned": {"sortPriority": 1, "sortAsc": false},
                                                     "sortDraftRanks": {"sortPriority": 100, "sortAsc": true, "value": "STANDARD"}}});
        let params: Params = vec![
            ("view".into(), ParamValue::Str("kona_player_info".into())),
            ("scoringPeriodId".into(), ParamValue::Str(league.current_week.to_string())),
        ];
        let data = league.espn_request.league_get(&params, &filter_header(&filters), "")?;
        let players = api::get(&data, "players")?.cloned().unwrap_or(Value::Array(vec![]));
        let mut out = Vec::new();
        for raw in players.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            out.push((api::Player::new(raw)?, raw.clone()));
        }
        Ok(out)
    }

    /// `load()`: league settings, rosters, free agents and the transaction budget.
    pub fn load_context(&mut self) -> anyhow::Result<LeagueContext> {
        let league = Arc::new(self.league_new()?);
        *self.league.lock().unwrap_or_else(|e| e.into_inner()) = Some(league.clone());
        let year = self.settings.espn_year.unwrap_or(league.year);
        let fetched = (|| -> anyhow::Result<LoadedJson> {
            let raw_league = league.espn_request.get_league()?; // cache hit: same request League just made
            let raw_settings = league.espn_request.league_get(&api::view_param("mSettings"), &[], "")?;
            let mut fa_raw = Vec::new();
            for pos in FA_POSITIONS {
                fa_raw.extend(self.free_agents_raw(&league, pos)?);
            }
            Ok((raw_league, raw_settings, fa_raw))
        })();
        let (raw_league, raw_settings, fa_raw) = fetched.map_err(|e| match e.downcast_ref::<CacheMiss>() {
            Some(m) => ProviderError::Message(format!("Offline mode and ESPN data not cached: {m}")).into(),
            None => e,
        })?;

        let owned = ownership_from_league(&raw_league);
        let roster_settings = get_lenient(&raw_settings, "settings")
            .and_then(|s| get_lenient(s, "rosterSettings"))
            .filter(|r| truthy(r));
        let counts = roster_settings.and_then(|r| get_lenient(r, "lineupSlotCounts"));

        let swid = self.settings.espn_swid.as_ref().map(|x| x.expose().to_string());
        let mine = find_my_team(&league.teams, swid.as_deref(), self.settings.espn_team.as_deref())?;
        let mut teams = Vec::new();
        for (i, t) in league.teams.iter().enumerate() {
            let mut slots = Vec::new();
            for p in &t.roster {
                let slot = slot_name(&p.lineup_slot).unwrap_or("BN");
                let own = owned.get(&py_str(&p.player_id));
                slots.push(RosterSlot {
                    slot: slot.to_string(),
                    player: Some(player_from_espn(p, year, None, own)?),
                    starting: slot != "BN" && slot != "IR",
                });
            }
            teams.push(FantasyTeam {
                team_id: t.team_id.to_string(),
                name: t.team_name.clone(),
                owner_is_me: i == mine,
                slots,
                record: Some((t.wins, t.losses, t.ties)),
            });
        }

        let mut fas: Map<String, Player> = Map::new();
        for (ep, raw) in &fa_raw {
            let own = ownership_fields(Some(get_lenient(raw, "player").unwrap_or(&Value::Null)));
            let mut pl = player_from_espn(ep, year, None, Some(&own))?;
            pl.waiver_until = waiver_until_from_raw(raw);
            fas.entry(pl.cid.clone()).or_insert(pl);
        }

        let st = &league.settings;
        let name = match &st.name {
            Value::String(s) => s.clone(),
            other => py_str(other),
        };
        let items: Vec<Value> = st
            .raw_scoring_settings
            .get("scoringItems")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let scoring_type = st.scoring_type.as_ref().filter(|v| !v.is_null()).map(py_str);
        let mut ctx = new_context(
            self.settings.espn_league_id.map(|i| i.to_string()).unwrap_or_default(),
            year,
            name,
            scoring_from_settings(scoring_type.as_deref(), &items)?,
            roster_shape_from_counts(counts)?,
            teams,
            fas.into_values().collect(),
            as_int_key(&league.current_matchup_period),
            self.today,
        );
        ctx.position_limits = position_limits_from_settings(roster_settings);
        ctx.max_roster_size = max_roster_from_counts(counts)?;
        ctx.stat_limits = Some(stat_limits_from_settings(roster_settings));

        let n_before = self.warnings_now().len();
        let my_team_id = league.teams[mine].team_id.to_string();
        self.apply_budget(&mut ctx, &league, &raw_league, &raw_settings, &my_team_id);
        ctx.warnings.extend(self.warnings_now().into_iter().skip(n_before));

        self.ctx_cids = Some((
            ctx.free_agents.iter().map(|p| p.cid.clone()).collect(),
            ctx.teams.iter().flat_map(|t| t.players_iter().map(|p| p.cid.clone())).collect(),
        ));
        self.extras = ProviderExtras {
            kind: "EspnProvider".into(),
            warnings: self.warnings_now(),
            acquisition_settings: self.acquisition_settings.clone(),
            waiver_rank: self.waiver_rank,
            espn_league: Some(league_meta(&league)),
            ..Default::default()
        };
        Ok(ctx)
    }

    /// Fill the context's transaction budget (moves_* / period_* / recent_adds /
    /// faab_remaining) from settings.acquisitionSettings, my team's transactionCounter and the
    /// activity feed. Best effort: failures become warnings.
    fn apply_budget(&mut self, ctx: &mut LeagueContext, league: &League, raw_league: &Value, raw_settings: &Value, my_team_id: &str) {
        if let Err(e) = self.apply_budget_inner(ctx, league, raw_league, raw_settings, my_team_id) {
            self.warn(format!("ESPN move limits unavailable: {}: {e}", exc_name(&e)));
        }
    }

    fn apply_budget_inner(
        &mut self,
        ctx: &mut LeagueContext,
        league: &League,
        raw_league: &Value,
        raw_settings: &Value,
        my_team_id: &str,
    ) -> anyhow::Result<()> {
        let empty = Value::Object(Default::default());
        let st = get_lenient(raw_settings, "settings").filter(|s| truthy(s)).unwrap_or(&empty);
        let acq = get_lenient(st, "acquisitionSettings").filter(|a| truthy(a)).unwrap_or(&empty);
        self.acquisition_settings = acq.as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
        let status = get_lenient(raw_settings, "status")
            .filter(|s| truthy(s))
            .or_else(|| get_lenient(raw_league, "status").filter(|s| truthy(s)))
            .unwrap_or(&empty);
        let today = ctx.as_of;
        let cur_mp_v = get_lenient(status, "currentMatchupPeriod")
            .filter(|v| truthy(v))
            .unwrap_or(&league.current_matchup_period);
        let cur_mp = match cur_mp_v {
            Value::Number(n) if n.as_f64().is_some_and(|x| x != 0.0) => py_int(cur_mp_v),
            _ => None,
        };
        let n_periods = get_lenient(st, "scheduleSettings")
            .and_then(|s| get_lenient(s, "matchupPeriods"))
            .map(|m| match m {
                Value::Object(o) => o.len() as i64,
                Value::Array(a) => a.len() as i64,
                _ => 0,
            })
            .filter(|&n| n != 0);
        let final_v = get_lenient(status, "finalScoringPeriod").filter(|v| truthy(v)).cloned();
        let final_sp: Option<i64> = match &final_v {
            Some(v) => v.as_i64(),
            None => Some(league.final_scoring_period),
        };
        let limits = acquisition_limits(Some(acq), None);
        let mut bounds = None;
        if limits.per_period.is_some() || limits.season.is_some() {
            let mut opening = None;
            let mut gpd: Map<NaiveDate, i64> = Map::new();
            match league.espn_request.get_pro_schedule() {
                Ok(data) => {
                    let (dates, g) = espn_scoring_dates(Some(&data));
                    gpd = g;
                    opening = dates.get(&1).copied();
                }
                Err(e) => self.warn(format!(
                    "ESPN pro schedule unavailable ({}); move-limit period dates are approximate",
                    exc_name(&e)
                )),
            }
            let sp = league.current_week;
            if opening.is_none() && sp > 1 {
                opening = Some(today - Duration::days(sp - 1));
            }
            if let Some(op) = opening {
                bounds = espn_period_bounds(op, final_sp, n_periods, today, Some(&gpd), cur_mp);
            }
        }
        let bounds = bounds.unwrap_or_else(|| {
            let start = monday(today);
            (start, start + Duration::days(6))
        });
        ctx.period_start = Some(bounds.0);
        ctx.period_end = Some(bounds.1);
        let limits = acquisition_limits(Some(acq), Some((bounds.1 - bounds.0).num_days() + 1));
        ctx.moves_limit_per_period = limits.per_period;
        ctx.moves_limit_season = limits.season;
        ctx.moves_period_label = Some("matchup period".into());
        let team_json = get_lenient(raw_league, "teams")
            .and_then(Value::as_array)
            .and_then(|ts| ts.iter().find(|t| py_str(get_lenient(t, "id").unwrap_or(&Value::Null)) == my_team_id));
        self.waiver_rank = team_json.and_then(|t| get_lenient(t, "waiverRank")).and_then(as_int_key);
        let (ctr_period, ctr_season, spent) = transaction_counter(team_json, cur_mp);
        if let Some(b) = limits.faab_budget {
            ctx.faab_remaining = Some(b - spent.unwrap_or(0.0));
        }
        let since = bounds.0.min(today - Duration::days(RECENT_ADDS_WINDOW));
        let items = self.activity(Some(since));
        ctx.recent_adds = recent_adds_from(&items, my_team_id, today, RECENT_ADDS_WINDOW);
        let from_feed = count_adds(&items, my_team_id, Some(bounds.0), Some(bounds.1)) as i64;
        ctx.moves_used_this_period = Some(ctr_period.map_or(from_feed, |c| c.max(from_feed)));
        ctx.moves_used_season = ctr_season;
        if let Some(c) = ctr_period {
            if c != from_feed && ctx.moves_limit_per_period.is_some() {
                ctx.source_notes.push(format!(
                    "ESPN moves: the league counter shows {c} acquisition{} this matchup period, the activity feed \
                     {from_feed} add{} since {} {}; using {} (the more conservative)",
                    if c != 1 { "s" } else { "" },
                    if from_feed != 1 { "s" } else { "" },
                    bounds.0.format("%b"),
                    bounds.0.day(),
                    ctx.moves_used_this_period.unwrap_or(0),
                ));
            }
        }
        Ok(())
    }
}

/// The espn_api League attributes compute code reads (`snapshot.provider.espn_league`).
pub fn league_meta(league: &League) -> EspnLeagueMeta {
    let st = &league.settings;
    let matchup_periods: Map<String, Vec<i64>> = st
        .matchup_periods
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_array().map(|a| a.iter().filter_map(as_int_key).collect()).unwrap_or_default()))
                .collect()
        })
        .unwrap_or_default();
    EspnLeagueMeta {
        current_week: Some(league.current_week),
        current_matchup_period: as_int_key(&league.current_matchup_period),
        final_scoring_period: Some(league.final_scoring_period),
        scoring_period_id: Some(league.scoring_period_id),
        settings: EspnLeagueSettings {
            matchup_periods,
            reg_season_count: as_int_key(&st.reg_season_count),
            playoff_team_count: as_int_key(&st.playoff_team_count),
            trade_deadline: as_int_key(&st.trade_deadline),
            team_count: as_int_key(&st.team_count),
            playoff_matchup_period_length: as_int_key(&st.playoff_matchup_period_length),
            name: st.name.as_str().map(str::to_string),
        },
    }
}

impl LeagueProvider for EspnProvider {
    fn name(&self) -> &str {
        "espn"
    }

    fn load(&mut self) -> anyhow::Result<LeagueContext> {
        self.load_context()
    }

    fn extras(&self) -> &ProviderExtras {
        &self.extras
    }

    fn activity(&self, since: Option<NaiveDate>) -> anyhow::Result<Vec<ActivityItem>> {
        Ok(EspnProvider::activity(self, since))
    }

    fn box_scores(&self, scoring_period: i64, day: Option<NaiveDate>) -> anyhow::Result<Vec<LineupDay>> {
        Ok(EspnProvider::box_scores(self, scoring_period, day))
    }

    fn scoring_period_for(&self, day: NaiveDate, today: Option<NaiveDate>) -> Option<i64> {
        EspnProvider::scoring_period_for(self, day, today)
    }

    fn trending(&self, limit: usize, fallers: bool) -> anyhow::Result<Vec<Map<String, Value>>> {
        Ok(EspnProvider::trending(self, limit as i64, fallers))
    }

    fn espn_league_get(&self, params: &[(String, String)]) -> anyhow::Result<Value> {
        let league = self.league_cached()?;
        league.espn_request.league_get(&params_from_pairs(params), &[], "")
    }

    fn espn_scoreboard(&self, matchup_period: i64) -> anyhow::Result<Vec<Map<String, Value>>> {
        let league = self.league_cached()?;
        let num = |v: &Value| match v {
            Value::Number(n) => n.as_f64().map(Value::from).unwrap_or(Value::Null),
            other => other.clone(),
        };
        Ok(league
            .scoreboard(Some(matchup_period))?
            .into_iter()
            .map(|m| {
                let mut row = Map::new();
                row.insert("home_team".into(), m.home_team.clone());
                row.insert("away_team".into(), m.away_team.clone());
                row.insert("home_score".into(), num(&m.home_final_score));
                row.insert("away_score".into(), num(&m.away_final_score));
                row
            })
            .collect())
    }
}
