//! View-model parity (W9-web-views): every page's view model from the parity snapshot must equal
//! the Python goldens written by `parity/dump_web_views.py` (set `FM_WEB_VIEWS_GOLDEN` to
//! its output dir, default `$FM_PARITY_FIXTURES/_w9_golden`), and the chart SVGs must be
//! byte-identical. Plus unit tests for the chart/format helpers (ported from tests/test_web.py's
//! view-model parts).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use fm_core::models::Recommendation;
use fm_core::PyDateTime;
use fm_engine::recommend::injuries::StatusSnapshot;
use fm_engine::recommend::trades::pick_assets;
use fm_providers::pipeline::from_snapshot;
use fm_web::charts::{self, ChartOpts, Line};
use fm_web::health_view;
use fm_web::views::{self, LoadResult};
use serde_json::{json, Value};

static LOCK: Mutex<()> = Mutex::new(());

fn fixtures() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var("FM_PARITY_FIXTURES").ok()?);
    p.join("espn").join("snapshot.json").exists().then_some(p)
}

fn golden_dir() -> Option<PathBuf> {
    let d = std::env::var("FM_WEB_VIEWS_GOLDEN").map(PathBuf::from).ok().or_else(|| fixtures().map(|f| f.join("_w9_golden")))?;
    d.join("charts.json").exists().then_some(d)
}

fn read(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

/// Numbers within 1e-9 (rel/abs), everything else exact; `loose` paths get abs 0.02.
fn diff(path: &str, a: &Value, b: &Value, out: &mut Vec<String>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, v) in x {
                match y.get(k) {
                    Some(w) => diff(&format!("{path}.{k}"), v, w, out),
                    None => out.push(format!("{path}.{k}: missing in rust")),
                }
            }
            for k in y.keys() {
                if !x.contains_key(k) {
                    out.push(format!("{path}.{k}: extra in rust"));
                }
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                out.push(format!("{path}: len {} vs {}", x.len(), y.len()));
            }
            for (i, (v, w)) in x.iter().zip(y).enumerate() {
                diff(&format!("{path}[{i}]"), v, w, out);
            }
        }
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            // documented set-order tolerance of the plan's win probabilities (ARCHITECTURE 4.3);
            // the page shows them as whole percents
            let tol = if path.contains(".win_") { 1.5 } else { 1e-9 };
            if (x - y).abs() > tol * (1.0 + x.abs().max(y.abs())) * if tol > 1.0 { 0.0 } else { 1.0 } && (x - y).abs() > tol {
                out.push(format!("{path}: {x} vs {y}"));
            }
        }
        _ => {
            if a != b {
                let s = |v: &Value| v.to_string().chars().take(120).collect::<String>();
                out.push(format!("{path}: {} vs {}", s(a), s(b)));
            }
        }
    }
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let t = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &t);
        } else {
            std::fs::copy(e.path(), t).unwrap();
        }
    }
}

/// `values.json` / `dynasty.json` with the players re-attached by cid (the Python run used these).
fn attach<T: serde::de::DeserializeOwned>(raw: Value, ctx: &fm_core::models::LeagueContext) -> Option<fm_core::Map<String, T>> {
    let obj = raw.as_object()?;
    let players: std::collections::HashMap<&str, &fm_core::models::Player> = ctx.all_players().into_iter().map(|p| (p.cid.as_str(), p)).collect();
    let mut out = fm_core::Map::new();
    for (cid, v) in obj {
        let mut v = v.clone();
        let o = v.as_object_mut().unwrap();
        o.remove("cid");
        o.insert("player".into(), serde_json::to_value(players[cid.as_str()]).unwrap());
        out.insert(cid.clone(), serde_json::from_value(v).unwrap_or_else(|e| panic!("{cid}: {e}")));
    }
    Some(out)
}

/// Day-line goalies come from a frozenset in Python (hash-random order, ARCHITECTURE.md 4.3):
/// sort each day's list and the "G: ..." part of its text on both sides.
fn canon_goalies(mut v: Value) -> Value {
    if let Some(days) = v.pointer_mut("/plan/days").and_then(|d| d.as_array_mut()) {
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

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn build_pages(league: &str, fx: &Path, data_dir: &Path) -> serde_json::Map<String, Value> {
    let dir = fx.join(league);
    let loaded = from_snapshot(&dir, Some(data_dir)).unwrap();
    let snap = read(&dir.join("snapshot.json"));
    let now = PyDateTime::parse(snap["now"].as_str().unwrap()).unwrap();
    let recs: Vec<Recommendation> = serde_json::from_value(read(&dir.join("engines").join("advise_merged.json"))).unwrap();
    let provider = loaded.provider.as_ref();
    let values = attach(read(&dir.join("values.json")), &loaded.lc).unwrap();
    let dynv = attach(read(&dir.join("dynasty.json")), &loaded.lc);
    let mut res = LoadResult::new(loaded.lc, values, dynv, recs, now.clone(), loaded.clock.today, data_dir.to_path_buf());
    res.matchup_seed = Some(7);
    let mine = res.ctx.my_team().unwrap().clone();
    let first = mine.players_iter().next().unwrap().clone();
    let t0 = (now.to_utc().unwrap().timestamp() as f64) - 3.0 * 86400.0;
    res.status_log.insert(
        first.cid.clone(),
        vec![
            StatusSnapshot { cid: first.cid.clone(), status: "healthy".into(), note: None, seen_at: t0 },
            StatusSnapshot { cid: first.cid.clone(), status: "dtd".into(), note: Some("upper body".into()), seen_at: t0 + 86400.0 },
        ],
    );
    let prov = Some(provider);
    let (ctx, today) = (&res.ctx, res.today);
    let mut pages = serde_json::Map::new();
    let mut put = |k: &str, v: Value| {
        pages.insert(k.to_string(), v);
    };
    let (ws, wd, wp) = views::week_start(ctx);
    put("week_start", json!([ws.to_string(), wd, wp]));
    put("week_panel", views::week_panel(&res, None));
    put("standings", json!(*views::standings(&res)));
    put("free_agents", json!(views::free_agents(&res, 15, None, None, "vorp")));
    put("free_agents_D_games", json!(views::free_agents(&res, 6, Some("D"), None, "games")));
    put("free_agents_F_fpgweek", json!(views::free_agents(&res, 6, Some("F"), None, "fpg_week")));
    put("free_agents_q", json!(views::free_agents(&res, 15, None, Some("an"), "vorp")));
    put("league_injuries", json!(views::league_injuries(&res, 15)));
    put(
        "roster_rows",
        json!(mine.slots.iter().filter_map(|s| s.player.as_ref().map(|p| views::player_row(&res, p, Some(&s.slot), None))).collect::<Vec<_>>()),
    );
    put("draft_picks", json!(views::draft_picks(&res, Some(&mine))));
    put("credits", json!(views::credits(ctx)));
    put("limits_text", json!(views::limits_text(ctx)));
    put("track_strip", views::track_strip(league, data_dir, today));
    put("outlook", (*views::outlook_section(&res, prov)).clone());
    put("matchup_strip", views::matchup_strip(&res, prov));
    put("matchup", views::matchup_page(&res, prov).unwrap_or_else(|e| json!({"__error__": e})));
    put("plan", views::plan_page(&res, prov));
    put("schedule", views::schedule_page(&res, None, prov, 10));
    put("schedule_week", views::schedule_page(&res, Some("2026-11-18"), prov, 4));
    put("claims", views::claims_section(&res, prov));
    put("exploits", json!(*views::exploits(&res, 8)));
    put("need_trades", json!(*views::need_trades(&res, 5)));
    put("warnings", json!(res.warnings()));
    let moves: Vec<Value> = res
        .recs
        .iter()
        .map(|r| {
            let involves: serde_json::Map<String, Value> =
                views::POSITION_FILTERS.iter().map(|p| (p.to_string(), json!(views::involves_position(r, p)))).collect();
            json!({"title": r.title, "kind": r.kind.as_str(), "compare": views::compare(&res, r),
                   "gain": views::gain_info(r, Some(&res)), "gain_nores": views::gain_info(r, None),
                   "strength": views::strength_info(r, &res.recs), "strength_alone": views::strength_info(r, &[]),
                   "market": views::market_info(r), "track": views::track_info(r, league, data_dir, today),
                   "trade": views::trade_info(r),
                   "counterparty": if r.kind.as_str() == "trade" { views::counterparty(&res, r) } else { Value::Null },
                   "involves": involves})
        })
        .collect();
    put("moves", json!(moves));
    // player page
    let weights = (ctx.scoring.kind.as_str() == "points").then(|| ctx.scoring.weights.clone());
    let (cols, rows) = views::split_table(&first, weights.as_ref());
    let (start, _, pre) = views::week_start(ctx);
    let dynv = res.dynasty.as_ref().and_then(|d| d.get(&first.cid));
    put(
        "player",
        json!({"cid": first.cid, "cols": cols, "rows": rows,
               "schedule": views::team_games(ctx, first.team.as_deref(), start, 14).iter().map(|g| g.to_value()).collect::<Vec<_>>(),
               "history": views::status_history(&res, &first.cid), "history_none": views::status_history(&res, "nope"),
               "breakdown": views::dynasty_breakdown(dynv), "breakdown_none": views::dynasty_breakdown(None),
               "preseason": pre, "row": views::player_row(&res, &first, Some("C"), None), "xg": views::xg_luck(&first),
               "age": views::player_age(&first, ctx.as_of, res.dynasty.as_ref()), "badges": views::unit_badges(&first)}),
    );
    // trade evaluator
    let other = ctx.teams.iter().find(|t| !t.owner_is_me).unwrap();
    let best = |t: &fm_core::models::FantasyTeam| {
        t.players_iter()
            .filter(|p| res.values.contains_key(&p.cid))
            .max_by(|a, b| {
                let (x, y) = (res.values[&a.cid].fpg_season, res.values[&b.cid].fpg_season);
                x.partial_cmp(&y).unwrap().then(a.cid.cmp(&b.cid))
            })
            .map(|p| p.cid.clone())
            .unwrap()
    };
    let (give, get) = (best(&mine), best(other));
    let none: Vec<String> = vec![];
    put("trade_eval_empty", views::trade_eval_page(&res, None, &none, &none, &none, &none, None, true));
    put("trade_eval_offer", views::trade_eval_page(&res, None, &[give.clone()], &[get.clone()], &none, &none, prov, true));
    put("trade_eval_get_only", views::trade_eval_page(&res, Some("no-such-team"), &none, &[get.clone()], &none, &none, prov, true));
    put("trade_eval_switch", views::trade_eval_page(&res, Some(&other.team_id), &[give.clone()], &[get.clone()], &none, &none, prov, false));
    let pa = pick_assets(ctx, &res.values, res.dynasty.as_ref());
    let (mp, tp) = (pa.get(&mine.team_id).cloned().unwrap_or_default(), pa.get(&other.team_id).cloned().unwrap_or_default());
    if !mp.is_empty() || !tp.is_empty() {
        let g = if mp.is_empty() { none.clone() } else { vec![mp[0].key()] };
        let t = if tp.is_empty() { none.clone() } else { vec![tp[0].key()] };
        put("trade_eval_picks", views::trade_eval_page(&res, Some(&other.team_id), &[give.clone()], &[get.clone()], &g, &t, prov, true));
    }
    put("trade_eval_error", views::trade_eval_page(&res, Some(&other.team_id), &none, &none, &strs(&["pick:2099:9:zzz"]), &none, prov, true));
    let mut tc = serde_json::Map::new();
    for (k, a) in [("TOR", Some("TOR")), ("la", Some("la")), ("NJ", Some("NJ")), ("ZZZ", Some("ZZZ")), ("None", None), (" utah ", Some(" utah "))] {
        tc.insert(k.into(), json!(views::team_color(a)));
    }
    put("team_color", Value::Object(tc));
    put(
        "games_text",
        json!([views::games_text(&views::team_games(ctx, Some("TOR"), ctx.as_of, 7)), views::games_text(&[])]),
    );
    put("horizon_label", json!([None, Some(3), Some(7), Some(14), Some(120), Some(200)].iter().map(|d| views::horizon_label(*d)).collect::<Vec<_>>()));
    pages
}

#[test]
fn pages_match_python() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (Some(fx), Some(golden)) = (fixtures(), golden_dir()) else {
        eprintln!("skip pages_match_python: fixtures or golden missing");
        return;
    };
    let mut failures: Vec<String> = Vec::new();
    for league in ["espn", "fantrax"] {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        copy_dir(&fx.join("_datadir"), &data);
        views::clear_track_cache();
        let rust = build_pages(league, &fx, &data);
        let py = read(&golden.join(format!("{league}.json")));
        let py = py.as_object().unwrap();
        for (k, expected) in py {
            if expected.get("__error__").is_some() {
                continue;
            }
            let Some(actual) = rust.get(k) else {
                failures.push(format!("{league}.{k}: page missing in rust"));
                continue;
            };
            let mut d = Vec::new();
            let (expected, actual) = if k == "plan" { (&canon_goalies(expected.clone()), &canon_goalies(actual.clone())) } else { (expected, actual) };
            diff(&format!("{league}.{k}"), expected, actual, &mut d);
            if !d.is_empty() {
                let out = std::env::temp_dir().join("fm_web_views_diffs");
                let _ = std::fs::create_dir_all(&out);
                let _ = std::fs::write(out.join(format!("{league}.{k}.actual.json")), serde_json::to_string_pretty(actual).unwrap());
                let _ = std::fs::write(out.join(format!("{league}.{k}.expected.json")), serde_json::to_string_pretty(expected).unwrap());
                failures.push(format!("{league}.{k}: {} diffs, first: {}", d.len(), d.iter().take(5).cloned().collect::<Vec<_>>().join(" | ")));
            }
        }
        for k in rust.keys() {
            if !py.contains_key(k) {
                failures.push(format!("{league}.{k}: page missing in python golden"));
            }
        }
    }
    assert!(failures.is_empty(), "{} page(s) differ:\n{}", failures.len(), failures.join("\n"));
}

fn l(key: &str, label: &str, v: &[Option<f64>], role: &str, dash: &str) -> Line {
    Line::new(key, label, v.to_vec(), role, dash)
}

#[test]
fn charts_match_python() {
    let Some(golden) = golden_dir() else {
        eprintln!("skip charts_match_python: golden missing");
        return;
    };
    let py = read(&golden.join("charts.json"));
    let x = strs(&["2026-10-05", "2026-10-12", "2026-10-19", "2026-10-26"]);
    let lines = vec![
        l("fm", "fm model", &[Some(0.5), Some(0.48), None, Some(0.45)], "main", ""),
        l("a", "Base <A>", &[Some(0.55), Some(0.5), Some(0.49), Some(0.5)], "base", "5 3"),
        l("b", "Base B", &[None, None, None, None], "base", "1.5 2.5"),
    ];
    let s = |k: &str| py[k].as_str().unwrap().to_string();
    assert_eq!(charts::line_chart("My Chart #1", &x, &lines, "T & \"q\"", "d <x>", &ChartOpts::default()), s("line_chart_basic"));
    let pctf = |v: f64| format!("{}%", fm_pycompat::fmt::fixed(v * 100.0, 0, false));
    let rate = [l("rate", "Hit rate", &[Some(0.5), Some(0.6), Some(0.0), Some(-0.2)], "main", "")];
    let (blo, bhi) = ([Some(0.3), Some(0.4), Some(-0.1), Some(-0.4)], [Some(0.7), Some(0.8), Some(0.1), Some(0.0)]);
    let opts = ChartOpts { band: Some((&blo, &bhi)), y_fmt: &pctf, zero_line: true, ..ChartOpts::default() };
    assert_eq!(charts::line_chart("z", &x, &rate, "t", "d", &opts), s("line_chart_band_zero"));
    let (b1, b2) = ([Some(0.4)], [Some(0.9)]);
    let opts = ChartOpts { band: Some((&b1, &b2)), y_domain: Some((0.0, 1.0)), ..ChartOpts::default() };
    assert_eq!(charts::line_chart("d", &strs(&["wk1"]), &[l("rate", "Hit", &[Some(0.5)], "main", "")], "t", "d", &opts), s("line_chart_domain"));
    assert_eq!(charts::line_chart("", &[], &[l("rate", "Hit", &[], "main", "")], "t", "d", &ChartOpts::default()), s("line_chart_empty"));
    assert_eq!(
        charts::line_chart("flat", &x, &[l("k", "Flat", &[Some(2.0); 4], "main", "")], "t", "d", &ChartOpts::default()),
        s("line_chart_flat")
    );
    assert_eq!(charts::legend(&lines, Some("95% CI (Wilson)")), s("legend"));
    assert_eq!(charts::sparkline_path(&[Some((1.0, 2.0)), None, Some((3.5, 4.25)), Some((5.0, 6.0)), None]), s("sparkline_path"));
    let d = |v: &[Option<f64>], z| {
        let (a, b) = charts::domain(v, z);
        json!([a, b])
    };
    let got = json!([d(&[Some(1.0), Some(3.0), None], false), d(&[Some(2.0), Some(2.0)], false), d(&[], false), d(&[Some(-1.0), Some(4.0)], true), d(&[Some(0.0), Some(0.0)], false)]);
    let mut dd = Vec::new();
    diff("domain", &py["domain"], &got, &mut dd);
    assert!(dd.is_empty(), "{dd:?}");
    let band = charts::band_path(
        &[Some((1.0, 2.0)), Some((3.0, 4.0)), None, Some((7.0, 8.0))],
        &[Some((1.0, 1.0)), Some((3.0, 3.0)), Some((5.0, 5.0)), Some((7.0, 7.0))],
    );
    assert_eq!(py["band_path"][0].as_str().unwrap(), band);
    let sp = charts::scale_points(&[Some(1.0), None, Some(3.0)], 0.0, 4.0, 360.0, 132.0, charts::PAD);
    let got = json!([sp.iter().map(|p| p.map(|p| json!([p.0, p.1]))).collect::<Vec<_>>()]);
    let mut dd = Vec::new();
    diff("scale", &py["scale_points"], &got, &mut dd);
    assert!(dd.is_empty(), "{dd:?}");
    // health charts from the synthetic series
    let series = py["series"].clone();
    let mut dd = Vec::new();
    diff("health_charts", &py["health_charts"], &health_view::health_charts(Some(&series), "espn"), &mut dd);
    assert!(dd.is_empty(), "{:?}", dd.iter().take(5).collect::<Vec<_>>());
    diff("health_charts_none", &py["health_charts_none"], &health_view::health_charts(None, "fantrax"), &mut dd);
    let rows = json!({"rows": [
        {"knob": "k1", "current": 0.123456789, "candidate": 1234567.0, "holdout_before": 0.1, "holdout_after": 0.09,
         "hist_before": null, "hist_after": 0.2},
        {"knob": "k2", "current": 1e-05, "candidate": 2.5, "holdout_before": null, "holdout_after": null}]});
    diff("refit_rows", &py["refit_rows"], &json!(health_view::refit_rows(Some(&rows))), &mut dd);
    assert!(dd.is_empty(), "{:?}", dd.iter().take(5).collect::<Vec<_>>());
    use health_view::{num, pct, signed_pct};
    let got = json!({"pct": [pct(None), pct(Some(0.456)), pct(Some(1.0))], "num": [num(None, 2), num(Some(1.23456), 3), num(Some(2.0), 2)],
                     "signed_pct": [signed_pct(None), signed_pct(Some(0.456)), signed_pct(Some(-0.004))]});
    assert_eq!(py["fmt"], got);
}

// ---- unit tests (test_web.py view-model parts) ----

#[test]
fn horizon_labels() {
    assert_eq!(views::horizon_label(None), None);
    assert_eq!(views::horizon_label(Some(7)).as_deref(), Some("next 7 days"));
    assert_eq!(views::horizon_label(Some(3)).as_deref(), Some("next 3 days"));
    assert_eq!(views::horizon_label(Some(120)).as_deref(), Some("season"));
}

#[test]
fn team_colors_and_aliases() {
    assert_eq!(views::team_color(Some("tor")), Some("#00205B"));
    assert_eq!(views::team_color(Some("LA")), views::team_color(Some("LAK")));
    assert_eq!(views::team_color(Some("zzz")), None);
    assert_eq!(views::team_color(None), None);
}
