//! Trade search timing on the parity snapshots (criterion-free; needs `[[bench]] name = "trades"
//! harness = false` in fm-engine/Cargo.toml). Run:
//!     FM_PARITY_FIXTURES=<fixtures dir> cargo bench -p fm-engine --bench trades [-- <repeats>]
//! Prints min / median wall time per league for `recommend_trades` (the advise call) and
//! `recommend_need_trades`. Python reference on the same fixtures (W7 report): ESPN 21.5 s /
//! 10.9 s, Fantrax 19.7 s / 8.6 s.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fm_core::snapshot::Snapshot;
use fm_core::values::{DynastyValue, PlayerValue};
use fm_core::Map;
use fm_engine::recommend::trades::{recommend_need_trades, recommend_trades, TradeOpts, NEED_PER_TEAM};
use fm_engine::valuation::params;
use serde_json::Value;

fn fixtures_root() -> PathBuf {
    std::env::var_os("FM_PARITY_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../parity/fixtures"))
}

/// `values.json` / `dynasty.json` with `player` re-attached from the snapshot by cid.
fn attach<T: serde::de::DeserializeOwned>(path: &Path, snap: &Snapshot) -> Option<Map<String, T>> {
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let obj = raw.as_object()?;
    let players: Map<&str, &fm_core::Player> = snap.ctx.all_players().into_iter().map(|p| (p.cid.as_str(), p)).collect();
    let mut out = Map::new();
    for (cid, v) in obj {
        let mut o = v.as_object()?.clone();
        o.shift_remove("cid");
        o.insert("player".into(), serde_json::to_value(players.get(cid.as_str())?).ok()?);
        out.insert(cid.clone(), serde_json::from_value(Value::Object(o)).ok()?);
    }
    Some(out)
}

fn stats(mut xs: Vec<Duration>) -> (Duration, Duration) {
    xs.sort();
    (xs[0], xs[xs.len() / 2])
}

fn main() {
    let repeats: usize = std::env::args().skip(1).find_map(|a| a.parse().ok()).unwrap_or(5);
    for league in ["espn", "fantrax"] {
        let dir = fixtures_root().join(league);
        let Ok(snap) = Snapshot::load(dir.join("snapshot.json")) else {
            eprintln!("{league}: fixtures missing under {}", dir.display());
            continue;
        };
        params::install_with_active(snap.params.merged.clone(), Some(snap.params.active.clone()));
        let values: Map<String, PlayerValue> = attach(&dir.join("values.json"), &snap).expect("values.json");
        let dynasty: Option<Map<String, DynastyValue>> = attach(&dir.join("dynasty.json"), &snap);
        let opts = TradeOpts { dynasty_values: dynasty.as_ref(), ..TradeOpts::default() };
        let (mut t_trades, mut t_need) = (Vec::new(), Vec::new());
        let mut n = (0, 0);
        for _ in 0..repeats {
            let t = Instant::now();
            n.0 = recommend_trades(&snap.ctx, &values, &opts, None, None).expect("trades").len();
            t_trades.push(t.elapsed());
            let t = Instant::now();
            n.1 = recommend_need_trades(&snap.ctx, &values, &opts, None, 5, NEED_PER_TEAM, None).expect("need").len();
            t_need.push(t.elapsed());
        }
        let (a, b) = (stats(t_trades), stats(t_need));
        println!(
            "{league}: recommend_trades min {:?} median {:?} ({} recs); recommend_need_trades min {:?} median {:?} ({} recs); {} threads",
            a.0,
            a.1,
            n.0,
            b.0,
            b.1,
            n.1,
            rayon::current_num_threads()
        );
    }
}
