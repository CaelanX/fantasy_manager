//! JSON comparison with tolerances (owner: W1-parity-harness). Status: TODO bodies.

use serde_json::Value;

/// Comparison rules. Paths use `$` root, `.key`, `[i]`, and `*` wildcards per segment
/// (e.g. `$.output.win_*`, `$.*.rates`).
#[derive(Clone, Debug)]
pub struct Tolerance {
    pub rel: f64,
    pub abs: f64,
    /// (path pattern, rel, abs) overrides
    pub per_path: Vec<(String, f64, f64)>,
    /// arrays compared as multisets / objects ignoring key order
    pub unordered: Vec<String>,
    /// paths skipped entirely
    pub ignore: Vec<String>,
    /// (path pattern, regex, replacement) applied to strings on both sides before comparing
    pub normalize: Vec<(String, String, String)>,
    /// compare object key ORDER too (byte-compat outputs)
    pub key_order: bool,
}

impl Default for Tolerance {
    fn default() -> Self {
        Tolerance {
            rel: 1e-9,
            abs: 1e-9,
            per_path: vec![],
            unordered: vec![],
            ignore: vec![],
            normalize: vec![],
            key_order: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Diff {
    pub path: String,
    pub expected: String,
    pub actual: String,
}

/// All differences between `expected` (Python) and `actual` (Rust).
pub fn compare(_expected: &Value, _actual: &Value, _tol: &Tolerance) -> Vec<Diff> {
    todo!("W1-parity-harness")
}

/// Backing function of `assert_parity!`.
pub fn assert_parity_impl(_name: &str, _expected: &Value, _actual: &Value, _tol: &Tolerance) {
    todo!("W1-parity-harness")
}
