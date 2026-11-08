//! `fm keepers` over the parity snapshots (Rust-only command, no Python golden): the Fantrax
//! fixture is a 3-keeper league, ESPN is not. Runs `$FM_BIN` or the `fm` next to this test's
//! target dir (`cargo build -p fm-cli` first); skips without one or without fixtures.

use std::path::PathBuf;
use std::process::Command;

use fm_parity::{skip, Fixtures};
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

fn run(bin: &PathBuf, data_dir: &PathBuf, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(bin)
        .args(args)
        .env("FM_DATA_DIR", data_dir)
        .env("FM_OFFLINE", "1")
        .env_remove("FM_NOW")
        .output()
        .expect("running fm");
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn keepers_command() {
    let test = "keepers_command";
    let Some(bin) = fm_bin() else { return skip(test, "fm binary not built (cargo build -p fm-cli, or set FM_BIN)") };
    if !Fixtures::league("fantrax").available() || !Fixtures::league("espn").available() {
        return skip(test, "fixtures missing");
    }
    let (data_dir, _guard) = Fixtures::temp_datadir().expect("temp data dir");
    let root = Fixtures::root().to_string_lossy().to_string();

    let (code, out, err) = run(&bin, &data_dir, &["--from-snapshot", &root, "--league", "fantrax", "--json", "keepers"]);
    assert_eq!(code, 0, "{err}");
    let v: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("not JSON ({e}): {out}"));
    assert_eq!(v["k"], 3);
    let teams = v["teams"].as_array().unwrap();
    assert!(teams.len() > 1 && teams[0]["me"] == true);
    assert_eq!(v["mine"], teams[0]);
    for t in teams {
        let ks = t["keepers"].as_array().unwrap();
        assert!(ks.len() <= 3);
        let ranks: Vec<u64> = ks.iter().chain(t["bubble"].as_array().unwrap()).map(|c| c["rank"].as_u64().unwrap()).collect();
        assert_eq!(ranks, (1..=ranks.len() as u64).collect::<Vec<_>>(), "{}", t["team"]);
    }
    let pool = v["pool"].as_array().unwrap();
    assert!(!pool.is_empty() && pool.len() <= 30 && v["pool_size"].as_u64().unwrap() >= pool.len() as u64);
    let kept: Vec<&Value> = teams.iter().flat_map(|t| t["keepers"].as_array().unwrap()).map(|c| &c["cid"]).collect();
    assert!(pool.iter().all(|x| !kept.contains(&&x["cid"])));
    assert!(pool.windows(2).all(|w| w[0]["value"].as_f64() >= w[1]["value"].as_f64()));

    let (code, out, err) = run(&bin, &data_dir, &["--from-snapshot", &root, "--league", "fantrax", "keepers"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Your keepers") && out.contains("Draft pool preview") && out.contains("Predicted keepers by team"), "{out}");

    let (code, out, err) = run(&bin, &data_dir, &["--from-snapshot", &root, "--league", "espn", "keepers"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("is not a keeper league"), "{out}");
}
