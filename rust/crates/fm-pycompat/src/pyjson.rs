//! A `json.dumps` work-alike over `serde_json::Value` (which must be built with the
//! `preserve_order` feature so object key order = Python dict insertion order).
//! Implementation: `W1-pycompat`.
//!
//! Python defaults: `ensure_ascii=True` (non-ASCII -> `\uXXXX`, astral chars as surrogate pairs),
//! `separators=(", ", ": ")` without indent and `(",", ": ")` with indent, floats via
//! `float.__repr__` ([`crate::fmt::repr_f64`]: `1.0`, `1e-05`), ints as ints, `allow_nan=True`
//! (`NaN`, `Infinity`, `-Infinity`). serde_json numbers keep the int/float distinction
//! (`Value::from(1.0)` is a float), which is what lets `1.0` print as `1.0`.
//! NaN cannot live in a serde_json `Value`; the port maps it to `null` (documented deviation).

use serde_json::Value;

#[derive(Clone, Debug)]
pub struct DumpOpts {
    /// `indent=N` (None = single line).
    pub indent: Option<usize>,
    pub ensure_ascii: bool,
    pub sort_keys: bool,
    /// `(item_sep, key_sep)`; None = Python's default for the indent setting.
    pub separators: Option<(&'static str, &'static str)>,
}

impl Default for DumpOpts {
    fn default() -> Self {
        DumpOpts { indent: None, ensure_ascii: true, sort_keys: false, separators: None }
    }
}

impl DumpOpts {
    /// The CLI's `json.dumps(data, indent=2, default=str)`.
    pub fn cli() -> Self {
        DumpOpts { indent: Some(2), ..Default::default() }
    }
    /// `separators=(",", ":")`.
    pub fn compact() -> Self {
        DumpOpts { separators: Some((",", ":")), ..Default::default() }
    }
}

/// `json.dumps(value, **opts)`.
pub fn dumps(_value: &Value, _opts: &DumpOpts) -> String {
    todo!("W1-pycompat")
}

/// `json.dumps(s)` for a single string (used inside cache keys).
pub fn dumps_str(_s: &str, _ensure_ascii: bool) -> String {
    todo!("W1-pycompat")
}
