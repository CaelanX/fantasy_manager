//! `analysis/schedule_grid.py`: `monday`, `per_day`, `season_bounds`, `calendar_weeks` (moved
//! here so fm-providers and fm-engine can share them; the Python originals stay put).

use chrono::{Datelike, Duration, NaiveDate};

use crate::models::LeagueContext;
use crate::Map;

/// A normal week with fewer than this share of the median week's NHL games is a break week.
pub const LIGHT_WEEK_SHARE: f64 = 0.4;

/// The Monday of `d`'s week.
pub fn monday(d: NaiveDate) -> NaiveDate {
    d - Duration::days(d.weekday().num_days_from_monday() as i64)
}

/// NHL games per date (`ctx.games_per_day`, else derived from the team schedules, date-sorted).
pub fn per_day(ctx: &LeagueContext) -> Map<NaiveDate, i64> {
    if !ctx.games_per_day.is_empty() {
        return ctx.games_per_day.clone();
    }
    let mut teams: std::collections::BTreeMap<NaiveDate, i64> = std::collections::BTreeMap::new();
    for dates in ctx.schedule.values() {
        let uniq: std::collections::BTreeSet<&NaiveDate> = dates.iter().collect();
        for d in uniq {
            *teams.entry(*d).or_insert(0) += 1;
        }
    }
    teams.into_iter().map(|(d, n)| (d, (n + 1) / 2)).collect()
}

/// (first, last) regular-season game date in the loaded schedule.
pub fn season_bounds(ctx: &LeagueContext) -> Option<(NaiveDate, NaiveDate)> {
    let mut it = ctx.schedule.values().flatten();
    let first = *it.next()?;
    let (mut lo, mut hi) = (first, first);
    for d in it {
        lo = lo.min(*d);
        hi = hi.max(*d);
    }
    Some((lo, hi))
}

/// Monday-Sunday weeks covering [first, last] (default: the loaded regular season). A light week
/// (fewer than 40% of a normal week's NHL games: the All-Star break) is merged into the week after
/// it, as Fantrax does; while there are still more weeks than `n_periods` the first two are merged
/// (a short opening week joins week 2).
pub fn calendar_weeks(
    ctx: &LeagueContext,
    n_periods: Option<i64>,
    first: Option<NaiveDate>,
    last: Option<NaiveDate>,
) -> Vec<(NaiveDate, NaiveDate)> {
    let bounds = season_bounds(ctx);
    let (Some(first), Some(last)) = (first.or(bounds.map(|b| b.0)), last.or(bounds.map(|b| b.1))) else {
        return Vec::new();
    };
    if last < first {
        return Vec::new();
    }
    let gpd = per_day(ctx);
    let mut weeks: Vec<(NaiveDate, NaiveDate)> = Vec::new();
    let mut w = monday(first);
    while w <= last {
        weeks.push((w.max(first), (w + Duration::days(6)).min(last)));
        w += Duration::days(7);
    }
    if !gpd.is_empty() && weeks.len() > 1 {
        let counts: Vec<i64> = weeks
            .iter()
            .map(|&(a, b)| (0..=(b - a).num_days()).map(|i| gpd.get(&(a + Duration::days(i))).copied().unwrap_or(0)).sum())
            .collect();
        let mut full: Vec<i64> =
            weeks.iter().zip(&counts).filter(|((a, b), _)| (*b - *a).num_days() == 6).map(|(_, c)| *c).collect();
        full.sort();
        let median = if full.is_empty() { 0 } else { full[full.len() / 2] };
        let mut merged: Vec<(NaiveDate, NaiveDate)> = Vec::new();
        let mut carry: Option<NaiveDate> = None;
        for (i, (&(a, b), &c)) in weeks.iter().zip(&counts).enumerate() {
            let light = (b - a).num_days() == 6 && (c as f64) < LIGHT_WEEK_SHARE * median as f64;
            if light && i + 1 < weeks.len() {
                carry = carry.or(Some(a)); // a break week joins the week after it
                continue;
            }
            if light {
                if let Some(m) = merged.last_mut() {
                    m.1 = b;
                    continue;
                }
            }
            merged.push((carry.unwrap_or(a), b));
            carry = None;
        }
        weeks = merged;
    }
    if let Some(n) = n_periods.filter(|&n| n != 0) {
        while weeks.len() as i64 > n && weeks.len() > 1 {
            weeks[1].0 = weeks[0].0;
            weeks.remove(0);
        }
    }
    weeks
}
