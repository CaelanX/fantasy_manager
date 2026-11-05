//! CLI parity for W9-cli-core: `fm --from-snapshot <fixtures> --league <l> --json <cmd>` must
//! reproduce `fixtures/<league>/cli/{settings,picks,roster,waivers,lineup,injuries}.json`
//! (exit code + parsed output; `meta` ages normalized, set-order fields unordered). Also ports
//! the intent of tests/test_cli.py for these commands: help lists every command, `mode`
//! get/set/clear on a temp data dir, `--mode` does not persist, unknown league is a usage error.
//!
//! The `fm` binary is not a dependency of this crate: the test runs `$FM_BIN` or the `fm`
//! executable next to this test's target dir (`cargo build -p fm-cli` first); without one each
//! test skips with a message.

use std::path::PathBuf;
use std::process::Command;

use fm_parity::{assert_parity, normalize, skip, Fixtures, Tolerance};
use serde_json::Value;

fn fm_bin() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FM_BIN") {
        return Some(PathBuf::from(p));
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.parent()?; // target/<profile>/deps/x.exe -> target/<profile>
    let name = if cfg!(windows) { "fm.exe" } else { "fm" };
    let p = dir.join(name);
    p.is_file().then_some(p)
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run(bin: &PathBuf, data_dir: &PathBuf, args: &[&str]) -> Run {
    let out = Command::new(bin)
        .args(args)
        .env("FM_DATA_DIR", data_dir)
        .env("FM_OFFLINE", "1")
        .env_remove("FM_NOW")
        .output()
        .expect("running fm");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

const COMMANDS: [&str; 6] = ["settings", "picks", "roster", "waivers", "lineup", "injuries"];

fn cli_parity(league: &str) {
    let test = format!("cli_core_{league}");
    let Some(bin) = fm_bin() else { return skip(&test, "fm binary not built (cargo build -p fm-cli, or set FM_BIN)") };
    let fx = Fixtures::league(league);
    if !fx.available() {
        return skip(&test, "fixtures missing");
    }
    if league == "fantrax" && !Fixtures::datadir_path().join("fantrax_cache").is_dir() {
        // the Fantrax Rules page (settings.provider_settings) is read from the fxpa file cache
        return skip(&test, "no _datadir/fantrax_cache (gitignored; set FM_PARITY_FIXTURES)");
    }
    let (data_dir, _guard) = Fixtures::temp_datadir().expect("temp data dir");
    let root = Fixtures::root();
    let root_s = root.to_string_lossy().to_string();
    let tol = normalize::set_order_fields(normalize::cli_meta(Tolerance::default()));
    for name in COMMANDS {
        let file = fx.cli(name);
        let args: Vec<String> = file["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect();
        let mut argv: Vec<&str> = vec!["--from-snapshot", &root_s, "--league", league, "--json"];
        argv.extend(args.iter().map(|s| s.as_str()));
        let t = std::time::Instant::now();
        let r = run(&bin, &data_dir, &argv);
        eprintln!("{league} {name}: exit {} in {:?}", r.code, t.elapsed());
        assert_eq!(r.code as i64, file["exit_code"].as_i64().unwrap(), "{league} {name}: stderr {}", r.stderr);
        let actual: Value = serde_json::from_str(&r.stdout)
            .unwrap_or_else(|e| panic!("{league} {name}: not JSON ({e}): {}\nstderr {}", &r.stdout[..r.stdout.len().min(500)], r.stderr));
        assert_parity!(&format!("{test}_{name}"), file["output"], actual, tol.clone());
    }
}

#[test]
fn cli_core_espn() {
    cli_parity("espn");
}

#[test]
fn cli_core_fantrax() {
    cli_parity("fantrax");
}

#[test]
fn cli_core_human_output_runs() {
    let test = "cli_core_human_output_runs";
    let Some(bin) = fm_bin() else { return skip(test, "fm binary not built") };
    if !Fixtures::league("fantrax").available() {
        return skip(test, "fixtures missing");
    }
    let (data_dir, _guard) = Fixtures::temp_datadir().expect("temp data dir");
    let root = Fixtures::root().to_string_lossy().to_string();
    for league in ["espn", "fantrax"] {
        for cmd in [&["settings"][..], &["picks"], &["roster"], &["roster", "--all-teams"], &["waivers"], &["lineup"], &["lineup", "--horizon", "season"], &["injuries", "--no-record"]] {
            let mut argv = vec!["--from-snapshot", root.as_str(), "--league", league];
            argv.extend_from_slice(cmd);
            let r = run(&bin, &data_dir, &argv);
            assert_eq!(r.code, 0, "{league} {cmd:?}: {}", r.stderr);
            assert!(!r.stdout.trim().is_empty(), "{league} {cmd:?}: no output");
        }
    }
    // roster: per-league table title is my team, with the (me) tag and the prospect footnote
    let r = run(&bin, &data_dir, &["--from-snapshot", &root, "--league", "fantrax", "roster"]);
    assert!(r.stdout.contains("(me)") && r.stdout.contains("* prospect: age <= 22"), "{}", r.stdout);
    let r = run(&bin, &data_dir, &["--from-snapshot", &root, "--league", "espn", "settings"]);
    assert!(r.stdout.contains("Games-played limits:") && r.stdout.contains("Valuation params:"), "{}", r.stdout);
}

#[test]
fn help_lists_commands_and_usage_errors() {
    let test = "help_lists_commands";
    let Some(bin) = fm_bin() else { return skip(test, "fm binary not built") };
    let tmp = tempfile::tempdir().unwrap();
    let dd = tmp.path().to_path_buf();
    let r = run(&bin, &dd, &["--help"]);
    assert_eq!(r.code, 0);
    for c in ["settings", "picks", "roster", "waivers", "lineup", "injuries", "sync", "mode", "auth", "web", "web-export"] {
        assert!(r.stdout.contains(c), "{c} missing from help:\n{}", r.stdout);
    }
    for c in ["settings", "roster", "waivers", "lineup", "injuries", "sync", "mode", "picks"] {
        let r = run(&bin, &dd, &[c, "--help"]);
        assert_eq!(r.code, 0, "{c} --help: {}", r.stderr);
    }
    assert_eq!(run(&bin, &dd, &["roster", "--league", "yahoo"]).code, 2);
    assert_eq!(run(&bin, &dd, &["nope"]).code, 2);
}

#[test]
fn mode_get_set_clear_and_option_does_not_persist() {
    let test = "mode_get_set_clear";
    let Some(bin) = fm_bin() else { return skip(test, "fm binary not built") };
    let tmp = tempfile::tempdir().unwrap();
    let dd = tmp.path().to_path_buf();
    let j = |r: Run| -> Value { serde_json::from_str(&r.stdout).unwrap_or_else(|e| panic!("{e}: {} {}", r.stdout, r.stderr)) };
    let v = j(run(&bin, &dd, &["--json", "mode"]));
    assert_eq!((v["mode"].as_str(), v["source"].as_str(), v["changed"].as_bool()), (Some("balanced"), Some("default"), Some(false)));
    let v = j(run(&bin, &dd, &["mode", "rebuild", "--json"]));
    assert_eq!((v["mode"].as_str(), v["source"].as_str(), v["changed"].as_bool()), (Some("rebuild"), Some("prefs"), Some(true)));
    assert_eq!(v["source_label"], "dashboard/prefs");
    let prefs: Value = serde_json::from_str(&std::fs::read_to_string(dd.join("prefs.json")).unwrap()).unwrap();
    assert_eq!(prefs["dynasty_mode"], "rebuild");
    let r = run(&bin, &dd, &["mode"]);
    assert!(r.stdout.contains("Dynasty mode: rebuild (from dashboard/prefs)"), "{}", r.stdout);
    let v = j(run(&bin, &dd, &["mode", "--clear", "--json"]));
    assert_eq!(v["mode"], "balanced");
    let r = run(&bin, &dd, &["mode", "contend", "--clear"]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("Error: give a mode or --clear, not both"));

    // --mode on a command applies for that run only
    if !Fixtures::league("fantrax").available() {
        return skip(test, "fixtures missing (--mode part)");
    }
    let (data_dir, _guard) = Fixtures::temp_datadir().expect("temp data dir");
    let before = std::fs::read_to_string(data_dir.join("prefs.json")).ok();
    let root = Fixtures::root().to_string_lossy().to_string();
    let r = run(&bin, &data_dir, &["--from-snapshot", &root, "--league", "fantrax", "--json", "settings"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let r = run(&bin, &data_dir, &["--from-snapshot", &root, "--league", "fantrax", "roster", "--mode", "rebuild"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("Dynasty mode: rebuild (from --mode, this run only)"), "{}", r.stdout);
    assert_eq!(std::fs::read_to_string(data_dir.join("prefs.json")).ok(), before);
}

#[test]
fn sync_review_and_confirm() {
    let test = "sync_review_and_confirm";
    let Some(bin) = fm_bin() else { return skip(test, "fm binary not built") };
    let tmp = tempfile::tempdir().unwrap();
    let dd = tmp.path().to_path_buf();
    let r = run(&bin, &dd, &["sync", "--confirm", "espn:123=8478402", "--review", "--json"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v: Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(v["confirmed"][0]["nhl_id"], 8478402);
    assert_eq!(v["confirmed"][0]["confidence"], "confirmed");
    assert!(v["pending"].is_array() && v.get("sync").is_none());
    let r = run(&bin, &dd, &["sync", "--confirm", "bogus"]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.starts_with("Error: bad --confirm value"), "{}", r.stderr);
}
