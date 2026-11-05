//! `config.py`: runtime settings from the environment / `.env`. The struct is COMPLETE;
//! `Settings::load` is implemented by work package `W1-core-runtime`.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

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
    pub fn load(today: NaiveDate) -> anyhow::Result<Settings> {
        let env: Vec<(String, String)> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect();
        Settings::load_from(today, Some(Path::new(".env")), env)
    }

    /// `load` with an explicit env file (`Settings(_env_file=...)`; a missing file is fine) and
    /// explicit process-env pairs. Env values override the file's; empty values are ignored in
    /// both (`env_ignore_empty`).
    pub fn load_from(
        today: NaiveDate,
        env_file: Option<&Path>,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> anyhow::Result<Settings> {
        let mut vals: HashMap<String, String> = HashMap::new();
        if let Some(path) = env_file {
            if path.is_file() {
                for item in dotenvy::from_path_iter(path)? {
                    let (k, v) = item?;
                    if !v.is_empty() {
                        vals.insert(k.to_lowercase(), v);
                    }
                }
            }
        }
        for (k, v) in env {
            if !v.is_empty() {
                vals.insert(k.to_lowercase(), v);
            }
        }
        Settings::from_values(today, &vals)
    }

    fn from_values(today: NaiveDate, vals: &HashMap<String, String>) -> anyhow::Result<Settings> {
        let mut s = Settings::default();
        let mut fs: BTreeSet<String> = BTreeSet::new();
        let mut get = |name: &str| -> Option<String> {
            let v = vals.get(name).cloned();
            if v.is_some() {
                fs.insert(name.to_string());
            }
            v
        };
        let secret = |v: Option<String>| v.map(Secret::new);

        s.espn_league_id = get("espn_league_id").map(|v| parse_int("espn_league_id", &v)).transpose()?;
        s.espn_year = get("espn_year").map(|v| parse_int("espn_year", &v)).transpose()?;
        s.espn_s2 = secret(get("espn_s2"));
        s.espn_swid = secret(get("espn_swid"));
        s.espn_team = get("espn_team");
        s.fantrax_league_id = get("fantrax_league_id");
        s.fantrax_cookie = secret(get("fantrax_cookie"));
        s.fantrax_username = secret(get("fantrax_username"));
        s.fantrax_password = secret(get("fantrax_password"));
        s.fantrax_cookie_file = get("fantrax_cookie_file").map(PathBuf::from);
        s.fantrax_team = get("fantrax_team");
        if let Some(v) = get("fantrax_points") {
            s.fantrax_points = parse_points(&v)?;
        }
        if let Some(v) = get("fantrax_dynasty") {
            s.fantrax_dynasty = parse_bool("fantrax_dynasty", &v)?;
        }
        if let Some(v) = get("fantrax_keeper_horizon_years") {
            s.fantrax_keeper_horizon_years = parse_int("fantrax_keeper_horizon_years", &v)?;
        }
        s.fantrax_keepers = get("fantrax_keepers").map(|v| parse_int("fantrax_keepers", &v)).transpose()?;
        if let Some(v) = get("fantrax_mode") {
            s.fantrax_mode = DynastyMode::parse(&v).ok_or_else(|| {
                CoreError::Value(format!(
                    "fantrax_mode: Input should be 'contend', 'balanced' or 'rebuild' [input_value={:?}]",
                    v.trim().to_lowercase()
                ))
            })?;
        }
        s.openrouter_api_key = secret(get("openrouter_api_key"));
        if let Some(v) = get("fm_llm_model") {
            s.fm_llm_model = v;
        }
        if let Some(v) = get("fm_llm_fallbacks") {
            s.fm_llm_fallbacks = v;
        }
        if let Some(v) = get("fm_llm_allow_paid") {
            s.fm_llm_allow_paid = parse_bool("fm_llm_allow_paid", &v)?;
        }
        s.discord_webhook_url = secret(get("discord_webhook_url"));
        s.slack_webhook_url = secret(get("slack_webhook_url"));
        s.odds_api_key = secret(get("odds_api_key"));
        if let Some(v) = get("odds_region") {
            s.odds_region = v;
        }
        if let Some(v) = get("fm_data_dir") {
            s.fm_data_dir = PathBuf::from(v);
        }
        if let Some(v) = get("fm_offline") {
            s.fm_offline = parse_bool("fm_offline", &v)?;
        }
        s.fm_web_password = secret(get("fm_web_password"));
        s.fm_web_secret = secret(get("fm_web_secret"));
        if let Some(v) = get("fm_web_allow_insecure") {
            s.fm_web_allow_insecure = parse_bool("fm_web_allow_insecure", &v)?;
        }
        if let Some(v) = get("fm_web_session_days") {
            s.fm_web_session_days = parse_int("fm_web_session_days", &v)?;
        }
        if let Some(v) = get("fm_web_trusted_proxies") {
            s.fm_web_trusted_proxies = v;
        }
        if s.espn_year.is_none() {
            s.espn_year = Some(default_espn_year(today));
            // pydantic: the after-validator's assignment marks the field as set
            fs.insert("espn_year".to_string());
        }
        s.fields_set = fs;
        Ok(s)
    }
}

/// pydantic lax `int` from a string: surrounding whitespace ok, `3.0` ok, `3.5` is an error.
fn parse_int(name: &str, raw: &str) -> Result<i64, CoreError> {
    let t = raw.trim();
    if let Ok(n) = t.parse::<i64>() {
        return Ok(n);
    }
    if let Ok(f) = t.parse::<f64>() {
        if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.0e18 {
            return Ok(f as i64);
        }
    }
    Err(CoreError::Value(format!("{name}: Input should be a valid integer [input_value={raw:?}]")))
}

/// pydantic lax `bool` from a string.
fn parse_bool(name: &str, raw: &str) -> Result<bool, CoreError> {
    match raw.trim().to_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => Ok(true),
        "0" | "false" | "f" | "no" | "n" | "off" => Ok(false),
        _ => Err(CoreError::Value(format!("{name}: Input should be a valid boolean [input_value={raw:?}]"))),
    }
}
