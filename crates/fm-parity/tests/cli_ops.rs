//! CLI parity for W10-cli-ops: `fm harness ...` and `fm backtest ...` against the Python CLI.
//!
//! Both CLIs run on separate temp copies of `$FM_PARITY_FIXTURES/_datadir` (offline, cwd = a
//! temp dir so no `.env` is read) and their exit codes and `--json` outputs are compared
//! (timestamps / paths / the clock-dependent `last_run` normalised). The Rust binary is
//! `$FM_BIN` or `fm` next to this test's target dir (`cargo build -p fm-cli` first); the
//! Python side is a `legacy-python` checkout (`FM_LEGACY_ROOT`, first on `sys.path`) run with
//! `FM_PYTHON` (default its `.venv`). Missing pieces skip the tests.

use std::path::{Path, PathBuf};
use std::process::Command;

use fm_parity::{assert_parity, skip, Fixtures, Tolerance};
use serde_json::Value;

fn fm_bin() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FM_BIN") {
        return Some(PathBuf::from(p));
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.parent()?;
    let p = dir.join(if cfg!(windows) { "fm.exe" } else { "fm" });
    p.is_file().then_some(p)
}

/// A `legacy-python` checkout (`FM_LEGACY_ROOT`, default `../fm-legacy` beside the repo) and its
/// python (`FM_PYTHON`, default the checkout's `.venv`). None: the Python parity checks skip.
fn legacy() -> Option<(PathBuf, PathBuf)> {
    let root = std::env::var_os("FM_LEGACY_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fm-legacy"));
    if !root.join("fantasy_manager").is_dir() {
        return None;
    }
    let py = std::env::var_os("FM_PYTHON")
        .map(PathBuf::from)
        .or_else(|| ["Scripts/python.exe", "bin/python"].iter().map(|p| root.join(".venv").join(p)).find(|p| p.is_file()))?;
    py.is_file().then_some((root, py))
}

fn python() -> Option<PathBuf> {
    legacy().map(|(_, py)| py)
}

fn worktree() -> PathBuf {
    legacy().expect("legacy-python checkout").0
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run_rust(bin: &Path, data: &Path, cwd: &Path, args: &[&str]) -> Run {
    let out = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .env("FM_DATA_DIR", data)
        .env("FM_OFFLINE", "1")
        .env_remove("FM_NOW")
        .output()
        .expect("running fm");
    Run { code: out.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&out.stdout).into_owned(), stderr: String::from_utf8_lossy(&out.stderr).into_owned() }
}

fn run_py(py: &Path, data: &Path, cwd: &Path, args: &[&str]) -> Run {
    let code = "import sys; sys.path.insert(0, sys.argv.pop(1)); from fantasy_manager.cli import app; app()";
    let out = Command::new(py)
        .arg("-c")
        .arg(code)
        .arg(worktree())
        .args(args)
        .current_dir(cwd)
        .env("FM_DATA_DIR", data)
        .env("FM_OFFLINE", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .env_remove("FM_NOW")
        .output()
        .expect("running python");
    Run { code: out.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&out.stdout).into_owned(), stderr: String::from_utf8_lossy(&out.stderr).into_owned() }
}

struct Env {
    bin: PathBuf,
    py: PathBuf,
    rs_data: PathBuf,
    py_data: PathBuf,
    cwd: PathBuf,
    _guards: Vec<Box<dyn std::any::Any>>,
}

fn env(test: &str) -> Option<Env> {
    let Some(bin) = fm_bin() else {
        skip(test, "fm binary not built (cargo build -p fm-cli, or set FM_BIN)");
        return None;
    };
    let Some(py) = python() else {
        skip(test, "no legacy-python checkout (FM_LEGACY_ROOT)");
        return None;
    };
    if !Fixtures::datadir_path().is_dir() {
        skip(test, "fixtures _datadir missing");
        return None;
    }
    let (rs_data, g1) = Fixtures::temp_datadir().expect("temp data dir");
    let (py_data, g2) = Fixtures::temp_datadir().expect("temp data dir");
    let tmp = tempfile::tempdir().expect("cwd");
    let cwd = tmp.path().to_path_buf();
    Some(Env { bin, py, rs_data, py_data, cwd, _guards: vec![g1, g2, Box::new(tmp)] })
}

impl Env {
    fn both(&self, args: &[&str]) -> (Run, Run) {
        (run_py(&self.py, &self.py_data, &self.cwd, args), run_rust(&self.bin, &self.rs_data, &self.cwd, args))
    }

    /// Run `args` on both sides; exit codes must match; returns the parsed `--json` outputs.
    fn json(&self, name: &str, args: &[&str], tol: Tolerance) -> (Value, Value) {
        let (p, r) = self.both(args);
        assert_eq!(p.code, r.code, "{name}: exit code (python stderr {}, rust stderr {})", tail(&p.stderr), tail(&r.stderr));
        let parse = |who: &str, run: &Run| -> Value {
            serde_json::from_str(&run.stdout).unwrap_or_else(|e| panic!("{name}: {who} not JSON ({e}): {}\n{}", head(&run.stdout), tail(&run.stderr)))
        };
        let (pv, rv) = (parse("python", &p), parse("rust", &r));
        assert_parity!(name, pv, rv, tol);
        (pv, rv)
    }

    /// Text commands: only the exit code is compared.
    fn code(&self, name: &str, args: &[&str]) -> (Run, Run) {
        let (p, r) = self.both(args);
        assert_eq!(p.code, r.code, "{name}: exit code (python stderr {}, rust stderr {})", tail(&p.stderr), tail(&r.stderr));
        (p, r)
    }
}

/// Python writes CRLF on Windows and rich wraps at 80 columns: compare modulo whitespace.
fn ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn head(s: &str) -> String {
    s.chars().take(600).collect()
}

fn tail(s: &str) -> String {
    let n = s.chars().count();
    s.chars().skip(n.saturating_sub(800)).collect()
}

fn ignore_noise(tol: Tolerance) -> Tolerance {
    tol.with_ignore("db").with_ignore("dir").with_ignore("**/started_at").with_ignore("last_run.*")
}

#[test]
fn harness_status_params_ledger() {
    let t = "cli_ops_harness_status_params_ledger";
    let Some(e) = env(t) else { return };
    e.json("status", &["harness", "status", "--json"], ignore_noise(Tolerance::default()));
    e.json("status_espn", &["harness", "status", "--json", "--league", "espn"], ignore_noise(Tolerance::default()));
    e.json("params", &["harness", "params", "--json"], ignore_noise(Tolerance::default()));
    e.json("params_history", &["harness", "params", "--history", "--json"], ignore_noise(Tolerance::default()));
    e.json("ledger", &["harness", "ledger", "--json"], ignore_noise(Tolerance::default()));
    e.json("ledger_user_only", &["harness", "ledger", "--origin", "user_only", "--json", "-n", "5"], ignore_noise(Tolerance::default()));
    e.json("ledger_kind", &["harness", "ledger", "--kind", "waiver", "--league", "fantrax", "--json"], ignore_noise(Tolerance::default()));
    let (p, r) = e.code("params_text", &["harness", "params"]);
    assert!(!p.stdout.is_empty() && !r.stdout.is_empty());
    e.code("status_text", &["harness", "status"]);
    e.code("ledger_text", &["harness", "ledger"]);
    e.code("bad_league", &["harness", "status", "--league", "nhl"]);
}

#[test]
fn harness_auto_apply_rollback() {
    let t = "cli_ops_harness_auto_apply_rollback";
    let Some(e) = env(t) else { return };
    e.json("auto_show", &["harness", "auto-apply", "--json"], Tolerance::default());
    let (p, r) = e.both(&["harness", "auto-apply", "off", "--json"]);
    assert_eq!(ws(&p.stdout), ws(&r.stdout), "auto-apply off: compact json.dumps");
    assert_eq!(r.stdout, "{\"auto_apply\": false, \"from\": \"2026-11-16\"}
");
    let (p, r) = e.both(&["harness", "auto-apply", "on", "--json"]);
    assert_eq!(ws(&p.stdout), ws(&r.stdout));
    e.code("auto_bad", &["harness", "auto-apply", "maybe"]);
    // nothing to roll back on a fresh data dir: error JSON is compact, exit 1
    let (p, r) = e.both(&["harness", "rollback", "--json"]);
    assert_eq!(p.code, r.code);
    assert_eq!(ws(&p.stdout), ws(&r.stdout), "rollback error json");
    e.json("params_after", &["harness", "params", "--json"], ignore_noise(Tolerance::default()));
}

#[test]
fn harness_grade_refit_rebuild() {
    let t = "cli_ops_harness_grade_refit_rebuild";
    let Some(e) = env(t) else { return };
    let tol = ignore_noise(Tolerance::default().with_tol(1e-9, 1e-9));
    e.json("grade", &["harness", "grade", "--json", "--week", "2026-10-26"], tol.clone());
    e.json("refit_dry", &["harness", "refit", "--dry-run", "--json", "--as-of", "2026-11-02"], tol.clone());
    e.code("refit_excl", &["harness", "refit", "--dry-run", "--apply"]);
    e.code("refit_bad_group", &["harness", "refit", "--dry-run", "--only", "nope"]);
    e.code("rebuild", &["harness", "rebuild"]);
    e.json("status_after_rebuild", &["harness", "status", "--json"], tol.clone());
    e.json("ledger_after_rebuild", &["harness", "ledger", "--json"], tol);
}

#[test]
fn backtest_commands() {
    let t = "cli_ops_backtest";
    let Some(e) = env(t) else { return };
    // no stored actuals / report in a fresh data dir: same exit code and message
    let (p, r) = e.code("grade_no_data", &["backtest", "grade", "--season", "20252026"]);
    assert_eq!(ws(&p.stdout), ws(&r.stdout));
    let (p, r) = e.code("report_none", &["backtest", "report"]);
    assert_eq!(ws(&p.stdout), ws(&r.stdout));
    e.code("run_no_data", &["backtest", "run", "--json"]);
    e.code("help", &["backtest", "--help"]);
    e.code("harness_help", &["harness", "--help"]);
}
