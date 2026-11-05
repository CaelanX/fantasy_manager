//! Rust port of `fantasy_manager/providers/odds.py`: The Odds API (v4) client for NHL moneylines /
//! totals, the odds math (no-vig, Poisson expected totals, team-total split), the 20-hour response
//! cache and the daily archive (`<data dir>/archive/odds-YYYY-MM-DD.json`).
//! Owner: work package `W2-news-odds-llm`. The API key travels only in the query string and is
//! never stored, logged or put in an error message.
//!
//! Ported literally: `statistics.median`, the 60-step bisections, Poisson `MAX_GOALS = 25`,
//! Python 3.12's compensated `sum` (`fm_pycompat::num::py_sum`) and the hand-written US DST rule
//! in `eastern_date`. Deviation: an error from a custom `fetch_json` that is not an
//! [`OddsError`] is reported as `"Error: <message>"` (Python used the exception's class name).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc, Weekday};
use fm_core::{Map, PyDateTime};
use fm_pycompat::num::{median, py_round_i64, py_sum, round_to};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map as JsonMap, Value};
use thiserror::Error;

use crate::news::pyshim;

pub const ODDS_URL: &str = "https://api.the-odds-api.com/v4/sports/icehockey_nhl/odds";
pub const MARKETS: &str = "h2h,totals";
pub const CACHE_FILE: &str = "odds_cache.json";
pub const CACHE_TTL_HOURS: f64 = 20.0;
pub const ARCHIVE_VERSION: i64 = 1;
pub const LOOKBACK_DAYS: i64 = 7;
pub const MAX_GOALS: usize = 25;

/// The Odds API team names -> NHL abbreviations (providers/nhl.py NHL_TEAMS).
pub const TEAM_ABBREVS: &[(&str, &str)] = &[
    ("Anaheim Ducks", "ANA"), ("Boston Bruins", "BOS"), ("Buffalo Sabres", "BUF"), ("Calgary Flames", "CGY"),
    ("Carolina Hurricanes", "CAR"), ("Chicago Blackhawks", "CHI"), ("Colorado Avalanche", "COL"),
    ("Columbus Blue Jackets", "CBJ"), ("Dallas Stars", "DAL"), ("Detroit Red Wings", "DET"),
    ("Edmonton Oilers", "EDM"), ("Florida Panthers", "FLA"), ("Los Angeles Kings", "LAK"), ("Minnesota Wild", "MIN"),
    ("Montreal Canadiens", "MTL"), ("Nashville Predators", "NSH"), ("New Jersey Devils", "NJD"),
    ("New York Islanders", "NYI"), ("New York Rangers", "NYR"), ("Ottawa Senators", "OTT"),
    ("Philadelphia Flyers", "PHI"), ("Pittsburgh Penguins", "PIT"), ("San Jose Sharks", "SJS"),
    ("Seattle Kraken", "SEA"), ("St. Louis Blues", "STL"), ("Tampa Bay Lightning", "TBL"),
    ("Toronto Maple Leafs", "TOR"), ("Utah Mammoth", "UTA"), ("Vancouver Canucks", "VAN"),
    ("Vegas Golden Knights", "VGK"), ("Washington Capitals", "WSH"), ("Winnipeg Jets", "WPG"),
];
/// Spellings seen across books / seasons (accents and punctuation are normalised away anyway).
pub const TEAM_ALIASES: &[(&str, &str)] = &[("Saint Louis Blues", "STL"), ("Utah Hockey Club", "UTA"), ("Utah HC", "UTA")];

fn norm_name(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let ascii: String = name.nfkd().filter(|c| c.is_ascii()).collect();
    ascii.to_ascii_lowercase().chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).collect()
}

/// Full team name as The Odds API writes it ("Montréal Canadiens", "St Louis Blues") -> "MTL".
pub fn team_abbrev(name: Option<&str>) -> Option<&'static str> {
    let name = name.filter(|n| !n.is_empty())?;
    let key = norm_name(name);
    // {**TEAM_ABBREVS, **TEAM_ALIASES}: a later entry wins
    TEAM_ABBREVS.iter().chain(TEAM_ALIASES.iter()).rev().find(|(k, _)| norm_name(k) == key).map(|(_, v)| *v)
}

/// An Odds API failure; the message never contains the API key.
#[derive(Debug, Error, Clone, PartialEq)]
#[error("{message}")]
pub struct OddsError {
    pub message: String,
    pub status_code: Option<u16>,
    pub headers: Map<String, String>,
}

impl OddsError {
    pub fn new(message: impl Into<String>) -> Self {
        OddsError { message: message.into(), status_code: None, headers: Map::new() }
    }
}

// --------------------------------------------------------------------------- odds math

/// Implied probability of an American price, vig included: -150 -> 0.6, +150 -> 0.4.
pub fn american_to_prob(price: f64) -> anyhow::Result<f64> {
    if price == 0.0 || (-100.0 < price && price < 100.0) {
        anyhow::bail!("not an American price: {}", fm_pycompat::fmt::repr_f64(price));
    }
    Ok(if price < 0.0 { -price / (-price + 100.0) } else { 100.0 / (price + 100.0) })
}

/// Inverse of american_to_prob (rounded): 0.6 -> -150, 0.4 -> +150, 0.5 -> +100.
pub fn prob_to_american(p: f64) -> anyhow::Result<i64> {
    if !(0.0 < p && p < 1.0) {
        anyhow::bail!("probability out of range: {}", fm_pycompat::fmt::repr_f64(p));
    }
    Ok(if p > 0.5 { py_round_i64(-100.0 * p / (1.0 - p)) } else { py_round_i64(100.0 * (1.0 - p) / p) })
}

/// Remove the bookmaker margin proportionally: (0.6, 0.45) -> (0.5714, 0.4286).
pub fn no_vig(q_a: f64, q_b: f64) -> anyhow::Result<(f64, f64)> {
    let s = q_a + q_b;
    if s <= 0.0 {
        anyhow::bail!("probabilities must be positive");
    }
    Ok((q_a / s, q_b / s))
}

fn pmf(lam: f64, n: usize) -> Vec<f64> {
    let mut out = Vec::with_capacity(n + 1);
    out.push((-lam).exp());
    for k in 1..=n {
        let prev = out[k - 1];
        out.push(prev * lam / k as f64);
    }
    out
}

/// P(home wins) with independent Poisson goals; a regulation tie is won by the home team with
/// probability lam_home / (lam_home + lam_away).
pub fn win_probability(lam_home: f64, lam_away: f64) -> f64 {
    let ph = pmf(lam_home, MAX_GOALS);
    let pa = pmf(lam_away, MAX_GOALS);
    let mut cdf_a = Vec::with_capacity(pa.len());
    let mut acc = 0.0;
    for x in &pa {
        acc += x;
        cdf_a.push(acc);
    }
    let p_win = py_sum((1..=MAX_GOALS).map(|i| ph[i] * cdf_a[i - 1]));
    let p_tie = py_sum((0..=MAX_GOALS).map(|i| ph[i] * pa[i]));
    p_win + p_tie * lam_home / (lam_home + lam_away)
}

/// Split an expected game total into (home, away) implied team totals (Poisson split, bisection,
/// shares clamped to [0.1, 0.9]).
pub fn split_total(total: f64, p_home: f64) -> anyhow::Result<(f64, f64)> {
    if total <= 0.0 {
        anyhow::bail!("total must be positive");
    }
    let (mut lo, mut hi) = (0.1f64, 0.9f64);
    let s = if win_probability(lo * total, (1.0 - lo) * total) >= p_home {
        lo
    } else if win_probability(hi * total, (1.0 - hi) * total) <= p_home {
        hi
    } else {
        for _ in 0..60 {
            let mid = (lo + hi) / 2.0;
            if win_probability(mid * total, (1.0 - mid) * total) < p_home {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        (lo + hi) / 2.0
    };
    Ok((s * total, (1.0 - s) * total))
}

/// Python `xs[:k]` (negative k counts from the end).
fn head(xs: &[f64], k: i64) -> &[f64] {
    let n = xs.len() as i64;
    let k = if k < 0 { (n + k).max(0) } else { k.min(n) };
    &xs[..k as usize]
}

/// P(over `point` | no push) for Poisson(lam) goals.
fn p_over(lam: f64, point: f64) -> f64 {
    let pm = pmf(lam, MAX_GOALS + 10);
    let k = point.floor() as i64;
    let is_int = point.fract() == 0.0;
    let under = py_sum(head(&pm, k + if is_int { 0 } else { 1 }).iter().copied());
    let push = if is_int {
        let n = pm.len() as i64;
        let idx = if k < 0 { n + k } else { k };
        if idx >= 0 && idx < n {
            pm[idx as usize]
        } else {
            0.0
        }
    } else {
        0.0
    };
    let over = (1.0 - under - push).max(0.0);
    if over + under > 0.0 {
        over / (over + under)
    } else {
        0.5
    }
}

/// Expected game goals implied by a total line and its no-vig P(over). Without prices, the line
/// itself. Clamped to [0.5, 15].
pub fn expected_total(point: f64, p_over_nv: Option<f64>) -> f64 {
    let Some(target) = p_over_nv else { return point };
    let (mut lo, mut hi) = (0.5f64, 15.0f64);
    if p_over(lo, point) >= target {
        return lo;
    }
    if p_over(hi, point) <= target {
        return hi;
    }
    for _ in 0..60 {
        let mid = (lo + hi) / 2.0;
        if p_over(mid, point) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo + hi) / 2.0
}

// --------------------------------------------------------------------------- times

fn parse_utc(s: Option<&Value>) -> Option<DateTime<Utc>> {
    let v = s.filter(|v| pyshim::truthy(v))?;
    let s = pyshim::str_value(v).replace('Z', "+00:00");
    let dt = PyDateTime::parse(&s)?;
    Some(dt.to_utc().unwrap_or_else(|| dt.local.and_utc()))
}

fn nth_sunday(year: i32, month: u32, n: i64) -> NaiveDate {
    let d = NaiveDate::from_ymd_opt(year, month, 1).expect("valid month");
    let wd = d.weekday().num_days_from_monday() as i64;
    let first = d + Duration::days((6 - wd).rem_euclid(7));
    debug_assert_eq!(first.weekday(), Weekday::Sun);
    first + Duration::weeks(n - 1)
}

/// The NHL game date (US Eastern) of a UTC instant (US DST computed directly).
pub fn eastern_date(dt: DateTime<Utc>) -> NaiveDate {
    let y = dt.year();
    let dst_start = nth_sunday(y, 3, 2).and_hms_opt(0, 0, 0).expect("midnight").and_utc() + Duration::hours(7);
    let dst_end = nth_sunday(y, 11, 1).and_hms_opt(0, 0, 0).expect("midnight").and_utc() + Duration::hours(6);
    let offset = if dst_start <= dt && dt < dst_end { -4 } else { -5 };
    (dt + Duration::hours(offset)).date_naive()
}

fn iso_z(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

// --------------------------------------------------------------------------- parsing

/// `GameOdds` (dataclass). `books` are the per-book rows (raw JSON values as the API sent them).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GameOdds {
    /// US Eastern date of puck drop (the NHL schedule's date)
    pub game_date: NaiveDate,
    /// ISO 8601, "Z"
    pub commence_time_utc: String,
    pub home: String,
    pub away: String,
    pub home_ml: Option<i64>,
    pub away_ml: Option<i64>,
    pub total: Option<f64>,
    pub over_price: Option<i64>,
    pub under_price: Option<i64>,
    pub implied_home_total: Option<f64>,
    pub implied_away_total: Option<f64>,
    pub bookmaker_count: i64,
    #[serde(default)]
    pub home_win_prob: Option<f64>,
    #[serde(default)]
    pub expected_total: Option<f64>,
    #[serde(default)]
    pub event_id: Option<String>,
    #[serde(default)]
    pub books: Vec<JsonMap<String, Value>>,
}

const REQUIRED_FIELDS: [&str; 12] = [
    "game_date", "commence_time_utc", "home", "away", "home_ml", "away_ml", "total", "over_price", "under_price",
    "implied_home_total", "implied_away_total", "bookmaker_count",
];

impl GameOdds {
    pub fn to_dict(&self, with_books: bool) -> JsonMap<String, Value> {
        let mut d = match serde_json::to_value(self) {
            Ok(Value::Object(m)) => m,
            _ => JsonMap::new(),
        };
        if !with_books {
            d.shift_remove("books");
        }
        d
    }

    pub fn from_dict(d: &JsonMap<String, Value>) -> anyhow::Result<GameOdds> {
        for k in REQUIRED_FIELDS {
            if !d.contains_key(k) {
                anyhow::bail!("missing field {k}");
            }
        }
        let mut d = d.clone();
        if !d.get("books").is_some_and(pyshim::truthy) {
            d.insert("books".into(), json!([]));
        }
        Ok(serde_json::from_value(Value::Object(d))?)
    }
}

fn price_eq(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

fn book_row(bm: &JsonMap<String, Value>, home_name: &Value, away_name: &Value) -> anyhow::Result<Option<JsonMap<String, Value>>> {
    let mut row = JsonMap::new();
    let book = match bm.get("key") {
        Some(v) if pyshim::truthy(v) => v.clone(),
        _ => bm.get("title").cloned().unwrap_or(Value::Null),
    };
    row.insert("book".into(), book);
    let markets = bm.get("markets").filter(|v| pyshim::truthy(v)).and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for m in &markets {
        let outcomes: Vec<&JsonMap<String, Value>> = m
            .get("outcomes")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|o| o.as_object()).collect())
            .unwrap_or_default();
        match m.get("key").and_then(|k| k.as_str()) {
            Some("h2h") => {
                // {o.get("name"): o.get("price")}: the last outcome with a name wins
                let lookup = |name: &Value| -> Option<Value> {
                    outcomes
                        .iter()
                        .rev()
                        .find(|o| o.get("name").unwrap_or(&Value::Null) == name)
                        .map(|o| o.get("price").cloned().unwrap_or(Value::Null))
                };
                if let (Some(h), Some(a)) = (lookup(home_name), lookup(away_name)) {
                    if !h.is_null() && !a.is_null() {
                        row.insert("home_ml".into(), h);
                        row.insert("away_ml".into(), a);
                    }
                }
            }
            Some("totals") => {
                let find = |want: &str| {
                    outcomes.iter().find(|o| pyshim::str_value(o.get("name").unwrap_or(&Value::Null)).to_lowercase() == want)
                };
                if let (Some(over), Some(under)) = (find("over"), find("under")) {
                    if let Some(op) = over.get("point").filter(|p| !p.is_null()) {
                        if price_eq(op, under.get("point").unwrap_or(&Value::Null)) {
                            let point = pyshim::py_float(op).ok_or_else(|| anyhow::anyhow!("could not convert point to float"))?;
                            row.insert("point".into(), json!(point));
                            row.insert("over".into(), over.get("price").cloned().unwrap_or(Value::Null));
                            row.insert("under".into(), under.get("price").cloned().unwrap_or(Value::Null));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(if row.len() > 1 { Some(row) } else { None })
}

fn median_opt(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        None
    } else {
        Some(median(xs))
    }
}

fn safe_prob(price: Option<&Value>) -> Option<f64> {
    let p = pyshim::py_float(price?)?;
    american_to_prob(p).ok()
}

/// Median consensus over per-book rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Consensus {
    pub home_ml: Option<i64>,
    pub away_ml: Option<i64>,
    pub total: Option<f64>,
    pub over_price: Option<i64>,
    pub under_price: Option<i64>,
    pub home_win_prob: Option<f64>,
    pub expected_total: Option<f64>,
}

/// Median consensus over per-book rows (`home_ml`/`away_ml`, `point`/`over`/`under`).
pub fn consensus(books: &[JsonMap<String, Value>]) -> anyhow::Result<Consensus> {
    let mut out = Consensus::default();
    let (mut qh, mut qa, mut pw) = (vec![], vec![], vec![]);
    for b in books {
        let (Some(h), Some(a)) = (safe_prob(b.get("home_ml")), safe_prob(b.get("away_ml"))) else { continue };
        qh.push(h);
        qa.push(a);
        pw.push(no_vig(h, a)?.0);
    }
    if !pw.is_empty() {
        out.home_ml = Some(prob_to_american(median(&qh))?);
        out.away_ml = Some(prob_to_american(median(&qa))?);
        out.home_win_prob = Some(round_to(median(&pw), 4));
    }
    let tot: Vec<(&JsonMap<String, Value>, f64)> =
        books.iter().filter_map(|b| b.get("point").and_then(|p| p.as_f64()).map(|p| (b, p))).collect();
    if !tot.is_empty() {
        let points: Vec<f64> = tot.iter().map(|(_, p)| *p).collect();
        let line = median(&points);
        out.total = Some(line);
        let nearest = tot.iter().map(|(_, p)| (p - line).abs()).fold(f64::INFINITY, f64::min);
        let at_line: Vec<&JsonMap<String, Value>> =
            tot.iter().filter(|(_, p)| (p - line).abs() == nearest).map(|(b, _)| *b).collect();
        let qo: Vec<f64> = at_line.iter().filter_map(|b| safe_prob(b.get("over"))).collect();
        let qu: Vec<f64> = at_line.iter().filter_map(|b| safe_prob(b.get("under"))).collect();
        if !qo.is_empty() && !qu.is_empty() {
            out.over_price = Some(prob_to_american(median(&qo))?);
            out.under_price = Some(prob_to_american(median(&qu))?);
        }
        let mut lams = Vec::with_capacity(tot.len());
        for (b, point) in &tot {
            let (o, u) = (safe_prob(b.get("over")), safe_prob(b.get("under")));
            let p = match (o, u) {
                (Some(o), Some(u)) => Some(no_vig(o, u)?.0),
                _ => None,
            };
            lams.push(expected_total(*point, p));
        }
        out.expected_total = median_opt(&lams).map(|m| round_to(m, 3));
    }
    Ok(out)
}

/// The /odds response -> GameOdds (games already started at `now` are left out). Returns (games
/// sorted by puck drop, warnings).
pub fn parse_events(events: &[Value], now: Option<DateTime<Utc>>) -> anyhow::Result<(Vec<GameOdds>, Vec<String>)> {
    let mut games = Vec::new();
    let mut warnings = Vec::new();
    let null = Value::Null;
    for ev in events {
        let ev = ev.as_object().ok_or_else(|| anyhow::anyhow!("event is not an object"))?;
        let start = parse_utc(ev.get("commence_time"));
        let home_v = ev.get("home_team").unwrap_or(&null);
        let away_v = ev.get("away_team").unwrap_or(&null);
        let home = team_abbrev(home_v.as_str());
        let away = team_abbrev(away_v.as_str());
        let Some(start) = start else {
            warnings.push(format!("odds: event {} without a commence_time", pyshim::str_value(ev.get("id").unwrap_or(&null))));
            continue;
        };
        let (Some(home), Some(away)) = (home, away) else {
            warnings.push(format!(
                "odds: unknown team name {} / {}",
                pyshim::repr_value(home_v),
                pyshim::repr_value(away_v)
            ));
            continue;
        };
        if now.is_some_and(|n| start <= n) {
            continue;
        }
        let mut books = Vec::new();
        if let Some(bms) = ev.get("bookmakers").filter(|v| pyshim::truthy(v)).and_then(|v| v.as_array()) {
            for bm in bms {
                let bm = bm.as_object().ok_or_else(|| anyhow::anyhow!("bookmaker is not an object"))?;
                if let Some(r) = book_row(bm, home_v, away_v)? {
                    books.push(r);
                }
            }
        }
        let c = consensus(&books)?;
        let (mut ih, mut ia) = (None, None);
        if let (Some(et), Some(hp)) = (c.expected_total.filter(|x| *x != 0.0), c.home_win_prob) {
            let (h, a) = split_total(et, hp)?;
            ih = Some(round_to(h, 2));
            ia = Some(round_to(a, 2));
        }
        games.push(GameOdds {
            game_date: eastern_date(start),
            commence_time_utc: iso_z(start),
            home: home.into(),
            away: away.into(),
            home_ml: c.home_ml,
            away_ml: c.away_ml,
            total: c.total,
            over_price: c.over_price,
            under_price: c.under_price,
            implied_home_total: ih,
            implied_away_total: ia,
            bookmaker_count: books.len() as i64,
            home_win_prob: c.home_win_prob,
            expected_total: c.expected_total,
            event_id: ev.get("id").and_then(|v| v.as_str()).map(String::from),
            books,
        });
    }
    games.sort_by(|a, b| (&a.commence_time_utc, &a.home).cmp(&(&b.commence_time_utc, &b.home)));
    Ok((games, warnings))
}

/// Quota headers `{remaining, used, last}` in that order.
pub type Quota = Map<String, i64>;

/// `x-requests-remaining` / `-used` / `-last` response headers -> {remaining, used, last}.
pub fn parse_quota(headers: Option<&Map<String, String>>) -> Option<Quota> {
    let headers = headers.filter(|h| !h.is_empty())?;
    let h: Map<String, &String> = headers.iter().map(|(k, v)| (k.to_lowercase(), v)).collect();
    let mut out = Quota::new();
    for name in ["remaining", "used", "last"] {
        let Some(v) = h.get(&format!("x-requests-{name}")) else { continue };
        match pyshim::float_from_str(v) {
            Some(f) if f.is_finite() => {
                out.insert(name.into(), f.trunc() as i64);
            }
            _ => continue,
        }
    }
    (!out.is_empty()).then_some(out)
}

// --------------------------------------------------------------------------- client

static NO_KEY_LOGGED: AtomicBool = AtomicBool::new(false);

fn redact(text: &str, secret: Option<&str>) -> String {
    match secret.filter(|s| !s.is_empty()) {
        Some(s) => text.replace(s, "***"),
        None => text.to_string(),
    }
}

/// `fetch_json(url, params) -> (payload, response headers)`.
pub type FetchJson = dyn Fn(&str, &[(String, String)]) -> anyhow::Result<(Value, Map<String, String>)>;

/// GET -> (json, headers). Errors are [`OddsError`]s with a key-free message.
pub fn default_fetch_json(url: &str, params: &[(String, String)]) -> anyhow::Result<(Value, Map<String, String>)> {
    let secret = params.iter().find(|(k, _)| k == "apiKey").map(|(_, v)| v.as_str());
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent("fantasy-manager/0.1")
        .build()
        .map_err(|e| OddsError::new(redact(&format!("HTTPError: {e}"), secret)))?;
    let r = client.get(url).query(params).send().map_err(|e| {
        let kind = if e.is_timeout() {
            "TimeoutException"
        } else if e.is_connect() {
            "ConnectError"
        } else {
            "HTTPError"
        };
        OddsError::new(redact(&format!("{kind}: {}", e.without_url()), secret))
    })?;
    let status = r.status().as_u16();
    let headers: Map<String, String> = r
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
    let text = r.text().map_err(|e| OddsError::new(redact(&format!("HTTPError: {}", e.without_url()), secret)))?;
    if !(200..300).contains(&status) {
        let detail = match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(body)) => {
                let pick = body
                    .get("message")
                    .filter(|v| pyshim::truthy(v))
                    .or_else(|| body.get("error_code").filter(|v| pyshim::truthy(v)));
                pick.map(pyshim::str_value).unwrap_or_default()
            }
            _ => String::new(),
        };
        let hint = match status {
            401 => " (invalid ODDS_API_KEY?)",
            429 => " (quota or rate limit reached)",
            _ => "",
        };
        let mut msg = format!("The Odds API returned HTTP {status}{hint}");
        if !detail.is_empty() {
            msg.push_str(&format!(": {}", pyshim::take_chars(&detail, 200)));
        }
        return Err(OddsError { message: redact(&msg, secret), status_code: Some(status), headers }.into());
    }
    let body: Value = serde_json::from_str(&text).map_err(|e| OddsError::new(format!("JSONDecodeError: {e}")))?;
    Ok((body, headers))
}

/// NHL odds from The Odds API. Without a key `available` is false and every call is a no-op.
/// With `data_dir` the raw response is cached in `<data_dir>/odds_cache.json` (without the key)
/// and reused for `ttl` (20h); `offline` serves only that cache.
pub struct OddsClient {
    key: Option<String>,
    pub fetch_json: Box<FetchJson>,
    pub region: String,
    pub data_dir: Option<PathBuf>,
    pub offline: bool,
    pub ttl: Duration,
    pub clock: Box<dyn Fn() -> DateTime<Utc>>,
    pub quota: Option<Quota>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub from_cache: bool,
    pub calls: usize,
    pub warnings: Vec<String>,
    mem: Option<JsonMap<String, Value>>,
}

impl std::fmt::Debug for OddsClient {
    /// never show the key
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "OddsClient(available={}, region={})", if self.available() { "True" } else { "False" }, pyshim::repr_str(&self.region))
    }
}

impl OddsClient {
    pub fn new(
        api_key: Option<&str>,
        fetch_json: Option<Box<FetchJson>>,
        region: Option<&str>,
        data_dir: Option<&Path>,
        offline: bool,
        ttl_hours: f64,
        clock: Option<Box<dyn Fn() -> DateTime<Utc>>>,
    ) -> Self {
        let key = api_key.map(pyshim::strip).filter(|k| !k.is_empty()).map(String::from);
        let region = region.map(pyshim::strip).filter(|r| !r.is_empty()).unwrap_or("us").to_string();
        OddsClient {
            key,
            fetch_json: fetch_json.unwrap_or_else(|| Box::new(default_fetch_json)),
            region,
            data_dir: data_dir.map(Path::to_path_buf),
            offline,
            ttl: Duration::microseconds((ttl_hours * 3600.0 * 1e6) as i64),
            clock: clock.unwrap_or_else(|| Box::new(Utc::now)),
            quota: None,
            fetched_at: None,
            from_cache: false,
            calls: 0,
            warnings: vec![],
            mem: None,
        }
    }

    pub fn from_settings(settings: &fm_core::config::Settings, fetch_json: Option<Box<FetchJson>>) -> Self {
        let key = fm_core::config::secret_value(settings.odds_api_key.as_ref());
        let region = if settings.odds_region.is_empty() { "us" } else { settings.odds_region.as_str() };
        OddsClient::new(Some(&key), fetch_json, Some(region), Some(&settings.fm_data_dir), settings.fm_offline, CACHE_TTL_HOURS, None)
    }

    pub fn available(&self) -> bool {
        self.key.is_some()
    }

    pub fn cache_path(&self) -> Option<PathBuf> {
        self.data_dir.as_ref().map(|d| d.join(CACHE_FILE))
    }

    fn read_cache(&self) -> Option<JsonMap<String, Value>> {
        if let Some(m) = &self.mem {
            return Some(m.clone());
        }
        let p = self.cache_path()?;
        let text = std::fs::read_to_string(&p).ok()?;
        match serde_json::from_str::<Value>(&text).ok()? {
            Value::Object(d) if d.get("region").and_then(|r| r.as_str()) == Some(self.region.as_str()) => Some(d),
            _ => None,
        }
    }

    fn write_cache(&mut self, payload: JsonMap<String, Value>) -> anyhow::Result<()> {
        self.mem = Some(payload.clone());
        let Some(p) = self.cache_path() else { return Ok(()) };
        let text = pyshim::dumps(&Value::Object(payload), None, false, Some((",", ":")));
        pyshim::write_atomic(&p, ".tmp", &text)?;
        Ok(())
    }

    /// The raw /odds events: from the cache when fresh (or offline), else one API call.
    pub fn raw_events(&mut self) -> anyhow::Result<Vec<Value>> {
        if !self.available() {
            if !NO_KEY_LOGGED.swap(true, Ordering::Relaxed) {
                tracing::info!("ODDS_API_KEY not set: betting odds are disabled");
            }
            return Ok(vec![]);
        }
        let now = (self.clock)();
        if let Some(cached) = self.read_cache() {
            let at = parse_utc(cached.get("fetched_at"));
            if let Some(at) = at {
                if self.offline || now - at < self.ttl {
                    self.from_cache = true;
                    self.fetched_at = Some(at);
                    self.quota = cached.get("quota").and_then(quota_from_value);
                    return Ok(cached.get("events").and_then(|e| e.as_array()).cloned().unwrap_or_default());
                }
            }
        }
        if self.offline {
            return Err(OddsError::new("offline and no cached odds").into());
        }
        let key = self.key.clone().unwrap_or_default();
        let params: Vec<(String, String)> = vec![
            ("apiKey".into(), key.clone()),
            ("regions".into(), self.region.clone()),
            ("markets".into(), MARKETS.into()),
            ("oddsFormat".into(), "american".into()),
            ("dateFormat".into(), "iso".into()),
        ];
        self.calls += 1;
        let (body, headers) = match (self.fetch_json)(ODDS_URL, &params) {
            Ok(r) => r,
            Err(e) => {
                if let Some(oe) = e.downcast_ref::<OddsError>() {
                    self.quota = parse_quota(Some(&oe.headers)).or(self.quota.take());
                    return Err(e);
                }
                // re-raise without anything that could carry the key
                return Err(OddsError::new(redact(&format!("Error: {e}"), Some(&key))).into());
            }
        };
        let Value::Array(events) = body else {
            return Err(OddsError::new("unexpected odds response (expected a list of events)").into());
        };
        self.quota = parse_quota(Some(&headers));
        self.fetched_at = Some(now);
        self.from_cache = false;
        let mut payload = JsonMap::new();
        payload.insert("fetched_at".into(), json!(iso_z(now)));
        payload.insert("region".into(), json!(self.region));
        payload.insert("markets".into(), json!(MARKETS));
        payload.insert("quota".into(), quota_value(self.quota.as_ref()));
        payload.insert("events".into(), Value::Array(events.clone()));
        self.write_cache(payload)?;
        Ok(events)
    }

    /// Upcoming NHL games with consensus prices and implied team totals ([] without a key).
    pub fn nhl_odds(&mut self) -> anyhow::Result<Vec<GameOdds>> {
        let events = self.raw_events()?;
        if events.is_empty() {
            return Ok(vec![]);
        }
        let (games, warnings) = parse_events(&events, Some((self.clock)()))?;
        self.warnings.extend(warnings);
        Ok(games)
    }
}

fn quota_value(q: Option<&Quota>) -> Value {
    match q {
        None => Value::Null,
        Some(q) => Value::Object(q.iter().map(|(k, v)| (k.clone(), json!(v))).collect()),
    }
}

fn quota_from_value(v: &Value) -> Option<Quota> {
    let o = v.as_object()?;
    Some(o.iter().filter_map(|(k, v)| v.as_i64().map(|i| (k.clone(), i))).collect())
}

// --------------------------------------------------------------------------- archive

pub fn odds_archive_path(data_dir: &Path, day: NaiveDate) -> PathBuf {
    data_dir.join("archive").join(format!("odds-{}.json", day.format("%Y-%m-%d")))
}

/// Write `<data_dir>/archive/odds-<day>.json` once per day. Returns (path, "written" |
/// "exists"); an existing file is kept unless `force`.
pub fn archive_odds(
    games: &[GameOdds],
    data_dir: &Path,
    day: NaiveDate,
    fetched_at: Option<DateTime<Utc>>,
    quota: Option<&Quota>,
    region: &str,
    force: bool,
) -> anyhow::Result<(PathBuf, &'static str)> {
    let path = odds_archive_path(data_dir, day);
    if path.exists() && !force {
        return Ok((path, "exists"));
    }
    let mut payload = JsonMap::new();
    payload.insert("kind".into(), json!("odds"));
    payload.insert("version".into(), json!(ARCHIVE_VERSION));
    payload.insert("date".into(), json!(day.format("%Y-%m-%d").to_string()));
    payload.insert("source".into(), json!("the-odds-api"));
    payload.insert("region".into(), json!(region));
    payload.insert("markets".into(), json!(MARKETS));
    payload.insert("fetched_at".into(), fetched_at.map(|f| json!(iso_z(f))).unwrap_or(Value::Null));
    payload.insert("quota".into(), quota.filter(|q| !q.is_empty()).map(|q| quota_value(Some(q))).unwrap_or(Value::Null));
    payload.insert("games".into(), Value::Array(games.iter().map(|g| Value::Object(g.to_dict(true))).collect()));
    let text = pyshim::dumps(&Value::Object(payload), None, false, Some((",", ":")));
    pyshim::write_atomic(&path, ".tmp", &text)?;
    Ok((path, "written"))
}

/// The odds archive of one day: the header keys (everything but `games`) plus the games.
#[derive(Clone, Debug, PartialEq)]
pub struct OddsArchive {
    pub header: JsonMap<String, Value>,
    pub games: Vec<GameOdds>,
}

/// The odds archive for `day`, or None.
pub fn load_odds(data_dir: &Path, day: NaiveDate) -> Option<OddsArchive> {
    let path = odds_archive_path(data_dir, day);
    let text = std::fs::read_to_string(&path).ok()?;
    let Value::Object(mut d) = serde_json::from_str::<Value>(&text).ok()? else { return None };
    let raw_games = d.shift_remove("games").filter(pyshim::truthy);
    let mut games = Vec::new();
    if let Some(Value::Array(gs)) = raw_games {
        for g in gs {
            games.push(GameOdds::from_dict(g.as_object()?).ok()?);
        }
    }
    Some(OddsArchive { header: d, games })
}

/// One team's market context (`team_context` values, in Python's key order).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamOddsContext {
    pub implied_total: Option<f64>,
    pub opp_implied_total: Option<f64>,
    pub win_prob: Option<f64>,
    pub opponent: String,
    pub home: bool,
    pub game_total: Option<f64>,
    pub as_of: String,
}

/// Market context for the teams playing on `day` (US Eastern game date) from the most recent
/// archive on or before `day` (up to `lookback_days` back) that lists that day's games.
pub fn team_context(day: NaiveDate, data_dir: &Path, lookback_days: i64) -> Map<String, TeamOddsContext> {
    for back in 0..=lookback_days {
        let snap_day = day - Duration::days(back);
        let Some(snap) = load_odds(data_dir, snap_day) else { continue };
        let games: Vec<&GameOdds> = snap.games.iter().filter(|g| g.game_date == day).collect();
        if games.is_empty() {
            continue;
        }
        let mut out = Map::new();
        for g in games {
            let p = g.home_win_prob;
            for (team, opp, home) in [(&g.home, &g.away, true), (&g.away, &g.home, false)] {
                out.insert(
                    team.clone(),
                    TeamOddsContext {
                        implied_total: if home { g.implied_home_total } else { g.implied_away_total },
                        opp_implied_total: if home { g.implied_away_total } else { g.implied_home_total },
                        win_prob: p.map(|p| if home { p } else { round_to(1.0 - p, 4) }),
                        opponent: opp.clone(),
                        home,
                        game_total: g.total,
                        as_of: snap_day.format("%Y-%m-%d").to_string(),
                    },
                );
            }
        }
        return out;
    }
    Map::new()
}

/// `datetime(...)` helper for callers and tests.
pub fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).single().expect("valid datetime")
}
