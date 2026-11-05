//! Common normalizers for known run-dependent strings (owner: W1-parity-harness).
//!
//! Each preset takes a [`Tolerance`] and returns it extended, so presets compose:
//! `set_order_fields(cli_meta(Tolerance::default()))`.

use crate::compare::Tolerance;

/// Age phrases in source notes: "data 63m ago", "fetched 2h ago", "lines cached 12h", "23d ago".
/// Only differ between runs when the clock is not pinned, but cheap to neutralise.
const AGE_RE: &str = r"\b\d+(?:s|m|h|d)\b(?: ago)?";

/// Paths of the CLI `meta` block, both when the compared value is the command output
/// (`cli_output`) and when it is the whole `{args, exit_code, output}` file.
const META_PATHS: [&str; 2] = ["$.meta.**", "$.output.meta.**"];

/// The tolerance preset for CLI `meta` blocks (sources/warnings) and run timestamps.
/// * ages in `meta.sources` / `meta.warnings` text become `<age>`;
/// * `generated_at` / `fetched_at` / `ran_at` timestamps anywhere are ignored;
/// * temp data-dir paths (`.../Temp/...`, `/tmp/...`) become `<tmp>`.
pub fn cli_meta(mut tol: Tolerance) -> Tolerance {
    for p in META_PATHS {
        tol = tol.with_normalize(p, AGE_RE, "<age>");
    }
    for p in ["$.**.generated_at", "$.**.fetched_at", "$.**.ran_at"] {
        tol = tol.with_ignore(p);
    }
    tol.with_normalize("$.**", r#"(?:[A-Za-z]:)?[\\/][^\s'"]*[\\/](?:Temp|tmp)[\\/][^\s'"]*"#, "<tmp>")
}

/// Fields Python fills in hash-random order (ARCHITECTURE.md 4.3): PlayerValue.rates (unordered),
/// lineup `no_slot` (unordered), plan `win_*` (abs 0.02, no relative tolerance).
pub fn set_order_fields(tol: Tolerance) -> Tolerance {
    tol.with_unordered("$.**.rates")
        .with_unordered("$.**.no_slot")
        .with_path_tol("$.**.win_*", 0.0, 0.02)
}
