//! `recommend/base.py`: `RECENT_ADDS_WINDOW`, `count_adds`, `recent_adds_from` (moved here so
//! fm-providers and fm-engine can share them).

use chrono::{Duration, NaiveDate};

use crate::models::{ActivityAction, ActivityItem};
use crate::Map;

/// days of my adds kept in `LeagueContext.recent_adds`
pub const RECENT_ADDS_WINDOW: i64 = 14;

/// Python compares `str(it.team_id) != str(team_id)`; a missing id stringifies as "None".
fn is_mine(it: &ActivityItem, team_id: &str) -> bool {
    it.action == ActivityAction::Add && it.team_id.as_deref().unwrap_or("None") == team_id
}

/// My acquisitions in [start, end]: ADD items (free-agent adds and waiver claims) of `team_id`.
/// Drops, trades, IR moves and other teams' adds do not count.
pub fn count_adds(items: &[ActivityItem], team_id: &str, start: Option<NaiveDate>, end: Option<NaiveDate>) -> usize {
    items
        .iter()
        .filter(|it| is_mine(it, team_id))
        .filter(|it| {
            let d = it.ts.date();
            start.map_or(true, |s| d >= s) && end.map_or(true, |e| d <= e)
        })
        .count()
}

/// {cid: date of my latest add} for my ADD items in the `days` days up to `as_of`.
pub fn recent_adds_from(items: &[ActivityItem], team_id: &str, as_of: NaiveDate, days: i64) -> Map<String, NaiveDate> {
    let mut out: Map<String, NaiveDate> = Map::new();
    let since = as_of - Duration::days(days);
    for it in items.iter().filter(|it| is_mine(it, team_id)) {
        let Some(cid) = it.cid.as_deref().filter(|c| !c.is_empty()) else { continue };
        let d = it.ts.date();
        if since <= d && d <= as_of && out.get(cid).map_or(true, |&prev| d > prev) {
            out.insert(cid.to_string(), d);
        }
    }
    out
}
