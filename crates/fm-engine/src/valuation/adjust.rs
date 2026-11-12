//! Rust port of `fantasy_manager/valuation/adjust.py`: availability adjustments for injury /
//! suspension status (flat multipliers from `params`, season availability sized from the games
//! missed, status-note parsing, first match wins).
//! Owner: work package `W2-valuation-base` (wave 2).
//!
//! The note regexes are the Python ones verbatim (`regex` crate, leftmost-first like `re`;
//! `fancy-regex` only for `_MONTH_PHRASE`, which has a negative lookahead).
//! Dates are `chrono::NaiveDate`; `team_dates` is `Option<&[NaiveDate]>` (None / empty = no schedule).

use std::sync::{LazyLock, OnceLock, RwLock};

use chrono::{Datelike, Duration, NaiveDate};
use fancy_regex::Regex as FancyRegex;
use fm_core::values::Horizon;
use fm_core::Map;
use fm_pycompat::fmt::fixed;
use fm_pycompat::num::py_round_i64;
use regex::{Captures, Regex};
use serde_json::Value;

use super::params;

/// games per NHL team and days of a regular season when no schedule is loaded (the 82-game
/// seasons through 2025-26)
pub const SEASON_GAMES: i64 = 82;
pub const SEASON_DAYS: i64 = 186;
pub const FLOOR: f64 = 0.05;
pub const DAY_TO_DAY_DAYS: i64 = 3;
pub const WEEK_TO_WEEK_DAYS: i64 = 21;
pub const MONTH_TO_MONTH_DAYS: i64 = 45;
pub const IR_DEFAULT_DAYS: i64 = 30;
pub const SUSPENDED_DEFAULT_GAMES: i64 = 5;
pub const MONTH_DAYS: f64 = 30.4;
pub const OUT_STATUSES: [&str; 5] = ["dtd", "out", "ir", "ltir", "suspended"];

static AVAILABILITY: OnceLock<params::AvailabilityTable> = OnceLock::new();

/// `adjust.AVAILABILITY`: import-time snapshot (display / old imports).
pub fn availability_snapshot() -> &'static params::AvailabilityTable {
    AVAILABILITY.get_or_init(|| params::availability_table(None))
}

pub fn availability_multiplier(status: &str, horizon: Horizon, params: Option<&Value>) -> f64 {
    params::availability(status, horizon, params)
}

// --------------------------------------------------------------------------- note parsing

const NUMS: [(&str, i64); 24] = [
    ("a", 1), ("an", 1), ("one", 1), ("two", 2), ("three", 3), ("four", 4), ("five", 5), ("six", 6),
    ("seven", 7), ("eight", 8), ("nine", 9), ("ten", 10), ("eleven", 11), ("twelve", 12), ("thirteen", 13),
    ("fourteen", 14), ("fifteen", 15), ("sixteen", 16), ("seventeen", 17), ("eighteen", 18),
    ("nineteen", 19), ("twenty", 20), ("couple", 2), ("few", 3),
];
const MONTH_NAMES: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const MONTH_RE: &str = r"(Jan(?:uary)?|Feb(?:ruary)?|Mar(?:ch)?|Apr(?:il)?|May|June?|July?|Aug(?:ust)?|Sept?(?:ember)?|Oct(?:ober)?|Nov(?:ember)?|Dec(?:ember)?)";

fn unit_days(unit: &str) -> f64 {
    match unit {
        "day" => 1.0,
        "week" => 7.0,
        _ => MONTH_DAYS, // "month"
    }
}

fn num_re() -> String {
    // sorted(_NUMS, key=len, reverse=True): stable, longest first
    let mut names: Vec<&str> = NUMS.iter().map(|(n, _)| *n).collect();
    names.sort_by(|a, b| b.len().cmp(&a.len()));
    format!(r"(\d{{1,3}}|{})", names.join("|"))
}

fn build(pat: &str) -> Regex {
    let p = pat.replace("@NUM@", &num_re()).replace("@MONTH@", MONTH_RE);
    Regex::new(&p).unwrap_or_else(|e| panic!("bad regex {p}: {e}"))
}

static EST_RETURN: LazyLock<Regex> = LazyLock::new(|| build(r"(?i)est\.?\s*return\s+(\d{4}-\d{2}-\d{2})"));
static GAMES: LazyLock<[Regex; 3]> = LazyLock::new(|| {
    [
        build(r"(?i)\b@NUM@[- ]game\s+suspension\b"),
        build(r"(?i)\bsuspended\s+(?:for\s+)?@NUM@\s+games\b"),
        build(
            r"(?i)\b(?:miss|misses|missing|sit\s+out|serve|out\s+for|sidelined\s+for)\s+(?:at\s+least\s+)?(?:the\s+|his\s+|[A-Z][\w.]*(?:'s|’s)\s+)?(?:first\s+|next\s+|opening\s+)?@NUM@\s+(?:regular[- ]season\s+)?games\b",
        ),
    ]
});
static DATE: LazyLock<Regex> = LazyLock::new(|| {
    build(r"(?i)\b(?:return|returns|returning|back|activated|available|rejoin)\b[^.;]{0,25}?\b(?:on|by|for)?\s*@MONTH@\.?\s+(\d{1,2})\b")
});
static FIRST: LazyLock<Regex> = LazyLock::new(|| {
    build(r"(?i)\bfirst\s+(?:@NUM@\s+)?(week|month)s?\b(?:\s+of\s+(?:the\s+)?(?:\d{4}-\d{2}\s+)?(?:regular\s+)?(?:season|campaign|year))?")
});
static RANGE: LazyLock<Regex> = LazyLock::new(|| build(r"(?i)\b@NUM@\s*(?:-|–|to)\s*@NUM@\s+(day|week|month)s?\b"));
static SINGLE: LazyLock<Regex> = LazyLock::new(|| {
    build(r"(?i)\b(?:another|about|approximately|roughly|around|out|miss|sidelined|for|at\s+least)\s+(?:for\s+)?(?:about\s+|at\s+least\s+)?@NUM@\s+(day|week|month)s?\b")
});
static MONTH_PHRASE: LazyLock<FancyRegex> = LazyLock::new(|| {
    let p = r"(?i)\b(until|by|sometime\s+in|at\s+least|around|before|(?:return|back|rejoin\w*)(?:\s+\w+){0,3}?\s+in)\s+(?:at\s+least\s+)?(?:the\s+)?(?:(early|mid|late)[- ])?@MONTH@\b(?!\.?\s+\d)"
        .replace("@MONTH@", MONTH_RE);
    FancyRegex::new(&p).expect("month phrase regex")
});
static INJURED_IN: LazyLock<Regex> = LazyLock::new(|| {
    build(r"(?i)\b(?:surgery|procedure|injur\w*|hurt|operation)\b[^.;]{0,20}?\b(?:in|on)\s+(?:(early|mid|late)[- ])?@MONTH@\b")
});
static NOT_INJURY: LazyLock<Regex> = LazyLock::new(|| build(r"(?i)\bnot\s+injury[- ]related\b"));
static TO_PHRASES: LazyLock<[(Regex, i64); 3]> = LazyLock::new(|| {
    [
        (build(r"(?i)\bmonth[- ]to[- ]month\b"), MONTH_TO_MONTH_DAYS),
        (build(r"(?i)\bweek[- ]to[- ]week\b"), WEEK_TO_WEEK_DAYS),
        (build(r"(?i)\bday[- ]to[- ]day\b"), DAY_TO_DAY_DAYS),
    ]
});
static MISS_WORD: LazyLock<Regex> = LazyLock::new(|| build(r"(?i)\bmiss\b"));

fn num_word(s: &str) -> i64 {
    let s = s.to_lowercase();
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
        return s.parse().unwrap_or(0);
    }
    NUMS.iter().find(|(n, _)| *n == s).map(|(_, v)| *v).unwrap_or(0)
}

fn season_year(as_of: NaiveDate) -> i32 {
    if as_of.month() >= 7 {
        as_of.year()
    } else {
        as_of.year() - 1
    }
}

fn month_date(month: &str, day: i64, as_of: NaiveDate) -> Option<NaiveDate> {
    let key: String = month.chars().take(3).collect::<String>().to_lowercase();
    let m = MONTH_NAMES.iter().position(|x| *x == key)? as u32 + 1;
    let y = season_year(as_of);
    let year = if m >= 7 { y } else { y + 1 };
    if day < 1 || day > 31 {
        return None;
    }
    NaiveDate::from_ymd_opt(year, m, day as u32)
}

/// How long a player is expected to be out (`games_missed` of `games_remaining`).
#[derive(Clone, Debug, PartialEq)]
pub struct ReturnEstimate {
    pub return_date: Option<NaiveDate>,
    pub games_missed: f64,
    pub games_remaining: f64,
    pub multiplier: f64,
    /// what was read, e.g. `note: "est. return 2026-10-17"`
    pub source: String,
}

impl ReturnEstimate {
    pub fn text(&self) -> String {
        let when = match self.return_date {
            Some(d) => format!("back ~{}, ", d.format("%Y-%m-%d")),
            None => String::new(),
        };
        format!(
            "Expected return: {}~{} of {} remaining games missed ({}): season availability x{}",
            when,
            fixed(self.games_missed, 0, false),
            fixed(self.games_remaining, 0, false),
            self.source,
            fixed(self.multiplier, 2, false)
        )
    }
}

#[derive(Clone, Copy, Debug)]
enum Parsed {
    Date(NaiveDate),
    Games(i64),
    DaysFromStart(f64),
    Days(f64),
}

fn cap<'a>(c: &Captures<'a>, i: usize) -> Option<&'a str> {
    c.get(i).map(|m| m.as_str())
}

/// (kind, value, matched text) from the note.
fn parse(note: &str, as_of: NaiveDate) -> Option<(Parsed, String)> {
    if let Some(m) = EST_RETURN.captures(note) {
        if let Ok(d) = NaiveDate::parse_from_str(&m[1], "%Y-%m-%d") {
            return Some((Parsed::Date(d), m[0].to_string()));
        }
    }
    for pat in GAMES.iter() {
        if let Some(m) = pat.captures(note) {
            let n = num_word(&m[1]);
            if n > 0 {
                return Some((Parsed::Games(n), m[0].to_string()));
            }
        }
    }
    if let Some(m) = DATE.captures(note) {
        let day: i64 = m[2].parse().unwrap_or(0);
        if let Some(d) = month_date(&m[1], day, as_of) {
            return Some((Parsed::Date(d), m[0].to_string()));
        }
    }
    if let Some(m) = FIRST.captures(note) {
        let whole = m[0].to_lowercase();
        if whole.contains("season") || whole.contains("campaign") || whole.contains("year") || MISS_WORD.is_match(note) {
            let n = cap(&m, 1).map(num_word).unwrap_or(1);
            // `m.group(1)` falsy (None / "") -> 1
            let n = if n == 0 && cap(&m, 1).map(|s| s.is_empty()).unwrap_or(true) { 1 } else { n };
            return Some((Parsed::DaysFromStart(n as f64 * unit_days(&m[2].to_lowercase())), m[0].to_string()));
        }
    }
    if let Some(m) = RANGE.captures(note) {
        let (a, b) = (num_word(&m[1]), num_word(&m[2]));
        if a > 0 && b >= a {
            return Some((Parsed::Days((a + b) as f64 / 2.0 * unit_days(&m[3].to_lowercase())), m[0].to_string()));
        }
    }
    if let Some(m) = SINGLE.captures(note) {
        let n = num_word(&m[1]);
        if n > 0 {
            return Some((Parsed::Days(n as f64 * unit_days(&m[2].to_lowercase())), m[0].to_string()));
        }
    }
    if let Ok(Some(m)) = MONTH_PHRASE.captures(note) {
        let lead = m.get(1).map(|x| x.as_str().to_lowercase()).unwrap_or_default();
        let part = m.get(2).map(|x| x.as_str().to_lowercase()).unwrap_or_default();
        let month = m.get(3).map(|x| x.as_str()).unwrap_or("");
        let day = match part.as_str() {
            "early" => 5,
            "mid" => 15,
            "late" => 25,
            _ => {
                if matches!(lead.as_str(), "until" | "at least" | "before") {
                    1
                } else {
                    15
                }
            }
        };
        if let Some(d) = month_date(month, day, as_of) {
            return Some((Parsed::Date(d), m.get(0).map(|x| x.as_str()).unwrap_or("").to_string()));
        }
    }
    for (pat, days) in TO_PHRASES.iter() {
        if let Some(m) = pat.find(note) {
            return Some((Parsed::Days(*days as f64), m.as_str().to_string()));
        }
    }
    None
}

// --------------------------------------------------------------------------- schedule helpers

/// Length of the regular season, for a player whose own team has no dates on the schedule.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeasonLength {
    /// games per NHL team
    pub games: f64,
    /// days from the first game to the last, inclusive
    pub days: f64,
}

impl SeasonLength {
    /// SEASON_GAMES in SEASON_DAYS: no schedule loaded.
    pub const DEFAULT: SeasonLength = SeasonLength { games: SEASON_GAMES as f64, days: SEASON_DAYS as f64 };

    /// Read from the loaded schedule (team -> regular-season game dates): the most game dates any
    /// team has, over the days from the league's first game to its last. DEFAULT when it is empty.
    /// Python always assumed 82 games in 186 days; the 2026-27 schedule has 84 in 194.
    /// A length pinned with [`pin_season_length`] wins over the schedule.
    pub fn of(schedule: &Map<String, Vec<NaiveDate>>) -> SeasonLength {
        if let Some(pinned) = *PINNED_SEASON.read().unwrap_or_else(|e| e.into_inner()) {
            return pinned;
        }
        let mut all = schedule.values().flatten();
        let Some(&first) = all.next() else { return SeasonLength::DEFAULT };
        let (lo, hi) = all.fold((first, first), |(lo, hi), &d| (lo.min(d), hi.max(d)));
        let games = schedule.values().map(|ds| Dates::new(Some(ds)).0.len()).max().unwrap_or(0);
        SeasonLength { games: games as f64, days: ((hi - lo).num_days() + 1) as f64 }
    }
}

static PINNED_SEASON: RwLock<Option<SeasonLength>> = RwLock::new(None);

/// Make every later [`SeasonLength::of`] in this process return `season` whatever the schedule
/// says (None: read the schedule again). Only the Python parity tests and the CLI's hidden
/// `--from-snapshot` mode use it: their fixtures were produced by code that assumed 82 games in
/// 186 days, so they pin [`SeasonLength::DEFAULT`] to compare everything else exactly.
pub fn pin_season_length(season: Option<SeasonLength>) {
    *PINNED_SEASON.write().unwrap_or_else(|e| e.into_inner()) = season;
}

/// `set(team_dates)` sorted; empty = no schedule.
struct Dates(Vec<NaiveDate>);

impl Dates {
    fn new(team_dates: Option<&[NaiveDate]>) -> Dates {
        let mut v = team_dates.map(|d| d.to_vec()).unwrap_or_default();
        v.sort();
        v.dedup();
        Dates(v)
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// `_count_games`: games in [start, end) (end None: unbounded); None without a schedule.
    fn count(&self, start: NaiveDate, end: Option<NaiveDate>) -> Option<i64> {
        if self.is_empty() {
            return None;
        }
        let lo = self.0.partition_point(|d| *d < start);
        let hi = match end {
            Some(e) => self.0.partition_point(|d| *d < e),
            None => self.0.len(),
        };
        Some(hi.saturating_sub(lo) as i64)
    }
    /// `_date_after_games`
    fn after_games(&self, n: f64, as_of: NaiveDate) -> Option<NaiveDate> {
        if self.is_empty() {
            return None;
        }
        let lo = self.0.partition_point(|d| *d < as_of);
        let k = py_round_i64(n);
        if k < 0 {
            return None;
        }
        self.0[lo..].get(k as usize).copied()
    }
}

fn games_between(
    start: NaiveDate,
    end: NaiveDate,
    dates: &Dates,
    season_start: Option<NaiveDate>,
    season: SeasonLength,
) -> f64 {
    if let Some(n) = dates.count(start, Some(end)) {
        return n as f64;
    }
    let first = match season_start {
        Some(s) => start.max(s),
        None => start,
    };
    let days = (end - first).num_days() as f64;
    days.max(0.0) * season.games / season.days
}

fn games_remaining_in(as_of: NaiveDate, dates: &Dates, season_start: Option<NaiveDate>, season: SeasonLength) -> f64 {
    if let Some(n) = dates.count(as_of, None) {
        if n != 0 {
            return n as f64;
        }
    }
    match season_start {
        Some(s) if as_of > s => {
            (season.games * (1.0 - (as_of - s).num_days() as f64 / season.days)).max(1.0)
        }
        _ => season.games,
    }
}

/// `_games_remaining`: the team's games from `as_of` on (its schedule), else the share of the
/// season (`season`) left (at least 1; the full season before it starts).
pub fn games_remaining(
    as_of: NaiveDate,
    team_dates: Option<&[NaiveDate]>,
    season_start: Option<NaiveDate>,
    season: SeasonLength,
) -> f64 {
    games_remaining_in(as_of, &Dates::new(team_dates), season_start, season)
}

fn add_days(d: NaiveDate, days: i64) -> NaiveDate {
    d + Duration::days(days)
}

/// The sized absence of a player with `status`; None when healthy / unknown or when nothing can be
/// inferred (the flat multiplier applies). `season` sizes the games of a team without dates.
pub fn return_estimate(
    status: &str,
    note: Option<&str>,
    as_of: NaiveDate,
    team_dates: Option<&[NaiveDate]>,
    season_start: Option<NaiveDate>,
    season: SeasonLength,
) -> Option<ReturnEstimate> {
    if !OUT_STATUSES.contains(&status) {
        return None;
    }
    let dates = Dates::new(team_dates);
    let remaining = games_remaining_in(as_of, &dates, season_start, season);
    let anchor = match season_start {
        Some(s) => as_of.max(s),
        None => as_of,
    };
    let note_s = note.unwrap_or("");
    let got = parse(note_s, as_of);
    let ret: Option<NaiveDate>;
    let src: String;
    let mut missed: f64;
    if let Some((kind, text)) = got {
        let mut s = format!("note: \"{}\"", text.trim());
        match kind {
            Parsed::Games(n) => {
                missed = n as f64;
                ret = dates.after_games(missed, as_of);
            }
            other => {
                let r = match other {
                    Parsed::Date(d) => d,
                    Parsed::DaysFromStart(v) => add_days(anchor, py_round_i64(v)),
                    Parsed::Days(v) => {
                        let mut start = anchor;
                        if let Some(inj) = INJURED_IN.captures(note_s) {
                            let part = cap(&inj, 1).unwrap_or("").to_lowercase();
                            let dd = match part.as_str() {
                                "early" => 5,
                                "mid" => 15,
                                "late" => 25,
                                _ => 15,
                            };
                            let mut d = month_date(&inj[2], dd, as_of);
                            if let Some(x) = d {
                                if x > as_of {
                                    // "in June" said in September: last June
                                    d = x.with_year(x.year() - 1);
                                }
                            }
                            if let Some(x) = d {
                                if x < as_of {
                                    start = x;
                                    s = format!("{s}, from \"{}\"", inj[0].trim());
                                }
                            }
                        }
                        add_days(start, py_round_i64(v))
                    }
                    Parsed::Games(_) => unreachable!(),
                };
                ret = Some(r);
                missed = if r > as_of { games_between(as_of, r, &dates, season_start, season) } else { 0.0 };
            }
        }
        src = s;
    } else if status == "dtd" || (status == "out" && NOT_INJURY.is_match(note_s)) {
        let why = if status == "dtd" { "day-to-day" } else { "not injury related, no timetable" };
        let r = add_days(anchor, DAY_TO_DAY_DAYS);
        ret = Some(r);
        src = format!("{why}, ~{DAY_TO_DAY_DAYS} days assumed");
        missed = games_between(as_of, r, &dates, season_start, season);
    } else if status == "ir" {
        let r = add_days(anchor, IR_DEFAULT_DAYS);
        ret = Some(r);
        src = format!("IR without a timetable, {IR_DEFAULT_DAYS} days assumed");
        missed = games_between(as_of, r, &dates, season_start, season);
    } else if status == "ltir" {
        ret = None;
        src = "LTIR without a timetable: rest of the season assumed".to_string();
        missed = remaining;
    } else if status == "suspended" {
        missed = SUSPENDED_DEFAULT_GAMES as f64;
        src = format!("suspension length unknown, {SUSPENDED_DEFAULT_GAMES} games assumed");
        ret = dates.after_games(missed, as_of);
    } else {
        return None;
    }
    missed = missed.min(remaining);
    let multiplier = if remaining > 0.0 { FLOOR.max(1.0 - missed / remaining) } else { FLOOR };
    Some(ReturnEstimate { return_date: ret, games_missed: missed, games_remaining: remaining, multiplier, source: src })
}

/// (estimated return date or None, estimated games missed or None when nothing can be inferred).
pub fn expected_return(
    status: &str,
    note: Option<&str>,
    as_of: NaiveDate,
    team_dates: Option<&[NaiveDate]>,
    season_start: Option<NaiveDate>,
    season: SeasonLength,
) -> (Option<NaiveDate>, Option<f64>) {
    match return_estimate(status, note, as_of, team_dates, season_start, season) {
        None => (None, None),
        Some(e) => (e.return_date, Some(e.games_missed)),
    }
}

/// (season-horizon availability multiplier, the estimate behind it or None for the flat value).
pub fn season_availability(
    status: &str,
    note: Option<&str>,
    as_of: NaiveDate,
    team_dates: Option<&[NaiveDate]>,
    season_start: Option<NaiveDate>,
    season: SeasonLength,
    params: Option<&Value>,
) -> (f64, Option<ReturnEstimate>) {
    match return_estimate(status, note, as_of, team_dates, season_start, season) {
        None => (availability_multiplier(status, Horizon::Season, params), None),
        Some(e) => (e.multiplier, Some(e)),
    }
}
