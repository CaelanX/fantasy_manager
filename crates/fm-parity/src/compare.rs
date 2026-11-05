//! JSON comparison with tolerances (owner: W1-parity-harness).
//!
//! `compare(expected, actual, &tol)` walks both documents and returns every [`Diff`] with the JSON
//! path of the mismatch (`$.output.days[3].no_slot[0]`).
//!
//! Path patterns (used by `per_path`, `unordered`, `ignore`, `normalize`) share one syntax:
//! `$` root, `.key` (a `*` inside a key is a glob, `?` one char), `.*` any one segment (object key
//! or array index), `[*]` any array index, `[3]` one index, `.**` zero or more segments. A pattern
//! must match the whole path of the node it applies to. Examples: `$.output.win_*`,
//! `$.*.rates`, `$.**.no_slot`, `$.meta.sources[*]`.
//!
//! Semantics:
//! * numbers: equal if `|a-e| <= max(abs, rel * max(|a|,|e|))`; two integers must be equal unless
//!   a tolerance allows the gap. `NaN` never reaches here: `serde_json` turns it into `null` on
//!   both sides (the Python dump uses null too).
//! * `per_path` overrides `(rel, abs)` for the matching node and everything below it (first match
//!   wins; a deeper match overrides an inherited one).
//! * `unordered` on an array: compared as a multiset (greedy matching, same-index first); on an
//!   object: key order is not compared (values still are).
//! * `ignore`: the node is skipped (also when only one side has it).
//! * `normalize`: the regex replacement runs on strings at matching paths on both sides first.
//! * `key_order`: object key order must match (set differences are reported separately).

use std::fmt;
use std::path::PathBuf;

use regex::Regex;
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

impl Tolerance {
    /// Numbers must be bit-equal (rel = abs = 0), strings exact.
    pub fn exact() -> Self {
        Tolerance { rel: 0.0, abs: 0.0, ..Default::default() }
    }
    pub fn with_tol(mut self, rel: f64, abs: f64) -> Self {
        self.rel = rel;
        self.abs = abs;
        self
    }
    pub fn with_path_tol(mut self, pattern: &str, rel: f64, abs: f64) -> Self {
        self.per_path.push((pattern.to_string(), rel, abs));
        self
    }
    pub fn with_unordered(mut self, pattern: &str) -> Self {
        self.unordered.push(pattern.to_string());
        self
    }
    pub fn with_ignore(mut self, pattern: &str) -> Self {
        self.ignore.push(pattern.to_string());
        self
    }
    pub fn with_normalize(mut self, pattern: &str, regex: &str, replacement: &str) -> Self {
        self.normalize.push((pattern.to_string(), regex.to_string(), replacement.to_string()));
        self
    }
    pub fn without_key_order(mut self) -> Self {
        self.key_order = false;
        self
    }
}

#[derive(Clone, Debug)]
pub struct Diff {
    pub path: String,
    pub expected: String,
    pub actual: String,
}

impl fmt::Display for Diff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: expected {} but got {}", self.path, self.expected, self.actual)
    }
}

// ---------------------------------------------------------------------------------------------
// path patterns

#[derive(Clone, Debug, PartialEq)]
enum Seg {
    Key(String),
    Idx(usize),
}

#[derive(Clone, Debug)]
enum PSeg {
    /// `.*`: any key or index
    Any,
    /// `.**`: zero or more segments
    Deep,
    /// `[*]`
    AnyIdx,
    Idx(usize),
    /// key with optional `*` / `?` glob
    Key(String),
}

fn parse_pattern(p: &str) -> Vec<PSeg> {
    let mut s = p.trim();
    if let Some(r) = s.strip_prefix('$') {
        s = r;
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '.' => {
                i += 1;
                let st = i;
                while i < chars.len() && chars[i] != '.' && chars[i] != '[' {
                    i += 1;
                }
                let name: String = chars[st..i].iter().collect();
                out.push(match name.as_str() {
                    "*" => PSeg::Any,
                    "**" => PSeg::Deep,
                    _ => PSeg::Key(name),
                });
            }
            '[' => {
                i += 1;
                let st = i;
                while i < chars.len() && chars[i] != ']' {
                    i += 1;
                }
                let inner: String = chars[st..i].iter().collect();
                i += 1;
                out.push(if inner == "*" {
                    PSeg::AnyIdx
                } else if let Ok(n) = inner.parse::<usize>() {
                    PSeg::Idx(n)
                } else {
                    PSeg::Key(inner)
                });
            }
            _ => {
                // bare leading key ("output.x"): treat as a key segment
                let st = i;
                while i < chars.len() && chars[i] != '.' && chars[i] != '[' {
                    i += 1;
                }
                let name: String = chars[st..i].iter().collect();
                out.push(PSeg::Key(name));
            }
        }
    }
    out
}

fn glob_match(pat: &[char], s: &[char]) -> bool {
    match pat.split_first() {
        None => s.is_empty(),
        Some(('*', rest)) => (0..=s.len()).any(|k| glob_match(rest, &s[k..])),
        Some(('?', rest)) => !s.is_empty() && glob_match(rest, &s[1..]),
        Some((c, rest)) => s.first() == Some(c) && glob_match(rest, &s[1..]),
    }
}

fn key_matches(pat: &str, key: &str) -> bool {
    if !pat.contains('*') && !pat.contains('?') {
        return pat == key;
    }
    let p: Vec<char> = pat.chars().collect();
    let k: Vec<char> = key.chars().collect();
    glob_match(&p, &k)
}

fn path_matches(pat: &[PSeg], path: &[Seg]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((PSeg::Deep, rest)) => (0..=path.len()).any(|k| path_matches(rest, &path[k..])),
        Some((head, rest)) => match path.split_first() {
            None => false,
            Some((seg, tail)) => {
                let ok = match (head, seg) {
                    (PSeg::Any, _) => true,
                    (PSeg::AnyIdx, Seg::Idx(_)) => true,
                    (PSeg::Idx(a), Seg::Idx(b)) => a == b,
                    (PSeg::Key(k), Seg::Key(s)) => key_matches(k, s),
                    (PSeg::Key(k), Seg::Idx(b)) => k == &b.to_string(),
                    _ => false,
                };
                ok && path_matches(rest, tail)
            }
        },
    }
}

fn render_path(path: &[Seg]) -> String {
    let mut s = String::from("$");
    for seg in path {
        match seg {
            Seg::Key(k) => {
                s.push('.');
                s.push_str(k);
            }
            Seg::Idx(i) => {
                s.push('[');
                s.push_str(&i.to_string());
                s.push(']');
            }
        }
    }
    s
}

// ---------------------------------------------------------------------------------------------
// comparator

struct Compiled {
    rel: f64,
    abs: f64,
    key_order: bool,
    per_path: Vec<(Vec<PSeg>, f64, f64)>,
    unordered: Vec<Vec<PSeg>>,
    ignore: Vec<Vec<PSeg>>,
    normalize: Vec<(Vec<PSeg>, Regex, String)>,
}

fn compile(tol: &Tolerance) -> Result<Compiled, String> {
    let mut normalize = Vec::new();
    for (p, re, rep) in &tol.normalize {
        let re = Regex::new(re).map_err(|e| format!("bad normalize regex {re:?}: {e}"))?;
        normalize.push((parse_pattern(p), re, rep.clone()));
    }
    Ok(Compiled {
        rel: tol.rel,
        abs: tol.abs,
        key_order: tol.key_order,
        per_path: tol.per_path.iter().map(|(p, r, a)| (parse_pattern(p), *r, *a)).collect(),
        unordered: tol.unordered.iter().map(|p| parse_pattern(p)).collect(),
        ignore: tol.ignore.iter().map(|p| parse_pattern(p)).collect(),
        normalize,
    })
}

const MAX_SUMMARY: usize = 160;

fn summarize(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() <= MAX_SUMMARY {
        s
    } else {
        let cut: String = s.chars().take(MAX_SUMMARY).collect();
        format!("{cut}...")
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn numbers_equal(e: &serde_json::Number, a: &serde_json::Number, rel: f64, abs: f64) -> bool {
    if let (Some(x), Some(y)) = (e.as_i64(), a.as_i64()) {
        if x == y {
            return true;
        }
        if x.unsigned_abs() > (1u64 << 53) || y.unsigned_abs() > (1u64 << 53) {
            return false;
        }
    } else if let (Some(x), Some(y)) = (e.as_u64(), a.as_u64()) {
        if x == y {
            return true;
        }
        if x > (1u64 << 53) || y > (1u64 << 53) {
            return false;
        }
    }
    let (x, y) = match (e.as_f64(), a.as_f64()) {
        (Some(x), Some(y)) => (x, y),
        _ => return false,
    };
    if x == y {
        return true;
    }
    if !x.is_finite() || !y.is_finite() {
        return false;
    }
    let d = (x - y).abs();
    d <= abs || d <= rel * x.abs().max(y.abs())
}

struct Walker<'a> {
    c: &'a Compiled,
    diffs: Vec<Diff>,
}

impl Walker<'_> {
    fn push(&mut self, path: &[Seg], expected: String, actual: String) {
        self.diffs.push(Diff { path: render_path(path), expected, actual });
    }

    fn ignored(&self, path: &[Seg]) -> bool {
        self.c.ignore.iter().any(|p| path_matches(p, path))
    }

    fn is_unordered(&self, path: &[Seg]) -> bool {
        self.c.unordered.iter().any(|p| path_matches(p, path))
    }

    /// does `e` match `a` with no diffs (used by multiset matching)?
    fn matches(&mut self, e: &Value, a: &Value, path: &mut Vec<Seg>, rel: f64, abs: f64) -> bool {
        let n = self.diffs.len();
        self.walk(e, a, path, rel, abs);
        let ok = self.diffs.len() == n;
        self.diffs.truncate(n);
        ok
    }

    fn walk(&mut self, e: &Value, a: &Value, path: &mut Vec<Seg>, mut rel: f64, mut abs: f64) {
        if self.ignored(path) {
            return;
        }
        if let Some((_, r, ab)) = self.c.per_path.iter().find(|(p, _, _)| path_matches(p, path)) {
            rel = *r;
            abs = *ab;
        }
        match (e, a) {
            (Value::Number(x), Value::Number(y)) => {
                if !numbers_equal(x, y, rel, abs) {
                    self.push(path, e.to_string(), a.to_string());
                }
            }
            (Value::String(x), Value::String(y)) => {
                let (mut x, mut y) = (x.clone(), y.clone());
                for (p, re, rep) in &self.c.normalize {
                    if path_matches(p, path) {
                        x = re.replace_all(&x, rep.as_str()).into_owned();
                        y = re.replace_all(&y, rep.as_str()).into_owned();
                    }
                }
                if x != y {
                    self.push(path, summarize(&Value::String(x)), summarize(&Value::String(y)));
                }
            }
            (Value::Bool(x), Value::Bool(y)) => {
                if x != y {
                    self.push(path, e.to_string(), a.to_string());
                }
            }
            (Value::Null, Value::Null) => {}
            (Value::Array(x), Value::Array(y)) => self.walk_arrays(x, y, path, rel, abs),
            (Value::Object(x), Value::Object(y)) => self.walk_objects(x, y, path, rel, abs),
            _ => {
                // numbers vs strings etc.: report the types plus short values
                self.push(
                    path,
                    format!("{} {}", type_name(e), summarize(e)),
                    format!("{} {}", type_name(a), summarize(a)),
                );
            }
        }
    }

    fn walk_objects(
        &mut self,
        x: &serde_json::Map<String, Value>,
        y: &serde_json::Map<String, Value>,
        path: &mut Vec<Seg>,
        rel: f64,
        abs: f64,
    ) {
        let mut sets_equal = true;
        for (k, ev) in x {
            path.push(Seg::Key(k.clone()));
            match y.get(k) {
                Some(av) => self.walk(ev, av, path, rel, abs),
                None => {
                    sets_equal = false;
                    if !self.ignored(path) {
                        self.push(path, summarize(ev), "<missing>".into());
                    }
                }
            }
            path.pop();
        }
        for (k, av) in y {
            if !x.contains_key(k) {
                sets_equal = false;
                path.push(Seg::Key(k.clone()));
                if !self.ignored(path) {
                    self.push(path, "<missing>".into(), summarize(av));
                }
                path.pop();
            }
        }
        if sets_equal && self.c.key_order && !self.is_unordered(path) {
            let ek: Vec<&String> = x.keys().collect();
            let ak: Vec<&String> = y.keys().collect();
            if ek != ak {
                let first = ek.iter().zip(&ak).position(|(a, b)| a != b).unwrap_or(0);
                self.push(
                    path,
                    format!("key order [{}]", join_keys(&ek, first)),
                    format!("key order [{}]", join_keys(&ak, first)),
                );
            }
        }
    }

    fn walk_arrays(&mut self, x: &[Value], y: &[Value], path: &mut Vec<Seg>, rel: f64, abs: f64) {
        if self.is_unordered(path) {
            return self.walk_multiset(x, y, path, rel, abs);
        }
        if x.len() != y.len() {
            self.push(path, format!("array of {} items", x.len()), format!("array of {} items", y.len()));
        }
        for (i, (ev, av)) in x.iter().zip(y).enumerate() {
            path.push(Seg::Idx(i));
            self.walk(ev, av, path, rel, abs);
            path.pop();
        }
    }

    fn walk_multiset(&mut self, x: &[Value], y: &[Value], path: &mut Vec<Seg>, rel: f64, abs: f64) {
        let mut used = vec![false; y.len()];
        let mut left_e: Vec<usize> = Vec::new();
        for (i, ev) in x.iter().enumerate() {
            path.push(Seg::Idx(i));
            let mut found = None;
            if i < y.len() && !used[i] && self.matches(ev, &y[i], path, rel, abs) {
                found = Some(i);
            } else {
                for (j, av) in y.iter().enumerate() {
                    if !used[j] && j != i && self.matches(ev, av, path, rel, abs) {
                        found = Some(j);
                        break;
                    }
                }
            }
            path.pop();
            match found {
                Some(j) => used[j] = true,
                None => left_e.push(i),
            }
        }
        let left_a: Vec<usize> = (0..y.len()).filter(|j| !used[*j]).collect();
        // pair leftovers in order so the report shows what differs inside them
        let paired = left_e.len().min(left_a.len());
        for k in 0..paired {
            path.push(Seg::Idx(left_e[k]));
            self.walk(&x[left_e[k]], &y[left_a[k]], path, rel, abs);
            path.pop();
        }
        for &i in &left_e[paired..] {
            path.push(Seg::Idx(i));
            self.push(path, summarize(&x[i]), "<no matching element>".into());
            path.pop();
        }
        for &j in &left_a[paired..] {
            path.push(Seg::Idx(j));
            self.push(path, "<no matching element>".into(), summarize(&y[j]));
            path.pop();
        }
    }
}

fn join_keys(keys: &[&String], from: usize) -> String {
    let tail: Vec<&str> = keys.iter().skip(from).take(4).map(|s| s.as_str()).collect();
    let more = if keys.len() > from + 4 { ", ..." } else { "" };
    format!("@{from}: {}{more}", tail.join(", "))
}

/// All differences between `expected` (Python) and `actual` (Rust). Panics on an invalid
/// normalize regex (a test-authoring bug); see [`try_compare`].
pub fn compare(expected: &Value, actual: &Value, tol: &Tolerance) -> Vec<Diff> {
    try_compare(expected, actual, tol).unwrap_or_else(|e| panic!("{e}"))
}

pub fn try_compare(expected: &Value, actual: &Value, tol: &Tolerance) -> Result<Vec<Diff>, String> {
    let c = compile(tol)?;
    let mut w = Walker { c: &c, diffs: Vec::new() };
    let mut path = Vec::new();
    w.walk(expected, actual, &mut path, c.rel, c.abs);
    Ok(w.diffs)
}

/// How many diffs `assert_parity!` prints before pointing at the report file.
pub const PRINT_LIMIT: usize = 25;

/// `<workspace>/parity/out` (gitignored).
pub fn out_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../parity/out")
}

/// Full text report of `diffs` (one per line).
pub fn report(diffs: &[Diff]) -> String {
    let mut s = String::new();
    for d in diffs {
        s.push_str(&d.to_string());
        s.push('\n');
    }
    s
}

/// Backing function of `assert_parity!`.
pub fn assert_parity_impl(name: &str, expected: &Value, actual: &Value, tol: &Tolerance) {
    let diffs = compare(expected, actual, tol);
    if diffs.is_empty() {
        return;
    }
    let safe: String =
        name.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
    let dir = out_dir();
    let file = dir.join(format!("{safe}.txt"));
    let written = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&file, report(&diffs))).is_ok();
    let mut msg = format!("parity mismatch in `{name}`: {} difference(s)\n", diffs.len());
    for d in diffs.iter().take(PRINT_LIMIT) {
        msg.push_str(&format!("  {d}\n"));
    }
    if diffs.len() > PRINT_LIMIT {
        msg.push_str(&format!("  ... and {} more\n", diffs.len() - PRINT_LIMIT));
    }
    if written {
        msg.push_str(&format!("full report: {}\n", file.display()));
    }
    panic!("{msg}");
}
