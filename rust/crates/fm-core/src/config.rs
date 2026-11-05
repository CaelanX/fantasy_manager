//! `config.py`: runtime settings from the environment / `.env`. The struct is COMPLETE;
//! `Settings::load` is implemented by work package `W2-core-runtime`.

use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;

use chrono::{Datelike, NaiveDate};

use crate::models::DynastyMode;
use crate::{CoreError, Map};

pub const GOALIE_PREFIX: &str = "goalie.";

/// ESPN labels a season by the year it ends: Sep 2026 onward -> 2027.
pub fn default_espn_year(today: NaiveDate) -> i64 {
    if today.month() >= 9 {
        today.year() as i64 + 1
    } else {
        today.year() as i64
    }
}

/// `parse_points("G=3,A=2,goalie.G=20")` -> {stat: points} (aliases FIGHTS/FTS -> FT, keys
/// uppercased, `goalie.` prefix kept). Errors like Python's ValueError.
pub fn parse_points(raw: &str) -> Result<Map<String, f64>, CoreError> {
    let mut out = Map::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some((key, val)) = part.split_once('=') else {
            return Err(CoreError::Value(format!("bad points entry {part:?}; expected STAT=value")));
        };
        let mut k = key.trim();
        let mut prefix = "";
        if k.to_lowercase().starts_with(GOALIE_PREFIX) {
            prefix = GOALIE_PREFIX;
            k = &k[GOALIE_PREFIX.len()..];
        }
        let k = k.trim().to_uppercase();
        if k.is_empty() {
            return Err(CoreError::Value(format!("bad points entry {key:?}; expected STAT=value")));
        }
        let k = match k.as_str() {
            "FIGHTS" | "FTS" => "FT".to_string(),
            _ => k,
        };
        let v: f64 = val
            .trim()
            .parse()
            .map_err(|_| CoreError::Value(format!("could not convert string to float: {:?}", val.trim())))?;
        out.insert(format!("{prefix}{k}"), v);
    }
    Ok(out)
}

/// {stat: pts} with optional `goalie.` keys -> (base weights, goalie-group overrides).
pub fn split_points(points: &Map<String, f64>) -> (Map<String, f64>, Map<String, f64>) {
    let mut base = Map::new();
    let mut goalie = Map::new();
    for (k, v) in points {
        if let Some(stat) = k.strip_prefix(GOALIE_PREFIX) {
            goalie.insert(stat.to_string(), *v);
        } else {
            base.insert(k.clone(), *v);
        }
    }
    (base, goalie)
}

/// Inverse of parse_points: "G=4,A=2,...,goalie.G=20" (values with Python `:g`).
pub fn format_points(weights: &Map<String, f64>, goalie_weights: Option<&Map<String, f64>>) -> String {
    let mut parts: Vec<String> =
        weights.iter().map(|(k, v)| format!("{k}={}", fm_pycompat::fmt::general(*v, None, false))).collect();
    if let Some(g) = goalie_weights {
        parts.extend(g.iter().map(|(k, v)| format!("{GOALIE_PREFIX}{k}={}", fm_pycompat::fmt::general(*v, None, false))));
    }
    parts.join(",")
}

/// `pydantic.SecretStr`: masked in Debug/Display, excluded from any dump.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Secret(s.into())
    }
    /// The plain value. Never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret('**********')")
    }
}

/// `config.secret_value(v)`: '' when unset.
pub fn secret_value(v: Option<&Secret>) -> String {
    v.map(|s| s.expose().to_string()).unwrap_or_default()
}

/// `config.Settings`. Field names = Python field names = lowercased env var names.
#[derive(Clone, Debug)]
pub struct Settings {
    // ESPN
    pub espn_league_id: Option<i64>,
    /// filled from `default_espn_year(today)` when unset
    pub espn_year: Option<i64>,
    pub espn_s2: Option<Secret>,
    pub espn_swid: Option<Secret>,
    pub espn_team: Option<String>,
    // Fantrax
    pub fantrax_league_id: Option<String>,
    pub fantrax_cookie: Option<Secret>,
    pub fantrax_username: Option<Secret>,
    pub fantrax_password: Option<Secret>,
    pub fantrax_cookie_file: Option<PathBuf>,
    pub fantrax_team: Option<String>,
    /// parsed by `parse_points` (goalie.* keys kept)
    pub fantrax_points: Map<String, f64>,
    pub fantrax_dynasty: bool,
    pub fantrax_keeper_horizon_years: i64,
    pub fantrax_keepers: Option<i64>,
    pub fantrax_mode: DynastyMode,
    // LLM / notifications
    pub openrouter_api_key: Option<Secret>,
    pub fm_llm_model: String,
    pub fm_llm_fallbacks: String,
    pub fm_llm_allow_paid: bool,
    pub discord_webhook_url: Option<Secret>,
    pub slack_webhook_url: Option<Secret>,
    // odds
    pub odds_api_key: Option<Secret>,
    pub odds_region: String,
    // runtime
    pub fm_data_dir: PathBuf,
    pub fm_offline: bool,
    // web
    pub fm_web_password: Option<Secret>,
    pub fm_web_secret: Option<Secret>,
    pub fm_web_allow_insecure: bool,
    pub fm_web_session_days: i64,
    pub fm_web_trusted_proxies: String,
    /// pydantic `model_fields_set`: fields given explicitly by env/.env (prefs.py checks
    /// `"fantrax_mode" in settings.model_fields_set`).
    pub fields_set: BTreeSet<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            espn_league_id: None,
            espn_year: None,
            espn_s2: None,
            espn_swid: None,
            espn_team: None,
            fantrax_league_id: None,
            fantrax_cookie: None,
            fantrax_username: None,
            fantrax_password: None,
            fantrax_cookie_file: None,
            fantrax_team: None,
            fantrax_points: Map::new(),
            fantrax_dynasty: false,
            fantrax_keeper_horizon_years: 3,
            fantrax_keepers: None,
            fantrax_mode: DynastyMode::Balanced,
            openrouter_api_key: None,
            fm_llm_model: "openrouter/free".into(),
            fm_llm_fallbacks: String::new(),
            fm_llm_allow_paid: false,
            discord_webhook_url: None,
            slack_webhook_url: None,
            odds_api_key: None,
            odds_region: "us".into(),
            fm_data_dir: PathBuf::from("./data"),
            fm_offline: false,
            fm_web_password: None,
            fm_web_secret: None,
            fm_web_allow_insecure: false,
            fm_web_session_days: 30,
            fm_web_trusted_proxies: "127.0.0.1,::1".into(),
            fields_set: BTreeSet::new(),
        }
    }
}

impl Settings {
    /// pydantic-settings semantics: `.env` in the cwd (utf-8), then process env overrides,
    /// case-insensitive names, empty values ignored, unknown keys ignored; bools accept
    /// 1/0/true/false/yes/no/on/off/t/f/y/n; `fantrax_mode` stripped + lowercased;
    /// `fantrax_points` via `parse_points`; `espn_year` defaulted from `today`.
    pub fn load(_today: NaiveDate) -> anyhow::Result<Settings> {
        todo!("W2-core-runtime")
    }
}
