//! Parity harness (owner: W1-parity-harness). Stage tests live in tests/ and are owned by the
//! packages that implement each stage (see WORK_PACKAGES.json). The signatures below are the
//! architect's intended API; W1-parity-harness implements them (and may extend, not break, them).

pub mod compare;
pub mod fixtures;
pub mod normalize;

pub use compare::{compare, Diff, Tolerance};
pub use fixtures::Fixtures;

/// Assert that `actual` matches `expected` under `tol`; on failure print the first diffs and
/// write the full list to `rust/parity/out/<name>.txt`.
#[macro_export]
macro_rules! assert_parity {
    ($name:expr, $expected:expr, $actual:expr, $tol:expr) => {
        $crate::compare::assert_parity_impl($name, &$expected, &$actual, &$tol)
    };
}
