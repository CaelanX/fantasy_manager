//! `providers.fantrax.lineup_lock_for` (moved here from the provider so compute code can call it
//! without depending on `fm-providers`, see ARCHITECTURE.md section 1) and the Rules-page parser
//! it needs. Owner: work package `W6-lineup-waivers`.
//!
//! `parse_lineup_lock` is also in `fm_providers::fantrax` (the provider's own property); the copy
//! here keeps `fm-engine` free of a provider dependency. Both are the same three lines.

use fm_core::models::{LeagueContext, LineupLock};
use fm_core::provider::LeagueProvider;
use fm_pycompat::text::{py_lower, split_ws};

/// Rules-page label whose value says how lineup changes take effect.
pub const LINEUP_RULE: &str = "Lineup changes are executed";

/// "daily" or "weekly" from the Rules page's "Lineup changes are executed" value ("Daily",
/// "Weekly every Monday", "Per scoring period"...). Unknown / missing -> "weekly".
pub fn parse_lineup_lock(text: Option<&str>) -> &'static str {
    let t = py_lower(&split_ws(text.unwrap_or("")).join(" "));
    if t.contains("dai") {
        "daily"
    } else {
        "weekly"
    }
}

fn ctx_lock(ctx: &LeagueContext) -> Option<&'static str> {
    ctx.lineup_lock.map(|l| match l {
        LineupLock::Daily => "daily",
        LineupLock::Weekly => "weekly",
    })
}

/// How lineup changes take effect: `provider.lineup_lock` (Fantrax), else the provider's parsed
/// `rules`, else `ctx.lineup_lock`, else "weekly" for Fantrax and "daily" for every other provider.
pub fn lineup_lock_for(ctx: &LeagueContext, provider: Option<&dyn LeagueProvider>) -> &'static str {
    if ctx.provider != "fantrax" {
        return ctx_lock(ctx).unwrap_or("daily");
    }
    if let Some(p) = provider {
        match p.extras().lineup_lock.as_deref() {
            Some("daily") => return "daily",
            Some("weekly") => return "weekly",
            _ => {}
        }
        if let Some(rules) = p.rules() {
            return parse_lineup_lock(rules.get(LINEUP_RULE));
        }
    }
    ctx_lock(ctx).unwrap_or("weekly")
}

/// `recommend.lineup._lock(ctx)`: [`lineup_lock_for`] without a provider.
pub fn lock(ctx: &LeagueContext) -> &'static str {
    lineup_lock_for(ctx, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        assert_eq!(parse_lineup_lock(Some("Daily")), "daily");
        assert_eq!(parse_lineup_lock(Some("  Weekly   every Monday")), "weekly");
        assert_eq!(parse_lineup_lock(None), "weekly");
    }
}
