//! `providers/preseason_enrich.py`: `is_unproven` and its constants (moved here so fm-engine and
//! fm-providers can share them).

use crate::models::Player;

/// fewer career NHL regular-season games = unproven
pub const UNPROVEN_CAREER_GP: i64 = 82;
/// a prior season with this many GP = proven (unless career < 82)
pub const PROVEN_SEASON_GP: i64 = 20;
pub const PRIOR_SPLITS: [&str; 3] = ["prior", "prior2", "prior3"];

/// Career NHL GP known and < 82, or no prior season (N-1..N-3) with >= 20 GP.
pub fn is_unproven(p: &Player) -> bool {
    if p.career_gp.is_some_and(|g| g < UNPROVEN_CAREER_GP) {
        return true;
    }
    !PRIOR_SPLITS.iter().any(|s| p.gp(s) >= PROVEN_SEASON_GP)
}
