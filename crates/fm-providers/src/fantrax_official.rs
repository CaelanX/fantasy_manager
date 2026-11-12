//! Thin client for Fantrax's official REST API (`https://www.fantrax.com/fxea/general/`, v1.8 beta).
//! See docs/knowledge/fantrax-official-api.md for the endpoints and docs/knowledge/fantrax-official-api-plan.md
//! for how it is meant to be used.
//!
//! Every read is "try it, otherwise the caller falls back to the private path and adds one warning":
//! nothing here may be required for a league load to succeed. One function per endpoint over
//! [`HttpCache::get_text`] (so the shared cache gives per-call TTLs, offline mode and tape replay),
//! each returning plain typed values. Only the official API's own ids are used; they are the same
//! strings the private endpoint calls `scorerId` / team id, so results join by id with no name matching.

use std::sync::Arc;

use chrono::{Duration, NaiveDate};
use serde_json::Value;

use fm_core::Map;
use fm_http::{CacheMiss, HttpCache, HttpError, ParamValue, Params};

pub const BASE_URL: &str = "https://www.fantrax.com/fxea/general";
/// Substring of every official request URL, for clearing their cache rows (`HttpCache::clear`).
pub const URL_MARK: &str = "/fxea/general/";

/// League settings, calendar and schedule change rarely (the owner edits them in the offseason).
pub const TTL_LEAGUE_INFO: f64 = 6.0 * 3600.0;
/// Pick ownership changes only when a trade is accepted. The official list replaces the private one
/// (which arrives with the 2-minute private roster reads), so it must not be cached longer than those.
pub const TTL_DRAFT_PICKS: f64 = 2.0 * 60.0;
/// Standings move when a scoring period finishes; ten minutes keeps a live session close enough.
pub const TTL_STANDINGS: f64 = 10.0 * 60.0;
/// Live scores of the current period, so as short as the private lineup reads would be useful.
pub const TTL_MATCHUP_SCORES: f64 = 5.0 * 60.0;
/// Same as the private lineup calls (`fantrax::LINEUP_TTL`): rosters change with every lineup move.
pub const TTL_TEAM_ROSTERS: f64 = 2.0 * 60.0;
/// ADP is a site-wide average that drifts slowly.
pub const TTL_ADP: f64 = 24.0 * 3600.0;
/// Player names, teams and positions are static data (the plan's "long TTLs for static data"), and the
/// response is about 1 MB, so it is kept as long as ADP.
pub const TTL_PLAYER_IDS: f64 = 24.0 * 3600.0;

/// Fantrax answers `getAdp` with this value for a player nobody has drafted (seen once in 972 NHL rows).
pub const ADP_MISSING: f64 = 10000.0;

/// Why an official read gave nothing. Never fatal: the caller uses the private path.
#[derive(Clone, Debug, PartialEq)]
pub enum OfficialError {
    /// Offline mode and the response is not in the cache. Silent by design (the plan's offline rule).
    NotCached,
    /// The call or its body was unusable. The text is safe to show: it never holds a URL, an HTTP status
    /// number or the words login, cookie or session, so the health classifier cannot read it as an
    /// expired private login.
    Failed(String),
}

impl OfficialError {
    /// The one warning to add for this failure, or None when it must stay silent (offline cache miss).
    /// `what` names the data, for example "league info". Starts with "Fantrax official API" so
    /// `enrich::provider_warning` attributes it to the Fantrax provider.
    pub fn warning(&self, what: &str) -> Option<String> {
        match self {
            OfficialError::NotCached => None,
            OfficialError::Failed(detail) => {
                Some(format!("Fantrax official API {what} unavailable ({detail}); using the private data instead"))
            }
        }
    }
}

/// Words that would make a warning look like an expired private login (`enrich::classify_provider_warning`,
/// `pregame::failure_message`) or leak a URL or status code.
const FORBIDDEN: [&str; 11] =
    ["login", "log in", "logged", "cookie", "session", "http", "://", "401", "403", "unauthori", "forbidden"];

/// Make an error detail safe to show: clipped, and replaced with a generic phrase when it holds a
/// forbidden word (a body message is the API's own text and could say anything).
fn safe_detail(detail: &str) -> String {
    let low = detail.to_lowercase();
    if FORBIDDEN.iter().any(|w| low.contains(w)) {
        return "the API returned an error".to_string();
    }
    let t = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() > 100 {
        format!("{}...", t.chars().take(97).collect::<String>())
    } else {
        t
    }
}

fn classify_transport(e: &anyhow::Error) -> OfficialError {
    if e.downcast_ref::<CacheMiss>().is_some() {
        return OfficialError::NotCached;
    }
    if let Some(h) = e.downcast_ref::<HttpError>() {
        // Fantrax answers a request without a program-naming User-Agent with 403 (error 1010).
        return OfficialError::Failed(if h.status_code == 403 {
            "the API rejected the request (User-Agent)".to_string()
        } else {
            "the API answered with an error".to_string()
        });
    }
    OfficialError::Failed("could not reach the API".to_string())
}

// --------------------------------------------------------------------------- typed results

/// A scoring or roster period. `first` is the calendar date of `startDate` and `last` is the calendar date
/// of `endDate` minus one day, both in the offset the API sent: Fantrax ends a period at the same
/// clock time the next one starts, so the plain end date would overlap the next period by one day.
#[derive(Clone, Debug, PartialEq)]
pub struct OfficialPeriod {
    pub number: i64,
    pub first: NaiveDate,
    pub last: NaiveDate,
}

impl OfficialPeriod {
    pub fn days(&self) -> i64 {
        (self.last - self.first).num_days() + 1
    }
    pub fn contains(&self, d: NaiveDate) -> bool {
        self.first <= d && d <= self.last
    }
}

/// The period holding `date`, if any (use it to pick the `period=N` sent to `getTeamRosters`).
pub fn period_for_date(periods: &[OfficialPeriod], date: NaiveDate) -> Option<i64> {
    periods.iter().find(|p| p.contains(date)).map(|p| p.number)
}

#[derive(Clone, Debug, PartialEq)]
pub struct OfficialPlayoffs {
    pub used: bool,
    pub last_regular_period: i64,
    pub first_playoff_period: i64,
    pub num_playoff_teams: i64,
    pub merge_playoff_periods: bool,
}

impl OfficialPlayoffs {
    /// Playoff periods hold seed placeholders instead of team ids, so this flag, not the matchup list,
    /// says whether a period is a playoff one.
    pub fn is_playoff_period(&self, period: i64) -> bool {
        self.used && period >= self.first_playoff_period
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OfficialTeam {
    pub id: String,
    pub name: String,
}

/// One side of a head-to-head pairing in `getLeagueInfo.matchups` (always a real team: placeholder
/// sides are skipped when parsing).
#[derive(Clone, Debug, PartialEq)]
pub struct OfficialSide {
    pub id: String,
    pub name: String,
    pub short_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OfficialPairing {
    pub away: OfficialSide,
    pub home: OfficialSide,
}

/// The pairings of one scoring period. Periods whose sides are `{seed:N}` / `{bye:true}` (the playoffs)
/// have no pairing with two real teams, so `pairings` is empty for them.
#[derive(Clone, Debug, PartialEq)]
pub struct OfficialPeriodMatchups {
    pub period: i64,
    pub pairings: Vec<OfficialPairing>,
}

/// Point values by stat short name (`G`, `A`, `SOG`, `GA`, ...), in the order the API lists them.
#[derive(Clone, Debug, PartialEq)]
pub struct OfficialScoring {
    /// `HEAD_TO_HEAD_POINTS_BASED` for our league.
    pub kind: String,
    pub skater: Map<String, f64>,
    pub goalie: Map<String, f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OfficialRosterLimits {
    pub max_total: i64,
    pub max_active: i64,
    pub max_reserve: i64,
    /// Active maximum per position code (`F`, `D`, `G`).
    pub max_active_by_position: Map<String, i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OfficialLeagueInfo {
    pub name: String,
    /// The year the season starts in (2026 for the 2026-27 season), not the label the program uses.
    pub season_year: i64,
    pub start_date: Option<NaiveDate>,
    pub end_date: Option<NaiveDate>,
    pub teams: Vec<OfficialTeam>,
    pub scoring_periods: Vec<OfficialPeriod>,
    pub roster_periods: Vec<OfficialPeriod>,
    pub playoffs: Option<OfficialPlayoffs>,
    pub matchups: Vec<OfficialPeriodMatchups>,
    pub scoring: Option<OfficialScoring>,
    pub roster_limits: Option<OfficialRosterLimits>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OfficialPick {
    pub year: i64,
    pub round: i64,
    pub owner_team_id: String,
    pub original_owner_team_id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OfficialDraftPicks {
    pub future: Vec<OfficialPick>,
    pub current: Vec<OfficialPick>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OfficialStanding {
    pub team_id: String,
    pub team_name: String,
    pub rank: i64,
    /// (wins, losses, ties) from the "W-L-T" text of `points`; None when it is not in that form.
    pub record: Option<(i64, i64, i64)>,
    pub points_for: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OfficialScoreSide {
    pub team_id: String,
    pub team_name: String,
    pub score: Option<f64>,
    pub games_played: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OfficialScoreboard {
    pub period: i64,
    /// Pairings with two real teams (playoff placeholder sides are skipped).
    pub matchups: Vec<(OfficialScoreSide, OfficialScoreSide)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OfficialRosterStatus {
    Active,
    Reserve,
    InjuredReserve,
    Other(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct OfficialRosterItem {
    pub player_id: String,
    /// Position code as the API gives it (`F`, `D`, `G`).
    pub position: String,
    pub status: OfficialRosterStatus,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OfficialTeamRoster {
    pub team_id: String,
    pub team_name: String,
    pub items: Vec<OfficialRosterItem>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OfficialRosters {
    pub period: i64,
    pub teams: Vec<OfficialTeamRoster>,
}

/// One `getPlayerIds` entry: the name flipped to "First Last", the NHL team code as sent (`(N/A)` for a
/// player without a team) and the NHL position (`C`, `LW`, `RW`, `D`, `G`, ...).
#[derive(Clone, Debug, PartialEq)]
pub struct OfficialPlayerName {
    pub name: String,
    pub team: String,
    pub position: String,
}

// --------------------------------------------------------------------------- parsers

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

fn int(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(Value::as_i64)
}

/// Calendar date of an API timestamp ("2026-11-09T19:59:59.0-0500") or plain date, read as sent (the
/// offset is not applied: the first ten characters are the date in the league's own zone).
fn date_as_sent(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text.get(..10)?, "%Y-%m-%d").ok()
}

/// `rosterPeriods` / `scoringPeriods`: the rule is first = date(startDate), last = date(endDate) - 1 day.
pub fn parse_periods(list: &Value) -> Result<Vec<OfficialPeriod>, String> {
    let arr = list.as_array().ok_or("periods are not a list")?;
    arr.iter()
        .map(|p| {
            let number = int(p, "number").ok_or("a period has no number")?;
            let first = s(p, "startDate").as_deref().and_then(date_as_sent).ok_or_else(|| format!("period {number} has no start date"))?;
            let end = s(p, "endDate").as_deref().and_then(date_as_sent).ok_or_else(|| format!("period {number} has no end date"))?;
            let last = end - Duration::days(1);
            if last < first {
                return Err(format!("period {number} ends before it starts"));
            }
            Ok(OfficialPeriod { number, first, last })
        })
        .collect()
}

fn parse_playoffs(v: &Value) -> Option<OfficialPlayoffs> {
    Some(OfficialPlayoffs {
        used: v.get("used").and_then(Value::as_bool)?,
        last_regular_period: int(v, "lastRegularSeasonPeriod")?,
        first_playoff_period: int(v, "firstPlayoffPeriod")?,
        num_playoff_teams: int(v, "numPlayoffTeams")?,
        merge_playoff_periods: v.get("mergePlayoffPeriods").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn side(v: &Value) -> Option<OfficialSide> {
    Some(OfficialSide { id: s(v, "id")?, name: s(v, "name")?, short_name: s(v, "shortName") })
}

fn parse_matchups(list: &Value) -> Result<Vec<OfficialPeriodMatchups>, String> {
    let arr = list.as_array().ok_or("matchups are not a list")?;
    arr.iter()
        .map(|m| {
            let period = int(m, "period").ok_or("a matchup period has no number")?;
            let pairings = m
                .get("matchupList")
                .and_then(Value::as_array)
                .map(|l| {
                    l.iter()
                        .filter_map(|p| Some(OfficialPairing { away: side(p.get("away")?)?, home: side(p.get("home")?)? }))
                        .collect()
                })
                .unwrap_or_default();
            Ok(OfficialPeriodMatchups { period, pairings })
        })
        .collect()
}

fn parse_scoring(v: &Value) -> Option<OfficialScoring> {
    let kind = s(v, "type")?;
    let mut skater = Map::new();
    let mut goalie = Map::new();
    for group in v.get("scoringCategorySettings")?.as_array()? {
        let target = match group.get("group")?.get("code")?.as_str()? {
            "HOCKEY_GOALIE" => &mut goalie,
            "HOCKEY_SKATING" => &mut skater,
            _ => continue,
        };
        for c in group.get("configs")?.as_array()? {
            let name = c.get("scoringCategory")?.get("shortName")?.as_str()?.to_string();
            target.insert(name, c.get("points")?.as_f64()?);
        }
    }
    Some(OfficialScoring { kind, skater, goalie })
}

fn parse_roster_limits(v: &Value) -> Option<OfficialRosterLimits> {
    let mut by_pos = Map::new();
    for (pos, c) in v.get("positionConstraints")?.as_object()? {
        by_pos.insert(pos.clone(), int(c, "maxActive")?);
    }
    Some(OfficialRosterLimits {
        max_total: int(v, "maxTotalPlayers")?,
        max_active: int(v, "maxTotalActivePlayers")?,
        max_reserve: int(v, "maxTotalReservePlayers")?,
        max_active_by_position: by_pos,
    })
}

/// `getLeagueInfo` (called with `excludePlayerInfo=true`). The calendar and team list are required; the
/// playoff, scoring, schedule and roster-limit parts are None / empty for league types that lack them.
pub fn parse_league_info(v: &Value) -> Result<OfficialLeagueInfo, String> {
    let teams = v
        .get("teamInfo")
        .and_then(Value::as_object)
        .ok_or("no team list")?
        .iter()
        .map(|(id, t)| OfficialTeam { id: id.clone(), name: s(t, "name").unwrap_or_default() })
        .collect();
    let periods = |k: &str| v.get(k).map(parse_periods).transpose().map(Option::unwrap_or_default);
    Ok(OfficialLeagueInfo {
        name: s(v, "leagueName").unwrap_or_default(),
        season_year: int(v, "seasonYear").ok_or("no season year")?,
        start_date: s(v, "startDate").as_deref().and_then(date_as_sent),
        end_date: s(v, "endDate").as_deref().and_then(date_as_sent),
        teams,
        scoring_periods: periods("scoringPeriods")?,
        roster_periods: periods("rosterPeriods")?,
        playoffs: v.get("playoffs").and_then(parse_playoffs),
        matchups: v.get("matchups").map(parse_matchups).transpose()?.unwrap_or_default(),
        scoring: v.get("scoringSystem").and_then(parse_scoring),
        roster_limits: v.get("rosterInfo").and_then(parse_roster_limits),
    })
}

fn parse_picks(list: &Value) -> Vec<OfficialPick> {
    list.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    Some(OfficialPick {
                        year: int(p, "year")?,
                        round: int(p, "round")?,
                        owner_team_id: s(p, "currentOwnerTeamId")?,
                        original_owner_team_id: s(p, "originalOwnerTeamId")?,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `getDraftPicks`. The shape of `currentDraftPicks` when it is populated has not been seen; it is read
/// with the same fields as `futureDraftPicks` and entries that do not fit are left out.
pub fn parse_draft_picks(v: &Value) -> Result<OfficialDraftPicks, String> {
    let future = v.get("futureDraftPicks").ok_or("no future picks")?;
    Ok(OfficialDraftPicks { future: parse_picks(future), current: v.get("currentDraftPicks").map(parse_picks).unwrap_or_default() })
}

/// "W-L-T" as three non-negative integers.
fn split_record(text: &str) -> Option<(i64, i64, i64)> {
    let mut it = text.split('-').map(|p| p.trim().parse::<i64>().ok().filter(|n| *n >= 0));
    let rec = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(rec)
}

/// `getStandings`: a list with one entry per team.
pub fn parse_standings(v: &Value) -> Result<Vec<OfficialStanding>, String> {
    let arr = v.as_array().ok_or("standings are not a list")?;
    arr.iter()
        .map(|t| {
            Ok(OfficialStanding {
                team_id: s(t, "teamId").ok_or("a standings row has no team id")?,
                team_name: s(t, "teamName").unwrap_or_default(),
                rank: int(t, "rank").ok_or("a standings row has no rank")?,
                record: t.get("points").and_then(Value::as_str).and_then(split_record),
                points_for: t.get("totalPointsFor").and_then(Value::as_f64).unwrap_or(0.0),
            })
        })
        .collect()
}

fn score_side(v: &Value) -> Option<OfficialScoreSide> {
    Some(OfficialScoreSide {
        team_id: s(v, "teamId")?,
        team_name: s(v, "teamName").unwrap_or_default(),
        score: v.get("score").and_then(Value::as_f64),
        games_played: int(v, "gamesPlayed"),
    })
}

/// `getMatchupScores`: pairings whose two sides both carry a team id.
pub fn parse_matchup_scores(v: &Value) -> Result<OfficialScoreboard, String> {
    let period = int(v, "period").ok_or("no period")?;
    let list = v.get("matchups").and_then(Value::as_array).ok_or("no matchups")?;
    let matchups = list.iter().filter_map(|m| Some((score_side(m.get("away")?)?, score_side(m.get("home")?)?))).collect();
    Ok(OfficialScoreboard { period, matchups })
}

/// The owner's game in scoring period `period` as (opponent id, my points, their points): the opponent
/// from the league's matchup list, the points from that period's scoreboard (a side without a score yet
/// counts 0, as the private view does). `Ok(None)` when the matchup list has no pairing of two real teams
/// for `my_id` (playoff periods hold seed placeholders); `Err` when the scoreboard lacks that pairing,
/// which would mean the two official reads disagree.
pub fn my_matchup(
    info: &OfficialLeagueInfo,
    board: &OfficialScoreboard,
    my_id: &str,
    period: i64,
) -> Result<Option<(String, f64, f64)>, String> {
    let Some(pairing) = info
        .matchups
        .iter()
        .filter(|m| m.period == period)
        .flat_map(|m| &m.pairings)
        .find(|p| p.away.id == my_id || p.home.id == my_id)
    else {
        return Ok(None);
    };
    let opponent = if pairing.away.id == my_id { &pairing.home.id } else { &pairing.away.id };
    let sides = board.matchups.iter().find_map(|(a, h)| match (a.team_id == my_id, h.team_id == my_id) {
        (true, _) if h.team_id == *opponent => Some((a, h)),
        (_, true) if a.team_id == *opponent => Some((h, a)),
        _ => None,
    });
    let Some((mine, theirs)) = sides else {
        return Err("the matchup list and the scores disagree".to_string());
    };
    Ok(Some((opponent.clone(), mine.score.unwrap_or(0.0), theirs.score.unwrap_or(0.0))))
}

fn roster_status(text: &str) -> OfficialRosterStatus {
    match text {
        "ACTIVE" => OfficialRosterStatus::Active,
        "RESERVE" => OfficialRosterStatus::Reserve,
        "INJURED_RESERVE" => OfficialRosterStatus::InjuredReserve,
        other => OfficialRosterStatus::Other(other.to_string()),
    }
}

/// `getTeamRosters`: every team's players with status and position. There are no empty slots and no
/// player details; names come from `getPlayerIds` or the private endpoint.
pub fn parse_team_rosters(v: &Value) -> Result<OfficialRosters, String> {
    let period = int(v, "period").ok_or("no period")?;
    let rosters = v.get("rosters").and_then(Value::as_object).ok_or("no rosters")?;
    let teams = rosters
        .iter()
        .map(|(id, t)| {
            let items = t
                .get("rosterItems")
                .and_then(Value::as_array)
                .ok_or("a team has no roster items")?
                .iter()
                .filter_map(|i| {
                    Some(OfficialRosterItem {
                        player_id: s(i, "id")?,
                        position: s(i, "position").unwrap_or_default(),
                        status: roster_status(&s(i, "status")?),
                    })
                })
                .collect();
            Ok(OfficialTeamRoster { team_id: id.clone(), team_name: s(t, "teamName").unwrap_or_default(), items })
        })
        .collect::<Result<_, String>>()?;
    Ok(OfficialRosters { period, teams })
}

/// `getAdp`: player id to average draft position. Rows with the 10000 sentinel (never drafted) or no
/// number are left out, so a missing id means "no ADP".
pub fn parse_adp(v: &Value) -> Result<Map<String, f64>, String> {
    let arr = v.as_array().ok_or("ADP is not a list")?;
    let mut out = Map::new();
    for row in arr {
        let (Some(id), Some(adp)) = (s(row, "id"), row.get("ADP").and_then(Value::as_f64)) else { continue };
        if adp.is_finite() && adp != ADP_MISSING {
            out.insert(id, adp);
        }
    }
    Ok(out)
}

/// "Last, First" as "First Last" (split at the first comma, so "Smith Jr., John" is "John Smith Jr.").
/// A name without a comma is kept. The API strips apostrophes ("OReilly, Ryan"), which is not undone.
pub fn flip_name(name: &str) -> String {
    match name.split_once(',') {
        Some((last, first)) if !first.trim().is_empty() => format!("{} {}", first.trim(), last.trim()),
        _ => name.trim().to_string(),
    }
}

/// `getPlayerIds`: player id to name, team and position. The pseudo-players for team totals (ids
/// containing `#`, such as "30010#2030" for "Team Skaters") are left out.
pub fn parse_player_ids(v: &Value) -> Result<Map<String, OfficialPlayerName>, String> {
    let obj = v.as_object().ok_or("player ids are not an object")?;
    let mut out = Map::new();
    for (id, p) in obj {
        if id.contains('#') {
            continue;
        }
        let Some(name) = s(p, "name") else { continue };
        out.insert(
            id.clone(),
            OfficialPlayerName { name: flip_name(&name), team: s(p, "team").unwrap_or_default(), position: s(p, "position").unwrap_or_default() },
        );
    }
    Ok(out)
}

// --------------------------------------------------------------------------- client

pub struct FantraxOfficial {
    cache: Arc<HttpCache>,
    league_id: String,
}

fn params(items: &[(&str, &str)]) -> Params {
    items.iter().map(|(k, v)| ((*k).to_string(), ParamValue::Str((*v).to_string()))).collect()
}

impl FantraxOfficial {
    pub fn new(cache: Arc<HttpCache>, league_id: impl Into<String>) -> Self {
        FantraxOfficial { cache, league_id: league_id.into() }
    }

    /// One GET through the shared cache. The API reports some errors as HTTP 200 with an `error` field
    /// and the cache stores every 2xx body, so a body that is an error or does not parse is deleted
    /// from the cache (every row of that endpoint, since rows are keyed by URL plus parameters) and
    /// reported as a failure; it would otherwise be replayed until its TTL ends, or for ever offline.
    fn call<T>(
        &self,
        endpoint: &str,
        p: Params,
        ttl: f64,
        parse: impl FnOnce(&Value) -> Result<T, String>,
    ) -> Result<T, OfficialError> {
        let url = format!("{BASE_URL}/{endpoint}");
        let text = self.cache.get_text(&url, &p, &[], &[], ttl).map_err(|e| classify_transport(&e))?;
        let parsed = serde_json::from_str::<Value>(&text).map_err(|_| "the response was not JSON".to_string()).and_then(|v| {
            match v.get("error") {
                Some(err) => Err(err.get("message").and_then(Value::as_str).unwrap_or("the API returned an error").to_string()),
                None => parse(&v).map_err(|e| format!("unexpected response: {e}")),
            }
        });
        parsed.map_err(|detail| {
            let _ = self.cache.clear(Some(&url));
            OfficialError::Failed(safe_detail(&detail))
        })
    }

    pub fn league_info(&self) -> Result<OfficialLeagueInfo, OfficialError> {
        let p = params(&[("leagueId", &self.league_id), ("excludePlayerInfo", "true")]);
        self.call("getLeagueInfo", p, TTL_LEAGUE_INFO, parse_league_info)
    }

    pub fn draft_picks(&self) -> Result<OfficialDraftPicks, OfficialError> {
        self.call("getDraftPicks", params(&[("leagueId", &self.league_id)]), TTL_DRAFT_PICKS, parse_draft_picks)
    }

    pub fn standings(&self) -> Result<Vec<OfficialStanding>, OfficialError> {
        self.call("getStandings", params(&[("leagueId", &self.league_id)]), TTL_STANDINGS, parse_standings)
    }

    /// Scores of scoring period `period`, or of the current one when None.
    pub fn matchup_scores(&self, period: Option<i64>) -> Result<OfficialScoreboard, OfficialError> {
        let mut items = vec![("leagueId", self.league_id.clone())];
        if let Some(n) = period {
            items.push(("period", n.to_string()));
        }
        let p: Params = items.into_iter().map(|(k, v)| (k.to_string(), ParamValue::Str(v))).collect();
        self.call("getMatchupScores", p, TTL_MATCHUP_SCORES, parse_matchup_scores)
    }

    /// Rosters of roster period `period`. Without a period the API returns the upcoming lineup once the
    /// day's last game has started, so callers should always pass the period for the date they want
    /// (see [`period_for_date`] over `OfficialLeagueInfo::roster_periods`).
    pub fn team_rosters(&self, period: Option<i64>) -> Result<OfficialRosters, OfficialError> {
        let mut items = vec![("leagueId", self.league_id.clone())];
        if let Some(n) = period {
            items.push(("period", n.to_string()));
        }
        let p: Params = items.into_iter().map(|(k, v)| (k.to_string(), ParamValue::Str(v))).collect();
        self.call("getTeamRosters", p, TTL_TEAM_ROSTERS, parse_team_rosters)
    }

    /// Every NHL player's name, team and position, by Fantrax player id.
    pub fn player_ids(&self) -> Result<Map<String, OfficialPlayerName>, OfficialError> {
        self.call("getPlayerIds", params(&[("sport", "NHL")]), TTL_PLAYER_IDS, parse_player_ids)
    }

    /// Average draft position of every NHL player that has one, by Fantrax player id.
    pub fn adp(&self) -> Result<Map<String, f64>, OfficialError> {
        self.call("getAdp", params(&[("sport", "NHL")]), TTL_ADP, parse_adp)
    }
}
