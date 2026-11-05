//! CLI parity for W10-cli-analysis: `fm --from-snapshot <fixtures> --league <l> --json <cmd>` must
//! reproduce `fixtures/<league>/cli/{trades,trades_need,trade_eval,flags,advise,alerts,trending,
//! lines,goalies,schedule,playoffs,outlook,matchup,plan,claims}.json` (exit code + parsed output;
//! `meta` ages normalized, set-order fields unordered). Also runs every command's human output and
//! ports the intent of tests/test_cli.py for these commands (usage errors, help, `notify` / `ask`
//! without configuration, `trade-eval` argument errors).
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
        .env("COLUMNS", "200")
        .env_remove("FM_NOW")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("DISCORD_WEBHOOK_URL")
        .env_remove("SLACK_WEBHOOK_URL")
        .output()
        .expect("running fm");
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

const COMMANDS: [&str; 15] = [
    "trades", "trades_need", "trade_eval", "flags", "advise", "alerts", "trending", "lines", "goalies", "schedule", "playoffs", "outlook",
    "matchup", "plan", "claims",
];

/// `days[*].goalies` is a set in Python (documented set-order exception): sort it on both sides.
fn canon_goalies(mut v: Value) -> Value {
    if let Some(days) = v.get_mut("days").and_then(|d| d.as_array_mut()) {
        for day in days {
            let Some(gs) = day.get("goalies").and_then(|g| g.as_array()).cloned() else { continue };
            let names: Vec<String> = gs.iter().filter_map(|g| g.as_str().map(str::to_string)).collect();
            let mut sorted = names.clone();
            sorted.sort();
            if let Some(Value::String(t)) = day.get_mut("text") {
                *t = t.replace(&format!("G: {}", names.join(", ")), &format!("G: {}", sorted.join(", ")));
            }
            day["goalies"] = Value::from(sorted);
        }
    }
    v
}

fn cli_parity(league: &str) {
    let test = format!("cli_analysis_{league}");
    let Some(bin) = fm_bin() else { return skip(&test, "fm binary not built (cargo build -p fm-cli, or set FM_BIN)") };
    let fx = Fixtures::league(league);
    if !fx.available() {
        return skip(&test, "fixtures missing");
    }
    let root = Fixtures::root();
    let root_s = root.to_string_lossy().to_string();
    let tol = normalize::set_order_fields(normalize::cli_meta(Tolerance::default()))
        // documented set-order exception: the Monte Carlo player order moves the percentage in advice text
        .with_normalize("$.advice[*]", r"\(\d+% to win\)", "(N% to win)")
        // `schedule` loads an unvalued context in Python (fewer NHL player page requests than the snapshot's)
        .with_normalize("$.meta.sources[*]", r"NHL player pages: \d+ requests", "NHL player pages: N requests");
    let mut failures: Vec<String> = Vec::new();
    for name in COMMANDS {
        let file = fx.cli(name);
        let args: Vec<String> = file["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect();
        // a fresh data dir per command: plan memory / snapshots written by one must not leak into the next
        let (data_dir, _guard) = Fixtures::temp_datadir().expect("temp data dir");
        let mut argv: Vec<&str> = vec!["--from-snapshot", &root_s, "--league", league, "--json"];
        argv.extend(args.iter().map(|s| s.as_str()));
        let t = std::time::Instant::now();
        let r = run(&bin, &data_dir, &argv);
        eprintln!("{league} {name}: exit {} in {:?}", r.code, t.elapsed());
        if r.code as i64 != file["exit_code"].as_i64().unwrap() {
            failures.push(format!("{league} {name}: exit {} (expected {}), stderr {}", r.code, file["exit_code"], r.stderr));
            continue;
        }
        let actual: Value = match serde_json::from_str(&r.stdout) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{league} {name}: not JSON ({e}): {}\nstderr {}", &r.stdout[..r.stdout.len().min(500)], r.stderr));
                continue;
            }
        };
        let mut expected = file["output"].clone();
        // Python process-state leak in the fixture dump: the ESPN provider's "trending unavailable"
        // warning (raised by the `trending` command) showed up in the meta of later commands
        if let Some(Value::Array(w)) = expected.pointer_mut("/meta/warnings") {
            w.retain(|x| !x.as_str().is_some_and(|s| s.starts_with("ESPN trending players unavailable")));
        }
        let (exp, act) = (canon_goalies(expected), canon_goalies(actual));
        let diffs = fm_parity::compare(&exp, &act, &tol);
        if !diffs.is_empty() {
            let shown: Vec<String> = diffs.iter().take(12).map(|d| d.to_string()).collect();
            failures.push(format!("{league} {name}: {} differences\n{}", diffs.len(), shown.join("\n")));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn cli_analysis_espn() {
    cli_parity("espn");
}

#[test]
fn cli_analysis_fantrax() {
    cli_parity("fantrax");
}

#[test]
fn cli_analysis_human_output_runs() {
    let test = "cli_analysis_human_output_runs";
    let Some(bin) = fm_bin() else { return skip(test, "fm binary not built") };
    if !Fixtures::league("fantrax").available() {
        return skip(test, "fixtures missing");
    }
    let root = Fixtures::root().to_string_lossy().to_string();
    let cases: Vec<(&str, Vec<&str>)> = vec![
        ("espn", vec!["trades"]),
        ("espn", vec!["trades", "--need"]),
        ("fantrax", vec!["trades"]),
        ("fantrax", vec!["trades", "--need"]),
        ("espn", vec!["trade-eval", "--give", "Leon Draisaitl", "--get", "Macklin Celebrini"]),
        ("fantrax", vec!["trade-eval", "--give", "Jack Eichel", "--get", "Brady Tkachuk"]),
        ("espn", vec!["flags"]),
        ("fantrax", vec!["flags"]),
        ("espn", vec!["advise"]),
        ("fantrax", vec!["advise"]),
        ("espn", vec!["alerts"]),
        ("espn", vec!["trending"]),
        ("fantrax", vec!["trending", "--fallers"]),
        ("espn", vec!["lines"]),
        ("fantrax", vec!["lines"]),
        ("espn", vec!["goalies"]),
        ("fantrax", vec!["goalies", "--week"]),
        ("espn", vec!["goalies", "--week"]),
        ("espn", vec!["news"]),
        ("fantrax", vec!["news", "--all", "-n", "5"]),
        ("espn", vec!["schedule"]),
        ("espn", vec!["schedule", "--stream", "--playoffs", "--season"]),
        ("fantrax", vec!["schedule", "--stream", "--playoffs", "--season"]),
        ("espn", vec!["playoffs"]),
        ("fantrax", vec!["playoffs"]),
        ("espn", vec!["outlook"]),
        ("fantrax", vec!["outlook"]),
        ("espn", vec!["matchup", "--seed", "7"]),
        ("fantrax", vec!["matchup"]),
        ("espn", vec!["plan", "--no-archive"]),
        ("fantrax", vec!["plan"]),
        ("espn", vec!["claims", "-n", "3"]),
        ("fantrax", vec!["claims"]),
        ("espn", vec!["report"]),
        ("fantrax", vec!["report", "--no-record", "-n", "5"]),
    ];
    for (league, cmd) in cases {
        let (data_dir, _guard) = Fixtures::temp_datadir().expect("temp data dir");
        let mut argv = vec!["--from-snapshot", root.as_str(), "--league", league];
        argv.extend_from_slice(&cmd);
        let r = run(&bin, &data_dir, &argv);
        assert_eq!(r.code, 0, "{league} {cmd:?}: {}", r.stderr);
        assert!(!r.stdout.trim().is_empty(), "{league} {cmd:?}: no output");
    }
    // the digest files were written under the data dir
    let (data_dir, _guard) = Fixtures::temp_datadir().expect("temp data dir");
    let r = run(&bin, &data_dir, &["--from-snapshot", &root, "--league", "espn", "--json", "report", "--no-record"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v: Value = serde_json::from_str(&r.stdout).unwrap();
    assert!(std::path::Path::new(v["markdown"].as_str().unwrap()).is_file());
    assert!(std::path::Path::new(v["html"].as_str().unwrap()).is_file());
    assert!(v["summary"].is_string() && v["recommendations"].is_number());
}

#[test]
fn trade_eval_argument_errors_and_help() {
    let test = "trade_eval_argument_errors_and_help";
    let Some(bin) = fm_bin() else { return skip(test, "fm binary not built") };
    let tmp = tempfile::tempdir().unwrap();
    let dd = tmp.path().to_path_buf();
    // no offer named: exit 2 before any league load
    let r = run(&bin, &dd, &["trade-eval"]);
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("Name the offer"), "{}", r.stderr);
    // help for every command in this package
    for c in [
        "trades", "trade-eval", "flags", "advise", "alerts", "trending", "lines", "goalies", "news", "ask", "report", "notify", "schedule",
        "playoffs", "outlook", "matchup", "plan", "claims",
    ] {
        let r = run(&bin, &dd, &[c, "--help"]);
        assert_eq!(r.code, 0, "{c} --help: {}", r.stderr);
        let h = run(&bin, &dd, &["--help"]);
        assert!(h.stdout.contains(c), "{c} missing from help");
    }
    // typer-style usage errors
    assert_eq!(run(&bin, &dd, &["claims", "--league", "yahoo"]).code, 2);
    assert_eq!(run(&bin, &dd, &["matchup", "--seed", "x"]).code, 2);
    assert_eq!(run(&bin, &dd, &["notify"]).code, 2);
}

#[test]
fn notify_and_ask_without_configuration_exit_1() {
    let test = "notify_and_ask_without_configuration";
    let Some(bin) = fm_bin() else { return skip(test, "fm binary not built") };
    let tmp = tempfile::tempdir().unwrap();
    let dd = tmp.path().to_path_buf();
    let r = run(&bin, &dd, &["notify", "hello", "--json"]);
    assert_eq!(r.code, 1);
    let v: Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(v["configured"], false);
    assert!(v["results"].as_array().is_some_and(|a| !a.is_empty()));
    let r = run(&bin, &dd, &["ask", "who should I start?"]);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("`fm ask` needs an LLM"), "{}", r.stderr);
}

#[test]
fn trade_eval_unknown_player_and_pick_errors_exit_2() {
    let test = "trade_eval_unknown_player";
    let Some(bin) = fm_bin() else { return skip(test, "fm binary not built") };
    if !Fixtures::league("espn").available() {
        return skip(test, "fixtures missing");
    }
    let (data_dir, _guard) = Fixtures::temp_datadir().expect("temp data dir");
    let root = Fixtures::root().to_string_lossy().to_string();
    let r = run(&bin, &data_dir, &["--from-snapshot", &root, "--league", "espn", "trade-eval", "--give", "Wayne Gretzky"]);
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.starts_with("Error:"), "{}", r.stderr);
    let r = run(&bin, &data_dir, &["--from-snapshot", &root, "--league", "espn", "trade-eval", "--give-pick", "2027 R1", "--team", "x"]);
    assert_eq!(r.code, 2, "{}", r.stderr);
}

// `assert_parity!` is used by the shared harness; keep the import alive for tolerance tweaks.
#[allow(dead_code)]
fn _uses_macro(a: &Value, b: &Value) {
    assert_parity!("noop", a.clone(), b.clone(), Tolerance::default());
}
