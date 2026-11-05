//! Common normalizers for known run-dependent strings (owner: W1-parity-harness). Status: TODO.
//! E.g. source notes "data 37m ago" / "fetched 2h ago" (only when the clock was not pinned),
//! `generated_at` timestamps, temp data-dir paths.

use crate::compare::Tolerance;

/// The tolerance preset for CLI `meta` blocks (sources/warnings) and run timestamps.
pub fn cli_meta(_tol: Tolerance) -> Tolerance {
    todo!("W1-parity-harness")
}

/// Fields Python fills in hash-random order (ARCHITECTURE.md 4.3): PlayerValue.rates (unordered),
/// lineup `no_slot`, plan `win_*` (abs 0.02).
pub fn set_order_fields(_tol: Tolerance) -> Tolerance {
    todo!("W1-parity-harness")
}
