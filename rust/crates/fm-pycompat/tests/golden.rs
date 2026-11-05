//! Golden tests: every fm-pycompat helper against CPython 3.12 output.
//! Regenerate with `python rust/crates/fm-pycompat/tests/golden/gen_golden.py` (project venv).
//! Floats in the goldens are IEEE-754 bit patterns, so comparisons are bit-exact.

use fm_pycompat::{fmt, num, order, pyjson, random::PyRandom, text, time};
use serde_json::Value;
use std::path::PathBuf;

fn load(name: &str) -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(format!("{name}.json"));
    let s = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    serde_json::from_str(&s).expect("golden json")
}

fn f(v: &Value) -> f64 {
    f64::from_bits(v.as_u64().expect("float bits"))
}

fn bits_eq(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
}

/// Collect failures and report up to 20 of them with the total count.
struct Fails(Vec<String>, usize);
impl Fails {
    fn new() -> Self {
        Fails(Vec::new(), 0)
    }
    fn push(&mut self, s: String) {
        self.1 += 1;
        if self.0.len() < 20 {
            self.0.push(s);
        }
    }
    fn check(self, what: &str, total: usize) {
        assert!(total > 0, "{what}: no cases");
        if self.1 > 0 {
            panic!("{what}: {} of {total} cases differ from CPython:\n{}", self.1, self.0.join("\n"));
        }
        eprintln!("{what}: {total} cases ok");
    }
}

#[test]
fn golden_sum() {
    let g = load("num");
    let mut fails = Fails::new();
    let cases = g["sum"].as_array().unwrap();
    for c in cases {
        let xs: Vec<f64> = c["xs"].as_array().unwrap().iter().map(f).collect();
        let want = f(&c["sum"]);
        let got = num::py_sum(xs.iter().copied());
        // Python's sum([]) is the int 0 -> float bits of 0.0 as well
        if !bits_eq(got, want) {
            fails.push(format!("sum({xs:?}) = {got:e}, python {want:e}"));
        }
    }
    fails.check("sum", cases.len());
}

#[test]
fn golden_round() {
    let g = load("num");
    let mut fails = Fails::new();
    let cases = g["round"].as_array().unwrap();
    for c in cases {
        let x = f(&c["x"]);
        match c["n"].as_i64() {
            None => {
                let want = f(&c["r"]);
                let got = num::py_round(x);
                // Python's round(x) is an int (no signed zero); compare by value
                if got != want {
                    fails.push(format!("round({x:e}) = {got}, python {want}"));
                }
                if let Some(i) = c["i"].as_i64() {
                    if num::py_round_i64(x) != i {
                        fails.push(format!("int(round({x:e})) = {}, python {i}", num::py_round_i64(x)));
                    }
                }
            }
            Some(n) => {
                let got = num::round_to(x, n as i32);
                match c["r"].as_u64() {
                    Some(b) => {
                        let want = f64::from_bits(b);
                        if !bits_eq(got, want) {
                            fails.push(format!("round({x:e}, {n}) = {got:e}, python {want:e}"));
                        }
                    }
                    None => {
                        if !got.is_infinite() {
                            fails.push(format!("round({x:e}, {n}) = {got:e}, python OverflowError"));
                        }
                    }
                }
            }
        }
    }
    fails.check("round", cases.len());
}

#[test]
fn golden_divmod_median() {
    let g = load("num");
    let mut fails = Fails::new();
    let cases = g["div"].as_array().unwrap();
    for c in cases {
        let (a, b) = (f(&c["a"]), f(&c["b"]));
        let (fd, md) = (f(&c["fd"]), f(&c["md"]));
        if !bits_eq(num::floordiv(a, b), fd) || !bits_eq(num::pymod(a, b), md) {
            fails.push(format!("{a:e} // {b:e} = {:e} (py {fd:e}); % = {:e} (py {md:e})", num::floordiv(a, b),
                               num::pymod(a, b)));
        }
    }
    let icases = g["idiv"].as_array().unwrap();
    for c in icases {
        let v: Vec<i64> = c.as_array().unwrap().iter().map(|x| x.as_i64().unwrap()).collect();
        if num::floordiv_i64(v[0], v[1]) != v[2] || num::pymod_i64(v[0], v[1]) != v[3] {
            fails.push(format!("int divmod {v:?}"));
        }
    }
    let mcases = g["median"].as_array().unwrap();
    for c in mcases {
        let xs: Vec<f64> = c["xs"].as_array().unwrap().iter().map(f).collect();
        let want = f(&c["m"]);
        if !bits_eq(num::median(&xs), want) {
            fails.push(format!("median({xs:?}) = {}, python {want}", num::median(&xs)));
        }
    }
    fails.check("floordiv/mod/median", cases.len() + icases.len() + mcases.len());
}

#[test]
fn golden_float_format() {
    let g = load("fmt");
    let mut fails = Fails::new();
    let mut n = 0;
    for c in g["floats"].as_array().unwrap() {
        let x = f(&c["x"]);
        let want = c["repr"].as_str().unwrap();
        n += 1;
        if fmt::repr_f64(x) != want {
            fails.push(format!("repr({x:e}) = {}, python {want}", fmt::repr_f64(x)));
        }
        for (spec, want) in c["f"].as_object().unwrap() {
            let want = want.as_str().unwrap();
            n += 1;
            let got = fmt::format_float(x, spec).unwrap_or_else(|| "<None>".into());
            if got != want {
                fails.push(format!("format({x:e}, {spec:?}) = {got}, python {want}"));
            }
            // the dedicated helpers
            let p = |s: &str| s.parse::<usize>().unwrap();
            let dedicated = match spec.as_str() {
                s if s.starts_with("+.") && s.ends_with('f') => Some(fmt::fixed(x, p(&s[2..s.len() - 1]), true)),
                s if s.starts_with('.') && s.ends_with('f') => Some(fmt::fixed(x, p(&s[1..s.len() - 1]), false)),
                s if s.starts_with("+.") && s.ends_with('%') => Some(fmt::percent(x, p(&s[2..s.len() - 1]), true)),
                s if s.starts_with('.') && s.ends_with('%') => Some(fmt::percent(x, p(&s[1..s.len() - 1]), false)),
                "g" => Some(fmt::general(x, None, false)),
                "+g" => Some(fmt::general(x, None, true)),
                s if s.starts_with('.') && s.ends_with('g') => Some(fmt::general(x, Some(p(&s[1..s.len() - 1])), false)),
                "e" => Some(fmt::exponent(x, 6, false, false)),
                s if s.starts_with("+.") && s.ends_with('e') => Some(fmt::exponent(x, p(&s[2..s.len() - 1]), true, false)),
                s if s.starts_with('.') && s.ends_with('e') => Some(fmt::exponent(x, p(&s[1..s.len() - 1]), false, false)),
                _ => None,
            };
            if let Some(d) = dedicated {
                if d != want {
                    fails.push(format!("helper for {spec:?} of {x:e} = {d}, python {want}"));
                }
            }
        }
    }
    for c in g["reprs"].as_array().unwrap() {
        n += 1;
        let x = f(&c[0]);
        let want = c[1].as_str().unwrap();
        if fmt::repr_f64(x) != want {
            fails.push(format!("repr({x:e}) = {}, python {want}", fmt::repr_f64(x)));
        }
    }
    fails.check("float format/repr", n);
}

#[test]
fn golden_percent_format() {
    let g = load("fmt");
    let mut fails = Fails::new();
    let cases = g["percent"].as_array().unwrap();
    for c in cases {
        let x = f(&c["x"]);
        let spec = c["spec"].as_str().unwrap();
        let want = c["s"].as_str();
        let got = fmt::try_percent_format(spec, x);
        if got.as_deref() != want {
            fails.push(format!("'%{spec}' % {x:e} = {got:?}, python {want:?}"));
        }
    }
    fails.check("%-format", cases.len());
}

#[test]
fn golden_int_format() {
    let g = load("fmt");
    let mut fails = Fails::new();
    let cases = g["ints"].as_array().unwrap();
    for c in cases {
        let Some(n) = c["n"].as_i64() else { continue };
        let fm = &c["f"];
        let checks = [
            (",", fmt::thousands(n)),
            ("+d", format!("{n:+}")),
            ("02d", format!("{n:02}")),
            ("04d", format!("{n:04}")),
            ("d", format!("{n}")),
        ];
        for (spec, got) in checks {
            let want = fm[spec].as_str().unwrap();
            if got != want {
                fails.push(format!("format({n}, {spec:?}) = {got}, python {want}"));
            }
        }
    }
    fails.check("int format", cases.len());
}

#[test]
fn golden_json_dumps() {
    let g = load("json");
    let mut fails = Fails::new();
    let cases = g["dumps"].as_array().unwrap();
    for c in cases {
        let v: Value = serde_json::from_str(c["v"].as_str().unwrap()).expect("value");
        let o = &c["o"];
        let opts = pyjson::DumpOpts {
            indent: o.get("indent").and_then(|x| x.as_u64()).map(|x| x as usize),
            ensure_ascii: o.get("ensure_ascii").and_then(|x| x.as_bool()).unwrap_or(true),
            sort_keys: o.get("sort_keys").and_then(|x| x.as_bool()).unwrap_or(false),
            separators: o.get("separators").map(|s| {
                let a = s.as_array().unwrap();
                let leak = |x: &Value| -> &'static str { Box::leak(x.as_str().unwrap().to_string().into_boxed_str()) };
                (leak(&a[0]), leak(&a[1]))
            }),
        };
        let want = c["s"].as_str().unwrap();
        let got = pyjson::dumps(&v, &opts);
        if got != want {
            fails.push(format!("dumps({}, {o}) =\n  {got}\npython\n  {want}", c["v"]));
        }
    }
    let scases = g["strs"].as_array().unwrap();
    for c in scases {
        let s = c["s"].as_str().unwrap();
        if pyjson::dumps_str(s, true) != c["a"].as_str().unwrap() || pyjson::dumps_str(s, false) != c["u"].as_str().unwrap() {
            fails.push(format!("dumps_str({s:?})"));
        }
    }
    fails.check("json.dumps", cases.len() + scases.len());
}

fn seeded(seed: &Value) -> PyRandom {
    match seed.as_i64() {
        Some(s) => PyRandom::new(s),
        None => {
            // seeds beyond i64 are not in the corpus; u64 seeds via from_key
            let u = seed.as_u64().expect("seed");
            PyRandom::from_key(&[u as u32, (u >> 32) as u32])
        }
    }
}

#[test]
fn golden_random_ops() {
    let g = load("random");
    let mut fails = Fails::new();
    let mut n = 0;
    for case in g["ops"].as_array().unwrap() {
        let seed = &case["seed"];
        let mut r = seeded(seed);
        for (i, op) in case["ops"].as_array().unwrap().iter().enumerate() {
            n += 1;
            let name = op[0].as_str().unwrap();
            let arg = &op[1];
            let want = &op[2];
            let ok = match name {
                "random" => bits_eq(r.random(), f(want)),
                "getrandbits" => r.getrandbits(arg.as_u64().unwrap() as u32) == want.as_u64().unwrap(),
                "randbelow" => r.randbelow(arg.as_u64().unwrap()) == want.as_u64().unwrap(),
                "randrange" => r.randrange(arg[0].as_i64().unwrap(), arg[1].as_i64().unwrap()) == want.as_i64().unwrap(),
                "shuffle" => {
                    let mut xs: Vec<u64> = (0..arg.as_u64().unwrap()).collect();
                    r.shuffle(&mut xs);
                    xs == want.as_array().unwrap().iter().map(|x| x.as_u64().unwrap()).collect::<Vec<_>>()
                }
                "choices" => {
                    let got = r.choices_idx(arg[0].as_u64().unwrap() as usize, arg[1].as_u64().unwrap() as usize);
                    got == want.as_array().unwrap().iter().map(|x| x.as_u64().unwrap() as usize).collect::<Vec<_>>()
                }
                "gauss" => bits_eq(r.gauss(f(&arg[0]), f(&arg[1])), f(want)),
                "choice" => r.choice_idx(arg.as_u64().unwrap() as usize) as u64 == want.as_u64().unwrap(),
                other => panic!("unknown op {other}"),
            };
            if !ok {
                fails.push(format!("seed {seed} op #{i} {name}({arg}) != python {want}"));
                break; // the stream is out of sync after the first difference
            }
        }
    }
    fails.check("random ops", n);
}

#[test]
fn golden_random_streams() {
    let g = load("random");
    let mut fails = Fails::new();
    let mut n = 0;
    for s in g["streams"].as_array().unwrap() {
        let mut r = seeded(&s["seed"]);
        let kind = s["kind"].as_str().unwrap();
        for (i, v) in s["vals"].as_array().unwrap().iter().enumerate() {
            n += 1;
            let got = if kind == "random" { r.random() } else { r.gauss(1.5, 0.7) };
            if !bits_eq(got, f(v)) {
                fails.push(format!("seed {} {kind} #{i}: {got:e} != python {:e}", s["seed"], f(v)));
                break;
            }
        }
    }
    fails.check("random streams", n);
}

fn opt_i(v: &Value) -> Option<isize> {
    v.as_i64().map(|x| x as isize)
}

#[test]
fn golden_text() {
    let g = load("text");
    let mut fails = Fails::new();
    let mut n = 0;
    for c in g["slices"].as_array().unwrap() {
        n += 1;
        let s = c[0].as_str().unwrap();
        let got = text::py_slice(s, opt_i(&c[1]), opt_i(&c[2]));
        if got != c[3].as_str().unwrap() {
            fails.push(format!("{s:?}[{}:{}] = {got:?}, python {}", c[1], c[2], c[3]));
        }
    }
    for c in g["case"].as_array().unwrap() {
        n += 1;
        let s = c[0].as_str().unwrap();
        if text::capitalize(s) != c[1].as_str().unwrap() {
            fails.push(format!("{s:?}.capitalize() = {:?}, python {}", text::capitalize(s), c[1]));
        }
        if text::title(s) != c[2].as_str().unwrap() {
            fails.push(format!("{s:?}.title() = {:?}, python {}", text::title(s), c[2]));
        }
        if text::py_lower(s) != c[3].as_str().unwrap() || text::py_upper(s) != c[4].as_str().unwrap() {
            fails.push(format!("{s:?}.lower()/upper() = {:?} / {:?}, python {} / {}", text::py_lower(s),
                               text::py_upper(s), c[3], c[4]));
        }
    }
    for c in g["isdigit"].as_array().unwrap() {
        n += 1;
        let s = c[0].as_str().unwrap();
        if text::is_digit(s) != c[1].as_bool().unwrap() {
            fails.push(format!("{s:?}.isdigit() = {}, python {}", text::is_digit(s), c[1]));
        }
    }
    for c in g["split"].as_array().unwrap() {
        n += 1;
        let s = c[0].as_str().unwrap();
        let want: Vec<&str> = c[1].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
        if text::split_ws(s) != want || text::py_strip(s) != c[2].as_str().unwrap() {
            fails.push(format!("{s:?}.split()/strip() = {:?} / {:?}, python {want:?} / {}", text::split_ws(s),
                               text::py_strip(s), c[2]));
        }
    }
    // the embedded tables vs CPython, and is_space over every code point
    let (title_tbl, digit_tbl) = text::tables();
    let want_digits: Vec<(u32, u32)> = g["isdigit_ranges"].as_array().unwrap().iter()
        .map(|r| (r[0].as_u64().unwrap() as u32, r[1].as_u64().unwrap() as u32)).collect();
    if digit_tbl != want_digits.as_slice() {
        fails.push("ISDIGIT_RANGES differ from CPython".into());
    }
    let want_title: Vec<(char, String)> = g["title_diff"].as_array().unwrap().iter()
        .map(|r| (char::from_u32(r[0].as_u64().unwrap() as u32).unwrap(), r[1].as_str().unwrap().to_string())).collect();
    let got_title: Vec<(char, String)> = title_tbl.iter().map(|(c, s)| (*c, s.to_string())).collect();
    if got_title != want_title {
        fails.push("TITLE_SPECIAL differs from CPython".into());
    }
    let spaces: Vec<(u32, u32)> = g["isspace_ranges"].as_array().unwrap().iter()
        .map(|r| (r[0].as_u64().unwrap() as u32, r[1].as_u64().unwrap() as u32)).collect();
    for cp in 0..0x110000u32 {
        if let Some(c) = char::from_u32(cp) {
            let want = spaces.iter().any(|(a, b)| (*a..=*b).contains(&cp));
            if text::is_space(c) != want {
                fails.push(format!("is_space(U+{cp:04X}) = {}, python {want}", text::is_space(c)));
            }
        }
    }
    fails.check("text", n);
}

/// `py_lower` / `py_upper` of every single code point vs CPython's (Unicode 15.0) mappings.
/// (Rust's plain `to_lowercase`/`to_uppercase` differ on the Unicode-16 changes; listed.)
#[test]
fn golden_case_maps() {
    let g = load("text");
    let mut diff = Vec::new();
    let mut std_diff = 0;
    for (key, upper) in [("lower_map", false), ("upper_map", true)] {
        let want: std::collections::HashMap<u32, String> = g[key].as_array().unwrap().iter()
            .map(|r| (r[0].as_u64().unwrap() as u32, r[1].as_str().unwrap().to_string())).collect();
        for cp in 0..0x110000u32 {
            let Some(c) = char::from_u32(cp) else { continue };
            let s = c.to_string();
            let got = if upper { text::py_upper(&s) } else { text::py_lower(&s) };
            let plain = if upper { s.to_uppercase() } else { s.to_lowercase() };
            let w = want.get(&cp).cloned().unwrap_or_else(|| s.clone());
            if got != w {
                diff.push((key, cp));
            }
            if plain != w {
                std_diff += 1;
            }
        }
    }
    eprintln!("case maps: Rust std differs from CPython on {std_diff} mappings; py_lower/py_upper on {}", diff.len());
    assert!(diff.is_empty(), "py_lower/py_upper differ from CPython: {diff:X?}");
}

#[test]
fn golden_time() {
    let g = load("time");
    let mut fails = Fails::new();
    let mut n = 0;
    for c in g["strftime"].as_array().unwrap() {
        n += 1;
        let dt = chrono::NaiveDateTime::parse_from_str(c[0].as_str().unwrap(), "%Y-%m-%dT%H:%M:%S").unwrap();
        let fm = c[1].as_str().unwrap();
        let got = time::strftime(dt, fm);
        if got != c[2].as_str().unwrap() {
            fails.push(format!("{dt}.strftime({fm:?}) = {got:?}, python {}", c[2]));
        }
        if !fm.contains('%') || !fm.contains("%H") && !fm.contains("%M") && !fm.contains("%S") && !fm.contains("%I")
            && !fm.contains("%p") {
            let gd = time::strftime_date(dt.date(), fm);
            if gd != c[2].as_str().unwrap() {
                fails.push(format!("{}.strftime({fm:?}) = {gd:?}, python {}", dt.date(), c[2]));
            }
        }
    }
    let iso = |dt: chrono::NaiveDateTime| {
        let s = dt.format("%Y-%m-%dT%H:%M:%S%.6f").to_string();
        s.strip_suffix(".000000").map(str::to_string).unwrap_or(s)
    };
    for c in g["strptime"].as_array().unwrap() {
        n += 1;
        let (s, fm) = (c[0].as_str().unwrap(), c[1].as_str().unwrap());
        let got = time::strptime(s, fm).map(iso);
        if got.as_deref() != c[2].as_str() {
            fails.push(format!("strptime({s:?}, {fm:?}) = {got:?}, python {}", c[2]));
        }
    }
    for c in g["fromisoformat"].as_array().unwrap() {
        n += 1;
        let s = c[0].as_str().unwrap();
        let got = time::fromisoformat(s).map(|(dt, off)| (iso(dt), off));
        let want = c[1].as_array().map(|a| (a[0].as_str().unwrap().to_string(), a[1].as_i64().map(|x| x as i32)));
        if got != want {
            fails.push(format!("datetime.fromisoformat({s:?}) = {got:?}, python {}", c[1]));
        }
        let gd = time::date_fromisoformat(s).map(|d| d.to_string());
        if gd.as_deref() != c[2].as_str() {
            fails.push(format!("date.fromisoformat({s:?}) = {gd:?}, python {}", c[2]));
        }
    }
    fails.check("time", n);
}

#[test]
fn combinations_match_itertools() {
    // itertools.combinations(range(5), 3) in order
    let want = [[0, 1, 2], [0, 1, 3], [0, 1, 4], [0, 2, 3], [0, 2, 4], [0, 3, 4], [1, 2, 3], [1, 2, 4], [1, 3, 4],
                [2, 3, 4]];
    let got = order::combinations(5, 3);
    assert_eq!(got.len(), want.len());
    for (g, w) in got.iter().zip(want.iter()) {
        assert_eq!(g.as_slice(), w.as_slice());
    }
}
