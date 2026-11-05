//! Rust port of `fantasy_manager/providers/preseason_enrich.py`: NHL preseason lines (gameType 1)
//! for unproven skaters, a weak signal for players with little or no NHL regular-season history.
//! Owner: work package `W4-enrich` (wave 4).
//!
//! [`enrich_preseason`] reads every finished preseason game before `ctx.as_of` once: the 32
//! club schedules list the games, then one box score + one gamecenter landing (goal strength)
//! per game. A game finished more than two days ago is cached 30 days, a more recent one 12 h.
//!
//! Deviations: the `_REGISTRY` is `ctx.enrich.preseason`; `is_unproven` and its constants live
//! in `fm_core::unproven` (re-exported here); `PreseasonLine` is `fm_core::snapshot::PreseasonLine`
//! (its `pts_per_game` / `per_game` / `summary` helpers are free functions here). `models.Split`
//! has no "preseason", so [`statline`] is always None and [`attach`] never stores (as in Python).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use chrono::{Duration, NaiveDate};
use fm_core::models::{LeagueContext, Player, StatLine};
use fm_core::snapshot::PreseasonLine;
use fm_core::Map;
use fm_http::HttpCache;
use fm_pycompat::fmt::fixed;
use fm_sources::nhl::{current_season, NhlClient, NhlSkaterGameLine};
use regex::Regex;

pub use fm_core::unproven::{is_unproven, PRIOR_SPLITS, PROVEN_SEASON_GP, UNPROVEN_CAREER_GP};

pub const HOUR: f64 = 3600.0;
/// games of the last two days (late stat corrections)
pub const TTL_RECENT_GAME: f64 = 12.0 * HOUR;
/// older final box scores never change
pub const TTL_FINAL_GAME: f64 = 30.0 * 24.0 * HOUR;
pub const RECENT_DAYS: i64 = 2;
/// same as providers.enrich (the club schedules are shared)
pub const TTL_SCHEDULE: f64 = 7.0 * 24.0 * HOUR;
/// Read preseason games until this many days after opening night.
pub const WINDOW_DAYS: i64 = 45;
pub const SPLIT: &str = "preseason";

fn gamecenter_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"/gamecenter/(\d+)/").expect("static regex"))
}

// --------------------------------------------------------------------------- PreseasonLine helpers

/// `PreseasonLine.pts_per_game`.
pub fn pts_per_game(line: &PreseasonLine) -> f64 {
    if line.gp != 0 {
        line.stats.get("PTS").copied().unwrap_or(0.0) / line.gp as f64
    } else {
        0.0
    }
}

/// `PreseasonLine.per_game()`.
pub fn per_game(line: &PreseasonLine) -> Map<String, f64> {
    if line.gp <= 0 {
        return Map::new();
    }
    line.stats.iter().filter(|(k, _)| *k != "GP").map(|(k, v)| (k.clone(), v / line.gp as f64)).collect()
}

/// `PreseasonLine.summary()`: "4 GP: 5 G, 3 A, 8 PTS, 12 SOG, 2 PPP, 16.4 TOI".
pub fn summary(line: &PreseasonLine) -> String {
    let s = &line.stats;
    let g = |k: &str| s.get(k).copied().unwrap_or(0.0);
    let mut parts = vec![
        format!("{} G", fixed(g("G"), 0, false)),
        format!("{} A", fixed(g("A"), 0, false)),
        format!("{} PTS", fixed(g("PTS"), 0, false)),
    ];
    if let Some(v) = s.get("SOG") {
        parts.push(format!("{} SOG", fixed(*v, 0, false)));
    }
    if let Some(v) = s.get("PPP") {
        parts.push(format!("{} PPP", fixed(*v, 0, false)));
    } else if let Some(v) = s.get("PPG") {
        parts.push(format!("{} PPG", fixed(*v, 0, false)));
    }
    if let Some(t) = line.toi_per_game() {
        parts.push(format!("{} TOI", fixed(t, 1, false)));
    }
    format!("{} GP: {}", line.gp, parts.join(", "))
}

// --------------------------------------------------------------------------- pure helpers

/// Sum per-game lines per player (one line per game id). PP / SH assists and points are kept
/// only when every game of the player had its scoring summary.
pub fn aggregate(lines: &[NhlSkaterGameLine], season: i64) -> Map<i64, PreseasonLine> {
    let mut by_player: Map<i64, Map<i64, &NhlSkaterGameLine>> = Map::new();
    for ln in lines {
        by_player.entry(ln.player_id).or_default().insert(ln.game_id, ln);
    }
    let mut out: Map<i64, PreseasonLine> = Map::new();
    for (pid, games) in by_player {
        let mut gl: Vec<&NhlSkaterGameLine> = games.into_values().collect();
        gl.sort_by_key(|g| (g.date, g.game_id));
        let mut tot: Map<String, f64> = Map::new();
        let mut toi = 0.0;
        let mut toi_n = 0;
        for g in &gl {
            for (k, v) in &g.stats {
                *tot.entry(k.clone()).or_insert(0.0) += *v;
            }
            if let Some(t) = g.toi.filter(|t| *t > 0.0) {
                toi += t;
                toi_n += 1;
            }
        }
        let strength = gl.iter().all(|g| g.has_strength);
        if !strength {
            for k in ["PPA", "PPP", "SHA", "SHP"] {
                tot.shift_remove(k);
            }
        }
        tot.insert("GP".into(), gl.len() as f64);
        let last = gl.last().expect("at least one game");
        out.insert(
            pid,
            PreseasonLine {
                nhl_id: pid,
                season,
                gp: gl.len() as i64,
                stats: tot,
                toi_seconds: toi,
                toi_games: toi_n,
                has_strength: strength,
                team: last.team.clone(),
                last_date: Some(last.date),
                games: gl.iter().map(|g| g.date).collect(),
            },
        );
    }
    out
}

/// `StatLine(split="preseason")` when `models.Split` accepts it: never (None), as in Python.
pub fn statline(_line: &PreseasonLine) -> Option<StatLine> {
    None
}

/// Put the line on `p.lines["preseason"]` if the model allows it; true when stored (never).
pub fn attach(p: &mut Player, line: &PreseasonLine) -> bool {
    match statline(line) {
        Some(sl) => {
            p.lines.insert(SPLIT.into(), sl);
            true
        }
        None => false,
    }
}

// --------------------------------------------------------------------------- registry

/// Register lines into `ctx.enrich.preseason`; returns how many.
pub fn register<'a>(ctx: &mut LeagueContext, lines: impl IntoIterator<Item = &'a PreseasonLine>) -> usize {
    let st = Arc::make_mut(&mut ctx.enrich);
    let mut n = 0;
    for ln in lines {
        st.preseason.insert((ln.season, ln.nhl_id), ln.clone());
        n += 1;
    }
    n
}

/// cid -> preseason line for the context's skaters (unproven ones only by default) in the season
/// of `ctx.as_of`. Empty when nothing was registered.
pub fn preseason_lines(ctx: &LeagueContext, unproven_only: bool) -> Map<String, PreseasonLine> {
    let mut out = Map::new();
    if ctx.enrich.preseason.is_empty() {
        return out;
    }
    let season = current_season(ctx.as_of);
    for p in ctx.all_players() {
        if p.is_goalie() || (unproven_only && !is_unproven(p)) {
            continue;
        }
        if let Some(ln) = ctx.enrich.preseason_line(p, season) {
            if ln.gp > 0 {
                out.insert(p.cid.clone(), ln.clone());
            }
        }
    }
    out
}

// --------------------------------------------------------------------------- orchestration

/// The preseason fetcher's TTL: game pages by game age, everything else the schedule TTL.
pub fn fetch_ttl(url: &str, as_of: NaiveDate, game_dates: &HashMap<i64, NaiveDate>) -> f64 {
    match gamecenter_re().captures(url) {
        Some(c) => {
            let d = c[1].parse::<i64>().ok().and_then(|id| game_dates.get(&id));
            match d {
                Some(d) if *d < as_of - Duration::days(RECENT_DAYS) => TTL_FINAL_GAME,
                _ => TTL_RECENT_GAME,
            }
        }
        None => TTL_SCHEDULE,
    }
}

/// True until WINDOW_DAYS after opening night (always when it is unknown).
pub fn in_window(ctx: &LeagueContext) -> bool {
    match ctx.season_start {
        None => true,
        Some(s) => ctx.as_of <= s + Duration::days(WINDOW_DAYS),
    }
}

/// `enrich_preseason`'s counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreseasonResult {
    pub games: i64,
    pub failed: i64,
    pub unproven: i64,
    pub with_games: i64,
    pub attached: i64,
}

/// Build preseason lines for the unproven skaters of `ctx` from every finished preseason game
/// before `ctx.as_of`; register them. `nhl` replaces the cached client (tests). Returns counts;
/// adds one source note. Fails only when the schedules cannot be read at all.
pub fn enrich_preseason(
    ctx: &mut LeagueContext,
    cache: Option<&Arc<HttpCache>>,
    nhl: Option<&NhlClient>,
    season: Option<i64>,
    teams: &[String],
) -> Result<PreseasonResult> {
    let season = season.unwrap_or_else(|| current_season(ctx.as_of));
    let mut res = PreseasonResult::default();
    if !in_window(ctx) {
        ctx.source_notes.push(format!(
            "Preseason: skipped ({} is more than {WINDOW_DAYS} days after opening night)",
            ctx.as_of
        ));
        return Ok(res);
    }
    let mut targets: Map<i64, usize> = Map::new();
    for (i, p) in ctx.all_players().iter().enumerate() {
        if let Some(nid) = p.nhl_id() {
            if !p.is_goalie() && is_unproven(p) {
                targets.insert(nid, i);
            }
        }
    }
    res.unproven = targets.len() as i64;
    if targets.is_empty() {
        return Ok(res);
    }
    let game_dates: Arc<Mutex<HashMap<i64, NaiveDate>>> = Arc::new(Mutex::new(HashMap::new()));
    let own: Option<NhlClient> = match nhl {
        Some(_) => None,
        None => {
            let Some(cache) = cache else { return Ok(res) };
            let cache = cache.clone();
            let gd = game_dates.clone();
            let as_of = ctx.as_of;
            Some(NhlClient::new(
                Box::new(move |url, params, _ttl| {
                    let ttl = fetch_ttl(url, as_of, &gd.lock().unwrap_or_else(|e| e.into_inner()));
                    cache.get_json(url, params, &[], &[], ttl)
                }),
                Some(season),
                ctx.as_of,
            ))
        }
    };
    let nhl: &NhlClient = nhl.or(own.as_ref()).expect("injected or built");
    let team_refs: Vec<&str> = teams.iter().map(String::as_str).collect();
    let games = nhl.preseason_games(Some(season), &team_refs, Some(ctx.as_of))?;
    game_dates.lock().unwrap_or_else(|e| e.into_inner()).extend(games.iter().map(|g| (g.game_id, g.date)));
    let mut lines: Vec<NhlSkaterGameLine> = Vec::new();
    // box scores are fetched concurrently (untracked client), consumed in game order
    let tasks: Vec<crate::enrich::Task<'_, Result<Vec<NhlSkaterGameLine>>>> =
        games.iter().map(|g| Box::new(move || nhl.game_skater_lines(g.game_id, true)) as crate::enrich::Task<'_, _>).collect();
    for got in crate::enrich::par_map(tasks) {
        match got {
            Ok(got) => {
                if !got.is_empty() {
                    res.games += 1;
                    lines.extend(got.into_iter().filter(|ln| targets.contains_key(&ln.player_id)));
                }
            }
            Err(_) => res.failed += 1,
        }
    }
    let agg = aggregate(&lines, season);
    register(ctx, agg.values());
    res.with_games = agg.len() as i64;
    {
        let mut players = crate::enrich::ctx_players_mut(ctx);
        for (pid, ln) in &agg {
            res.attached += attach(players[targets[pid]], ln) as i64;
        }
    }
    if !games.is_empty() {
        let failed = if res.failed > 0 { format!(", {} failed", res.failed) } else { String::new() };
        ctx.source_notes.push(format!(
            "Preseason (NHL box scores, gameType 1): {}/{} games read{failed}; {} of {} unproven skaters played (a weak signal: at most 25% of their baseline, reason PRESEASON)",
            res.games,
            games.len(),
            res.with_games,
            res.unproven
        ));
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    //! The enrich parts of `tests/test_preseason.py` (Ivar Stenberg's real 2026 preseason games).
    use super::*;
    use fm_http::Params;
    use serde_json::{json, Value};

    const STENBERG: i64 = 8486103;
    const SEASON: i64 = 20262027;
    const SCHED: &str = include_str!("../../../../tests/fixtures/nhl/preseason_club_schedule_SJS.json");
    const BOX: [(i64, &str, &str); 3] = [
        (
            2026010032,
            include_str!("../../../../tests/fixtures/nhl/preseason_boxscore_2026010032.json"),
            include_str!("../../../../tests/fixtures/nhl/preseason_landing_2026010032.json"),
        ),
        (
            2026010039,
            include_str!("../../../../tests/fixtures/nhl/preseason_boxscore_2026010039.json"),
            include_str!("../../../../tests/fixtures/nhl/preseason_landing_2026010039.json"),
        ),
        (
            2026010063,
            include_str!("../../../../tests/fixtures/nhl/preseason_boxscore_2026010063.json"),
            include_str!("../../../../tests/fixtures/nhl/preseason_landing_2026010063.json"),
        ),
    ];

    fn d(m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, m, day).unwrap()
    }

    /// `FakeFetch`: the SJS club schedule and the three fixture games; anything else fails.
    fn client(calls: Arc<Mutex<Vec<String>>>) -> NhlClient {
        NhlClient::new(
            Box::new(move |url: &str, _: &Params, _: f64| {
                calls.lock().unwrap().push(url.to_string());
                if url.contains("/club-schedule-season/SJS/") {
                    return Ok(serde_json::from_str(SCHED)?);
                }
                for (gid, b, l) in BOX {
                    if url.contains(&format!("/gamecenter/{gid}/boxscore")) {
                        return Ok(serde_json::from_str(b)?);
                    }
                    if url.contains(&format!("/gamecenter/{gid}/landing")) {
                        return Ok(serde_json::from_str(l)?);
                    }
                }
                Err(anyhow::anyhow!("KeyError: {url}"))
            }),
            Some(SEASON),
            d(9, 29),
        )
    }

    fn sk(cid: &str, nhl_id: i64, career: Option<i64>, prior_gp: Option<i64>) -> Value {
        let mut lines = json!({});
        if let Some(gp) = prior_gp {
            lines["prior"] = json!({"split": "prior", "gp": gp, "stats": {"GP": gp as f64}});
        }
        json!({"cid": cid, "name": cid, "name_norm": cid, "ids": {"nhl": nhl_id.to_string()}, "team": "SJS",
               "positions": ["LW"], "career_gp": career, "lines": lines})
    }

    fn ctx_with(fas: Vec<Value>, as_of: NaiveDate) -> LeagueContext {
        serde_json::from_value(json!({
            "provider": "test", "league_id": "1", "season": 2027, "name": "T",
            "scoring": {"kind": "points", "weights": {"G": 3.0}}, "roster_shape": {"F": 3},
            "teams": [{"team_id": "1", "name": "Mine", "owner_is_me": true, "slots": []}],
            "free_agents": fas, "matchup_period": 1, "as_of": as_of, "season_start": "2026-10-01",
        }))
        .unwrap()
    }

    fn teams() -> Vec<String> {
        vec!["SJS".into()]
    }

    #[test]
    fn aggregate_stenberg() {
        let c = client(Arc::new(Mutex::new(vec![])));
        let lines: Vec<NhlSkaterGameLine> = BOX.iter().flat_map(|(g, _, _)| c.game_skater_lines(*g, true).unwrap()).collect();
        let ln = &aggregate(&lines, SEASON)[&STENBERG];
        assert_eq!((ln.gp, ln.has_strength, ln.team.as_deref(), ln.last_date), (3, true, Some("SJS"), Some(d(9, 26))));
        let want = [("G", 3.0), ("A", 2.0), ("PTS", 5.0), ("SOG", 8.0), ("PPG", 1.0), ("PPA", 1.0), ("PPP", 2.0)];
        for (k, v) in want {
            assert_eq!(ln.stats[k], v, "{k}");
        }
        assert!((pts_per_game(ln) - 5.0 / 3.0).abs() < 1e-12);
        assert!((ln.toi_per_game().unwrap() - (1337.0 + 923.0 + 817.0) / 3.0 / 60.0).abs() < 1e-12);
        assert_eq!(summary(ln), "3 GP: 3 G, 2 A, 5 PTS, 8 SOG, 2 PPP, 17.1 TOI");
    }

    #[test]
    fn aggregate_drops_pp_assists_when_a_summary_is_missing() {
        let mk = |gid: i64, day: u32, toi: Option<f64>, strength: bool, stats: &[(&str, f64)]| NhlSkaterGameLine {
            player_id: 1,
            game_id: gid,
            game_type: Some(1),
            date: d(9, day),
            team: None,
            opponent: None,
            home: None,
            name: None,
            position: None,
            toi,
            shifts: None,
            has_strength: strength,
            stats: stats.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        };
        let a = mk(1, 20, Some(900.0), true, &[("GP", 1.0), ("G", 1.0), ("PPG", 1.0), ("PPA", 1.0), ("PPP", 2.0)]);
        let b = mk(2, 22, None, false, &[("GP", 1.0), ("G", 0.0), ("PPG", 0.0)]);
        let ln = &aggregate(&[a.clone(), b, a], SEASON)[&1];
        assert_eq!(ln.gp, 2);
        assert!(!ln.stats.contains_key("PPP") && !ln.stats.contains_key("PPA") && ln.stats["PPG"] == 1.0);
        assert!((ln.toi_per_game().unwrap() - 15.0).abs() < 1e-12);
    }

    #[test]
    fn enrich_registers_unproven_skaters_only() {
        let goalie = json!({"cid": "g", "name": "g", "name_norm": "g", "ids": {"nhl": "8485402"}, "team": "SJS", "positions": ["G"]});
        let mut ctx = ctx_with(vec![sk("stenberg", STENBERG, Some(0), None), sk("vet", 8484994, Some(300), Some(82)), goalie], d(9, 29));
        let c = client(Arc::new(Mutex::new(vec![])));
        let res = enrich_preseason(&mut ctx, None, Some(&c), Some(SEASON), &teams()).unwrap();
        assert_eq!(res, PreseasonResult { games: 3, failed: 1, unproven: 1, with_games: 1, attached: 0 });
        let lines = preseason_lines(&ctx, true);
        assert_eq!(lines.keys().collect::<Vec<_>>(), vec!["stenberg"]);
        assert_eq!(lines["stenberg"].stats["PTS"], 5.0);
        let vet = ctx.free_agents[1].clone();
        assert!(ctx.enrich.preseason_line(&vet, SEASON).is_none());
        assert!(!ctx.free_agents[0].lines.contains_key("preseason"));
        assert!(ctx
            .source_notes
            .iter()
            .any(|n| n.starts_with("Preseason (NHL box scores") && n.contains("3/4 games") && n.contains("1 failed")));
        // games on / after as_of are never read
        let mut ctx2 = ctx_with(vec![sk("s2", STENBERG, Some(0), None)], d(9, 24));
        assert_eq!(enrich_preseason(&mut ctx2, None, Some(&c), Some(SEASON), &teams()).unwrap().games, 1);
    }

    #[test]
    fn enrich_skips_after_the_window_and_without_targets() {
        let calls = Arc::new(Mutex::new(vec![]));
        let c = client(calls.clone());
        let mut late = ctx_with(vec![sk("s", STENBERG, Some(0), None)], d(11, 20));
        assert_eq!(enrich_preseason(&mut late, None, Some(&c), Some(SEASON), &teams()).unwrap().games, 0);
        assert!(calls.lock().unwrap().is_empty() && late.source_notes[0].contains("Preseason: skipped"));
        let mut none = ctx_with(vec![sk("vet", 8484994, Some(300), Some(82))], d(9, 29));
        assert_eq!(enrich_preseason(&mut none, None, Some(&c), Some(SEASON), &teams()).unwrap().unproven, 0);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn registry_is_keyed_by_season() {
        let mut ctx = ctx_with(vec![sk("stenberg", STENBERG, Some(0), None)], d(9, 29));
        let stats: Map<String, f64> = [("GP".to_string(), 4.0), ("PTS".to_string(), 8.0)].into_iter().collect();
        let line = PreseasonLine {
            nhl_id: STENBERG,
            season: 20252026,
            gp: 4,
            stats,
            toi_seconds: 0.0,
            toi_games: 0,
            has_strength: false,
            team: None,
            last_date: None,
            games: vec![],
        };
        register(&mut ctx, [&line]);
        assert!(preseason_lines(&ctx, true).is_empty());
    }

    #[test]
    fn cache_ttls_by_game_age() {
        let gd: HashMap<i64, NaiveDate> = [(1, d(9, 20)), (2, d(9, 28))].into_iter().collect();
        let ttl = |u: &str| fetch_ttl(u, d(9, 29), &gd);
        assert_eq!(ttl("https://api-web.nhle.com/v1/gamecenter/1/boxscore"), TTL_FINAL_GAME);
        assert_eq!(ttl("https://api-web.nhle.com/v1/gamecenter/2/boxscore"), TTL_RECENT_GAME);
        assert_eq!(ttl("https://api-web.nhle.com/v1/club-schedule-season/SJS/20262027"), TTL_SCHEDULE);
    }
}
