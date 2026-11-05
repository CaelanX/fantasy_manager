//! Rust port of `fantasy_manager/backtest/archive.py` (the writer/reader half). Owner: work
//! package `W5-archive-outcomes`.
//!
//! Daily snapshots under `<data_dir>/archive/`: `projections-<provider>-YYYY-MM-DD.json` (compact
//! JSON, one per provider per day) and `recs-<provider>-YYYY-MM-DD.json` (`indent=1`). Same
//! format as Python (version 3), so the two implementations share the data dir; an identical
//! snapshot is not rewritten (`_write_dedup`).
//!
//! Deviations (all deliberate):
//! * `code_hash` is `"rust:<crate version>"` (sha1 over the Python sources cannot be reproduced;
//!   readers must not compare it across implementations; ARCHITECTURE 4.6). `params_hash` is the
//!   current `valuation::params::params_hash(None)`.
//! * The clock is a parameter: `archived_at` takes `now`, the recs writer takes `as_of`
//!   (Python defaulted to `date.today()` / `datetime.now()`).
//! * Python's `Path.write_text` on Windows writes `\r\n` for the indented recs file; Rust always
//!   writes `\n` (JSON readers do not care).
//! * `score_snapshot` / `score_archive` (which need `fm_backtest`, a crate above this one) live in
//!   `fm_harness::outcomes`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::{NaiveDate, NaiveDateTime};
use fm_core::models::{LeagueContext, Player, Recommendation};
use fm_core::values::PlayerValue;
use fm_core::Map;
use fm_pycompat::num::round_to;
use fm_pycompat::pyjson::{dumps, DumpOpts};
use serde_json::{json, Value};

use crate::valuation::params::params_hash;
use crate::valuation::valuate::{history_baseline, positional_means, season_age, Means};

pub const ARCHIVE_VERSION: i64 = 3;
/// Player fields archived in `inputs` when set (v3).
pub const SIGNAL_FIELDS: [&str; 18] = [
    "toi_per_game", "pp_toi_per_game", "pp_share", "toi_trend", "pp_share_trend", "pct_owned_change",
    "pct_started", "adp", "adp_change", "line", "pp_unit", "pk_unit", "confirmed_start", "ixg_per_game",
    "goals_minus_ixg", "onice_sh_pct", "onice_xg_pct", "xg_split",
];
pub const INPUT_SPLITS: [&str; 4] = ["season", "last30", "last15", "last7"];

/// `"rust:<crate version>"` (see the module docs).
pub fn code_hash() -> String {
    format!("rust:{}", env!("CARGO_PKG_VERSION"))
}

fn hashes() -> (Value, Value) {
    (Value::String(params_hash(None)), Value::String(code_hash()))
}

/// `<data_dir>/archive` (created).
pub fn archive_dir(data_dir: &Path) -> Result<PathBuf> {
    let p = data_dir.join("archive");
    std::fs::create_dir_all(&p).with_context(|| format!("creating {}", p.display()))?;
    Ok(p)
}

fn fv(x: f64) -> Value {
    serde_json::Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null)
}

fn ofv(x: Option<f64>) -> Value {
    x.map(fv).unwrap_or(Value::Null)
}

/// `_num(v, nd)`: `round(float(v), nd)` or None.
fn num(v: Option<f64>, nd: i32) -> Option<f64> {
    v.map(|x| round_to(x, nd))
}

fn stats_obj(stats: &Map<String, f64>) -> Value {
    Value::Object(stats.iter().map(|(k, v)| (k.clone(), fv(*v))).collect())
}

fn rounded_obj(m: &Map<String, f64>, nd: i32) -> Value {
    Value::Object(m.iter().map(|(k, v)| (k.clone(), fv(round_to(*v, nd)))).collect())
}

fn line_json(line: Option<&fm_core::models::StatLine>) -> Value {
    match line {
        None => Value::Null,
        Some(l) => json!({"split": l.split.as_str(), "gp": l.gp, "stats": stats_obj(&l.stats)}),
    }
}

/// `_player_ref`.
pub fn player_ref(p: &Player) -> Value {
    json!({"cid": p.cid, "name": p.name, "nhl_id": p.nhl_id(), "team": p.team, "positions": p.positions})
}

/// Python `==` over parsed JSON (an int equals the same float).
fn py_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            if x.is_f64() || y.is_f64() {
                x.as_f64() == y.as_f64()
            } else {
                x == y
            }
        }
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| py_eq(v, w)))
        }
        _ => a == b,
    }
}

/// `_write_dedup`: "created" / "updated" / "unchanged" (same content as the existing file).
/// `indent` None = compact separators.
pub fn write_dedup(path: &Path, payload: &Value, content_key: &str, indent: Option<usize>) -> Result<&'static str> {
    let mut status = "created";
    if path.exists() {
        let old: Option<Value> = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok());
        if let Some(old) = old {
            if let (Some(o), Some(n)) = (old.get(content_key), payload.get(content_key)) {
                if py_eq(o, n) {
                    return Ok("unchanged");
                }
            }
        }
        status = "updated";
    }
    let tmp = path.with_extension("tmp");
    let opts = match indent {
        Some(n) => DumpOpts { indent: Some(n), ..Default::default() },
        None => DumpOpts::compact(),
    };
    std::fs::write(&tmp, dumps(payload, &opts)).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(status)
}

/// `_fm_block`.
fn fm_block(pv: Option<&PlayerValue>) -> Value {
    let Some(pv) = pv else { return Value::Null };
    let mut out = serde_json::Map::new();
    out.insert("fpg".into(), fv(round_to(pv.fpg, 4)));
    out.insert("rates".into(), rounded_obj(&pv.rates, 5));
    for (key, v) in [
        ("fpg_season", Some(pv.fpg_season)),
        ("fpg_week", Some(pv.fpg_week)),
        ("proj_week", pv.proj_week),
        ("vorp", Some(pv.vorp)),
    ] {
        if let Some(x) = num(v, 4) {
            out.insert(key.into(), fv(x));
        }
    }
    Value::Object(out)
}

/// `player_signals`: the v3 signals the Player carries (floats rounded to 5 places).
pub fn player_signals(p: &Player) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    let f = |x: Option<f64>| x.map(|v| fv(round_to(v, 5)));
    let s = |x: &Option<String>| x.as_ref().map(|v| Value::String(v.clone()));
    for name in SIGNAL_FIELDS {
        let v = match name {
            "toi_per_game" => f(p.toi_per_game),
            "pp_toi_per_game" => f(p.pp_toi_per_game),
            "pp_share" => f(p.pp_share),
            "toi_trend" => f(p.toi_trend),
            "pp_share_trend" => f(p.pp_share_trend),
            "pct_owned_change" => f(p.pct_owned_change),
            "pct_started" => f(p.pct_started),
            "adp" => f(p.adp),
            "adp_change" => f(p.adp_change),
            "line" => s(&p.line),
            "pp_unit" => s(&p.pp_unit),
            "pk_unit" => s(&p.pk_unit),
            "confirmed_start" => p.confirmed_start.map(Value::Bool),
            "ixg_per_game" => f(p.ixg_per_game),
            "goals_minus_ixg" => f(p.goals_minus_ixg),
            "onice_sh_pct" => f(p.onice_sh_pct),
            "onice_xg_pct" => f(p.onice_xg_pct),
            "xg_split" => s(&p.xg_split),
            _ => None,
        };
        if let Some(v) = v {
            out.insert(name.to_string(), v);
        }
    }
    out
}

/// `signal_inputs`: every `SIGNAL_FIELDS` value of an archived `inputs` block (null when absent).
pub fn signal_inputs(inputs: Option<&Value>) -> Map<String, Value> {
    SIGNAL_FIELDS
        .iter()
        .map(|f| (f.to_string(), inputs.and_then(|i| i.get(*f)).cloned().unwrap_or(Value::Null)))
        .collect()
}

/// `player_inputs`: the v2/v3 `inputs` block (everything the valuation read for this player).
pub fn player_inputs(p: &Player, pv: Option<&PlayerValue>, as_of: NaiveDate, means: Option<&Means>) -> Value {
    let age = season_age(p, as_of);
    let mut inp = serde_json::Map::new();
    for split in INPUT_SPLITS {
        if let Some(ln) = p.lines.get(split) {
            inp.insert(split.into(), json!({"gp": ln.gp, "stats": stats_obj(&ln.stats), "zeros": true}));
        }
    }
    let mut hist = Value::Null;
    if let Some(m) = means {
        let (rates, gp, _) = history_baseline(p, m, age);
        if !rates.is_empty() {
            hist = json!({"rates": rounded_obj(&rates, 5), "gp": gp});
        }
    }
    inp.insert("history".into(), hist);
    inp.insert(
        "projection".into(),
        p.lines.get("projected").map(|l| json!({"gp": l.gp, "stats": stats_obj(&l.stats)})).unwrap_or(Value::Null),
    );
    inp.insert("status".into(), json!(p.status.as_str()));
    inp.insert("status_note".into(), json!(p.status_note));
    inp.insert("games_next7".into(), json!(pv.and_then(|v| v.games_next7)));
    inp.insert("offnight_next7".into(), json!(pv.and_then(|v| v.offnight_next7)));
    inp.insert("start_share".into(), ofv(num(pv.and_then(|v| v.start_share), 4)));
    let share = match pv.and_then(|v| v.share_source.as_deref()).filter(|s| !s.is_empty()) {
        Some(src) => {
            let pv = pv.unwrap();
            json!({"source": src, "starts": ofv(num(pv.share_starts, 4)), "team_games": ofv(num(pv.share_team_games, 4))})
        }
        None => Value::Null,
    };
    inp.insert("share".into(), share);
    inp.insert("birth_date".into(), json!(p.birth_date.map(|d| d.format("%Y-%m-%d").to_string())));
    inp.insert("age".into(), ofv(age));
    inp.insert("pct_owned".into(), ofv(p.pct_owned));
    inp.insert("positions".into(), json!(p.positions));
    for (k, v) in player_signals(p) {
        inp.insert(k, v);
    }
    Value::Object(inp)
}

fn position_means_of(players: &[&Player]) -> Means {
    positional_means(players)
}

/// `projection_records`: one record per player, sorted by cid. `means` None = computed from the
/// context's players.
pub fn projection_records(ctx: &LeagueContext, values: Option<&Map<String, PlayerValue>>, means: Option<&Means>) -> Vec<Value> {
    let mut players = ctx.all_players();
    let computed;
    let means = match means {
        Some(m) => m,
        None => {
            computed = position_means_of(&players);
            &computed
        }
    };
    players.sort_by(|a, b| a.cid.cmp(&b.cid));
    let day = ctx.as_of;
    players
        .into_iter()
        .map(|p| {
            let pv = values.and_then(|v| v.get(&p.cid));
            let mut rec = match player_ref(p) {
                Value::Object(m) => m,
                _ => unreachable!(),
            };
            rec.insert("ids".into(), json!(p.ids));
            rec.insert("status".into(), json!(p.status.as_str()));
            rec.insert("pct_owned".into(), ofv(p.pct_owned));
            rec.insert("projected".into(), line_json(p.lines.get("projected")));
            rec.insert("fm".into(), fm_block(pv));
            rec.insert("inputs".into(), player_inputs(p, pv, day, Some(means)));
            Value::Object(rec)
        })
        .collect()
}

fn iso_seconds(now: NaiveDateTime) -> String {
    now.format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// `archive_projections`: snapshot projections for `ctx.provider` on `as_of` (default
/// `ctx.as_of`); `now` stamps `archived_at`. Returns the file and "created"/"updated"/"unchanged".
pub fn archive_projections(
    ctx: &LeagueContext,
    data_dir: &Path,
    values: Option<&Map<String, PlayerValue>>,
    as_of: Option<NaiveDate>,
    now: NaiveDateTime,
) -> Result<(PathBuf, &'static str)> {
    let day = as_of.unwrap_or(ctx.as_of);
    let day_s = day.format("%Y-%m-%d").to_string();
    let path = archive_dir(data_dir)?.join(format!("projections-{}-{}.json", ctx.provider, day_s));
    let all = ctx.all_players();
    let means = position_means_of(&all);
    let (ph, ch) = hashes();
    let pos_means: serde_json::Map<String, Value> =
        means.iter().map(|(g, m)| (g.clone(), rounded_obj(m, 6))).collect();
    let payload = json!({
        "version": ARCHIVE_VERSION, "provider": ctx.provider, "league_id": ctx.league_id,
        "season": ctx.season, "as_of": day_s, "archived_at": iso_seconds(now),
        "params_hash": ph, "code_hash": ch,
        "scoring": serde_json::to_value(&ctx.scoring)?,
        "position_means": Value::Object(pos_means),
        "players": projection_records(ctx, values, Some(&means)),
    });
    let status = write_dedup(&path, &payload, "players", None)?;
    Ok((path, status))
}

/// `recommendation_records`.
pub fn recommendation_records(recs: &[Recommendation]) -> Vec<Value> {
    recs.iter()
        .map(|r| {
            json!({
                "kind": r.kind.as_str(), "score": round_to(r.score, 4), "title": r.title,
                "add": r.add.iter().map(player_ref).collect::<Vec<_>>(),
                "drop": r.drop.iter().map(player_ref).collect::<Vec<_>>(),
                "subjects": r.subjects.iter().map(player_ref).collect::<Vec<_>>(),
                "counterparty": r.counterparty,
                "predicted_gain": ofv(num(r.predicted_gain, 4)),
                "gain_units": r.gain_units.map(|g| g.as_str()),
                "horizon_days": r.horizon_days,
                "strength": ofv(num(r.strength, 2)),
                "reasons": r.reasons.iter().map(|x| json!({
                    "code": x.code, "text": x.text, "value": x.value.map(fv), "baseline": x.baseline.map(fv)
                })).collect::<Vec<_>>(),
            })
        })
        .collect()
}

/// `archive_recommendations`: `as_of` is the day (Python default `date.today()`), `now` stamps
/// `archived_at`.
pub fn archive_recommendations(
    recs: &[Recommendation],
    provider: &str,
    data_dir: &Path,
    as_of: NaiveDate,
    league_id: Option<&str>,
    now: NaiveDateTime,
) -> Result<(PathBuf, &'static str)> {
    let day_s = as_of.format("%Y-%m-%d").to_string();
    let path = archive_dir(data_dir)?.join(format!("recs-{provider}-{day_s}.json"));
    let (ph, ch) = hashes();
    let payload = json!({
        "version": ARCHIVE_VERSION, "provider": provider, "league_id": league_id, "as_of": day_s,
        "archived_at": iso_seconds(now), "params_hash": ph, "code_hash": ch,
        "recommendations": recommendation_records(recs),
    });
    let status = write_dedup(&path, &payload, "recommendations", Some(1))?;
    Ok((path, status))
}

/// `load_snapshot`: read an archive file (v1, v2 or v3); v2-only keys are filled with null / []
/// for v1 files.
pub fn load_snapshot(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut data: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let obj = data.as_object_mut().ok_or_else(|| anyhow!("{}: not a JSON object", path.display()))?;
    obj.entry("version").or_insert(json!(1));
    obj.entry("params_hash").or_insert(Value::Null);
    obj.entry("code_hash").or_insert(Value::Null);
    if obj.contains_key("players") {
        obj.entry("position_means").or_insert(Value::Null);
    }
    if let Some(Value::Array(players)) = obj.get_mut("players") {
        for rec in players.iter_mut() {
            if let Some(o) = rec.as_object_mut() {
                o.entry("inputs").or_insert(Value::Null);
            }
        }
    }
    if let Some(Value::Array(recs)) = obj.get_mut("recommendations") {
        for rec in recs.iter_mut() {
            if let Some(o) = rec.as_object_mut() {
                for k in ["predicted_gain", "gain_units", "horizon_days", "strength"] {
                    o.entry(k).or_insert(Value::Null);
                }
                o.entry("subjects").or_insert(json!([]));
            }
        }
    }
    Ok(data)
}

/// `sorted(dir.glob(f"{kind}-{provider}-????-??-??.json"))` without creating the directory
/// (empty when it does not exist). `?` matches any one character, as in a glob.
pub fn snapshot_files(archive: &Path, kind: &str, provider: &str) -> Vec<PathBuf> {
    let prefix = format!("{kind}-{provider}-");
    let mut out: Vec<PathBuf> = Vec::new();
    let Ok(rd) = std::fs::read_dir(archive) else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(&prefix).and_then(|r| r.strip_suffix(".json")) else { continue };
        let c: Vec<char> = rest.chars().collect();
        if c.len() == 10 && c[4] == '-' && c[7] == '-' {
            out.push(e.path());
        }
    }
    out.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    out
}

/// `list_snapshots(data_dir, provider, kind)`.
pub fn list_snapshots(data_dir: &Path, provider: &str, kind: &str) -> Result<Vec<PathBuf>> {
    Ok(snapshot_files(&archive_dir(data_dir)?, kind, provider))
}

/// `_snapshot_date`: the trailing `YYYY-MM-DD` of the file stem.
pub fn snapshot_date(path: &Path) -> Option<NaiveDate> {
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let chars: Vec<char> = stem.chars().collect();
    if chars.len() < 10 {
        return None;
    }
    let tail: String = chars[chars.len() - 10..].iter().collect();
    NaiveDate::parse_from_str(&tail, "%Y-%m-%d").ok()
}

/// `pick_snapshot`: "first" / "last" snapshot (optionally within [start, end]) or an ISO date.
pub fn pick_snapshot(
    data_dir: &Path,
    provider: &str,
    which: &str,
    start: Option<NaiveDate>,
    end: Option<NaiveDate>,
) -> Result<Option<PathBuf>> {
    let snaps: Vec<PathBuf> = list_snapshots(data_dir, provider, "projections")?
        .into_iter()
        .filter(|p| {
            let d = snapshot_date(p);
            start.is_none_or(|s| d.is_some_and(|d| d >= s)) && end.is_none_or(|e| d.is_some_and(|d| d <= e))
        })
        .collect();
    if snaps.is_empty() {
        return Ok(None);
    }
    Ok(match which {
        "first" => snaps.first().cloned(),
        "last" => snaps.last().cloned(),
        w => snaps
            .iter()
            .find(|p| p.file_stem().is_some_and(|s| s.to_string_lossy().ends_with(w)))
            .cloned(),
    })
}
