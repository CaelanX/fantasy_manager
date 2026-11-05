//! Market worth, packages and the acceptance model (`market_worth`, `pick_worth`,
//! `package_value`, `AcceptParams`, `Acceptance`, `team_standing`, `calibrate_acceptance`).

use fm_core::models::LeagueContext;
use fm_core::values::PickAsset;
use fm_core::Map;
use fm_pycompat::num::py_sum;
use fm_pycompat::order::{cmp_f64, sort_desc_by};
use serde_json::Value;

use super::{ACCEPT_K, ACCEPT_X0, CALIBRATION_MIN_N, DEPTH_WEIGHT, MARKET_CONVEXITY, PICK_CONTENDER_MULT, PICK_REBUILDER_MULT, P_CEILING};
use crate::recommend::market::{pick_percentile, MarketPool};

/// Market worth (0-100) of a player at market percentile `pct`: 100 x (pct/100)^MARKET_CONVEXITY.
pub fn market_worth(pct: f64) -> f64 {
    let x = pct.max(0.0).min(100.0) / 100.0;
    100.0 * x.powf(MARKET_CONVEXITY)
}

/// Market worth of a draft pick to a counterparty with `standing` (model-only percentile among
/// the rostered players' dynasty values, x PICK_REBUILDER_MULT / PICK_CONTENDER_MULT).
pub fn pick_worth(market: &MarketPool<'_>, pick: &PickAsset, standing: Option<&str>) -> f64 {
    let mult = match standing.unwrap_or("") {
        "rebuilder" => PICK_REBUILDER_MULT,
        "contender" => PICK_CONTENDER_MULT,
        _ => 1.0,
    };
    market_worth(pick_percentile(market, pick.value)) * mult
}

/// Market worth of a package: the best player in full, each further one at DEPTH_WEIGHT.
pub fn package_value(worths: impl IntoIterator<Item = f64>) -> f64 {
    let mut xs: Vec<f64> = worths.into_iter().collect();
    sort_desc_by(&mut xs, |a, b| cmp_f64(*a, *b));
    py_sum(xs.iter().enumerate().map(|(i, &x)| if i == 0 { x } else { DEPTH_WEIGHT * x }))
}

/// Logistic acceptance curve p = 1 / (1 + exp(-k (perceived - x0))), capped at `ceiling`.
#[derive(Clone, Debug, PartialEq)]
pub struct AcceptParams {
    pub x0: f64,
    pub k: f64,
    pub ceiling: f64,
    /// logged proposals behind the fit (0 or < CALIBRATION_MIN_N: prior)
    pub n: i64,
    pub source: String,
}

impl Default for AcceptParams {
    fn default() -> Self {
        AcceptParams { x0: ACCEPT_X0, k: ACCEPT_K, ceiling: P_CEILING, n: 0, source: "prior".to_string() }
    }
}

impl AcceptParams {
    pub fn p(&self, perceived: f64) -> f64 {
        let z = (-50.0f64).max(50.0f64.min(self.k * (perceived - self.x0)));
        self.ceiling.min(1.0 / (1.0 + (-z).exp()))
    }

    /// Perceived fairness at which the (uncapped) curve reaches `p`.
    pub fn perceived_for(&self, p: f64) -> f64 {
        let p = p.max(1e-6).min(1.0 - 1e-6);
        self.x0 + (p / (1.0 - p)).ln() / self.k.max(1e-9)
    }
}

/// The counterparty's view of a deal (market worth points, + = in their favour).
#[derive(Clone, Debug, PartialEq)]
pub struct Acceptance {
    pub gets: f64,
    pub gives: f64,
    pub perceived: f64,
    pub p_raw: f64,
    pub p: f64,
    pub notes: Vec<String>,
    pub legal: bool,
    pub legal_note: Option<String>,
    pub sources: Vec<&'static str>,
    pub params: AcceptParams,
}

impl Acceptance {
    pub fn base(&self) -> f64 {
        self.gets - self.gives
    }
}

/// team_id -> "contender" (top half by win %), "rebuilder" (bottom third) or "middle"; empty
/// until at least two teams have played a game.
pub fn team_standing(ctx: &LeagueContext) -> Map<String, String> {
    let mut rows: Vec<(&str, f64)> = Vec::new();
    for t in &ctx.teams {
        let Some((w, l, ti)) = t.record else { continue };
        let gp = w + l + ti;
        if gp > 0 {
            rows.push((t.team_id.as_str(), (w as f64 + 0.5 * ti as f64) / gp as f64));
        }
    }
    if rows.len() < 2 {
        return Map::new();
    }
    rows.sort_by(|a, b| cmp_f64(-a.1, -b.1));
    let n = rows.len() as f64;
    rows.iter()
        .enumerate()
        .map(|(i, (tid, _))| {
            let i = i as f64;
            let s = if i < n / 2.0 {
                "contender"
            } else if i >= n - n / 3.0 {
                "rebuilder"
            } else {
                "middle"
            };
            (tid.to_string(), s.to_string())
        })
        .collect()
}

/// Read access to the harness ledger (`fm_harness::ledger::Ledger.query`), which lives in a crate
/// above this one: rows as JSON objects.
pub trait LedgerQuery {
    fn query(&self, sql: &str, args: &[Value]) -> anyhow::Result<Vec<Map<String, Value>>>;
}

/// (perceived fairness, accepted) per logged trade proposal (MARKET_VIEW value of the episode's
/// first logged rec); [] on any ledger error.
pub fn acceptance_observations(ledger: &dyn LedgerQuery, league: Option<&str>) -> Vec<(f64, i64)> {
    let mut sql = "SELECT league, rec_key, status FROM rec_episodes WHERE kind='trade' AND status IN ('proposed','followed')"
        .to_string();
    let mut args: Vec<Value> = Vec::new();
    if let Some(l) = league.filter(|l| !l.is_empty()) {
        sql += " AND league=?";
        args.push(Value::String(l.to_string()));
    }
    let run = || -> anyhow::Result<Vec<(f64, i64)>> {
        let mut out = Vec::new();
        for ep in ledger.query(&sql, &args)? {
            let rows = ledger.query(
                "SELECT reasons_json FROM recs WHERE league=? AND rec_key=? ORDER BY as_of LIMIT 1",
                &[ep.get("league").cloned().unwrap_or(Value::Null), ep.get("rec_key").cloned().unwrap_or(Value::Null)],
            )?;
            let Some(text) = rows.first().and_then(|r| r.get("reasons_json")).and_then(|v| v.as_str()).filter(|s| !s.is_empty())
            else {
                continue;
            };
            let reasons: Value = serde_json::from_str(text)?;
            let x = reasons.as_array().and_then(|rs| {
                rs.iter()
                    .find(|r| r.get("code").and_then(|c| c.as_str()) == Some("MARKET_VIEW"))
                    .and_then(|r| r.get("value"))
                    .cloned()
            });
            // `isinstance(x, (int, float))` (a JSON bool is not a number here)
            if let Some(x) = x.as_ref().and_then(|v| v.as_f64()) {
                let followed = ep.get("status").and_then(|s| s.as_str()) == Some("followed");
                out.push((x, if followed { 1 } else { 0 }));
            }
        }
        Ok(out)
    };
    run().unwrap_or_default()
}

/// Logistic parameters for `p_accept` from logged observations: the prior (with `n`) until
/// `min_n` exist, else a ridge-regularised logistic regression shrunk toward the prior.
pub fn calibrate_from(obs: &[(f64, i64)], min_n: usize, prior: &AcceptParams) -> AcceptParams {
    if obs.len() < min_n {
        return AcceptParams { n: obs.len() as i64, ..prior.clone() };
    }
    let (a0, b0) = (-prior.k * prior.x0, prior.k);
    let (lam_a, lam_b) = (1.0, 100.0);
    let (mut a, mut b) = (a0, b0);
    for _ in 0..100 {
        let (mut ga, mut gb) = (-lam_a * (a - a0), -lam_b * (b - b0));
        let (mut haa, mut hab, mut hbb) = (-lam_a, 0.0, -lam_b);
        for &(x, y) in obs {
            let z = (-50.0f64).max(50.0f64.min(a + b * x));
            let p = 1.0 / (1.0 + (-z).exp());
            let (r, w) = (y as f64 - p, p * (1.0 - p));
            ga += r;
            gb += r * x;
            haa -= w;
            hab -= w * x;
            hbb -= w * x * x;
        }
        let det = haa * hbb - hab * hab;
        if det.abs() < 1e-12 {
            break;
        }
        let (da, db) = ((hbb * ga - hab * gb) / det, (haa * gb - hab * ga) / det);
        a -= da;
        b -= db;
        if da.abs().max(db.abs()) < 1e-9 {
            break;
        }
    }
    let k = b.max(1e-3);
    AcceptParams { x0: -a / k, k, ceiling: prior.ceiling, n: obs.len() as i64, source: format!("fit on {} proposals", obs.len()) }
}

/// `calibrate_acceptance(ledger, league, min_n, prior)`.
pub fn calibrate_acceptance(ledger: &dyn LedgerQuery, league: Option<&str>, min_n: Option<usize>, prior: Option<&AcceptParams>) -> AcceptParams {
    let obs = acceptance_observations(ledger, league);
    let default = AcceptParams::default();
    calibrate_from(&obs, min_n.unwrap_or(CALIBRATION_MIN_N), prior.unwrap_or(&default))
}
