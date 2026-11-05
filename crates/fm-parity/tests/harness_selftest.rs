//! Self tests of the parity harness: comparator semantics on small documents, every tracked
//! fixture compared with itself (must pass) and with perturbed copies (must report the right
//! paths), and the typed loaders.

use fm_parity::compare::{compare, try_compare, Tolerance};
use fm_parity::{assert_parity, normalize, Fixtures};
use serde_json::{json, Value};

fn paths(d: &[fm_parity::Diff]) -> Vec<String> {
    d.iter().map(|d| d.path.clone()).collect()
}

// ---- comparator semantics -------------------------------------------------------------------

#[test]
fn identical_and_numeric_tolerance() {
    let t = Tolerance::default();
    assert!(compare(&json!({"a": 1.0, "b": [1, 2]}), &json!({"a": 1.0, "b": [1, 2]}), &t).is_empty());
    // within 1e-9 relative
    assert!(compare(&json!({"a": 1000.0}), &json!({"a": 1000.0000000005}), &t).is_empty());
    let d = compare(&json!({"a": 1000.0}), &json!({"a": 1000.001}), &t);
    assert_eq!(paths(&d), ["$.a"]);
    // ints must match exactly; int vs float of the same value is fine
    assert_eq!(paths(&compare(&json!({"n": 3}), &json!({"n": 4}), &t)), ["$.n"]);
    assert!(compare(&json!({"n": 3}), &json!({"n": 3.0}), &t).is_empty());
    // large ints are not rounded through f64
    assert_eq!(compare(&json!(9007199254740993_i64), &json!(9007199254740992_i64), &t).len(), 1);
}

#[test]
fn paths_for_nested_mismatches() {
    let e = json!({"output": {"days": [{"x": 1, "s": "a"}, {"x": 2, "s": "b"}], "ok": true}});
    let a = json!({"output": {"days": [{"x": 1, "s": "a"}, {"x": 9, "s": "c"}], "ok": false}});
    let d = compare(&e, &a, &Tolerance::default());
    assert_eq!(paths(&d), ["$.output.days[1].x", "$.output.days[1].s", "$.output.ok"]);
}

#[test]
fn missing_extra_keys_length_and_type() {
    let t = Tolerance::default();
    let d = compare(&json!({"a": 1, "b": 2}), &json!({"a": 1, "c": 2}), &t);
    assert_eq!(paths(&d), ["$.b", "$.c"]);
    assert_eq!(d[0].actual, "<missing>");
    assert_eq!(d[1].expected, "<missing>");
    let d = compare(&json!({"l": [1, 2, 3]}), &json!({"l": [1, 2]}), &t);
    assert_eq!(paths(&d), ["$.l"]);
    let d = compare(&json!({"l": 1}), &json!({"l": "1"}), &t);
    assert_eq!(paths(&d), ["$.l"]);
    assert!(d[0].expected.starts_with("number"));
    // null vs value (NaN is null on both sides)
    assert!(compare(&json!({"a": null}), &json!({"a": null}), &t).is_empty());
    assert_eq!(paths(&compare(&json!({"a": null}), &json!({"a": 0}), &t)), ["$.a"]);
}

#[test]
fn key_order() {
    let e: Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
    let a: Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
    assert_eq!(paths(&compare(&e, &a, &Tolerance::default())), ["$"]);
    assert!(compare(&e, &a, &Tolerance::default().without_key_order()).is_empty());
    assert!(compare(&e, &a, &Tolerance::default().with_unordered("$")).is_empty());
}

#[test]
fn per_path_overrides_and_wildcards() {
    let e = json!({"output": {"win_plan": 0.5, "win_lineup": 0.3, "proj": 10.0}});
    let a = json!({"output": {"win_plan": 0.51, "win_lineup": 0.29, "proj": 10.01}});
    let t = Tolerance::default().with_path_tol("$.output.win_*", 0.0, 0.02);
    assert_eq!(paths(&compare(&e, &a, &t)), ["$.output.proj"]);
    // `**` and subtree inheritance
    let t = Tolerance::default().with_path_tol("$.**.output", 0.0, 0.05);
    assert!(compare(&e, &a, &t).is_empty()); // `**` also matches zero segments
    let t = Tolerance::default().with_path_tol("$.output", 0.0, 0.05);
    assert!(compare(&e, &a, &t).is_empty());
    let t = Tolerance::default().with_path_tol("$.*", 0.0, 0.05);
    assert!(compare(&e, &a, &t).is_empty());
    // exceeding the override is still reported
    let big = json!({"output": {"win_plan": 0.6, "win_lineup": 0.3, "proj": 10.0}});
    let t = Tolerance::default().with_path_tol("$.output.win_*", 0.0, 0.02);
    assert_eq!(paths(&compare(&e, &big, &t)), ["$.output.win_plan"]);
}

#[test]
fn unordered_arrays_and_objects() {
    let t = Tolerance::default().with_unordered("$.**.no_slot");
    let e = json!({"days": [{"no_slot": ["A", "B", "C"]}]});
    let a = json!({"days": [{"no_slot": ["C", "A", "B"]}]});
    assert!(compare(&e, &a, &t).is_empty());
    // ordered by default
    assert!(!compare(&e, &a, &Tolerance::default()).is_empty());
    // multiset: duplicates counted, missing/extra reported
    let a2 = json!({"days": [{"no_slot": ["C", "A", "A"]}]});
    let d = compare(&e, &a2, &t);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].path, "$.days[0].no_slot[1]");
    let a3 = json!({"days": [{"no_slot": ["C", "A", "B", "D"]}]});
    let d = compare(&e, &a3, &t);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].expected, "<no matching element>");
    // objects: key order ignored but values compared (with tolerance)
    let t = Tolerance::default().with_unordered("$.*.rates");
    let e: Value = serde_json::from_str(r#"{"p":{"rates":{"G":1.0,"A":2.0}}}"#).unwrap();
    let a: Value = serde_json::from_str(r#"{"p":{"rates":{"A":2.0,"G":1.0}}}"#).unwrap();
    assert!(compare(&e, &a, &t).is_empty());
    let a: Value = serde_json::from_str(r#"{"p":{"rates":{"A":2.5,"G":1.0}}}"#).unwrap();
    assert_eq!(paths(&compare(&e, &a, &t)), ["$.p.rates.A"]);
}

#[test]
fn ignore_and_normalize() {
    let e = json!({"generated_at": "t1", "meta": {"sources": ["fetched 5m ago", "data 3h ago"]}, "v": 1});
    let a = json!({"generated_at": "t2", "meta": {"sources": ["fetched 9m ago", "data 20h ago"]}, "v": 1});
    assert!(!compare(&e, &a, &Tolerance::default()).is_empty());
    let t = normalize::cli_meta(Tolerance::default());
    assert!(compare(&e, &a, &t).is_empty());
    // non-age text is still compared
    let a = json!({"generated_at": "t2", "meta": {"sources": ["fetched 9m ago", "data 20h later"]}, "v": 1});
    assert_eq!(paths(&compare(&e, &a, &t)), ["$.meta.sources[1]"]);
    // ignore removes keys present on one side only
    let t = Tolerance::default().with_ignore("$.extra");
    assert!(compare(&json!({"a": 1}), &json!({"a": 1, "extra": 5}), &t).is_empty());
    // invalid regex is an error from try_compare
    let bad = Tolerance::default().with_normalize("$", "(", "");
    assert!(try_compare(&json!(1), &json!(1), &bad).is_err());
}

#[test]
fn set_order_preset() {
    let t = normalize::set_order_fields(Tolerance::default());
    let e = json!({"output": {"win_plan": 0.539, "days": [{"no_slot": ["A", "B"]}]}});
    let a = json!({"output": {"win_plan": 0.55, "days": [{"no_slot": ["B", "A"]}]}});
    assert!(compare(&e, &a, &t).is_empty());
    let a = json!({"output": {"win_plan": 0.6, "days": [{"no_slot": ["B", "A"]}]}});
    assert_eq!(paths(&compare(&e, &a, &t)), ["$.output.win_plan"]);
}

#[test]
#[should_panic(expected = "parity mismatch in `selftest_assert_macro`")]
fn assert_parity_macro_panics_with_path() {
    let t = Tolerance::default();
    assert_parity!("selftest_assert_macro", json!({"a": [1]}), json!({"a": [2]}), t);
}

#[test]
fn assert_parity_macro_writes_report() {
    let t = Tolerance::default();
    let r = std::panic::catch_unwind(|| {
        assert_parity!("selftest_report", json!({"a": [1]}), json!({"a": [2]}), t);
    });
    assert!(r.is_err());
    let f = fm_parity::compare::out_dir().join("selftest_report.txt");
    let text = std::fs::read_to_string(&f).expect("report file written");
    assert!(text.contains("$.a[0]"), "{text}");
}

// ---- fixtures -------------------------------------------------------------------------------

/// Deterministic perturbations: each yields (mutated copy, path of the single expected diff).
fn perturbations(v: &Value) -> Vec<(Value, String)> {
    #[derive(Default)]
    struct Found {
        number: Option<Vec<String>>,
        string: Option<Vec<String>>,
        bool_: Option<Vec<String>>,
        object_key: Option<Vec<String>>,
    }
    fn find(v: &Value, path: &mut Vec<String>, f: &mut Found, depth: usize) {
        match v {
            Value::Number(n) if f.number.is_none() && n.as_f64().map_or(false, |x| x != 0.0) => {
                f.number = Some(path.clone())
            }
            Value::String(s) if f.string.is_none() && !s.is_empty() => f.string = Some(path.clone()),
            Value::Bool(_) if f.bool_.is_none() => f.bool_ = Some(path.clone()),
            Value::Array(a) => {
                for (i, x) in a.iter().take(3).enumerate() {
                    path.push(format!("[{i}]"));
                    find(x, path, f, depth + 1);
                    path.pop();
                }
            }
            Value::Object(o) => {
                if f.object_key.is_none() && depth > 0 && o.len() > 1 {
                    f.object_key = Some(path.clone());
                }
                for (k, x) in o.iter().take(40) {
                    path.push(format!(".{k}"));
                    find(x, path, f, depth + 1);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let mut f = Found::default();
    find(v, &mut Vec::new(), &mut f, 0);

    fn at<'a>(v: &'a mut Value, path: &[String]) -> &'a mut Value {
        let mut cur = v;
        for seg in path {
            cur = if let Some(k) = seg.strip_prefix('.') {
                cur.get_mut(k).unwrap()
            } else {
                let i: usize = seg.trim_matches(|c| c == '[' || c == ']').parse().unwrap();
                cur.get_mut(i).unwrap()
            };
        }
        cur
    }
    let render = |p: &[String]| format!("${}", p.concat());
    let mut out = Vec::new();
    if let Some(p) = f.number {
        let mut m = v.clone();
        let t = at(&mut m, &p);
        let n = t.as_f64().unwrap();
        *t = if t.is_i64() || t.is_u64() { json!(t.as_i64().unwrap_or(0) + 1) } else { json!(n * 1.5 + 1.0) };
        out.push((m, render(&p)));
    }
    if let Some(p) = f.string {
        let mut m = v.clone();
        let t = at(&mut m, &p);
        *t = Value::String(format!("{}~", t.as_str().unwrap()));
        out.push((m, render(&p)));
    }
    if let Some(p) = f.bool_ {
        let mut m = v.clone();
        let t = at(&mut m, &p);
        *t = Value::Bool(!t.as_bool().unwrap());
        out.push((m, render(&p)));
    }
    if let Some(p) = f.object_key {
        let mut m = v.clone();
        let t = at(&mut m, &p);
        let key = t.as_object().unwrap().keys().next().unwrap().clone();
        t.as_object_mut().unwrap().shift_remove(&key);
        out.push((m, format!("{}.{key}", render(&p))));
    }
    out
}

fn check_self_and_perturbed(label: &str, v: &Value) {
    let t = Tolerance::default();
    let d = compare(v, v, &t);
    assert!(d.is_empty(), "{label}: self-compare differs: {:?}", &d[..d.len().min(3)]);
    for (mutated, expected_path) in perturbations(v) {
        let d = compare(v, &mutated, &t);
        assert_eq!(paths(&d), [expected_path.clone()], "{label}: wrong diff paths for perturbation at {expected_path}");
        // symmetric: swapping sides reports the same path
        let d = compare(&mutated, v, &t);
        assert_eq!(paths(&d), [expected_path.clone()], "{label}: swapped");
    }
    // a root array/object truncation is caught
    if let Value::Object(o) = v {
        if let Some((k, Value::Array(a))) = o.iter().find(|(_, x)| x.as_array().map_or(false, |a| !a.is_empty())) {
            let mut m = v.clone();
            m[k].as_array_mut().unwrap().pop();
            let d = compare(v, &m, &t);
            assert!(paths(&d).contains(&format!("$.{k}")), "{label}: truncation of {k} ({} items) not caught", a.len());
        }
    }
}

fn walk_json_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            walk_json_files(&p, out);
        } else if p.extension().map_or(false, |x| x == "json") {
            out.push(p);
        }
    }
}

#[test]
fn every_fixture_matches_itself_and_catches_perturbations() {
    let mut n = 0;
    for fx in Fixtures::all() {
        if !fx.available() {
            fm_parity::skip("fixture self-compare", &format!("no fixtures at {}", fx.dir.display()));
            continue;
        }
        let mut files = Vec::new();
        walk_json_files(&fx.dir, &mut files);
        assert!(!files.is_empty());
        for f in files {
            let rel = f.strip_prefix(&fx.dir).unwrap().to_string_lossy().replace('\\', "/");
            let v = fx.json(&rel);
            check_self_and_perturbed(&format!("{}/{rel}", fx.league), &v);
            n += 1;
        }
    }
    eprintln!("checked {n} fixture files");
}

#[test]
fn typed_loaders() {
    for fx in Fixtures::all() {
        if !fx.available() {
            fm_parity::skip("typed_loaders", &format!("no fixtures at {}", fx.dir.display()));
            continue;
        }
        let snap = fx.snapshot();
        assert_eq!(snap.league, fx.league);
        let values = fx.values(&snap);
        let raw = fx.json("values.json");
        assert_eq!(values.len(), raw.as_object().unwrap().len());
        for (cid, v) in &values {
            assert_eq!(&v.player.cid, cid);
        }
        let dyn_raw = fx.json("dynasty.json");
        match fx.dynasty(&snap) {
            None => assert!(dyn_raw.is_null()),
            Some(d) => assert_eq!(d.len(), dyn_raw.as_object().unwrap().len()),
        }
        let picks = fx.picks();
        assert_eq!(picks.len(), fx.json("picks.json").as_object().unwrap().len());
        assert!(fx.cli_output("roster").is_object() || fx.cli_output("roster").is_array());
        assert!(fx.cli("roster").get("exit_code").is_some());
        assert!(fx.engine("trades").is_object() || fx.engine("trades").is_array());
        assert!(fx.raw_ctx().is_object());
        assert!(fx.try_json("nope/missing.json").is_err());
    }
}

#[test]
fn optional_fixtures_are_options() {
    for fx in Fixtures::all() {
        match fx.tape_path() {
            Some(p) => assert!(p.exists()),
            None => fm_parity::skip("tape", "tape.json.gz absent (gitignored)"),
        }
    }
    if Fixtures::harness_db().is_none() {
        fm_parity::skip("harness_db", "harness.db absent (gitignored)");
    }
    if Fixtures::root().join("_datadir").is_dir() {
        let (dir, _guard) = Fixtures::temp_datadir().unwrap();
        assert!(dir.join("prefs.json").exists());
        assert_ne!(dir, Fixtures::datadir_path());
    }
}
