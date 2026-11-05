//! Port of `fantasy_manager/providers/injuries.py`: NHL injury report from ESPN's public site API.

use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use chrono::NaiveDate;
use fm_core::PyDateTime;
use fm_http::{FetchJson, HttpCache};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const INJURIES_URL: &str = "https://site.api.espn.com/apis/site/v2/sports/hockey/nhl/injuries";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InjuryStatus {
    Dtd,
    Out,
    Ir,
    Ltir,
    Suspended,
    Unknown,
}

impl InjuryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            InjuryStatus::Dtd => "dtd",
            InjuryStatus::Out => "out",
            InjuryStatus::Ir => "ir",
            InjuryStatus::Ltir => "ltir",
            InjuryStatus::Suspended => "suspended",
            InjuryStatus::Unknown => "unknown",
        }
    }
}

/// ESPN team display name -> NHL abbreviation.
pub const ESPN_TEAM_TO_NHL: &[(&str, &str)] = &[
    ("Anaheim Ducks", "ANA"),
    ("Boston Bruins", "BOS"),
    ("Buffalo Sabres", "BUF"),
    ("Calgary Flames", "CGY"),
    ("Carolina Hurricanes", "CAR"),
    ("Chicago Blackhawks", "CHI"),
    ("Colorado Avalanche", "COL"),
    ("Columbus Blue Jackets", "CBJ"),
    ("Dallas Stars", "DAL"),
    ("Detroit Red Wings", "DET"),
    ("Edmonton Oilers", "EDM"),
    ("Florida Panthers", "FLA"),
    ("Los Angeles Kings", "LAK"),
    ("Minnesota Wild", "MIN"),
    ("Montreal Canadiens", "MTL"),
    ("Montr\u{e9}al Canadiens", "MTL"),
    ("Nashville Predators", "NSH"),
    ("New Jersey Devils", "NJD"),
    ("New York Islanders", "NYI"),
    ("New York Rangers", "NYR"),
    ("Ottawa Senators", "OTT"),
    ("Philadelphia Flyers", "PHI"),
    ("Pittsburgh Penguins", "PIT"),
    ("San Jose Sharks", "SJS"),
    ("Seattle Kraken", "SEA"),
    ("St. Louis Blues", "STL"),
    ("Tampa Bay Lightning", "TBL"),
    ("Toronto Maple Leafs", "TOR"),
    ("Utah Mammoth", "UTA"),
    ("Utah Hockey Club", "UTA"),
    ("Utah HC", "UTA"),
    ("Vancouver Canucks", "VAN"),
    ("Vegas Golden Knights", "VGK"),
    ("Washington Capitals", "WSH"),
    ("Winnipeg Jets", "WPG"),
];

/// ESPN team abbreviation -> NHL abbreviation (only where they differ).
pub const ESPN_ABBREV_TO_NHL: &[(&str, &str)] =
    &[("NJ", "NJD"), ("SJ", "SJS"), ("TB", "TBL"), ("LA", "LAK"), ("UTAH", "UTA"), ("WAS", "WSH")];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InjuryReport {
    pub player_name: String,
    #[serde(default)]
    pub espn_player_id: Option<i64>,
    /// ESPN team group the entry was listed under
    #[serde(default)]
    pub team_name: Option<String>,
    /// NHL abbreviation (athlete's own team if given)
    #[serde(default)]
    pub team: Option<String>,
    /// ESPN abbreviation, e.g. LW / D / G
    #[serde(default)]
    pub position: Option<String>,
    pub status_raw: String,
    pub status: InjuryStatus,
    /// e.g. "Upper Body", "Hip", "Suspension"
    #[serde(default)]
    pub injury_type: Option<String>,
    /// ESPN's estimated return
    #[serde(default)]
    pub return_date: Option<NaiveDate>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub updated: Option<PyDateTime>,
}

impl InjuryReport {
    pub fn is_out(&self) -> bool {
        matches!(self.status, InjuryStatus::Out | InjuryStatus::Ir | InjuryStatus::Ltir | InjuryStatus::Suspended)
    }
}

/// Drop the cached ESPN injury feed so the next load reads it live. Returns how many entries
/// were removed.
pub fn clear_cache(cache: &HttpCache) -> Result<usize> {
    cache.clear(Some(INJURIES_URL))
}

/// Uncached GET returning parsed JSON (`default_fetch_json`).
pub fn default_fetch_json(url: &str, _params: &fm_http::Params, _ttl: f64) -> Result<Value> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent("fantasy-manager/0.1")
        .build()?;
    Ok(client.get(url).send()?.error_for_status()?.json()?)
}

fn re_ir() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\bir\b").unwrap())
}
fn re_dtd() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\bdtd\b").unwrap())
}
fn re_out() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\bout\b").unwrap())
}
fn re_id() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"/id/(\d+)").unwrap())
}
fn re_uid() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"a:(\d+)").unwrap())
}

/// Map ESPN's status text (or its INJURY_STATUS_* type name) to a canonical status.
pub fn map_status(raw: Option<&str>, type_name: Option<&str>) -> InjuryStatus {
    let parts: Vec<&str> = [raw, type_name].into_iter().flatten().filter(|s| !s.is_empty()).collect();
    let text = parts.join(" ").to_lowercase().replace('_', " ");
    if text.is_empty() {
        return InjuryStatus::Unknown;
    }
    if text.contains("long-term") || text.contains("long term") || text.contains("ltir") {
        return InjuryStatus::Ltir;
    }
    if text.contains("suspen") {
        return InjuryStatus::Suspended;
    }
    if text.contains("injured reserve") || re_ir().is_match(&text) {
        return InjuryStatus::Ir;
    }
    if text.contains("day-to-day")
        || text.contains("daytoday")
        || text.contains("day to day")
        || re_dtd().is_match(&text)
        || text.contains("questionable")
        || text.contains("probable")
    {
        return InjuryStatus::Dtd;
    }
    if re_out().is_match(&text) || text.contains("doubtful") || text.contains("week-to-week") {
        return InjuryStatus::Out;
    }
    InjuryStatus::Unknown
}

pub fn team_to_nhl(display_name: Option<&str>, espn_abbrev: Option<&str>) -> Option<String> {
    if let Some(a) = espn_abbrev.filter(|a| !a.is_empty()) {
        let a = a.trim().to_uppercase();
        return Some(
            ESPN_ABBREV_TO_NHL.iter().find(|(k, _)| *k == a).map(|(_, v)| v.to_string()).unwrap_or(a),
        );
    }
    if let Some(n) = display_name.filter(|n| !n.is_empty()) {
        let n = n.trim();
        return ESPN_TEAM_TO_NHL.iter().find(|(k, _)| *k == n).map(|(_, v)| v.to_string());
    }
    None
}

fn parse_espn_dt(value: Option<&str>) -> Option<PyDateTime> {
    let v = value.filter(|v| !v.is_empty())?;
    PyDateTime::parse(&v.replace('Z', "+00:00"))
}

fn parse_espn_id(athlete: &Value) -> Option<i64> {
    if let Some(links) = athlete.get("links").and_then(Value::as_array) {
        for link in links {
            let href = link.get("href").and_then(Value::as_str).unwrap_or("");
            if let Some(m) = re_id().captures(href) {
                return m[1].parse().ok();
            }
        }
    }
    let uid = athlete.get("uid").and_then(Value::as_str).unwrap_or("");
    re_uid().captures(uid).and_then(|m| m[1].parse().ok())
}

/// `x.get(k)` as a non-empty string (Python truthiness for the `a or b` chains).
fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

pub fn parse_injuries(payload: &Value) -> Result<Vec<InjuryReport>> {
    let mut out = Vec::new();
    let Some(teams) = payload.get("injuries").and_then(Value::as_array) else { return Ok(out) };
    for team in teams {
        let team_name = s(team, "displayName");
        let Some(injs) = team.get("injuries").and_then(Value::as_array) else { continue };
        for inj in injs {
            let null = Value::Null;
            let ath = inj.get("athlete").filter(|a| a.is_object()).unwrap_or(&null);
            let name: String = match s(ath, "displayName") {
                Some(n) => n.to_string(),
                None => [s(ath, "firstName"), s(ath, "lastName")].into_iter().flatten().collect::<Vec<_>>().join(" "),
            };
            if name.is_empty() {
                continue;
            }
            let details = inj.get("details").filter(|d| d.is_object()).unwrap_or(&null);
            let typ = inj.get("type").filter(|d| d.is_object()).unwrap_or(&null);
            let type_name = s(typ, "name");
            let status_raw = s(inj, "status").or_else(|| s(typ, "description")).unwrap_or("");
            let ath_team = ath.get("team").filter(|d| d.is_object()).unwrap_or(&null);
            let comment = s(inj, "longComment").or_else(|| s(inj, "shortComment"));
            let ret = s(details, "returnDate");
            let return_date = match ret {
                Some(r) => {
                    let head: String = r.chars().take(10).collect();
                    Some(
                        NaiveDate::parse_from_str(&head, "%Y-%m-%d")
                            .map_err(|_| anyhow!("Invalid isoformat string: {r:?}"))?,
                    )
                }
                None => None,
            };
            out.push(InjuryReport {
                player_name: name,
                espn_player_id: parse_espn_id(ath),
                team_name: team_name.map(str::to_string),
                team: team_to_nhl(s(ath_team, "displayName"), s(ath_team, "abbreviation"))
                    .or_else(|| team_to_nhl(team_name, None)),
                position: ath.get("position").and_then(|p| s(p, "abbreviation")).map(str::to_string),
                status_raw: status_raw.to_string(),
                status: map_status(Some(status_raw), type_name),
                injury_type: s(details, "type").map(str::to_string),
                return_date,
                comment: comment.map(str::to_string),
                updated: parse_espn_dt(s(inj, "date")),
            });
        }
    }
    Ok(out)
}

/// `fetch_injuries(fetch_json)`; `ttl` is what the (cached) fetcher is asked for.
pub fn fetch_injuries(fetch_json: Option<&FetchJson>, ttl: f64) -> Result<Vec<InjuryReport>> {
    let payload = match fetch_json {
        Some(f) => f(INJURIES_URL, &Vec::new(), ttl)?,
        None => default_fetch_json(INJURIES_URL, &Vec::new(), ttl)?,
    };
    let payload = if payload.is_null() { Value::Object(Default::default()) } else { payload };
    parse_injuries(&payload)
}
