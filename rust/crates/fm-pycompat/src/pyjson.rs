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

/// `json.dumps(value, **opts)`. Numbers: a serde_json float (`is_f64`) prints with
/// `float.__repr__` (`1.0`, `1e-05`, `1e+16`), an integer as an integer. With `indent`, empty
/// containers print as `[]` / `{}` and `indent=0` still inserts newlines, like Python.
/// `sort_keys` sorts by code point (Rust `str` order == Python `str` order).
pub fn dumps(value: &Value, opts: &DumpOpts) -> String {
    let (item_sep, key_sep) = opts.separators.unwrap_or(if opts.indent.is_some() { (",", ": ") } else { (", ", ": ") });
    let mut out = String::new();
    let enc = Enc { opts, item_sep, key_sep };
    enc.value(value, 0, &mut out);
    out
}

/// `json.dumps(s)` / `json.dumps(s, ensure_ascii=False)` for a single string (used inside cache
/// keys).
pub fn dumps_str(s: &str, ensure_ascii: bool) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    write_str(s, ensure_ascii, &mut out);
    out
}

/// `json.dumps(x)` for a float outside a `Value` (keeps Python's `NaN` / `Infinity` /
/// `-Infinity` literals, which a `serde_json::Value` cannot hold).
pub fn dumps_f64(x: f64) -> String {
    if x.is_nan() {
        "NaN".into()
    } else if x.is_infinite() {
        if x > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else {
        crate::fmt::repr_f64(x)
    }
}

struct Enc<'a> {
    opts: &'a DumpOpts,
    item_sep: &'a str,
    key_sep: &'a str,
}

impl Enc<'_> {
    fn newline(&self, level: usize, out: &mut String) {
        if let Some(ind) = self.opts.indent {
            out.push('\n');
            for _ in 0..ind * level {
                out.push(' ');
            }
        }
    }

    fn value(&self, v: &Value, level: usize, out: &mut String) {
        match v {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => {
                if n.is_f64() {
                    out.push_str(&dumps_f64(n.as_f64().unwrap_or(f64::NAN)));
                } else {
                    out.push_str(&n.to_string());
                }
            }
            Value::String(s) => write_str(s, self.opts.ensure_ascii, out),
            Value::Array(a) => {
                if a.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push_str(self.item_sep);
                    }
                    self.newline(level + 1, out);
                    self.value(x, level + 1, out);
                }
                self.newline(level, out);
                out.push(']');
            }
            Value::Object(m) => {
                if m.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push('{');
                let mut items: Vec<(&String, &Value)> = m.iter().collect();
                if self.opts.sort_keys {
                    items.sort_by(|a, b| a.0.cmp(b.0));
                }
                for (i, (k, x)) in items.into_iter().enumerate() {
                    if i > 0 {
                        out.push_str(self.item_sep);
                    }
                    self.newline(level + 1, out);
                    write_str(k, self.opts.ensure_ascii, out);
                    out.push_str(self.key_sep);
                    self.value(x, level + 1, out);
                }
                self.newline(level, out);
                out.push('}');
            }
        }
    }
}

/// Python's `py_encode_basestring_ascii` (ensure_ascii: everything outside `' '..='~'` becomes
/// `\uXXXX` with lowercase hex, astral chars as surrogate pairs) / `py_encode_basestring`
/// (only `"`, `\` and C0 controls are escaped).
fn write_str(s: &str, ensure_ascii: bool, out: &mut String) {
    use std::fmt::Write;
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c if ensure_ascii && (c as u32) > 0x7e => {
                let cp = c as u32;
                if cp >= 0x10000 {
                    let v = cp - 0x10000;
                    let _ = write!(out, "\\u{:04x}\\u{:04x}", 0xd800 | (v >> 10), 0xdc00 | (v & 0x3ff));
                } else {
                    let _ = write!(out, "\\u{:04x}", cp);
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn basics() {
        let v = json!({"b": 1, "a": [1.0, 1e-5, "\u{e9}\u{1f600}"], "e": {}, "l": []});
        let esc = format!("{0}u00e9{0}ud83d{0}ude00", '\\');
        assert_eq!(dumps(&v, &DumpOpts::default()),
                   format!("{{\"b\": 1, \"a\": [1.0, 1e-05, \"{esc}\"], \"e\": {{}}, \"l\": []}}"));
        assert_eq!(dumps(&v, &DumpOpts::compact()), format!("{{\"b\":1,\"a\":[1.0,1e-05,\"{esc}\"],\"e\":{{}},\"l\":[]}}"));
        assert_eq!(dumps(&json!({"x": [1]}), &DumpOpts::cli()), "{\n  \"x\": [\n    1\n  ]\n}");
        assert_eq!(dumps_str("a\"\u{7f}", false), "\"a\\\"\u{7f}\"");
        assert_eq!(dumps_f64(f64::NEG_INFINITY), "-Infinity");
    }
}
