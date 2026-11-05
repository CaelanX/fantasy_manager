//! Rust port of `fantasy_manager/recommend/injuries.py`: injury alerts (status changes since the
//! last snapshot, IR moves + the best add, IR activations), hold-or-cut verdicts (incl. the
//! keeper-league `swap_values` / `protected_reason` rules) and the status-history store
//! (`status_history.db`, same schema as Python).
//!
//! Owner: work package `W7-injuries-flags-alerts-matchup`.
//!
//! Deviations from Python (behaviour-preserving):
//! * `history` is a [`History`] enum (a [`StatusHistory`] whose latest snapshot is read, or a plain
//!   map cid -> [`StatusSnapshot`]) instead of a duck-typed argument.
//! * The clock (`now`) and the schedule memo are explicit (they are passed to the waiver engine,
//!   which the IR-move step reruns on a copy of the context: `LeagueContext::clone`, cheap since
//!   `enrich` is an `Arc`).
//! * `StatusHistory.record(players, when=None)` takes `when: Option<f64>` (None / 0 = now).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{Datelike, NaiveDate};
use fm_core::models::{GainUnits, LeagueContext, Player, Reason, RecKind, Recommendation};
use fm_core::normalize::normalize_team;
use fm_core::values::{DynastyValue, Horizon, PlayerValue};
use fm_core::{Cid, CoreError, Map, PyDateTime};
use fm_pycompat::fmt::{fixed, percent};
use fm_pycompat::num::round_to;
use fm_pycompat::order::{first_max_by, first_min_by, cmp_f64, sort_desc_by};
use fm_pycompat::time::strftime_date;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::recommend::base::{
    cap_text, capped_positions, churn_text, droppable_players, dynasty_floats, dynasty_objects, keeper_k,
    moves_budget_reason, moves_left, no_moves_text, position_room, protected_reason, recently_added,
    roster_legal_after, shares_default_slot, swap_values, CHURN_EXEMPT_STATUSES, IR_SLOT_NAMES, RECENT_ADD_DAYS,
};
use crate::recommend::strength::{apply_ranks, apply_strength, IR_FLOOR};
use crate::recommend::waivers::{recommend_waivers, WaiverOpts, DYNASTY_MIN_RATIO};
use crate::valuation::adjust::{games_remaining, season_availability};
use crate::valuation::schedule::ScheduleMemo;

pub const IR_ELIGIBLE: [&str; 3] = ["ir", "ltir", "out"];
pub const HOLD_CUT_STATUSES: [&str; 4] = ["out", "ir", "ltir", "suspended"];
/// a cut must beat holding by this share of the hold points (more when the return date is a guess)
pub const CUT_EDGE: f64 = 0.10;
pub const CUT_EDGE_GUESSED: f64 = 0.25;
/// a hold verdict is advice not to act: shown, never ranked above real moves
pub const HOLD_STRENGTH: f64 = 1.0;

/// `SEVERITY.get(status, 0)`.
pub fn severity(status: &str) -> i64 {
    match status {
        "dtd" => 1,
        "suspended" | "out" => 2,
        "ir" => 3,
        "ltir" => 4,
        _ => 0,
    }
}

// --------------------------------------------------------------------------- status history

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub cid: String,
    pub status: String,
    #[serde(default)]
    pub note: Option<String>,
    pub seen_at: f64,
}

/// Append-only status log (a row is written only when a player's status changes).
pub struct StatusHistory {
    pub data_dir: PathBuf,
    db: Mutex<Connection>,
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

impl StatusHistory {
    pub fn new(data_dir: impl AsRef<Path>) -> anyhow::Result<StatusHistory> {
        let data_dir = data_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&data_dir)?;
        let db = Connection::open(data_dir.join("status_history.db"))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS status_history ( cid TEXT NOT NULL, status TEXT NOT NULL, note TEXT, seen_at REAL NOT NULL);\
             CREATE INDEX IF NOT EXISTS ix_status_cid ON status_history(cid, seen_at);",
        )?;
        Ok(StatusHistory { data_dir, db: Mutex::new(db) })
    }

    fn rows(&self, sql: &str) -> anyhow::Result<Vec<StatusSnapshot>> {
        let db = self.db.lock().unwrap_or_else(|e| e.into_inner());
        let mut st = db.prepare(sql)?;
        let rows = st.query_map([], |r| {
            Ok(StatusSnapshot { cid: r.get(0)?, status: r.get(1)?, note: r.get(2)?, seen_at: r.get(3)? })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Latest snapshot per player.
    pub fn last(&self) -> anyhow::Result<Map<String, StatusSnapshot>> {
        let rows = self.rows(
            "SELECT h.cid, h.status, h.note, h.seen_at FROM status_history h \
             JOIN (SELECT cid, MAX(seen_at) m FROM status_history GROUP BY cid) x \
             ON h.cid = x.cid AND h.seen_at = x.m",
        )?;
        Ok(rows.into_iter().map(|s| (s.cid.clone(), s)).collect())
    }

    /// Every recorded status change per player, oldest first.
    pub fn by_player(&self) -> anyhow::Result<Map<String, Vec<StatusSnapshot>>> {
        let rows = self.rows("SELECT cid, status, note, seen_at FROM status_history ORDER BY cid, seen_at")?;
        let mut out: Map<String, Vec<StatusSnapshot>> = Map::new();
        for s in rows {
            out.entry(s.cid.clone()).or_default().push(s);
        }
        Ok(out)
    }

    /// Store statuses that differ from the last snapshot. Returns rows written.
    pub fn record<'a>(&self, players: impl IntoIterator<Item = &'a Player>, when: Option<f64>) -> anyhow::Result<usize> {
        let when = when.filter(|w| *w != 0.0).unwrap_or_else(now_epoch);
        let prev = self.last()?;
        // {p.cid: p for p in players}: first position, last value
        let mut uniq: Map<&str, &Player> = Map::new();
        for p in players {
            uniq.insert(p.cid.as_str(), p);
        }
        let mut rows: Vec<(&str, &str, Option<&str>)> = Vec::new();
        for p in uniq.values() {
            let note = p.status_note.as_deref().filter(|s| !s.is_empty());
            let changed = match prev.get(&p.cid) {
                None => true,
                Some(old) => old.status != p.status.as_str() || old.note.as_deref().filter(|s| !s.is_empty()) != note,
            };
            if changed {
                rows.push((p.cid.as_str(), p.status.as_str(), p.status_note.as_deref()));
            }
        }
        if !rows.is_empty() {
            let mut db = self.db.lock().unwrap_or_else(|e| e.into_inner());
            let tx = db.transaction()?;
            {
                let mut st = tx.prepare("INSERT INTO status_history(cid, status, note, seen_at) VALUES (?,?,?,?)")?;
                for (cid, status, note) in &rows {
                    st.execute(params![cid, status, note, when])?;
                }
            }
            tx.commit()?;
        }
        Ok(rows.len())
    }
}

/// `history` argument of [`recommend_injuries`]: a StatusHistory (its latest snapshot is read,
/// not written), a mapping cid -> snapshot, or nothing.
pub enum History<'a> {
    Store(&'a StatusHistory),
    Snapshots(&'a Map<String, StatusSnapshot>),
    None,
}

// --------------------------------------------------------------------------- helpers

fn status_text(p: &Player) -> String {
    match p.status_note.as_deref() {
        Some(n) if !n.is_empty() => format!("{} ({n})", p.status.as_str()),
        _ => p.status.as_str().to_string(),
    }
}

fn pos_text(p: &Player) -> String {
    p.positions.iter().filter(|x| *x != "F").cloned().collect::<Vec<_>>().join("/")
}

fn healthy_like(p: &Player) -> bool {
    p.status.is_in(&["healthy", "unknown", "dtd"])
}

/// (best add, the waiver rec it came from or None for the open-spot fallback). Only no-drop adds
/// qualify. (None, None) when no moves are left this period.
fn best_add_rec<'c>(
    ctx: &'c LeagueContext,
    values: &Map<Cid, PlayerValue>,
    injured: &Player,
    horizon: Horizon,
    now: &PyDateTime,
    memo: &ScheduleMemo,
) -> Result<(Option<Player>, Option<Recommendation>), CoreError> {
    if moves_left(ctx) == Some(0) {
        return Ok((None, None));
    }
    let opts = WaiverOpts { limit: 10, horizon, ..WaiverOpts::default() };
    for rec in recommend_waivers(ctx, values, &opts, None, now, memo)? {
        if !rec.add.is_empty() && rec.drop.is_empty() && shares_default_slot(&rec.add[0], injured) {
            return Ok((Some(rec.add[0].clone()), Some(rec)));
        }
    }
    // the waiver engine demands a gain over a drop; with an open roster spot any positive VORP helps
    let team = ctx.my_team()?;
    let pool: Vec<&'c Player> = ctx
        .free_agents
        .iter()
        .filter(|fa| {
            values.contains_key(&fa.cid)
                && shares_default_slot(fa, injured)
                && healthy_like(fa)
                && position_room(team, ctx, fa) > 0
        })
        .collect();
    let positive: Vec<&Player> = pool.iter().copied().filter(|fa| values[&fa.cid].vorp_for(horizon) > 0.0).collect();
    let pool = if positive.is_empty() { pool } else { positive };
    let best = first_max_by(pool, |a, b| cmp_f64(values[&a.cid].fpg_for(horizon), values[&b.cid].fpg_for(horizon)));
    Ok((best.cloned(), None))
}

/// Gain of an add into an open spot (no drop): his projected week or his FPG.
fn add_gain(add: &Player, values: &Map<Cid, PlayerValue>, horizon: Horizon) -> (f64, GainUnits, Option<i64>) {
    let av = &values[&add.cid];
    if horizon == Horizon::Week {
        if let Some(pw) = av.proj_week {
            return (pw, GainUnits::WeekPts, Some(7));
        }
    }
    (av.fpg_for(horizon), GainUnits::SeasonFpg, if horizon == Horizon::Week { Some(7) } else { None })
}

/// How long `p` is out over the rest of the season and what he is worth when back.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Absence {
    /// his team's games left
    pub remaining: f64,
    pub missed: f64,
    /// season value per game once healthy
    pub healthy_fpg: f64,
    /// "back ~Nov 21" / "return unknown"
    pub back: String,
    pub source: String,
    /// the length is assumed (no timetable in the note)
    pub guessed: bool,
}

impl Absence {
    pub fn hold_pts(&self) -> f64 {
        self.healthy_fpg * (self.remaining - self.missed).max(0.0)
    }
}

pub fn absence(ctx: &LeagueContext, p: &Player, pv: &PlayerValue) -> Absence {
    let team = normalize_team(p.team.as_deref()).unwrap_or_default();
    let team_dates: Option<&[NaiveDate]> = ctx.schedule.get(&team).map(|v| v.as_slice());
    let (a_season, est) =
        season_availability(p.status.as_str(), p.status_note.as_deref(), ctx.as_of, team_dates, ctx.season_start, None);
    let healthy = if a_season > 0.0 { pv.fpg_season / a_season } else { pv.fpg };
    let Some(est) = est else {
        let remaining = games_remaining(ctx.as_of, team_dates, ctx.season_start);
        return Absence {
            remaining,
            missed: remaining * (1.0 - a_season),
            healthy_fpg: healthy,
            back: "return unknown".into(),
            source: format!("no timetable, flat {} value x{}", p.status.as_str(), fixed(a_season, 2, false)),
            guessed: true,
        };
    };
    let back = match est.return_date {
        Some(r) => format!("back ~{} {}", strftime_date(r, "%b"), r.day()),
        None => "out for the season".into(),
    };
    Absence {
        remaining: est.games_remaining,
        missed: est.games_missed,
        healthy_fpg: healthy,
        back,
        guessed: est.source.contains("assumed"),
        source: est.source,
    }
}

/// Best free agent (season value) on an NHL team who can take `p`'s roster spot.
fn replacement<'c>(ctx: &'c LeagueContext, values: &Map<Cid, PlayerValue>, p: &Player) -> Result<Option<&'c Player>, CoreError> {
    let team = ctx.my_team()?;
    let pool = ctx.free_agents.iter().filter(|fa| {
        values.contains_key(&fa.cid)
            && normalize_team(fa.team.as_deref()).is_some()
            && shares_default_slot(fa, p)
            && healthy_like(fa)
            && roster_legal_after(team, ctx, &[fa], &[p], &[]).0
    });
    Ok(first_max_by(pool, |a, b| cmp_f64(values[&a.cid].fpg_season, values[&b.cid].fpg_season)))
}

fn estimate_reason(p: &Player, ab: &Absence) -> Reason {
    Reason {
        code: "RETURN_ESTIMATE".into(),
        text: format!(
            "{}: {}, ~{} of {} games missed ({}{}",
            p.name,
            ab.back,
            fixed(ab.missed, 0, false),
            fixed(ab.remaining, 0, false),
            ab.source,
            if ab.guessed { ", a guess)" } else { ")" }
        ),
        value: Some(round_to(ab.missed, 1)),
        baseline: Some(round_to(ab.remaining, 1)),
    }
}

fn hold_rec(p: &Player, ab: &Absence, title: String, reasons: Vec<Reason>) -> Recommendation {
    let mut r = Recommendation::new(RecKind::Injury, 0.0, title);
    r.strength = Some(HOLD_STRENGTH);
    r.reasons = std::iter::once(estimate_reason(p, ab)).chain(reasons).collect();
    r
}

/// Hold-or-cut verdicts (module docstring) for `players` (injured, blocking an active spot) and
/// `in_ir` (injured, already in an IR slot: holding is free).
pub fn hold_or_cut(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    players: &[&Player],
    in_ir: &[&Player],
    dynasty_values: Option<&Map<Cid, DynastyValue>>,
) -> Result<Vec<Recommendation>, CoreError> {
    let mut recs: Vec<Recommendation> = Vec::new();
    for p in in_ir {
        let Some(pv) = values.get(&p.cid) else { continue };
        let ab = absence(ctx, p, pv);
        let hp = ab.hold_pts();
        recs.push(hold_rec(
            p,
            &ab,
            format!("Hold {} in IR ({}): it costs no roster spot", p.name, ab.back),
            vec![Reason {
                code: "HOLD_VERDICT".into(),
                text: format!(
                    "{} sits in an IR slot, so keeping him costs nothing; ~{} pts after his return",
                    p.name,
                    fixed(hp, 0, false)
                ),
                value: Some(round_to(hp, 1)),
                baseline: None,
            }],
        ));
    }
    let dobj = dynasty_objects(ctx, values, dynasty_values);
    let dobj_ref: Option<&Map<Cid, DynastyValue>> = dobj.as_deref();
    let dyn_ = dynasty_floats(dobj_ref);
    let keepers = if dyn_.is_some() { keeper_k(ctx, dobj_ref) } else { None };
    let my_team = ctx.my_team()?;
    for p in players {
        let pv = values.get(&p.cid);
        let fa = replacement(ctx, values, p)?;
        let (Some(pv), Some(fa)) = (pv, fa) else { continue };
        let ab = absence(ctx, p, pv);
        let r = values[&fa.cid].fpg_season;
        let hold = ab.hold_pts();
        let cut = r * ab.remaining;
        let edge = if ab.guessed { CUT_EDGE_GUESSED } else { CUT_EDGE };
        // games missed at which cutting starts to win by `edge`: h (R - n)(1 + edge) = r R
        let h = ab.healthy_fpg;
        let breakeven = if h > 0.0 { ab.remaining - r * ab.remaining / (h * (1.0 + edge)) } else { 0.0 };
        let fa_team = fa.team.as_deref().unwrap_or("None");
        let mut reasons = vec![
            Reason {
                code: "HOLD_PTS".into(),
                text: format!(
                    "Hold: {} FPG x {} games after his return = {} pts (his roster spot is dead until then: no IR slot free)",
                    fixed(h, 2, false),
                    fixed((ab.remaining - ab.missed).max(0.0), 0, false),
                    fixed(hold, 0, false)
                ),
                value: Some(round_to(hold, 1)),
                baseline: None,
            },
            Reason {
                code: "CUT_PTS".into(),
                text: format!(
                    "Cut: {} ({}, {}) {} FPG x {} games = {} pts",
                    fa.name,
                    pos_text(fa),
                    fa_team,
                    fixed(r, 2, false),
                    fixed(ab.remaining, 0, false),
                    fixed(cut, 0, false)
                ),
                value: Some(round_to(cut, 1)),
                baseline: None,
            },
            Reason {
                code: "BREAK_EVEN".into(),
                text: if 0.0 < breakeven && breakeven < ab.remaining {
                    format!("Cut only if he misses more than ~{} games", fixed(breakeven, 0, false))
                } else if breakeven <= 0.0 {
                    format!("{} out-produces him even healthy: cut", fa.name)
                } else {
                    "Holding wins however long he is out".to_string()
                },
                value: Some(round_to(breakeven, 1)),
                baseline: Some(round_to(ab.missed, 1)),
            },
        ];
        let mut protect: Option<String> = None;
        if let Some(dmap) = dyn_.as_ref() {
            let (d_fa, d_p) = if keepers.is_none() {
                (dmap.get(&fa.cid).copied().unwrap_or(0.0), dmap.get(&p.cid).copied().unwrap_or(0.0))
            } else {
                // keeper league: exact contributions to my roster (this season + my top-K keepers)
                swap_values(ctx, dobj_ref, my_team.players_iter(), Some(fa), Some(p))
            };
            if let Some(why) = protected_reason(p, ctx, dynasty_values, dobj_ref) {
                protect = Some(format!("protected dynasty asset ({why})"));
            } else if d_p > DYNASTY_MIN_RATIO * d_fa {
                protect = Some(format!("long-term value {} vs {} {}", fixed(d_p, 1, false), fa.name, fixed(d_fa, 1, false)));
            }
            let label = match keepers {
                None => "Dynasty value".to_string(),
                Some(k) => format!("Value (this season + top-{k} keepers)"),
            };
            let tail = if d_p > d_fa { format!(": cutting gives up {}", fixed(d_p - d_fa, 1, false)) } else { String::new() };
            reasons.push(Reason {
                code: "DYNASTY_COST".into(),
                text: format!("{label} {} {} vs {} {}{tail}", p.name, fixed(d_p, 1, false), fa.name, fixed(d_fa, 1, false)),
                value: Some(d_fa - d_p),
                baseline: Some(d_p),
            });
        }
        if cut <= hold * (1.0 + edge) || protect.is_some() {
            let why_hold = if protect.is_some() && cut > hold * (1.0 + edge) {
                format!("keep him: {}", protect.as_deref().unwrap_or(""))
            } else if cut > hold {
                format!(
                    "close call, cutting for {} gains only {} pts (under the {} margin{})",
                    fa.name,
                    fixed(cut - hold, 0, false),
                    percent(edge, 0, false),
                    if ab.guessed { " for a guessed return" } else { "" }
                )
            } else {
                format!("{} pts vs cutting for {}", fixed(hold - cut, 0, true), fa.name)
            };
            let title = format!("Hold {} through his injury ({}): {why_hold}", p.name, ab.back);
            recs.push(hold_rec(p, &ab, title, reasons));
            continue;
        }
        if let Some(budget) = moves_budget_reason(ctx) {
            reasons.push(budget);
        }
        if moves_left(ctx) == Some(0) {
            reasons.push(Reason {
                code: "NO_MOVES".into(),
                text: no_moves_text(ctx) + ": make the cut when moves reset",
                value: Some(0.0),
                baseline: None,
            });
        }
        let gain = if ab.remaining > 0.0 { (cut - hold) / ab.remaining } else { 0.0 };
        let mut rec = Recommendation::new(
            RecKind::Injury,
            gain + 0.5,
            format!("Cut {}, add {}: {} pts rest of season", p.name, fa.name, fixed(cut - hold, 0, true)),
        );
        rec.add = vec![fa.clone()];
        rec.drop = vec![(*p).clone()];
        rec.predicted_gain = Some(gain);
        rec.gain_units = Some(GainUnits::SeasonFpg);
        rec.reasons = std::iter::once(estimate_reason(p, &ab)).chain(reasons).collect();
        recs.push(rec);
    }
    Ok(recs)
}

/// Worsened statuses since the last snapshot, IR moves (+ best add) and IR activations. Call
/// [`StatusHistory::record`] afterwards to advance the baseline.
pub fn recommend_injuries(
    ctx: &LeagueContext,
    values: &Map<Cid, PlayerValue>,
    history: History<'_>,
    horizon: Horizon,
    dynasty_values: Option<&Map<Cid, DynastyValue>>,
    now: &PyDateTime,
    memo: &ScheduleMemo,
) -> anyhow::Result<Vec<Recommendation>> {
    let owned;
    let prev: &Map<String, StatusSnapshot> = match history {
        History::Store(h) => {
            owned = h.last()?;
            &owned
        }
        History::Snapshots(m) => m,
        History::None => {
            owned = Map::new();
            &owned
        }
    };
    let team = ctx.my_team()?;
    let mut recs: Vec<Recommendation> = Vec::new();
    let fpg = |p: &Player| values.get(&p.cid).map(|pv| pv.fpg).unwrap_or(0.0);

    // 1. newly worsened statuses (my roster)
    for s in &team.slots {
        let Some(p) = s.player.as_ref() else { continue };
        let Some(old) = prev.get(&p.cid) else { continue };
        let (sn, so) = (severity(p.status.as_str()), severity(&old.status));
        if sn <= so {
            continue;
        }
        let mut r = Recommendation::new(
            RecKind::Injury,
            fpg(p) * (1.0 + sn as f64),
            format!("{} now {} (was {})", p.name, p.status.as_str().to_uppercase(), old.status),
        );
        r.subjects = vec![p.clone()];
        r.reasons = vec![Reason {
            code: "STATUS_CHANGE".into(),
            text: format!("{}: {} -> {}", p.name, old.status, status_text(p)),
            value: Some(sn as f64),
            baseline: Some(so as f64),
        }];
        recs.push(r);
    }

    // 2. move to IR when a slot is free
    let ir_cap = ctx.roster_shape.get("IR").copied().unwrap_or(0);
    let ir_used = team.slots.iter().filter(|s| s.slot == "IR" && s.player.is_some()).count() as i64;
    let free_ir = (ir_cap - ir_used).max(0);
    let mut candidates: Vec<&Player> = team
        .slots
        .iter()
        .filter(|s| s.slot != "IR")
        .filter_map(|s| s.player.as_ref())
        .filter(|p| p.status.is_in(&IR_ELIGIBLE))
        .collect();
    candidates.sort_by(|a, b| {
        severity(b.status.as_str()).cmp(&severity(a.status.as_str())).then_with(|| cmp_f64(-fpg(a), -fpg(b)))
    });
    let moving_list: Vec<&Player> = candidates.iter().copied().take(free_ir as usize).collect();
    for p in &moving_list {
        let mut moved = ctx.clone();
        if let Some(i) = moved.my_team_index() {
            for slot in moved.teams[i].slots.iter_mut() {
                if slot.player.as_ref().is_some_and(|x| x.cid == p.cid) {
                    slot.slot = "IR".into();
                    slot.starting = false;
                }
            }
        }
        let (add, wrec) = best_add_rec(&moved, values, p, horizon, now, memo)?;
        let mt = moved.my_team()?;
        let mut reasons = vec![
            Reason::new("STATUS", format!("{} is {}", p.name, status_text(p))),
            Reason::new("IR_SLOT", format!("{free_ir} of {ir_cap} IR slot(s) free")).with_value(Some(free_ir as f64)),
        ];
        if p.status.as_str() == "out" {
            reasons.push(Reason::new("IR_RULES", "Status is OUT, not IR: check your league allows it in IR"));
        }
        let capped = capped_positions(mt, &moved, p);
        if add.is_none() && !capped.is_empty() {
            reasons.push(Reason::new(
                "POSITION_CAP",
                format!(
                    "No {} pickup without a drop: {} reached (players in IR still count)",
                    capped.join("/"),
                    capped.iter().map(|k| cap_text(&moved, k, None)).collect::<Vec<_>>().join(", ")
                ),
            ));
        }
        let mut title = format!("Move {} to IR", p.name);
        if let Some(a) = add.as_ref() {
            let av = &values[&a.cid];
            title += &format!(" and add {}", a.name);
            reasons.push(
                Reason::new(
                    "WAIVER_ADD",
                    format!(
                        "{} ({}, {}): {} FPG, no drop needed",
                        a.name,
                        pos_text(a),
                        a.team.as_deref().filter(|t| !t.is_empty()).unwrap_or("FA"),
                        fixed(av.fpg_for(horizon), 2, false)
                    ),
                )
                .with_value(Some(av.fpg_for(horizon))),
            );
        }
        if add.is_some() {
            if let Some(budget) = moves_budget_reason(&moved) {
                reasons.push(budget);
            }
        } else if moves_left(&moved) == Some(0) {
            reasons.push(
                Reason::new("NO_MOVES", no_moves_text(&moved) + " (the IR move itself is free)").with_value(Some(0.0)),
            );
        }
        let (gain, units, days) = match (&wrec, &add) {
            (Some(w), _) => (w.predicted_gain, w.gain_units, w.horizon_days),
            (None, Some(a)) => {
                let (g, u, d) = add_gain(a, values, horizon);
                (Some(g), Some(u), d)
            }
            (None, None) => (None, None, None),
        };
        let score = add.as_ref().map(|a| values[&a.cid].fpg_for(horizon)).unwrap_or(0.0) + 0.5;
        let mut rec = Recommendation::new(RecKind::Injury, score, title);
        rec.add = add.iter().cloned().collect();
        rec.reasons = reasons;
        rec.subjects = vec![(*p).clone()];
        rec.predicted_gain = gain;
        rec.gain_units = units;
        rec.horizon_days = days;
        if let Some(ws) = wrec.as_ref().and_then(|w| w.strength) {
            rec.strength = Some(round_to(IR_FLOOR.max(ws), 2)); // inherit the waiver add's strength
        }
        if let Some(pv) = values.get(&p.cid) {
            let ab = absence(ctx, p, pv);
            let hp = ab.hold_pts();
            rec.reasons.push(Reason {
                code: "HOLD_VERDICT".into(),
                text: format!(
                    "Hold, don't cut: with an IR slot free keeping {} costs no roster spot ({}, ~{} pts after his return)",
                    p.name,
                    ab.back,
                    fixed(hp, 0, false)
                ),
                value: Some(round_to(hp, 1)),
                baseline: None,
            });
        }
        recs.push(rec);
    }

    // 2b. hold or cut: injured players with no free IR slot, and those already in one
    let moving: Vec<&str> = moving_list.iter().map(|p| p.cid.as_str()).collect();
    let in_ir_slot = |slot: &str| IR_SLOT_NAMES.contains(&slot);
    let mut blocking: Vec<&Player> = team
        .slots
        .iter()
        .filter(|s| !in_ir_slot(&s.slot))
        .filter_map(|s| s.player.as_ref())
        .filter(|p| p.status.is_in(&HOLD_CUT_STATUSES) && !moving.contains(&p.cid.as_str()))
        .collect();
    let stashed: Vec<&Player> = team
        .slots
        .iter()
        .filter(|s| in_ir_slot(&s.slot))
        .filter_map(|s| s.player.as_ref())
        .filter(|p| p.status.is_in(&HOLD_CUT_STATUSES))
        .collect();
    // an IR slot held by a healthy / day-to-day player can be swapped: holding is free there too
    let swappable: Vec<&Player> = team
        .slots
        .iter()
        .filter(|s| in_ir_slot(&s.slot))
        .filter_map(|s| s.player.as_ref())
        .filter(|p| p.status.is_in(&["healthy", "dtd"]))
        .collect();
    blocking.sort_by(|a, b| cmp_f64(-fpg(a), -fpg(b)));
    for (p, back) in blocking.iter().zip(swappable.iter()) {
        let Some(pv) = values.get(&p.cid) else { continue };
        let ab = absence(ctx, p, pv);
        let hp = ab.hold_pts();
        recs.push(hold_rec(
            p,
            &ab,
            format!(
                "Hold {} ({}): activate {} ({}) and move {} into his IR slot",
                p.name,
                ab.back,
                back.name,
                back.status.as_str(),
                p.name
            ),
            vec![Reason {
                code: "HOLD_VERDICT".into(),
                text: format!("Swapping IR slots makes keeping {} free; ~{} pts after his return", p.name, fixed(hp, 0, false)),
                value: Some(round_to(hp, 1)),
                baseline: None,
            }],
        ));
    }
    let rest: Vec<&Player> = blocking.iter().copied().skip(swappable.len()).collect();
    recs.extend(hold_or_cut(ctx, values, &rest, &stashed, dynasty_values)?);

    // 3. activate healthy players from IR
    let mut droppable: Vec<&Player> = droppable_players(team).into_iter().filter(|d| values.contains_key(&d.cid)).collect();
    for s in &team.slots {
        let Some(p) = s.player.as_ref() else { continue };
        if s.slot != "IR" || p.status.as_str() != "healthy" {
            continue;
        }
        let mut reasons = vec![Reason::new("STATUS", format!("{} is healthy but in an IR slot", p.name))];
        let mut drop: Option<&Player> = None;
        let filled = team.slots.iter().filter(|x| x.player.is_some() && x.slot != "IR").count() as i64;
        let cap: i64 = ctx.roster_shape.iter().filter(|(k, _)| k.as_str() != "IR").map(|(_, n)| *n).sum();
        let full = filled >= cap;
        // churn guard: never drop a player I just added (unless he is hurt) to activate someone
        let pool: Vec<&Player> = droppable
            .iter()
            .copied()
            .filter(|d| !recently_added(ctx, d, RECENT_ADD_DAYS) || CHURN_EXEMPT_STATUSES.contains(&d.status.as_str()))
            .collect();
        if full && pool.len() < droppable.len() {
            let kept: Vec<String> = droppable
                .iter()
                .filter(|d| !pool.iter().any(|x| x.cid == d.cid))
                .map(|d| format!("{} ({})", d.name, churn_text(ctx, d)))
                .collect();
            reasons.push(Reason::new("CHURN_GUARD", format!("Not dropping {}: just added", kept.join(", "))));
        }
        droppable = pool;
        if full && !droppable.is_empty() {
            let d = first_min_by(droppable.iter().copied(), |a, b| {
                cmp_f64(values[&a.cid].fpg_for(Horizon::Season), values[&b.cid].fpg_for(Horizon::Season))
            })
            .expect("non-empty");
            let dv = values[&d.cid].fpg_for(Horizon::Season);
            reasons.push(
                Reason::new("DROP", format!("Roster is full: weakest is {} ({} FPG)", d.name, fixed(dv, 2, false)))
                    .with_value(Some(dv)),
            );
            drop = Some(d);
        }
        let gain = values
            .get(&p.cid)
            .map(|pv| pv.fpg_season - drop.map(|d| values[&d.cid].fpg_season).unwrap_or(0.0));
        let mut r = Recommendation::new(RecKind::Injury, fpg(p) + 1.0, format!("Activate {} from IR", p.name));
        r.add = vec![p.clone()];
        r.drop = drop.into_iter().cloned().collect();
        r.reasons = reasons;
        r.predicted_gain = gain;
        r.gain_units = gain.map(|_| GainUnits::SeasonFpg);
        recs.push(r);
    }
    sort_desc_by(&mut recs, |a, b| cmp_f64(a.score, b.score));
    apply_strength(&mut recs);
    apply_ranks(&mut recs);
    Ok(recs)
}

#[cfg(test)]
mod tests {
    //! Port of tests/test_injury_recs.py (intent).
    use super::*;
    use crate::recommend::lineup::tests::now_at;
    use crate::scoring::PointsScoring;
    use crate::valuation::valuate::valuate_league;
    use fm_core::models::{FantasyTeam, RosterSlot, Status};
    use serde_json::json;

    pub(crate) fn mk(cid: &str, pos: &[&str], goals_pg: f64, status: &str) -> Player {
        let lines = json!({"projected": {"split": "projected", "gp": 80, "stats": {"G": goals_pg * 80.0, "GP": 80.0}}});
        serde_json::from_value(json!({"cid": cid, "name": cid, "name_norm": cid, "ids": {}, "team": "EDM",
                                      "positions": pos, "status": status, "lines": lines}))
        .unwrap()
    }

    fn ctx_for(mine: &[(&str, &Player)], fas: Vec<Player>, shape: &[(&str, i64)]) -> LeagueContext {
        let slots: Vec<RosterSlot> = mine
            .iter()
            .map(|(s, p)| RosterSlot { slot: s.to_string(), player: Some((*p).clone()), starting: !["BN", "IR"].contains(s) })
            .collect();
        let sh: Map<String, i64> = shape.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        let t = FantasyTeam { team_id: "1".into(), name: "me".into(), owner_is_me: true, slots, record: None };
        serde_json::from_value(json!({"provider": "t", "league_id": "1", "season": 2027, "name": "T",
            "scoring": {"kind": "points", "weights": {"G": 1.0}}, "roster_shape": sh, "teams": [t],
            "free_agents": fas, "matchup_period": 1, "as_of": "2026-10-07"}))
        .unwrap()
    }

    fn valuate(ctx: &LeagueContext) -> Map<Cid, PlayerValue> {
        let mut c = ctx.clone();
        let w: Map<String, f64> = [("G".to_string(), 1.0)].into_iter().collect();
        valuate_league(&mut c, &PointsScoring::new(w, None))
    }

    fn snap(xs: &[(&str, &str)]) -> Map<String, StatusSnapshot> {
        xs.iter()
            .map(|(c, s)| (c.to_string(), StatusSnapshot { cid: c.to_string(), status: s.to_string(), note: None, seen_at: 1.0 }))
            .collect()
    }

    fn run_with(ctx: &LeagueContext, vals: &Map<Cid, PlayerValue>, prev: &Map<String, StatusSnapshot>, dynv: Option<&Map<Cid, DynastyValue>>) -> Vec<Recommendation> {
        recommend_injuries(ctx, vals, History::Snapshots(prev), Horizon::Week, dynv, &now_at(12, 0, 7), &ScheduleMemo::new(None)).unwrap()
    }

    fn run(ctx: &LeagueContext, vals: &Map<Cid, PlayerValue>) -> Vec<Recommendation> {
        run_with(ctx, vals, &Map::new(), None)
    }

    fn verdicts(recs: Vec<Recommendation>) -> Vec<Recommendation> {
        recs.into_iter().filter(|r| r.title.starts_with("Hold") || r.title.starts_with("Cut")).collect()
    }

    fn code<'a>(r: &'a Recommendation, c: &str) -> &'a Reason {
        r.reasons.iter().find(|x| x.code == c).unwrap_or_else(|| panic!("no {c} in {:?}", r.reasons))
    }

    #[test]
    fn status_history_records_only_changes() {
        let dir = tempfile::tempdir().unwrap();
        let h = StatusHistory::new(dir.path()).unwrap();
        let a = mk("a", &["C"], 0.5, "healthy");
        let mut b = mk("b", &["D"], 0.3, "dtd");
        assert!(h.last().unwrap().is_empty());
        assert_eq!(h.record([&a, &b], Some(100.0)).unwrap(), 2);
        assert_eq!(h.record([&a, &b], Some(200.0)).unwrap(), 0);
        b.status = Status::Out;
        assert_eq!(h.record([&a, &b], Some(300.0)).unwrap(), 1);
        let last = h.last().unwrap();
        assert_eq!(last["b"].status, "out");
        assert_eq!(last["b"].seen_at, 300.0);
        assert_eq!(last["a"].seen_at, 100.0);
        assert_eq!(h.by_player().unwrap()["b"].len(), 2);
        drop(h);
        assert_eq!(StatusHistory::new(dir.path()).unwrap().last().unwrap()["b"].status, "out");
    }

    #[test]
    fn worsened_status_since_snapshot() {
        let worse = mk("worse", &["C"], 0.8, "out");
        let better = mk("better", &["LW"], 0.5, "dtd");
        let same = mk("same", &["D"], 0.3, "dtd");
        let ctx = ctx_for(&[("C", &worse), ("LW", &better), ("D", &same)], vec![], &[("C", 1), ("LW", 1), ("D", 1)]);
        let vals = valuate(&ctx);
        let recs = run_with(&ctx, &vals, &snap(&[("worse", "dtd"), ("better", "ir"), ("same", "dtd")]), None);
        let titles: Vec<&str> = recs.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, vec!["worse now OUT (was dtd)"]);
        let r = &recs[0];
        assert_eq!(r.reasons[0].code, "STATUS_CHANGE");
        assert_eq!((r.reasons[0].value, r.reasons[0].baseline), (Some(2.0), Some(1.0)));
        assert!(!run(&ctx, &vals).iter().any(|x| x.title.contains("now")));
    }

    #[test]
    fn move_to_ir_chains_best_waiver_add() {
        let hurt = mk("hurt", &["C"], 1.0, "ir");
        let other = mk("other", &["LW"], 0.4, "healthy");
        let fas = vec![mk("fa_c", &["C"], 0.6, "healthy"), mk("fa_d", &["D"], 0.9, "healthy")];
        let ctx = ctx_for(&[("C", &hurt), ("LW", &other)], fas, &[("C", 1), ("LW", 1), ("BN", 1), ("IR", 1)]);
        let vals = valuate(&ctx);
        let dir = tempfile::tempdir().unwrap();
        let h = StatusHistory::new(dir.path()).unwrap();
        let recs = recommend_injuries(&ctx, &vals, History::Store(&h), Horizon::Week, None, &now_at(12, 0, 7), &ScheduleMemo::new(None)).unwrap();
        let mv = recs.iter().find(|r| r.title.starts_with("Move hurt to IR")).unwrap();
        assert_eq!(mv.title, "Move hurt to IR and add fa_c");
        assert_eq!(mv.add[0].cid, "fa_c");
        assert!(mv.drop.is_empty());
        let codes: Vec<&str> = mv.reasons.iter().map(|x| x.code.as_str()).collect();
        assert_eq!(&codes[..2], &["STATUS", "IR_SLOT"]);
        assert!(codes.contains(&"WAIVER_ADD"));
        assert_eq!(ctx.my_team().unwrap().slots[0].slot, "C");
        // the waiver add's gain and strength carry over
        assert!(mv.predicted_gain.is_some());
        assert!(matches!(mv.gain_units, Some(GainUnits::WeekPts) | Some(GainUnits::SeasonFpg)));
        assert_eq!(mv.subjects[0].cid, "hurt");
        assert!(mv.strength.unwrap() >= 5.0);
    }

    #[test]
    fn no_ir_move_without_free_slot_and_out_flagged() {
        let stash = mk("stash", &["D"], 0.1, "ir");
        let out = mk("out", &["C"], 1.0, "out");
        let full = ctx_for(&[("IR", &stash), ("C", &out)], vec![], &[("C", 1), ("IR", 1)]);
        let vals = valuate(&full);
        assert!(!run(&full, &vals).iter().any(|r| r.title.starts_with("Move")));
        let free = ctx_for(&[("C", &out)], vec![], &[("C", 1), ("IR", 1)]);
        let vals = valuate(&free);
        let recs = run(&free, &vals);
        let mv = recs.iter().find(|r| r.title.starts_with("Move")).unwrap();
        assert!(mv.reasons.iter().any(|x| x.code == "IR_RULES"));
    }

    #[test]
    fn activate_from_ir_suggests_drop_when_full() {
        let back = mk("back", &["C"], 1.0, "healthy");
        let weak = mk("weak", &["C"], 0.1, "healthy");
        let strong = mk("strong", &["LW"], 0.9, "healthy");
        let ctx = ctx_for(&[("IR", &back), ("C", &weak), ("LW", &strong)], vec![], &[("C", 1), ("LW", 1), ("IR", 1)]);
        let vals = valuate(&ctx);
        let recs: Vec<Recommendation> = run(&ctx, &vals).into_iter().filter(|r| r.title.starts_with("Activate")).collect();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].add[0].cid, "back");
        assert_eq!(recs[0].drop[0].cid, "weak");
        assert!((recs[0].score - (vals["back"].fpg + 1.0)).abs() < 1e-9);
        assert_eq!(recs[0].gain_units, Some(GainUnits::SeasonFpg));
        assert!((recs[0].predicted_gain.unwrap() - (vals["back"].fpg_season - vals["weak"].fpg_season)).abs() < 1e-9);
    }

    #[test]
    fn ir_chain_add_respects_goalie_cap() {
        let hurt = mk("hurt_g", &["G"], 1.0, "ir");
        let g2 = mk("g2", &["G"], 0.5, "healthy");
        let g3 = mk("g3", &["G"], 0.3, "healthy");
        let fa = mk("fa_g", &["G"], 0.9, "healthy");
        let mut ctx = ctx_for(&[("G", &hurt), ("G", &g2), ("BN", &g3)], vec![fa], &[("G", 2), ("BN", 1), ("IR", 1)]);
        let vals = valuate(&ctx);
        let recs = run(&ctx, &vals);
        let r = recs.iter().find(|r| r.title.starts_with("Move hurt_g")).unwrap();
        assert_eq!(r.title, "Move hurt_g to IR and add fa_g");
        ctx.position_limits.insert("G".into(), 3);
        let recs = run(&ctx, &vals);
        let r = recs.iter().find(|r| r.title.starts_with("Move hurt_g")).unwrap();
        assert_eq!(r.title, "Move hurt_g to IR");
        assert!(r.add.is_empty());
        assert!(code(r, "POSITION_CAP").text.contains("G limit 3"));
    }

    #[test]
    fn hold_a_star_through_a_short_injury() {
        let mut star = mk("star", &["C"], 1.5, "out");
        star.status_note = Some("Upper Body: out (est. return 2026-10-20)".into());
        let ctx = ctx_for(&[("C", &star)], vec![mk("fa", &["C"], 0.6, "healthy")], &[("C", 1)]);
        let vals = valuate(&ctx);
        let v = verdicts(run(&ctx, &vals));
        assert_eq!(v.len(), 1);
        let r = &v[0];
        assert!(r.title.starts_with("Hold star through his injury (back ~Oct 20)"), "{}", r.title);
        assert!(r.add.is_empty() && r.drop.is_empty());
        assert_eq!(r.strength, Some(1.0));
        assert!(code(r, "HOLD_PTS").value > code(r, "CUT_PTS").value);
        assert!(code(r, "BREAK_EVEN").text.contains("Cut only if he misses more than"));
    }

    #[test]
    fn cut_a_marginal_player_out_long_term() {
        let mut meh = mk("meh", &["C"], 0.5, "ir");
        meh.status_note = Some("Knee: out (est. return 2027-03-01)".into());
        let ctx = ctx_for(&[("C", &meh)], vec![mk("fa", &["C"], 0.6, "healthy")], &[("C", 1)]);
        let vals = valuate(&ctx);
        let v = verdicts(run(&ctx, &vals));
        assert_eq!(v.len(), 1);
        let r = &v[0];
        assert!(r.title.starts_with("Cut meh, add fa:"), "{}", r.title);
        assert_eq!(r.add[0].cid, "fa");
        assert_eq!(r.drop[0].cid, "meh");
        assert_eq!(r.gain_units, Some(GainUnits::SeasonFpg));
        assert!(r.predicted_gain.unwrap() > 0.0);
        assert!(code(r, "CUT_PTS").value > code(r, "HOLD_PTS").value);
    }

    #[test]
    fn free_ir_slot_means_hold_without_a_verdict_rec() {
        let mut meh = mk("meh", &["C"], 0.5, "ir");
        meh.status_note = Some("Knee: out (est. return 2027-03-01)".into());
        let fa = mk("fa", &["C"], 0.6, "healthy");
        let ctx = ctx_for(&[("C", &meh)], vec![fa.clone()], &[("C", 1), ("BN", 1), ("IR", 1)]);
        let vals = valuate(&ctx);
        let recs = run(&ctx, &vals);
        assert!(!recs.iter().any(|r| r.title.starts_with("Hold") || r.title.starts_with("Cut")));
        let mv = recs.iter().find(|r| r.title.starts_with("Move meh to IR")).unwrap();
        assert!(mv.reasons.iter().any(|x| x.code == "HOLD_VERDICT"));
        let c = mk("c", &["C"], 0.4, "healthy");
        let stashed = ctx_for(&[("IR", &meh), ("C", &c)], vec![fa], &[("C", 1), ("IR", 1)]);
        let vals = valuate(&stashed);
        let v = verdicts(run(&stashed, &vals));
        assert_eq!(v.len(), 1);
        assert!(v[0].title.starts_with("Hold meh in IR"));
        assert_eq!(v[0].strength, Some(1.0));
    }

    #[test]
    fn dynasty_never_cuts_a_protected_player() {
        let mut kid = mk("kid", &["C"], 0.5, "ir");
        kid.status_note = Some("Knee: out (est. return 2027-03-01)".into());
        kid.birth_date = NaiveDate::from_ymd_opt(2006, 1, 1);
        kid.draft_overall = Some(3);
        let fa = mk("fa", &["C"], 0.6, "healthy");
        let mut ctx = ctx_for(&[("C", &kid)], vec![fa.clone()], &[("C", 1)]);
        ctx.dynasty = true;
        let vals = valuate(&ctx);
        let dv = |p: &Player, v: f64| -> DynastyValue {
            serde_json::from_value(json!({"player": p, "value": v, "age": null, "age_mult": 1.0, "upside": 0.0, "horizon_years": 3}))
                .unwrap()
        };
        let dynv: Map<Cid, DynastyValue> = [("kid".to_string(), dv(&kid, 9.0)), ("fa".to_string(), dv(&fa, 3.0))].into_iter().collect();
        let v = verdicts(run_with(&ctx, &vals, &Map::new(), Some(&dynv)));
        assert_eq!(v.len(), 1);
        assert!(v[0].title.starts_with("Hold kid") && v[0].title.contains("protected dynasty asset"), "{}", v[0].title);
        assert!(v[0].reasons.iter().any(|x| x.code == "DYNASTY_COST"));
    }

    #[test]
    fn guessed_return_needs_a_bigger_edge_to_cut() {
        let p = mk("p", &["C"], 1.0, "ir");
        let ctx = ctx_for(&[("C", &p)], vec![mk("fa", &["C"], 0.6, "healthy")], &[("C", 1)]);
        let vals = valuate(&ctx);
        let v = verdicts(run(&ctx, &vals));
        assert_eq!(v.len(), 1);
        assert!(code(&v[0], "RETURN_ESTIMATE").text.contains("a guess"));
        assert!(v[0].title.starts_with("Hold p"));
    }

    #[test]
    fn replacement_must_have_an_nhl_team() {
        let mut meh = mk("meh", &["C"], 0.5, "ir");
        meh.status_note = Some("Knee: out (est. return 2027-03-01)".into());
        let mut unsigned = mk("unsigned", &["C"], 2.0, "healthy");
        unsigned.team = None;
        let ctx = ctx_for(&[("C", &meh)], vec![unsigned, mk("fa", &["C"], 0.6, "healthy")], &[("C", 1)]);
        let vals = valuate(&ctx);
        let v = verdicts(run(&ctx, &vals));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].add[0].cid, "fa");
    }

    #[test]
    fn ir_slot_held_by_a_dtd_player_is_a_swap_not_a_cut() {
        let mut meh = mk("meh", &["C"], 0.5, "ir");
        meh.status_note = Some("Knee: out (est. return 2027-03-01)".into());
        let dtd = mk("dtd", &["LW"], 0.8, "dtd");
        let ctx = ctx_for(&[("C", &meh), ("IR", &dtd)], vec![mk("fa", &["C"], 0.6, "healthy")], &[("C", 1), ("IR", 1)]);
        let vals = valuate(&ctx);
        let v = verdicts(run(&ctx, &vals));
        assert_eq!(v.len(), 1);
        assert!(v[0].title.starts_with("Hold meh") && v[0].title.contains("activate dtd (dtd)"), "{}", v[0].title);
        assert!(v[0].add.is_empty() && v[0].drop.is_empty());
    }
}
