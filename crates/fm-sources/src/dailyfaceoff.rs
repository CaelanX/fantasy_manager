//! Port of `fantasy_manager/providers/dailyfaceoff.py`: Daily Faceoff line combinations,
//! power-play / penalty-kill units and starting goalies, parsed from the `__NEXT_DATA__` JSON
//! blob of the Next.js pages (depth-first walk in document order, like Python's `walk_dicts`).

use std::sync::OnceLock;

use anyhow::Result;
use chrono::NaiveDate;
use fm_core::normalize::normalize_team;
use fm_core::{Map, PyDateTime};
use fm_http::{FetchText, HttpCache, Params};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const BASE_URL: &str = "https://www.dailyfaceoff.com";
pub const USER_AGENT: &str = "fantasy-manager/0.1 (personal use)";
pub const HOUR: f64 = 3600.0;
pub const TTL_LINES: f64 = 12.0 * HOUR;
pub const TTL_GOALIES: f64 = 3.0 * HOUR;
/// pre-game run: refetch a team line page older than this
pub const TTL_LINES_PREGAME: f64 = 6.0 * HOUR;

/// NHL abbreviation -> Daily Faceoff team slug (insertion order = Python dict order).
pub const TEAM_SLUGS: &[(&str, &str)] = &[
    ("ANA", "anaheim-ducks"),
    ("BOS", "boston-bruins"),
    ("BUF", "buffalo-sabres"),
    ("CGY", "calgary-flames"),
    ("CAR", "carolina-hurricanes"),
    ("CHI", "chicago-blackhawks"),
    ("COL", "colorado-avalanche"),
    ("CBJ", "columbus-blue-jackets"),
    ("DAL", "dallas-stars"),
    ("DET", "detroit-red-wings"),
    ("EDM", "edmonton-oilers"),
    ("FLA", "florida-panthers"),
    ("LAK", "los-angeles-kings"),
    ("MIN", "minnesota-wild"),
    ("MTL", "montreal-canadiens"),
    ("NSH", "nashville-predators"),
    ("NJD", "new-jersey-devils"),
    ("NYI", "new-york-islanders"),
    ("NYR", "new-york-rangers"),
    ("OTT", "ottawa-senators"),
    ("PHI", "philadelphia-flyers"),
    ("PIT", "pittsburgh-penguins"),
    ("SJS", "san-jose-sharks"),
    ("SEA", "seattle-kraken"),
    ("STL", "st-louis-blues"),
    ("TBL", "tampa-bay-lightning"),
    ("TOR", "toronto-maple-leafs"),
    ("UTA", "utah-mammoth"),
    ("VAN", "vancouver-canucks"),
    ("VGK", "vegas-golden-knights"),
    ("WSH", "washington-capitals"),
    ("WPG", "winnipeg-jets"),
];

pub fn team_slug(team: &str) -> Option<&'static str> {
    TEAM_SLUGS.iter().find(|(t, _)| *t == team).map(|(_, s)| *s)
}
pub fn slug_to_team(slug: &str) -> Option<&'static str> {
    TEAM_SLUGS.iter().find(|(_, s)| *s == slug).map(|(t, _)| *t)
}

pub const EV_GROUPS: [&str; 9] = ["f1", "f2", "f3", "f4", "d1", "d2", "d3", "d4", "g"];
pub const PP_GROUPS: [&str; 2] = ["pp1", "pp2"];
pub const PK_GROUPS: [&str; 2] = ["pk1", "pk2"];
/// Goalie-page news strengths that name the starter (Unconfirmed / null do not).
pub const START_STRENGTHS: [&str; 3] = ["confirmed", "likely", "expected"];

/// A Daily Faceoff page could not be fetched or parsed.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct DfoError(pub String);

fn dfo_err<T>(msg: impl Into<String>) -> Result<T> {
    Err(DfoError(msg.into()).into())
}

fn pos_of(id: &str) -> Option<&'static str> {
    match id {
        "lw" => Some("LW"),
        "c" => Some("C"),
        "rw" => Some("RW"),
        "ld" | "rd" | "d" => Some("D"),
        _ => None,
    }
}

/// One player on a team's line-combinations page (all his groups merged).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinePlayer {
    pub name: String,
    pub dfo_id: Option<i64>,
    pub fantasydata_id: Option<i64>,
    /// C / LW / RW / D / G (from the even-strength row)
    pub position: Option<String>,
    /// primary group: the even-strength line (f1.., d1.., g) or "ir"
    pub group: String,
    /// every group he is listed in
    pub groups: Vec<String>,
    /// f1..f4, d1..d4, g (None: not in the even-strength lineup)
    pub line: Option<String>,
    /// 1 / 2 from g1 / g2
    pub goalie_depth: Option<i64>,
    pub pp_unit: Option<String>,
    pub pk_unit: Option<String>,
    /// Daily Faceoff's own tag, e.g. "out", "dtd", "ir"
    pub injury_status: Option<String>,
    /// game-time decision
    pub gtd: bool,
    pub toi_last5: Option<f64>,
    pub toi_last10: Option<f64>,
    pub ppp_last5: Option<i64>,
    pub ppp_last10: Option<i64>,
    pub gp_last5: Option<i64>,
    pub gp_last10: Option<i64>,
}

impl LinePlayer {
    pub fn in_lineup(&self) -> bool {
        self.line.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TeamLines {
    /// NHL abbreviation
    pub team: String,
    pub slug: Option<String>,
    pub updated_at: Option<PyDateTime>,
    /// who reported the lines (sourceName), e.g. a beat writer
    pub source: Option<String>,
    pub source_url: Option<String>,
    pub players: Vec<LinePlayer>,
}

impl TeamLines {
    pub fn by_group(&self, group: &str) -> Vec<&LinePlayer> {
        self.players.iter().filter(|p| p.groups.iter().any(|g| g == group)).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GoalieStart {
    /// "AWAY@HOME", e.g. "FLA@CAR"
    pub game: String,
    /// puck drop (UTC)
    pub game_time: Option<PyDateTime>,
    /// NHL abbreviation of the goalie's team
    pub team: String,
    pub opponent: Option<String>,
    pub home: bool,
    pub goalie_name: String,
    pub dfo_id: Option<i64>,
    /// Confirmed / Likely / Expected / Unconfirmed / None
    pub strength: Option<String>,
    /// who reported it
    pub source: Option<String>,
    pub created_at: Option<PyDateTime>,
}

impl GoalieStart {
    /// True when the report names him as the starter (confirmed, likely or expected).
    pub fn is_start(&self) -> bool {
        let s = self.strength.as_deref().unwrap_or("").trim().to_lowercase();
        START_STRENGTHS.contains(&s.as_str())
    }
    pub fn is_confirmed(&self) -> bool {
        self.strength.as_deref().unwrap_or("").trim().to_lowercase() == "confirmed"
    }
}

// --------------------------------------------------------------------------- fetching

pub fn team_url(team_abbrev: &str) -> Result<String> {
    let team = normalize_team(Some(team_abbrev)).unwrap_or_default();
    match team_slug(&team) {
        Some(slug) => Ok(format!("{BASE_URL}/teams/{slug}/line-combinations")),
        None => dfo_err(format!("unknown team {team_abbrev:?}")),
    }
}

pub fn goalies_url(day: NaiveDate) -> String {
    format!("{BASE_URL}/starting-goalies/{}", day.format("%Y-%m-%d"))
}

pub const GOALIES_PREFIX: &str = "https://www.dailyfaceoff.com/starting-goalies/";

/// Drop every cached starting-goalies page so the next load sees late confirmations; team line
/// pages are kept. Returns how many entries were removed.
pub fn clear_goalie_cache(cache: &HttpCache) -> Result<usize> {
    cache.clear(Some(GOALIES_PREFIX))
}

pub fn ttl_for(url: &str) -> f64 {
    if url.contains("/starting-goalies/") {
        TTL_GOALIES
    } else {
        TTL_LINES
    }
}

/// Plain uncached GET returning the response text.
pub fn default_fetch_text(url: &str, params: &Params, _ttl: f64) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent(USER_AGENT)
        .build()?;
    let mut req = client.get(url);
    let q: Vec<(&str, &str)> = params
        .iter()
        .filter_map(|(k, v)| match v {
            fm_http::ParamValue::Str(s) => Some((k.as_str(), s.as_str())),
            _ => None,
        })
        .collect();
    if !q.is_empty() {
        req = req.query(&q);
    }
    Ok(req.send()?.error_for_status()?.text()?)
}

/// Fetch through `HttpCache::get_text` with the Daily Faceoff TTLs and User-Agent.
/// `goalies_max_age` / `lines_max_age` (seconds) override the TTL of the starting-goalies page /
/// the team line pages: `goalies_max_age = Some(0.0)` bypasses the cache and stores the fresh
/// page, so later fetches with the normal TTL see it. The ttl the caller passes is ignored.
pub fn make_cached_fetch_text<'a>(
    cache: &'a HttpCache,
    goalies_max_age: Option<f64>,
    lines_max_age: Option<f64>,
) -> impl Fn(&str, &Params, f64) -> Result<String> + Send + Sync + 'a {
    move |url: &str, params: &Params, _ttl: f64| {
        let mut ttl = ttl_for(url);
        if url.contains("/starting-goalies/") {
            if let Some(g) = goalies_max_age {
                ttl = g;
            }
        } else if let Some(l) = lines_max_age {
            ttl = l;
        }
        cache.get_text(url, params, &[("User-Agent".to_string(), USER_AGENT.to_string())], &[], ttl)
    }
}

/// Force-refresh fetcher: the goalie page always from the network, team line pages only when
/// the cached copy is older than `lines_max_age` (default [`TTL_LINES_PREGAME`]).
pub fn make_refresh_fetch_text<'a>(
    cache: &'a HttpCache,
    lines_max_age: f64,
) -> impl Fn(&str, &Params, f64) -> Result<String> + Send + Sync + 'a {
    make_cached_fetch_text(cache, Some(0.0), Some(lines_max_age))
}

// --------------------------------------------------------------------------- parsing helpers

fn next_data_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r#"(?is)<script[^>]*\bid=["']__NEXT_DATA__["'][^>]*>(.*?)</script>"#).unwrap()
    })
}

pub fn extract_next_data(html: &str) -> Result<Value> {
    let Some(m) = next_data_re().captures(html) else {
        return dfo_err("no __NEXT_DATA__ script on the page");
    };
    match serde_json::from_str::<Value>(&m[1]) {
        Ok(v) => Ok(v),
        Err(e) => dfo_err(format!("__NEXT_DATA__ is not valid JSON ({e})")),
    }
}

type Obj = serde_json::Map<String, Value>;

/// Every dict in a JSON tree (depth first, document order).
pub fn walk_dicts(obj: &Value) -> Vec<&Obj> {
    let mut out = Vec::new();
    let mut stack = vec![obj];
    while let Some(cur) = stack.pop() {
        match cur {
            Value::Object(m) => {
                out.push(m);
                let vals: Vec<&Value> = m.values().collect();
                stack.extend(vals.into_iter().rev());
            }
            Value::Array(a) => stack.extend(a.iter().rev()),
            _ => {}
        }
    }
    out
}

fn parse_dt(value: Option<&Value>) -> Option<PyDateTime> {
    let s = value?.as_str().filter(|s| !s.is_empty())?;
    PyDateTime::parse(&s.replace('Z', "+00:00"))
}

/// Python truthiness of a JSON value.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|x| x != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `str(v or "")`.
fn str_or_empty(v: Option<&Value>) -> String {
    if !truthy(v) {
        return String::new();
    }
    match v.unwrap() {
        Value::String(s) => s.clone(),
        Value::Bool(_) => "True".to_string(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.to_string(),
            None => fm_pycompat::fmt::repr_f64(n.as_f64().unwrap_or(f64::NAN)),
        },
        other => other.to_string(),
    }
}

/// `_int(v)`: `int(v)` unless None / "" / not convertible.
fn int_of(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => match n.as_i64() {
            Some(i) => Some(i),
            None => n.as_f64().filter(|x| x.is_finite()).map(|x| x.trunc() as i64),
        },
        Value::Bool(b) => Some(*b as i64),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// `float(v)`.
fn float_of(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(*b as i64 as f64),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn toi_per_game(split: &Value) -> Option<f64> {
    let m = split.as_object()?;
    let gp = int_of(m.get("gamesPlayed")).filter(|g| *g != 0)?;
    let mins = match m.get("toiMinutes") {
        Some(v) => Some(v),
        None => m.get("goalieMinutes"),
    }?;
    let secs_v = match m.get("toiSeconds") {
        Some(v) => Some(v),
        None => m.get("goalieSeconds"),
    };
    let secs = if truthy(secs_v) { float_of(secs_v.unwrap())? } else { 0.0 };
    let mins = float_of(mins)?;
    Some(fm_pycompat::num::round_to((mins + secs / 60.0) / gp as f64, 2))
}

fn split_val(split: &Value, key: &str) -> Option<i64> {
    int_of(split.as_object()?.get(key))
}

fn team_from(abbrev: Option<&Value>, slug: Option<&Value>) -> Option<String> {
    if let Some(Value::String(s)) = slug {
        if let Some(t) = slug_to_team(s) {
            return Some(t.to_string());
        }
    }
    if let Some(Value::String(a)) = abbrev {
        if !a.trim().is_empty() {
            return normalize_team(Some(a));
        }
    }
    None
}

fn is_line_entry(x: &Value) -> bool {
    x.as_object().is_some_and(|m| m.contains_key("groupIdentifier") && m.contains_key("name"))
}

fn opt_str(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

// --------------------------------------------------------------------------- line pages

/// Parse a team line-combinations page (HTML).
pub fn parse_team_lines(html: &str, team: Option<&str>) -> Result<TeamLines> {
    parse_team_lines_value(&extract_next_data(html)?, team)
}

/// Parse a team line-combinations page from its `__NEXT_DATA__` JSON.
pub fn parse_team_lines_value(data: &Value, team: Option<&str>) -> Result<TeamLines> {
    let holder = walk_dicts(data).into_iter().find(|d| {
        d.get("players").and_then(Value::as_array).is_some_and(|a| a.iter().any(is_line_entry))
    });
    let Some(holder) = holder else {
        return dfo_err("no line-combination players in __NEXT_DATA__");
    };
    let abbrev = team_from(holder.get("teamAbbreviation"), holder.get("teamSlug"))
        .or_else(|| normalize_team(team));
    let Some(abbrev) = abbrev else { return dfo_err("line page has no team") };

    #[derive(Hash, PartialEq, Eq)]
    enum Key {
        Id(i64),
        Name(String),
    }
    let mut merged: Map<Key, LinePlayer> = Map::new();
    let players = holder.get("players").and_then(Value::as_array).cloned().unwrap_or_default();
    for e in &players {
        if !is_line_entry(e) {
            continue;
        }
        let group = str_or_empty(e.get("groupIdentifier")).trim().to_lowercase();
        let name = str_or_empty(e.get("name")).trim().to_string();
        if group.is_empty() || name.is_empty() {
            continue;
        }
        let dfo_id = int_of(e.get("playerId"));
        let key = match dfo_id {
            Some(i) => Key::Id(i),
            None => Key::Name(name.to_lowercase()),
        };
        let lp = merged.entry(key).or_insert_with(|| LinePlayer {
            name: name.clone(),
            dfo_id,
            fantasydata_id: int_of(e.get("fantasydataId")),
            position: None,
            group: group.clone(),
            groups: Vec::new(),
            line: None,
            goalie_depth: None,
            pp_unit: None,
            pk_unit: None,
            injury_status: None,
            gtd: false,
            toi_last5: None,
            toi_last10: None,
            ppp_last5: None,
            ppp_last10: None,
            gp_last5: None,
            gp_last10: None,
        });
        if !lp.groups.contains(&group) {
            lp.groups.push(group.clone());
        }
        let pos_id = str_or_empty(e.get("positionIdentifier")).trim().to_lowercase();
        if EV_GROUPS.contains(&group.as_str()) {
            if lp.line.is_none() {
                lp.line = Some(group.clone());
            }
            lp.group = lp.line.clone().unwrap();
            if group == "g" {
                lp.position = Some("G".to_string());
                // re.match(r"g(\d)", pos_id)
                let b = pos_id.as_bytes();
                if b.len() >= 2 && b[0] == b'g' && b[1].is_ascii_digit() {
                    lp.goalie_depth = Some((b[1] - b'0') as i64);
                }
            } else if let Some(p) = pos_of(&pos_id) {
                lp.position = Some(p.to_string());
            }
        } else if PP_GROUPS.contains(&group.as_str()) {
            if lp.pp_unit.is_none() {
                lp.pp_unit = Some(group.clone());
            }
        } else if PK_GROUPS.contains(&group.as_str()) && lp.pk_unit.is_none() {
            lp.pk_unit = Some(group.clone());
        }
        if lp.line.is_none() && group == "ir" {
            lp.group = "ir".to_string();
        }
        if let Some(Value::String(st)) = e.get("injuryStatus") {
            if !st.trim().is_empty() {
                lp.injury_status = Some(st.trim().to_lowercase());
            }
        }
        lp.gtd = lp.gtd || truthy(e.get("gameTimeDecision"));
        for (n, attr) in [(5, "last5"), (10, "last10")] {
            if let Some(split) = e.get(attr).filter(|s| s.is_object()) {
                let (toi, ppp, gp) = if n == 5 {
                    (&mut lp.toi_last5, &mut lp.ppp_last5, &mut lp.gp_last5)
                } else {
                    (&mut lp.toi_last10, &mut lp.ppp_last10, &mut lp.gp_last10)
                };
                if toi.is_none() {
                    *toi = toi_per_game(split);
                }
                if ppp.is_none() {
                    *ppp = split_val(split, "powerplayPoints");
                }
                if gp.is_none() {
                    *gp = split_val(split, "gamesPlayed");
                }
            }
        }
    }
    // primary group: even-strength line, else ir, else the first group listed
    for lp in merged.values_mut() {
        if lp.line.is_none() && !lp.groups.iter().any(|g| g == "ir") {
            lp.group = lp.groups[0].clone();
        }
    }
    let slug = opt_str(holder.get("teamSlug")).or_else(|| team_slug(&abbrev).map(str::to_string));
    Ok(TeamLines {
        team: abbrev,
        slug,
        updated_at: parse_dt(holder.get("updatedAt")),
        source: opt_str(holder.get("sourceName")),
        source_url: opt_str(holder.get("source")),
        players: merged.into_values().collect(),
    })
}

/// `{NHL abbreviation: slug}` from any page listing teams (`slug` + `shortName` dicts), used to
/// re-verify [`TEAM_SLUGS`].
pub fn parse_team_slugs(html: &str) -> Result<Map<String, String>> {
    Ok(parse_team_slugs_value(&extract_next_data(html)?))
}

pub fn parse_team_slugs_value(data: &Value) -> Map<String, String> {
    let mut out: Map<String, String> = Map::new();
    for d in walk_dicts(data) {
        if let (Some(Value::String(slug)), Some(Value::String(short))) = (d.get("slug"), d.get("shortName")) {
            if let Some(team) = normalize_team(Some(short)) {
                out.entry(team).or_insert_with(|| slug.clone());
            }
        }
    }
    out
}

// --------------------------------------------------------------------------- starting goalies

fn is_goalie_game(d: &Obj) -> bool {
    d.contains_key("homeGoalieName") && d.contains_key("awayGoalieName")
}

/// Both goalies of every game on a starting-goalies page (HTML).
pub fn parse_starting_goalies(html: &str) -> Result<Vec<GoalieStart>> {
    parse_starting_goalies_value(&extract_next_data(html)?)
}

pub fn parse_starting_goalies_value(data: &Value) -> Result<Vec<GoalieStart>> {
    let dicts = walk_dicts(data);
    let games: Vec<&Obj> = dicts.iter().copied().filter(|d| is_goalie_game(d)).collect();
    if games.is_empty() && !dicts.iter().any(|d| d.contains_key("data")) {
        return dfo_err("no starting-goalie games in __NEXT_DATA__");
    }
    let mut out = Vec::new();
    for g in games {
        let home = team_from(g.get("homeTeamAbbreviation"), g.get("homeTeamSlug"));
        let away = team_from(g.get("awayTeamAbbreviation"), g.get("awayTeamSlug"));
        let (Some(home), Some(away)) = (home, away) else {
            tracing::info!(
                "dailyfaceoff: skipping game with unknown teams ({:?} / {:?})",
                g.get("homeTeamSlug"),
                g.get("awayTeamSlug")
            );
            continue;
        };
        let label = format!("{away}@{home}");
        let when = parse_dt(g.get("dateGmt"));
        for (side, is_home) in [("home", true), ("away", false)] {
            let name = str_or_empty(g.get(&format!("{side}GoalieName"))).trim().to_string();
            if name.is_empty() {
                continue;
            }
            let strength = g.get(&format!("{side}NewsStrengthName"));
            out.push(GoalieStart {
                game: label.clone(),
                game_time: when,
                team: if is_home { home.clone() } else { away.clone() },
                opponent: Some(if is_home { away.clone() } else { home.clone() }),
                home: is_home,
                goalie_name: name,
                dfo_id: int_of(g.get(&format!("{side}GoalieId"))),
                strength: if truthy(strength) { Some(str_or_empty(strength).trim().to_string()) } else { None },
                source: opt_str(g.get(&format!("{side}NewsSourceName"))),
                created_at: parse_dt(g.get(&format!("{side}NewsCreatedAt"))),
            });
        }
    }
    Ok(out)
}

// --------------------------------------------------------------------------- client

/// Fetch + parse Daily Faceoff pages. `fetch_text(url, params, ttl) -> str` is injectable
/// (tests, or [`make_cached_fetch_text`] for the HTTP cache).
pub struct DailyFaceoffClient<'a> {
    fetch_text: Box<FetchText<'a>>,
    pub warnings: Vec<String>,
}

impl<'a> DailyFaceoffClient<'a> {
    pub fn new(fetch_text: Box<FetchText<'a>>) -> Self {
        DailyFaceoffClient { fetch_text, warnings: Vec::new() }
    }

    /// Uncached client (`DailyFaceoffClient()`).
    pub fn with_default_fetch() -> Self {
        Self::new(Box::new(default_fetch_text))
    }

    fn fetch(&self, url: &str) -> Result<String> {
        (self.fetch_text)(url, &Vec::new(), ttl_for(url))
    }

    pub fn team_lines(&self, team_abbrev: &str) -> Result<TeamLines> {
        let url = team_url(team_abbrev)?;
        let html = self.fetch(&url)?;
        parse_team_lines(&html, normalize_team(Some(team_abbrev)).as_deref())
    }

    /// `{team: lines}` for every team (32 fetches); a failing team becomes a warning.
    pub fn all_lines(&mut self, teams: Option<&[String]>) -> Map<String, TeamLines> {
        let mut out: Map<String, TeamLines> = Map::new();
        let mut failed: Vec<String> = Vec::new();
        let mut last: Option<anyhow::Error> = None;
        let list: Vec<String> = match teams {
            Some(t) => t.to_vec(),
            None => TEAM_SLUGS.iter().map(|(t, _)| t.to_string()).collect(),
        };
        for t in list {
            let team = normalize_team(Some(&t)).unwrap_or_else(|| t.clone());
            match self.team_lines(&team) {
                Ok(l) => {
                    out.insert(team, l);
                }
                Err(e) => {
                    tracing::warn!("dailyfaceoff: {team} lines unavailable: {e}");
                    failed.push(team);
                    last = Some(e);
                }
            }
        }
        if !failed.is_empty() {
            let more = if failed.len() > 6 { "..." } else { "" };
            self.warnings.push(format!(
                "Daily Faceoff lines unavailable for {} team(s) ({}{}): {}",
                failed.len(),
                failed.iter().take(6).cloned().collect::<Vec<_>>().join(", "),
                more,
                short(last.as_ref().unwrap())
            ));
        }
        out
    }

    pub fn starting_goalies(&self, day: NaiveDate) -> Result<Vec<GoalieStart>> {
        parse_starting_goalies(&self.fetch(&goalies_url(day))?)
    }
}

fn short(e: &anyhow::Error) -> String {
    let s = e.to_string();
    let t = s.trim();
    if t.is_empty() {
        return "Error".to_string();
    }
    t.lines().next().unwrap_or("").chars().take(120).collect()
}
