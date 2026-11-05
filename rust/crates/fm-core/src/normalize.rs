//! `matching/normalize.py`: the RICH name normalizer (nicknames, suffixes, initials) used for
//! cross-source matching, plus team / position normalization. Distinct from
//! `models::normalize_name` (Player.name_norm). Implementation: work package `W2-core-runtime`.
//! Watch: NFKD + combining-mark drop + a translation table (Unicode 15 as in Python 3.12), the
//! lookahead regex `\.(?=\S)` (use fancy-regex or a manual scan), `str.split()` on Unicode
//! whitespace.

use crate::Set;

/// `normalize_name(name)` (matching flavour).
pub fn normalize_name(_name: Option<&str>) -> String {
    todo!("W2-core-runtime")
}

/// `normalize_team(code)`: canonical NHL team abbreviation via TEAM_ALIASES (None passes through
/// as None; unknown codes uppercased).
pub fn normalize_team(_team: Option<&str>) -> Option<String> {
    todo!("W2-core-runtime")
}

/// `normalize_positions("C/LW")` -> {"C","LW"} (a position STRING; iteration order is irrelevant).
pub fn normalize_positions(_pos: Option<&str>) -> Set<String> {
    todo!("W2-core-runtime")
}

/// `positions_agree(a, b)`.
pub fn positions_agree(_a: Option<&str>, _b: Option<&str>) -> bool {
    todo!("W2-core-runtime")
}

/// `strip_accents(s)`.
pub fn strip_accents(_s: &str) -> String {
    todo!("W2-core-runtime")
}
