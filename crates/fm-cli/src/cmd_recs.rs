//! Recommendation commands of `fantasy_manager/cli.py` (lines 1118-1987): `trades` (+ `--need`),
//! `trade-eval` (`resolve_offer`), `flags`, `advise`, `alerts`, `trending`, `lines`, `goalies`
//! (+ `--week`), `news`, `ask`, `report`, `notify`. Owner: work package `W10-cli-analysis`.
//!
//! Deviations: rich markup (colours / bold) is dropped from the human output and tables are
//! comfy-table boxes (see `common.rs`). `trending` always asks a fresh `EspnProvider` (also in an
//! ESPN league: the loaded provider's rows are tagged by name there, not by player id).

use anyhow::Result;
use clap::Args;
use fm_core::models::{LeagueContext, Player, Recommendation};
use fm_core::values::PickAsset;
use fm_core::DynastyMode;
use fm_core::normalize::{normalize_name, normalize_team};
use fm_core::Map;
use fm_engine::analysis::outlook::{build_outlook, recommendation_text, SIMS};
use fm_engine::recommend::advise::{advise, group_by_kind, AdviseOpts};
use fm_engine::recommend::flags::recommend_flags;
use fm_engine::recommend::injuries::History;
use fm_engine::recommend::plan::{build_week_plan, plan_summary, PlanOpts};
use fm_engine::recommend::trades::{
    acquire_player, evaluate_offer, exploit_opportunities, find_needs, find_pick, find_players, find_team, offer_summary, pick_assets,
    pitch_of, recommend_need_trades, recommend_trades, shop_player, EngineOpts, OfferEval, OutlookView, PickRef, TradeOpts,
    EXPLOIT_PER_TEAM, MIN_GAIN_ME, NEED_PER_TEAM,
};
use fm_engine::recommend::base::league_unit;
use fm_harness::metrics::HIT_KINDS;
use fm_harness::track::{as_json as track_json, rec_record, record_text, TrackBook, TRACK_DAYS};
use fm_providers::lines_enrich::{build_snapshot, diff_snapshots, LineSnapshotStore};
use fm_pycompat::fmt::{fixed, general, percent};
use fm_pycompat::num::round_to;
use fm_pycompat::time::{strftime, strftime_date};
use fm_sources::dailyfaceoff::{make_cached_fetch_text, DailyFaceoffClient, TEAM_SLUGS};
use fm_sources::llm::SupportsComplete;
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::common::*;

// --------------------------------------------------------------------------- shared helpers

fn pct0(p: f64) -> String {
    percent(p, 0, false)
}

/// `_print_*` helpers print through `echo`; rich's `console.print` of a plain string.
fn say(s: impl AsRef<str>) {
    echo(s);
}

fn core_msg(e: &anyhow::Error) -> Option<String> {
    e.downcast_ref::<fm_core::CoreError>().map(|c| c.to_string())
}

// --------------------------------------------------------------------------- trades

#[derive(Args, Clone, Debug)]
pub struct TradesArgs {
    /// Max proposals overall.
    #[arg(long = "limit", short = 'n', default_value_t = 10)]
    pub limit: usize,
    /// Max proposals per counterparty.
    #[arg(long = "per-team", default_value_t = 3)]
    pub per_team: usize,
    /// Also fetch NHL game logs for recent form.
    #[arg(long = "deep")]
    pub deep: bool,
    #[command(flatten)]
    pub mode: ModeOpt,
    /// Max exploit trades (teams under roster pressure); 0 = skip.
    #[arg(long = "exploits", default_value_t = 8, allow_negative_numbers = true)]
    pub exploit_limit: i64,
    /// Trade finder by need: deals that fill your weakest slots with partners heading the other way, each with a pitch to send.
    #[arg(long = "need")]
    pub need: bool,
    /// Shop one of your players (fuzzy name): the best packages each team would give for him.
    #[arg(long = "shop", conflicts_with_all = ["need", "target"])]
    pub shop: Option<String>,
    /// Get another team's player (fuzzy name): the cheapest packages of yours they would likely take for him.
    #[arg(long = "target", conflicts_with = "need")]
    pub target: Option<String>,
    #[command(flatten)]
    pub lo: LeagueOpts,
}

/// `trade_gain_text(r)`: "+0.66/g · +2.3/wk · +55/season".
pub fn trade_gain_text(r: &Recommendation) -> String {
    let fpg = reason_val(r, "DELTA_ME");
    let wk = reason_val(r, "GAIN_WEEK");
    let ssn = reason_val(r, "GAIN_SEASON");
    let mut parts = vec![match fpg {
        Some(_) => format!("{}/g", fmt_signed(fpg)),
        None => "-".to_string(),
    }];
    if let Some(w) = wk {
        parts.push(format!("{}/wk", fixed(w, 1, true)));
    }
    if let Some(s) = ssn {
        parts.push(format!("{}/season", fixed(s, 0, true)));
    }
    parts.join(" · ")
}

/// `_trade_table(title, recs, sweet)`.
fn trade_table(title: &str, recs: &[Recommendation], sweet: bool) -> Table {
    let mut tbl = Table::new(title).lines();
    tbl.cols(&[
        ("#", R),
        ("With", L),
        ("Give", L),
        ("Get", L),
        ("You gain", R),
        ("Market", R),
        ("Accept", R),
        ("Score", R),
        ("Strength", R),
        ("Why", L),
    ]);
    const CODES: [&str; 12] = [
        "NEED_MATCH",
        "STAR_CONSOLIDATION",
        "COMPLEMENT",
        "PICK_IN",
        "PICK_OUT",
        "THEIR_NEED",
        "TRADE_BLOCK",
        "ROSTER_CONSEQUENCE",
        "POSITION_CAP",
        "WIN_NOW_COST",
        "ROTO_BALANCE",
        "MOVE_BUDGET",
    ];
    for (i, r) in recs.iter().enumerate() {
        let i = i + 1;
        let edge = reason_val(r, "MY_EDGE");
        let dyn_ = reason_val(r, "DYNASTY_DELTA");
        let mut gain = trade_gain_text(r);
        if let Some(d) = dyn_ {
            gain.push_str(&format!("\ndyn {}", fixed(d, 2, true)));
        }
        let perceived = reason_val(r, "MARKET_VIEW");
        let p = reason_val(r, "P_ACCEPT");
        let label = reason(r, "MARKET_VIEW")
            .map(|mv| mv.text.split(" by market value").next().unwrap_or("").replace("Looks ", "").replace(" to them", ""))
            .unwrap_or_default();
        let ev = if edge.is_some() && p.is_some() { Some(r.score) } else { None };
        let mut why: Vec<String> = r.reasons.iter().filter(|x| CODES.contains(&x.code.as_str())).map(|x| x.text.clone()).collect();
        if !sweet && reason(r, "SWEET_SPOT").is_some() {
            why.insert(0, "Sweet spot".to_string());
        }
        let mut give: Vec<String> = r.drop.iter().map(|x| format!("{} ({})", x.name, pos(x))).collect();
        give.extend(r.reasons.iter().filter(|x| x.code == "PICK_OUT").map(|_| "+ draft pick".to_string()));
        let mut get: Vec<String> = r.add.iter().map(|x| format!("{} ({})", x.name, pos(x))).collect();
        get.extend(r.reasons.iter().filter(|x| x.code == "PICK_IN").map(|_| "+ draft pick".to_string()));
        let w = why_cell(r, &why);
        tbl.row([
            if sweet { i.to_string() } else { rank_cell(r, i) },
            r.counterparty.clone().unwrap_or_else(|| "-".into()),
            give.join("\n"),
            get.join("\n"),
            gain,
            match perceived {
                None => "-".to_string(),
                Some(v) => format!("{}\n{label}", fixed(v, 1, true)),
            },
            p.map(pct0).unwrap_or_else(|| "-".into()),
            fmt_signed(ev),
            strength_cell(r),
            if w.is_empty() { "-".to_string() } else { w },
        ]);
    }
    tbl
}

fn why_cell(r: &Recommendation, lines: &[String]) -> String {
    why(r, lines)
}

/// `_print_exploits(exploits)`.
fn print_exploits(exploits: &[Recommendation]) {
    if exploits.is_empty() {
        return;
    }
    say("Exploit (teams under roster pressure; relieving it counts +8 market points toward acceptance)");
    for (i, r) in exploits.iter().enumerate() {
        let text = r.reasons.iter().find(|x| x.code == "EXPLOIT").map(|x| x.text.as_str()).unwrap_or("");
        say(format!("  {}. {text}", i + 1));
    }
    trade_table("Exploit trades, ranked by expected value", exploits, true).print();
    say("Not checked: other teams' moves left this week (providers only report your own transaction counter).");
}

/// `_print_future(future, n=3)`.
fn print_future(future: &[Recommendation]) {
    if future.is_empty() {
        return;
    }
    say("Future-only (contend mode: not recommended, they cost this season's lineup)");
    for r in future.iter().take(3) {
        let cost = r.reasons.iter().find(|x| x.code == "WIN_NOW_COST").map(|x| x.text.as_str()).unwrap_or("");
        let dyn_ = r.reasons.iter().find(|x| x.code == "DYNASTY_DELTA").map(|x| x.text.as_str()).unwrap_or("");
        say(format!("  {}: {cost}; {}", r.title, dyn_.split(';').next().unwrap_or("")));
    }
}

pub fn trades(cli: &mut Cli, a: &TradesArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let d = cli.load_all(&league, a.deep, a.mode.val())?;
    let lc = &d.lc;
    let mut errors = d.errors.clone();
    let wants = trade_wants(lc, d.provider.as_ref());
    let offered = trade_offered(d.provider.as_ref());
    if a.need {
        return need_trades_out(&d, wants.as_ref(), offered.as_ref(), a.limit, as_json, errors);
    }
    if a.shop.is_some() || a.target.is_some() {
        return targeted_trades_out(&d, wants.as_ref(), offered.as_ref(), a, as_json);
    }
    let engine = EngineOpts { wants: wants.as_ref(), offered: offered.as_ref(), ..EngineOpts::default() };
    let topts = TradeOpts { dynasty_values: d.dyn_values.as_ref(), max_per_team: a.per_team, limit: a.limit, engine, ..TradeOpts::default() };
    let mut future: Vec<Recommendation> = Vec::new();
    let mut sweet: Vec<Recommendation> = Vec::new();
    let want_future = d.dyn_values.as_ref().is_some_and(|m| !m.is_empty()) && lc.dynasty_mode == DynastyMode::Contend;
    let recs = safe(&mut errors, "trades", || {
        recommend_trades(lc, &d.values, &topts, if want_future { Some(&mut future) } else { None }, Some(&mut sweet))
    })?
    .unwrap_or_default();
    let exploits = if a.exploit_limit > 0 {
        safe(&mut errors, "exploit trades", || exploit_opportunities(lc, &d.values, &topts, a.exploit_limit as usize, EXPLOIT_PER_TEAM))?
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if as_json {
        let mut w = serde_json::Map::new();
        for (k, v) in wants.iter().flatten() {
            let mut v = v.clone();
            v.sort();
            w.insert(k.clone(), json!(v));
        }
        json_out(
            json!({"trades": recs_json(&recs), "sweet_spot": recs_json(&sweet),
                   "future_only": recs_json(&future[..future.len().min(5)]),
                   "exploits": recs_json(&exploits), "trade_block_wants": Value::Object(w)}),
            Some(lc),
            &errors,
        );
        return Ok(());
    }
    if recs.is_empty() {
        say(format!(
            "No trade gains you more than +{} with at least a 25% chance of being accepted.",
            general(round_to(MIN_GAIN_ME * league_unit(lc), 2), None, false)
        ));
        print_exploits(&exploits);
        print_future(&future);
        footer(lc, &errors);
        return Ok(());
    }
    trade_table("Trade proposals, ranked by expected value (EV = your gain x acceptance chance)", &recs, false).print();
    say("You gain = your lineup's points per game (/g), per week (/wk, x your starters' games per week) and over the rest of the season. Market = how the deal looks to them by market value (ADP / % rostered worth points, + = in their favour); acceptance is a prior until the harness has logged 20 proposals (docs/trades.md).");
    if !sweet.is_empty() {
        trade_table("Sweet spot: the market calls it fair, our model says you win", &sweet, true).print();
    }
    print_exploits(&exploits);
    if let Some(w) = wants.as_ref().filter(|w| !w.is_empty()) {
        let parts: Vec<String> = w
            .iter()
            .map(|(k, v)| {
                let name = lc.teams.iter().find(|t| &t.team_id == k).map(|t| t.name.clone()).unwrap_or_else(|| k.clone());
                let mut v = v.clone();
                v.sort();
                format!("{name}: {}", v.join(", "))
            })
            .collect();
        say(format!("Trade-block wants: {}", parts.join("; ")));
    }
    print_future(&future);
    footer(lc, &errors);
    Ok(())
}

/// `_need_trades_out(d, wants, offered, limit, as_json)`.
fn need_trades_out(
    d: &fm_providers::pipeline::Loaded,
    wants: Option<&Map<String, Vec<String>>>,
    offered: Option<&Map<String, Vec<String>>>,
    limit: usize,
    as_json: bool,
    mut errors: Vec<String>,
) -> Result<()> {
    let lc = &d.lc;
    let outlook = safe(&mut errors, "outlook", || Ok(build_outlook(lc, &d.values, d.dyn_values.as_ref(), Some(d.provider.as_ref()), SIMS)))?;
    let view: Option<&dyn OutlookView> = outlook.as_ref().map(|o| o as &dyn OutlookView);
    let needs = safe(&mut errors, "needs", || Ok(find_needs(lc, &d.values, view, d.dyn_values.as_ref())))?.unwrap_or_default();
    let engine = EngineOpts { wants, offered, ..EngineOpts::default() };
    let topts = TradeOpts { dynasty_values: d.dyn_values.as_ref(), engine, ..TradeOpts::default() };
    let recs = safe(&mut errors, "need trades", || {
        recommend_need_trades(lc, &d.values, &topts, view, limit, NEED_PER_TEAM, Some(needs.clone()))
    })?
    .unwrap_or_default();
    if as_json {
        let needs_json: Vec<Value> = needs
            .iter()
            .map(|n| {
                json!({"slot": n.slot, "gap": if n.gap == f64::NEG_INFINITY { Value::Null } else { json!(n.gap) },
                       "text": n.text, "long_term": n.long_term})
            })
            .collect();
        json_out(
            json!({"needs": needs_json, "direction": outlook.as_ref().and_then(|o| o.recommended.clone()),
                   "need_trades": recs_json(&recs)}),
            Some(lc),
            &errors,
        );
        return Ok(());
    }
    if let Some(o) = &outlook {
        say(format!("Direction: {}", recommendation_text(o)));
    }
    let needs_txt: Vec<String> =
        needs.iter().map(|n| format!("{}{}", n.text, if n.long_term { " (long term)" } else { "" })).collect();
    say(format!("Your needs: {}", if needs_txt.is_empty() { "none found".to_string() } else { needs_txt.join("; ") }));
    if recs.is_empty() {
        say("No trade fills those needs with a gain for you and at least a 25% chance of being accepted.");
        footer(lc, &errors);
        return Ok(());
    }
    trade_table("Trades for your needs (ranked by EV; partners heading the other way and star consolidation first)", &recs, false).print();
    say("Pitches (copy, edit, send)");
    for (i, r) in recs.iter().enumerate() {
        say(format!("  {}. {}", i + 1, pitch_of(r).unwrap_or("")));
    }
    footer(lc, &errors);
    Ok(())
}

/// `fm trades --shop NAME` / `--target NAME`: deals built around one player.
fn targeted_trades_out(
    d: &fm_providers::pipeline::Loaded,
    wants: Option<&Map<String, Vec<String>>>,
    offered: Option<&Map<String, Vec<String>>>,
    a: &TradesArgs,
    as_json: bool,
) -> Result<()> {
    let lc = &d.lc;
    let shop = a.shop.is_some();
    let query = a.shop.as_deref().or(a.target.as_deref()).unwrap_or_default();
    let engine = EngineOpts { wants, offered, ..EngineOpts::default() };
    let outcome = (|| -> Result<(Player, Vec<OfferEval<'_>>)> {
        let me = lc.my_team()?;
        let pool: Vec<&Player> = if shop {
            me.players_iter().collect()
        } else {
            lc.teams.iter().filter(|t| t.team_id != me.team_id).flat_map(|t| t.players_iter()).collect()
        };
        let p = find_players(&[query], &pool)?.into_iter().next().ok_or_else(|| lookup("name a player"))?;
        let offers = if shop {
            shop_player(lc, &d.values, &p.cid, d.dyn_values.as_ref(), &engine, a.limit)?
        } else {
            acquire_player(lc, &d.values, &p.cid, d.dyn_values.as_ref(), &engine, a.limit)?
        };
        Ok((p.clone(), offers))
    })();
    let (p, offers) = match outcome {
        Ok(x) => x,
        Err(e) => match core_msg(&e) {
            Some(m) => return Err(fail_code(m, 2)),
            None => return Err(e),
        },
    };
    let recs: Vec<Recommendation> = offers.iter().map(|o| o.rec.clone()).collect();
    if as_json {
        json_out(
            json!({"mode": if shop { "shop" } else { "get" }, "player": {"cid": p.cid, "name": p.name},
                   "offers": offers.iter().map(offer_summary).collect::<Vec<_>>(), "trades": recs_json(&recs)}),
            Some(lc),
            &d.errors,
        );
        return Ok(());
    }
    if recs.is_empty() {
        say(if shop {
            format!(
                "No team gives a fair package for {}: nothing gains you more than +{} with at least a 25% chance of being accepted.",
                p.name,
                general(round_to(MIN_GAIN_ME * league_unit(lc), 2), None, false)
            )
        } else {
            format!("No package of yours gets {} with at least a 25% chance of being accepted and leaves you better off.", p.name)
        });
        footer(lc, &d.errors);
        return Ok(());
    }
    if shop {
        trade_table(&format!("Shopping {}: what teams would give, ranked by expected value (your gain x acceptance chance)", p.name), &recs, false).print();
    } else {
        trade_table(&format!("Getting {}: cheapest packages they would likely accept first (50%+), then your gain", p.name), &recs, false).print();
    }
    let verdicts: Vec<String> = offers.iter().enumerate().map(|(i, o)| format!("{}. {}", i + 1, o.verdict)).collect();
    say(format!("Verdicts (as fm trade-eval would judge each): {}", verdicts.join(" · ")));
    footer(lc, &d.errors);
    Ok(())
}

// --------------------------------------------------------------------------- trade-eval

#[derive(Args, Clone, Debug)]
pub struct TradeEvalArgs {
    /// Players you send, comma-separated (repeatable). Fuzzy names.
    #[arg(long = "give")]
    pub give: Vec<String>,
    /// Players you receive, comma-separated (repeatable).
    #[arg(long = "get")]
    pub get: Vec<String>,
    /// Your draft pick to send, e.g. "2027 R1" or "2027 R2 (from Team X)" (repeatable; see fm picks).
    #[arg(long = "give-pick")]
    pub give_pick: Vec<String>,
    /// Their draft pick to receive (repeatable).
    #[arg(long = "get-pick")]
    pub get_pick: Vec<String>,
    /// The other team (name or id); default: the owner of the --get players.
    #[arg(long = "team", short = 't')]
    pub team: Option<String>,
    /// Also fetch NHL game logs for recent form.
    #[arg(long = "deep")]
    pub deep: bool,
    #[command(flatten)]
    pub mode: ModeOpt,
    #[command(flatten)]
    pub lo: LeagueOpts,
}

/// `_split_names(items)`: repeatable, comma-separated option values -> names.
pub fn split_names(items: &[String]) -> Vec<String> {
    items.iter().flat_map(|it| it.split(',')).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

/// `evaluate_offer` keyword arguments from `resolve_offer`.
pub struct OfferKw {
    pub give: Vec<String>,
    pub get: Vec<String>,
    pub team_id: String,
    pub give_picks: Vec<PickRef>,
    pub get_picks: Vec<PickRef>,
}

fn lookup(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(fm_core::CoreError::Lookup(msg.into()))
}

/// `resolve_offer(lc, values, dyn, give, get, give_pick, get_pick, team)`: typed names ->
/// evaluate_offer arguments. CoreError (Lookup / Value) on bad input.
pub fn resolve_offer(
    lc: &LeagueContext,
    values: &Map<String, fm_core::PlayerValue>,
    dyn_values: Option<&Map<String, fm_core::DynastyValue>>,
    give: &[String],
    get: &[String],
    give_pick: &[String],
    get_pick: &[String],
    team: Option<&str>,
) -> Result<OfferKw> {
    let me = lc.my_team()?;
    let mut their = match team {
        Some(t) => Some(find_team(lc, t)?),
        None => None,
    };
    let others: Vec<&fm_core::models::FantasyTeam> = lc.teams.iter().filter(|t| t.team_id != me.team_id).collect();
    let mine_pool: Vec<&Player> = me.players_iter().collect();
    let give_q: Vec<&str> = give.iter().map(|s| s.as_str()).collect();
    let get_q: Vec<&str> = get.iter().map(|s| s.as_str()).collect();
    let give_ps = find_players(&give_q, &mine_pool)?;
    let get_pool: Vec<&Player> = match their {
        Some(t) => t.players_iter().collect(),
        None => others.iter().flat_map(|t| t.players_iter()).collect(),
    };
    let get_ps = find_players(&get_q, &get_pool)?;
    if their.is_none() && !get_ps.is_empty() {
        let cids: Vec<&str> = get_ps.iter().map(|p| p.cid.as_str()).collect();
        let owners: Vec<&&fm_core::models::FantasyTeam> =
            others.iter().filter(|t| t.players_iter().any(|p| cids.contains(&p.cid.as_str()))).collect();
        if owners.len() > 1 {
            let mut names: Vec<&str> = owners.iter().map(|t| t.name.as_str()).collect();
            names.sort();
            return Err(lookup(format!("the --get players are on different teams: {}", names.join(", "))));
        }
        their = owners.first().map(|t| **t);
    }
    let mut picks: Map<String, Vec<PickAsset>> = Map::new();
    if !give_pick.is_empty() || !get_pick.is_empty() {
        picks = pick_assets(lc, values, dyn_values);
        if picks.is_empty() {
            return Err(anyhow::Error::new(fm_core::CoreError::Value(
                "draft picks can only be traded in dynasty leagues whose provider reports picks (Fantrax)".into(),
            )));
        }
        if !get_pick.is_empty() && their.is_none() {
            return Err(lookup("--get-pick needs --team (or a --get player) to know whose pick it is"));
        }
    }
    let all_picks: Vec<&PickAsset> = picks.values().flatten().collect();
    let mut give_keys = Vec::new();
    for s in give_pick {
        give_keys.push(PickRef::Key(find_pick(&all_picks, s, &me.team_id)?.key()));
    }
    let mut get_keys = Vec::new();
    if let Some(t) = their {
        for s in get_pick {
            get_keys.push(PickRef::Key(find_pick(&all_picks, s, &t.team_id)?.key()));
        }
    }
    let Some(their) = their else {
        return Err(lookup("name the other team with --team (or add a --get player)"));
    };
    Ok(OfferKw {
        give: give_ps.iter().map(|p| p.cid.clone()).collect(),
        get: get_ps.iter().map(|p| p.cid.clone()).collect(),
        team_id: their.team_id.clone(),
        give_picks: give_keys,
        get_picks: get_keys,
    })
}

fn jf(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

pub fn trade_eval(cli: &mut Cli, a: &TradeEvalArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let gives = split_names(&a.give);
    let gets = split_names(&a.get);
    let gpicks: Vec<String> = a.give_pick.iter().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
    let tpicks: Vec<String> = a.get_pick.iter().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
    if gives.is_empty() && gets.is_empty() && gpicks.is_empty() && tpicks.is_empty() {
        return Err(fail_code("Name the offer: --give and/or --get players (and --give-pick / --get-pick for draft picks).", 2));
    }
    let d = cli.load_all(&league, a.deep, a.mode.val())?;
    let lc = &d.lc;
    let wants = trade_wants(lc, d.provider.as_ref());
    let offered = trade_offered(d.provider.as_ref());
    let outcome = (|| -> Result<(Value, Value, String)> {
        let kw = resolve_offer(lc, &d.values, d.dyn_values.as_ref(), &gives, &gets, &gpicks, &tpicks, a.team.as_deref())?;
        let give: Vec<&str> = kw.give.iter().map(|s| s.as_str()).collect();
        let get: Vec<&str> = kw.get.iter().map(|s| s.as_str()).collect();
        let engine = EngineOpts { wants: wants.as_ref(), offered: offered.as_ref(), ..EngineOpts::default() };
        let o = evaluate_offer(lc, &d.values, &give, &get, Some(&kw.team_id), d.dyn_values.as_ref(), &kw.give_picks, &kw.get_picks, &engine)?;
        Ok((offer_summary(&o), to_json(&o.rec), mode_line(lc)))
    })();
    let (s, rec, mline) = match outcome {
        Ok(x) => x,
        Err(e) => match core_msg(&e) {
            Some(m) => return Err(fail_code(m, 2)),
            None => return Err(e),
        },
    };
    if as_json {
        json_out(json!({"offer": s, "recommendation": rec}), Some(lc), &d.errors);
        return Ok(());
    }
    let names = |k: &str| -> Vec<String> { s[k].as_array().into_iter().flatten().map(|p| p["name"].as_str().unwrap_or("").to_string()).collect() };
    let labels = |k: &str| -> Vec<String> { s[k].as_array().into_iter().flatten().map(|p| p["label"].as_str().unwrap_or("").to_string()).collect() };
    let mut give_txt = names("give");
    give_txt.extend(labels("give_picks").into_iter().map(|l| format!("your {l} pick")));
    let mut get_txt = names("get");
    get_txt.extend(labels("get_picks").into_iter().map(|l| format!("their {l} pick")));
    let or_nothing = |v: &Vec<String>| if v.is_empty() { "nothing".to_string() } else { v.join(", ") };
    say(format!(
        "Trade with {}: you send {}; you get {}",
        s["team"].as_str().unwrap_or(""),
        or_nothing(&give_txt),
        or_nothing(&get_txt)
    ));
    if !s["mode"].is_null() {
        say(mline);
    }
    let v = s["verdict"].as_str().unwrap_or("");
    let reasons: Vec<&str> = s["verdict_reasons"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).collect();
    say(format!("Verdict: {} - {}", v.to_uppercase(), reasons.join("; ")));
    if let Some(c) = s["counter"].as_str() {
        say(format!("  Counter: {c}"));
    }
    let g = &s["my_gain"];
    say(format!(
        "  Your lineup:       {} pts/game · {} pts/week · {} rest of season",
        fixed(jf(&g["fpg"]), 2, true),
        fixed(jf(&g["week"]), 1, true),
        fixed(jf(&g["season"]), 0, true)
    ));
    say(format!("  Their lineup:      {} pts/game (our model)", fixed(jf(&s["their_gain"]["fpg"]), 2, true)));
    let f = &s["fairness"];
    let units = if s["units"] == "dynasty" { "dynasty value" } else { "season VORP" };
    let gap = if f["model_gap"].is_null() { "-".to_string() } else { pct0(jf(&f["model_gap"])) };
    say(format!(
        "  Fairness (model):  you get {}, send {} {units} (gap {gap})",
        fixed(jf(&f["value_in"]), 2, false),
        fixed(jf(&f["value_out"]), 2, false)
    ));
    if !f["market_label"].is_null() {
        say(format!(
            "  Fairness (market): {} ({} worth pts to them); they would accept ~{}",
            f["market_label"].as_str().unwrap_or(""),
            fixed(jf(&f["market_to_them"]), 0, true),
            pct0(jf(&f["p_accept"]))
        ));
    }
    if !s["dynasty_delta"].is_null() {
        let dd = &s["dynasty_delta"];
        say(format!(
            "  Dynasty value:     {} ({} FPG-equivalent); {} edge {}",
            fixed(jf(&dd["value"]), 2, true),
            fixed(jf(&dd["fpg_equiv"]), 2, true),
            s["mode"].as_str().unwrap_or(""),
            fixed(jf(&s["my_edge"]), 2, true)
        ));
    }
    let roster = s["roster"].as_str().unwrap_or("");
    say(format!("  Roster:            {}", roster.strip_prefix("Roster: ").unwrap_or(roster)));
    let checks: Vec<&str> = s["market_check"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).collect();
    if !checks.is_empty() {
        say("Market check");
        for t in checks {
            say(format!("  {t}"));
        }
    }
    footer(lc, &d.errors);
    Ok(())
}

// --------------------------------------------------------------------------- flags

#[derive(Args, Clone, Debug)]
pub struct FlagsArgs {
    /// Max flags per table.
    #[arg(long = "limit", short = 'n', default_value_t = 10)]
    pub limit: usize,
    /// Also fetch NHL game logs (needed for L15 form).
    #[arg(long = "deep")]
    pub deep: bool,
    #[command(flatten)]
    pub lo: LeagueOpts,
}

pub fn flags(cli: &mut Cli, a: &FlagsArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let d = cli.load_all(&league, a.deep, None)?;
    let lc = &d.lc;
    let mut errors = d.errors.clone();
    let recs = safe(&mut errors, "flags", || recommend_flags(lc, &d.values, None, None).map_err(Into::into))?.unwrap_or_default();
    let sell: Vec<Recommendation> = recs.iter().filter(|r| r.kind.as_str() == "sell_high").take(a.limit).cloned().collect();
    let buy: Vec<Recommendation> = recs.iter().filter(|r| r.kind.as_str() == "buy_low").take(a.limit).cloned().collect();
    if as_json {
        json_out(json!({"sell_high": recs_json(&sell), "buy_low": recs_json(&buy)}), Some(lc), &errors);
        return Ok(());
    }
    for (title, rows, drop) in [("Sell high (your players)", &sell, true), ("Buy low (other teams)", &buy, false)] {
        if rows.is_empty() {
            say(format!("{title}: none flagged."));
            continue;
        }
        let mut tbl = Table::new(title).lines();
        tbl.cols(&[("#", R), ("Player", L), ("Team", L), ("Form", R), ("Strength", R), ("Why", L)]);
        for (i, r) in rows.iter().enumerate() {
            let Some(p) = (if drop { r.drop.first() } else { r.add.first() }) else { continue };
            let form = reason_val(r, "FORM_RATIO");
            let lines: Vec<String> = r.reasons.iter().filter(|x| x.code != "FORM_RATIO").map(|x| x.text.clone()).collect();
            tbl.row([
                (i + 1).to_string(),
                format!("{} ({}, {})", p.name, pos(p), p.team.as_deref().filter(|t| !t.is_empty()).unwrap_or("-")),
                r.counterparty.clone().filter(|c| !c.is_empty()).unwrap_or_else(|| "me".into()),
                form.map(|f| format!("{}x", fixed(f, 2, false))).unwrap_or_else(|| "-".into()),
                strength_cell(r),
                why(r, &lines),
            ]);
        }
        tbl.print();
    }
    if !a.deep && recs.is_empty() {
        say("Flags need last-15-day splits; ESPN provides them, other leagues need --deep.");
    }
    footer(lc, &errors);
    Ok(())
}

// --------------------------------------------------------------------------- advise

#[derive(Args, Clone, Debug)]
pub struct AdviseArgs {
    /// Max recommendations overall.
    #[arg(long = "limit", short = 'n', default_value_t = 15)]
    pub limit: usize,
    /// Add short LLM explanations (needs OPENROUTER_API_KEY).
    #[arg(long = "explain")]
    pub explain: bool,
    /// Also fetch NHL game logs for recent form.
    #[arg(long = "deep")]
    pub deep: bool,
    #[command(flatten)]
    pub mode: ModeOpt,
    #[command(flatten)]
    pub lo: LeagueOpts,
}

pub fn advise_cmd(cli: &mut Cli, a: &AdviseArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let d = cli.load_all(&league, a.deep, a.mode.val())?;
    let lc = &d.lc;
    let mut errors = d.errors.clone();
    let mut notes: Vec<String> = Vec::new();
    let history = cli.history()?;
    let mut recs = run_advise(cli, &d, &History::Store(&history), &mut errors, Some(a.limit))?;
    drop(history);
    if a.explain {
        explain(cli, &mut recs, &d, &mut errors, None, Some(&mut notes));
    }
    let book = safe(&mut errors, "track record", || TrackBook::load(cli.data_dir(), &lc.provider, cli.today(), TRACK_DAYS))?
        .unwrap_or_else(|| TrackBook { league: lc.provider.clone(), days: TRACK_DAYS, ..Default::default() });
    if as_json {
        json_out(json!({"recommendations": recs_json(&recs), "notes": notes, "track_record": track_json(&book, &HIT_KINDS)}), Some(lc), &errors);
        return Ok(());
    }
    if recs.is_empty() {
        say("Nothing to do right now.");
    }
    for (kind, items) in group_by_kind(recs.clone(), false) {
        rec_table(&items, kind_title(&kind).unwrap_or(&kind)).print();
    }
    if !recs.is_empty() {
        say("Strength is absolute (0-10 from the predicted gain on fixed per-kind scales); # is the rank within the kind.");
        let mut shown: Vec<((String, Option<String>), String)> = Vec::new();
        for r in &recs {
            let tr = rec_record(&book, r);
            let key = (tr.kind.clone(), tr.subkind.clone());
            if !shown.iter().any(|(k, _)| *k == key) {
                shown.push((key, record_text(&tr)));
            }
        }
        for (_, text) in shown {
            say(text);
        }
    }
    for n in &notes {
        say(n);
    }
    footer(lc, &errors);
    Ok(())
}

// --------------------------------------------------------------------------- alerts

#[derive(Args, Clone, Debug)]
pub struct AlertsArgs {
    /// Max alerts.
    #[arg(long = "limit", short = 'n', default_value_t = 25)]
    pub limit: usize,
    #[command(flatten)]
    pub lo: LeagueOpts,
}

pub fn alerts_cmd(cli: &mut Cli, a: &AlertsArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let d = cli.load_all(&league, false, None)?;
    let lc = &d.lc;
    let mut errors = d.errors.clone();
    let mut logs: Vec<String> = Vec::new();
    let history = History::None;
    let include = ["alerts"];
    let aopts = AdviseOpts { dynasty_values: d.dyn_values.as_ref(), history: &history, include: &include, limit: Some(a.limit) };
    let recs = collect_logs("fm_engine::recommend::advise", &mut logs, "alerts", || {
        safe(&mut errors, "alerts", || Ok(advise(lc, &d.values, &aopts, &cli.now(), &cli.memo())))
    })?
    .unwrap_or_default();
    errors.extend(logs);
    if as_json {
        json_out(json!({"alerts": recs_json(&recs)}), Some(lc), &errors);
        return Ok(());
    }
    if !recs.is_empty() {
        rec_table(&recs, &format!("Alerts ({})", recs.len())).print();
    } else {
        say("No alerts: no line, power-play, role or goalie-start changes worth acting on.");
    }
    footer(lc, &errors);
    Ok(())
}

// --------------------------------------------------------------------------- trending

#[derive(Args, Clone, Debug)]
pub struct TrendingArgs {
    /// Biggest % rostered drops instead of rises.
    #[arg(long = "fallers")]
    pub fallers: bool,
    /// Players to list.
    #[arg(long = "limit", short = 'n', default_value_t = 25)]
    pub limit: usize,
    #[command(flatten)]
    pub lo: LeagueOpts,
}

pub fn trending_cmd(cli: &mut Cli, a: &TrendingArgs) -> Result<()> {
    use fm_providers::espn::EspnProvider;
    use fm_providers::registry::ESPN_FA_SIZE;

    let (league, as_json) = cli.opts(&a.lo);
    let cache = cli.cache()?;
    let mut errors: Vec<String> = Vec::new();
    let lc: Option<LeagueContext> = match cli.load_context(&league, None) {
        Ok((mut lc, _)) => {
            // Python's `_load_context` is not enriched (no season start, no source notes); a
            // snapshot carries the enriched context, so blank what enrichment adds to the meta
            if cli.snapshot.is_some() {
                lc.season_start = None;
                lc.source_notes.clear();
            }
            Some(lc)
        }
        Err(e) if e.downcast_ref::<Exit>().is_some() => None,
        Err(e) => return Err(e),
    };
    let espn = EspnProvider::new(cli.settings.clone(), cache, ESPN_FA_SIZE).with_today(cli.today());
    let mut rows = safe(&mut errors, "ESPN trending", || Ok(espn.trending(a.limit as i64, a.fallers)))?.unwrap_or_default();
    errors.extend(espn.warnings_now().into_iter().filter(|w| w.to_lowercase().contains("trending")));
    if let Some(lc) = &lc {
        let fa: Vec<String> = lc.free_agents.iter().map(|p| normalize_name(Some(&p.name))).collect();
        let rostered: Vec<String> = lc.teams.iter().flat_map(|t| t.players_iter()).map(|p| normalize_name(Some(&p.name))).collect();
        for r in rows.iter_mut() {
            let n = normalize_name(r.get("name").and_then(|v| v.as_str()));
            let tag = if fa.contains(&n) {
                json!("fa")
            } else if rostered.contains(&n) {
                json!("rostered")
            } else {
                Value::Null
            };
            r.insert("in_league".into(), tag);
        }
    }
    if as_json {
        json_out(json!({"fallers": a.fallers, "players": rows}), lc.as_ref(), &errors);
        return Ok(());
    }
    let what = if a.fallers { "fallers" } else { "risers" };
    if rows.is_empty() {
        say(format!("No ESPN {what} available."));
    } else {
        let mut tbl = Table::new(format!("ESPN % rostered {what} (all ESPN leagues, last 7 days)"));
        tbl.cols(&[("#", R), ("Player", L), ("Pos", L), ("NHL", L), ("Status", L), ("% ros", R), ("Change", R), ("% start", R), ("In my league", L)]);
        for (i, r) in rows.iter().enumerate() {
            let s = |k: &str| r.get(k).and_then(|v| v.as_str()).filter(|x| !x.is_empty()).map(String::from);
            let f = |k: &str| r.get(k).and_then(|v| v.as_f64());
            let positions: Vec<String> =
                r.get("positions").and_then(|v| v.as_array()).into_iter().flatten().filter_map(|x| x.as_str()).filter(|x| *x != "F").map(String::from).collect();
            let where_ = match r.get("in_league").and_then(|v| v.as_str()) {
                Some("fa") => "free agent",
                Some("rostered") => "rostered",
                _ => "-",
            };
            tbl.row([
                (i + 1).to_string(),
                s("name").unwrap_or_default(),
                if positions.is_empty() { "-".to_string() } else { positions.join("/") },
                s("team").unwrap_or_else(|| "-".into()),
                s("status").unwrap_or_else(|| "-".into()),
                f("pct_owned").map(|v| fixed(v, 1, false)).unwrap_or_else(|| "-".into()),
                fixed(f("pct_owned_change").unwrap_or(0.0), 1, true),
                f("pct_started").map(|v| fixed(v, 1, false)).unwrap_or_else(|| "-".into()),
                where_.to_string(),
            ]);
        }
        tbl.print();
    }
    say("Source: ESPN public player pool (fantasy.espn.com, cached 6h).");
    match &lc {
        Some(lc) => footer(lc, &errors),
        None => {
            for e in &errors {
                say(format!("! {e}"));
            }
        }
    }
    Ok(())
}

// --------------------------------------------------------------------------- lines

fn dfo_client(cache: &fm_http::HttpCache) -> DailyFaceoffClient<'_> {
    DailyFaceoffClient::new(Box::new(make_cached_fetch_text(cache, None, None)))
}

fn unit_label(x: Option<&str>) -> String {
    match x {
        Some(s) if !s.is_empty() => s.to_uppercase(),
        _ => "-".to_string(),
    }
}

#[derive(Args, Clone, Debug)]
pub struct LinesArgs {
    /// NHL team (e.g. EDM); default: my players.
    pub team: Option<String>,
    #[command(flatten)]
    pub lo: LeagueOpts,
}

pub fn lines_cmd(cli: &mut Cli, a: &LinesArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    if let Some(team) = a.team.as_deref().filter(|t| !t.is_empty()) {
        return lines_team(cli, &league, as_json, team);
    }
    let d = cli.load_all(&league, false, None)?;
    let lc = &d.lc;
    let mut players: Vec<&Player> = lc.my_team()?.players_iter().collect();
    players.sort_by(|x, y| {
        (x.is_goalie(), x.line.as_deref().unwrap_or("zz"), x.name.as_str()).cmp(&(y.is_goalie(), y.line.as_deref().unwrap_or("zz"), y.name.as_str()))
    });
    let rows: Vec<Value> = players
        .iter()
        .map(|p| {
            json!({"cid": p.cid, "name": p.name, "pos": pos(p), "team": p.team, "line": p.line, "pp_unit": p.pp_unit,
                   "pk_unit": p.pk_unit, "line_change": p.line_change, "confirmed_start": p.confirmed_start,
                   "start_source": p.start_source, "toi_per_game": p.toi_per_game, "pp_share": p.pp_share})
        })
        .collect();
    if as_json {
        json_out(json!({"players": rows}), Some(lc), &d.errors);
        return Ok(());
    }
    let mut tbl = Table::new(format!("{}: lines and power-play units (Daily Faceoff)", lc.my_team()?.name));
    tbl.cols(&[("Player", L), ("Pos", L), ("NHL", L), ("Line", L), ("PP", L), ("PK", L), ("TOI/GP", R), ("PP share", R), ("Change", L)]);
    for p in &players {
        let mut line = unit_label(p.line.as_deref());
        if p.line.as_deref() == Some("g") {
            if let Some(c) = p.confirmed_start {
                line = if c { "G (starts tonight)".into() } else { "G (not starting)".into() };
            }
        }
        tbl.row([
            p.name.clone(),
            pos(p),
            p.team.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| "-".into()),
            line,
            unit_label(p.pp_unit.as_deref()),
            unit_label(p.pk_unit.as_deref()),
            fmtv(p.toi_per_game),
            p.pp_share.map(pct0).unwrap_or_else(|| "-".into()),
            p.line_change.clone().filter(|c| !c.is_empty()).unwrap_or_else(|| "-".into()),
        ]);
    }
    tbl.print();
    if !players.iter().any(|p| p.line.as_deref().is_some_and(|l| !l.is_empty())) {
        say("No Daily Faceoff lines matched your players (offline, or the pages failed).");
    }
    footer(lc, &d.errors);
    Ok(())
}

fn lines_team(cli: &mut Cli, league: &str, as_json: bool, team: &str) -> Result<()> {
    let abbrev = normalize_team(Some(team));
    let Some(abbrev) = abbrev.filter(|a| TEAM_SLUGS.iter().any(|(t, _)| t == a)) else {
        return Err(fail(format!("unknown NHL team {team:?}; use an abbreviation like EDM, TOR or VGK")));
    };
    let cache = cli.cache()?;
    let mut errors: Vec<String> = Vec::new();
    let client = dfo_client(&cache);
    let tl = safe(&mut errors, &format!("Daily Faceoff {abbrev}"), || client.team_lines(&abbrev))?;
    let Some(tl) = tl else {
        return Err(fail(errors.first().cloned().unwrap_or_else(|| format!("Daily Faceoff lines for {abbrev} unavailable"))));
    };
    let today = cli.today();
    let store = LineSnapshotStore::new(cli.data_dir());
    let prev = store.previous(today);
    let mut by_team = Map::new();
    by_team.insert(abbrev.clone(), tl.clone());
    let cur = build_snapshot(today, &by_team, &[], &Map::new(), cli.now_utc());
    let changes = match &prev {
        Some((_, snap)) => diff_snapshots(Some(snap), &cur),
        None => Map::new(),
    };
    let mine: Vec<String> = match cli.load_context(league, None) {
        Ok((lc0, _)) => lc0.my_team().map(|t| t.players_iter().map(|p| normalize_name(Some(&p.name))).collect()).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    const GROUPS: [(&str, &str); 13] = [
        ("F1", "f1"),
        ("F2", "f2"),
        ("F3", "f3"),
        ("F4", "f4"),
        ("D1", "d1"),
        ("D2", "d2"),
        ("D3", "d3"),
        ("G", "g"),
        ("PP1", "pp1"),
        ("PP2", "pp2"),
        ("PK1", "pk1"),
        ("PK2", "pk2"),
        ("IR / out", "ir"),
    ];
    let key = |lp: &fm_sources::dailyfaceoff::LinePlayer| -> String {
        match lp.dfo_id {
            Some(id) => id.to_string(),
            None => format!("{abbrev}:{}", normalize_name(Some(&lp.name))),
        }
    };
    if as_json {
        let mut groups = serde_json::Map::new();
        for (g, k) in GROUPS {
            let members = tl.by_group(k);
            if members.is_empty() {
                continue;
            }
            let v: Vec<Value> = members
                .iter()
                .map(|lp| {
                    json!({"name": lp.name, "position": lp.position, "change": changes.get(&key(lp)),
                           "mine": mine.contains(&normalize_name(Some(&lp.name))), "injury": lp.injury_status})
                })
                .collect();
            groups.insert(g.to_string(), Value::Array(v));
        }
        dump(&json!({"team": abbrev, "updated_at": tl.updated_at.map(|d| d.py_str()), "source": tl.source,
                     "source_url": tl.source_url, "previous_snapshot": prev.as_ref().map(|p| p.0.to_string()),
                     "groups": Value::Object(groups), "errors": errors}));
        return Ok(());
    }
    let mut tbl = Table::new(format!("{abbrev} line combinations (Daily Faceoff)")).lines();
    tbl.cols(&[("Unit", L), ("Players", L)]);
    for (label, k) in GROUPS {
        let members = tl.by_group(k);
        if members.is_empty() {
            continue;
        }
        let cells: Vec<String> = members
            .iter()
            .map(|lp| {
                let mut name = lp.name.clone();
                if mine.contains(&normalize_name(Some(&lp.name))) {
                    name.push_str(" (mine)");
                }
                match changes.get(&key(lp)).filter(|c| !c.is_empty()) {
                    Some(ch) => format!("{name} ({ch})"),
                    None => name,
                }
            })
            .collect();
        tbl.row([label.to_string(), cells.join(", ")]);
    }
    tbl.print();
    let when = tl.updated_at.map(|d| strftime(d.local, "%b %d %H:%M UTC")).unwrap_or_else(|| "update time n/a".into());
    let since = match &prev {
        Some((d, _)) => format!("; changes vs snapshot {d}"),
        None => "; no earlier snapshot to compare".to_string(),
    };
    say(format!("{DFO_CREDIT}. Source: {}, updated {when}{since}.", tl.source.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "Daily Faceoff".into())));
    for e in &errors {
        say(format!("! {e}"));
    }
    Ok(())
}

// --------------------------------------------------------------------------- goalies

#[derive(Args, Clone, Debug)]
pub struct GoaliesArgs {
    /// Goalie start plan for the rest of the matchup: start probability, matchup and expected points per game, who to start each day, streaming goalies.
    #[arg(long = "week")]
    pub week: bool,
    #[command(flatten)]
    pub lo: LeagueOpts,
}

pub fn goalies_cmd(cli: &mut Cli, a: &GoaliesArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let d = cli.load_all(&league, false, None)?;
    let lc = &d.lc;
    let mut errors = d.errors.clone();
    if a.week {
        return goalie_week_out(cli, &d, as_json, errors);
    }
    let client = dfo_client(&d.cache);
    let mut starts = safe(&mut errors, "Daily Faceoff starting goalies", || client.starting_goalies(lc.as_of))?.unwrap_or_default();
    let mine: Vec<&Player> = lc.my_team()?.players_iter().filter(|p| p.is_goalie()).collect();
    let mine_names: Vec<String> = mine.iter().map(|p| normalize_name(Some(&p.name))).collect();
    let mut mine_rows: Vec<Value> = Vec::new();
    for p in &mine {
        let team = normalize_team(p.team.as_deref());
        let plays = lc.schedule.get(team.as_deref().unwrap_or("")).is_some_and(|ds| ds.contains(&lc.as_of));
        let pn = normalize_name(Some(&p.name));
        let listed = starts.iter().find(|x| normalize_name(Some(&x.goalie_name)) == pn);
        let state = if !lc.schedule.is_empty() && !plays {
            "no game tonight".to_string()
        } else if p.confirmed_start == Some(true) {
            "starting".to_string()
        } else if p.confirmed_start == Some(false) {
            "not starting".to_string()
        } else if let Some(l) = listed {
            format!("listed, {}", l.strength.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "unconfirmed".into()).to_lowercase())
        } else {
            "unknown".to_string()
        };
        mine_rows.push(json!({"cid": p.cid, "name": p.name, "team": p.team,
                              "plays_tonight": if lc.schedule.is_empty() { Value::Null } else { json!(plays) },
                              "state": state, "source": p.start_source}));
    }
    if as_json {
        json_out(
            json!({"day": lc.as_of.to_string(), "starts": starts.iter().map(to_json).collect::<Vec<_>>(), "my_goalies": mine_rows}),
            Some(lc),
            &errors,
        );
        return Ok(());
    }
    starts.sort_by(|x, y| {
        let kx = x.game_time.and_then(|t| t.to_utc());
        let ky = y.game_time.and_then(|t| t.to_utc());
        // None sorts last (datetime.max)
        let ord = match (kx, ky) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        ord.then_with(|| x.game.cmp(&y.game)).then_with(|| x.home.cmp(&y.home))
    });
    if starts.is_empty() {
        say("No starting-goalie reports for today yet.");
    } else {
        let mut tbl = Table::new(format!("Starting goalies {} (Daily Faceoff)", strftime_date(lc.as_of, "%a %b %d")));
        tbl.cols(&[("Game", L), ("Team", L), ("Goalie", L), ("Status", L), ("Reported by", L)]);
        for s in &starts {
            let mut name = s.goalie_name.clone();
            if mine_names.contains(&normalize_name(Some(&s.goalie_name))) {
                name = format!("{name} (mine)");
            }
            let when = s.created_at.map(|c| strftime(c.local, "%H:%M UTC")).unwrap_or_default();
            let by = [s.source.clone().unwrap_or_default(), when].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" ");
            tbl.row([
                s.game.clone(),
                s.team.clone(),
                name,
                s.strength.clone().filter(|x| !x.is_empty()).unwrap_or_else(|| "unconfirmed".into()),
                if by.is_empty() { "-".into() } else { by },
            ]);
        }
        tbl.print();
    }
    let mut mt = Table::new("My goalies tonight");
    mt.cols(&[("Goalie", L), ("NHL", L), ("Tonight", L), ("Source", L)]);
    for r in &mine_rows {
        mt.row([
            r["name"].as_str().unwrap_or("").to_string(),
            r["team"].as_str().filter(|t| !t.is_empty()).unwrap_or("-").to_string(),
            r["state"].as_str().unwrap_or("").to_string(),
            r["source"].as_str().filter(|t| !t.is_empty()).unwrap_or("-").to_string(),
        ]);
    }
    mt.print();
    say("The starting-goalies page is cached 3h; run again about an hour before puck drop for late confirmations.");
    footer(lc, &errors);
    Ok(())
}

/// `_goalie_week_out(d, as_json)`.
fn goalie_week_out(cli: &Cli, d: &fm_providers::pipeline::Loaded, as_json: bool, mut errors: Vec<String>) -> Result<()> {
    use fm_engine::analysis::goalie_plan::{goalie_week, GoalieWeekOpts};
    let lc = &d.lc;
    let opts = GoalieWeekOpts { data_dir: Some(cli.data_dir().to_path_buf()), dynasty_values: d.dyn_values.as_ref(), ..GoalieWeekOpts::default() };
    let gw = safe(&mut errors, "goalie plan", || Ok(goalie_week(lc, &d.values, &opts, &cli.now(), &cli.memo())))?;
    let Some(gw) = gw else {
        if as_json {
            json_out(json!({"goalie_week": Value::Null}), Some(lc), &errors);
        } else {
            footer(lc, &errors);
        }
        return Ok(());
    };
    if as_json {
        let mut data = to_json(&gw);
        if let Value::Object(o) = &mut data {
            o.remove("streams");
            for (key, rows) in [("mine", &gw.mine), ("free_agents", &gw.free_agents)] {
                if let Some(Value::Array(raw)) = o.get_mut(key) {
                    for (r, raw) in rows.iter().zip(raw.iter_mut()) {
                        if let Value::Object(ro) = raw {
                            ro.insert("exp_total".into(), json!(round_to(r.exp_total(), 2)));
                            ro.insert("starts".into(), json!(round_to(r.starts(), 2)));
                        }
                    }
                }
            }
            o.insert("exp_total".into(), json!(round_to(gw.exp_total(), 2)));
            o.insert("streams".into(), recs_json(&gw.streams));
        }
        json_out(json!({"goalie_week": data}), Some(lc), &errors);
        return Ok(());
    }
    let span = match (gw.days.first(), gw.days.last()) {
        (Some(a), Some(b)) => format!("{} - {}", strftime_date(*a, "%a %b %d"), strftime_date(*b, "%a %b %d")),
        _ => "no days left".to_string(),
    };
    for (label, rows) in [("My goalies", &gw.mine), ("Best free-agent goalies", &gw.free_agents)] {
        if rows.is_empty() {
            continue;
        }
        let mut t = Table::new(format!("{label}, rest of the matchup ({span})"));
        t.cols(&[("Goalie", L), ("Day", L), ("Opp", L), ("Start", R), ("Why", L), ("Win", R), ("Opp goals", R), ("Pts if starts", R), ("Exp pts", R)]);
        for r in rows {
            let mut head = format!("{} ({})", r.name, r.team.as_deref().filter(|t| !t.is_empty()).unwrap_or("FA"));
            if r.status.as_str() != "healthy" {
                head.push_str(&format!(" {}", r.status.as_str()));
            }
            if r.games.is_empty() {
                t.row([head.clone(), "-".into(), "-".into(), "-".into(), "no games left he can play".into(), "-".into(), "-".into(), "-".into(), "0.0".into()]);
            }
            for (k, g) in r.games.iter().enumerate() {
                t.row([
                    if k == 0 { head.clone() } else { String::new() },
                    strftime_date(g.day, "%a %d"),
                    g.opponent.clone(),
                    pct0(g.start_prob),
                    g.start_source.clone(),
                    g.win_prob.map(pct0).unwrap_or_else(|| "-".into()),
                    g.opp_goals.map(|v| fixed(v, 2, false)).unwrap_or_else(|| "-".into()),
                    fixed(g.pts_if_starts, 1, false),
                    fixed(g.exp_pts, 1, false),
                ]);
            }
            if !r.games.is_empty() {
                t.row([String::new(), String::new(), String::new(), fixed(r.starts(), 1, false), "expected starts".into(), String::new(), String::new(), String::new(), fixed(r.exp_total(), 1, false)]);
            }
        }
        t.print();
    }
    let names: Map<&str, &str> = gw.mine.iter().map(|r| (r.cid.as_str(), r.name.as_str())).collect();
    let plural = if gw.g_slots != 1 { "s" } else { "" };
    let mut pt = Table::new(format!("Who to start ({} G slot{plural})", gw.g_slots));
    pt.cols(&[("Day", L), ("Start", L), ("Sit (game, no slot)", L), ("Open G slots", L)]);
    for day in &gw.plan {
        let fmt = |cid: &String| -> String {
            let g = gw.row(cid).and_then(|r| r.games.iter().find(|x| x.day == day.day));
            match g {
                Some(g) => format!("{} {} ({})", names.get(cid.as_str()).copied().unwrap_or(""), fixed(g.exp_pts, 1, false), pct0(g.start_prob)),
                None => names.get(cid.as_str()).copied().unwrap_or("").to_string(),
            }
        };
        let start = day.start.iter().map(fmt).collect::<Vec<_>>().join(", ");
        let sit = day.sit.iter().map(fmt).collect::<Vec<_>>().join(", ");
        pt.row([
            strftime_date(day.day, "%a %b %d"),
            if start.is_empty() { "-".into() } else { start },
            if sit.is_empty() { "-".into() } else { sit },
            day.open_slots.to_string(),
        ]);
    }
    pt.print();
    say(format!("Expected goalie points from your G slots: {}", fixed(gw.exp_total(), 1, false)));
    let stream_nights: Vec<String> = gw.plan.iter().filter(|x| x.open_slots > 0).map(|x| strftime_date(x.day, "%a")).collect();
    if !gw.streams.is_empty() {
        rec_table(&gw.streams, "Goalie streams for this matchup").print();
    } else if !stream_nights.is_empty() {
        say(format!(
            "Open G slots on {}, but no free-agent goalie clears the pickup threshold (moves left / gain).",
            stream_nights.join(", ")
        ));
    }
    for n in &gw.notes {
        say(n);
    }
    footer(lc, &errors);
    Ok(())
}

// --------------------------------------------------------------------------- news

#[derive(Args, Clone, Debug)]
pub struct NewsArgs {
    /// Your roster and free agents (default).
    #[arg(long = "mine", overrides_with = "all")]
    pub mine: bool,
    /// Every player in the league.
    #[arg(long = "all", overrides_with = "mine")]
    pub all: bool,
    /// Max news items shown.
    #[arg(long = "limit", short = 'n', default_value_t = 30)]
    pub limit: i64,
    #[command(flatten)]
    pub lo: LeagueOpts,
}

fn pub_key(n: Option<&fm_sources::news::NewsItem>) -> i128 {
    // `n.published or datetime.min`: naive values are treated as UTC
    n.and_then(|n| n.published).map(|p| p.local.and_utc().timestamp_micros() as i128 - p.offset_secs.unwrap_or(0) as i128 * 1_000_000).unwrap_or(i128::MIN)
}

pub fn news_cmd(cli: &mut Cli, a: &NewsArgs) -> Result<()> {
    let (league, as_json) = cli.opts(&a.lo);
    let mine_only = !a.all;
    let d = cli.load_all(&league, false, None)?;
    let lc = &d.lc;
    let mut errors = d.errors.clone();
    let mut owner: Map<String, String> = Map::new();
    for p in &lc.free_agents {
        owner.entry(p.cid.clone()).or_insert_with(|| "FA".into());
    }
    for t in &lc.teams {
        if mine_only && !t.owner_is_me {
            continue;
        }
        for p in t.players_iter() {
            owner.insert(p.cid.clone(), if t.owner_is_me { "me".to_string() } else { t.name.clone() });
        }
    }
    let players: Vec<&Player> = lc.all_players().into_iter().filter(|p| owner.contains_key(&p.cid)).collect();
    let items = fetch_news(cli, &d.cache, &mut errors);
    let by_cid = match_news(&items, &players);
    let mut by_player: Map<&str, &Player> = Map::new();
    for p in &players {
        by_player.insert(p.cid.as_str(), p);
    }
    let sort_key = |c: &String| (pub_key(by_cid[c].first()), owner.get(c).map(|o| o == "me").unwrap_or(false));
    let mut order: Vec<&String> = by_cid.keys().collect();
    order.sort_by(|x, y| sort_key(y).cmp(&sort_key(x)));
    let mut shown = 0usize;
    let mut groups: Vec<(&Player, Vec<&fm_sources::news::NewsItem>)> = Vec::new();
    for cid in order {
        if shown as i64 >= a.limit {
            break;
        }
        let Some(p) = by_player.get(cid.as_str()) else { continue };
        let room = (a.limit - shown as i64).max(0) as usize;
        let take: Vec<&fm_sources::news::NewsItem> = by_cid[cid].iter().take(room).collect();
        shown += take.len();
        groups.push((p, take));
    }
    if as_json {
        let players_json: Vec<Value> = groups
            .iter()
            .map(|(p, its)| {
                json!({"cid": p.cid, "name": p.name, "owner": owner.get(&p.cid),
                       "items": its.iter().map(|n| Value::Object(n.model_dump_json())).collect::<Vec<_>>()})
            })
            .collect();
        json_out(json!({"items_fetched": items.len(), "players": players_json}), Some(lc), &errors);
        return Ok(());
    }
    if groups.is_empty() {
        let what = if mine_only { "your roster or free agents" } else { "league players" };
        say(format!("No news for {what}{}", if items.is_empty() { " (no news could be fetched)." } else { "." }));
    }
    let now = cli.now_utc();
    for (p, its) in groups {
        let who = match owner.get(&p.cid).map(String::as_str) {
            Some("me") => "my team".to_string(),
            Some("FA") => "free agent".to_string(),
            Some(o) => o.to_string(),
            None => String::new(),
        };
        say(format!("{} ({}, {}) - {who} {}", p.name, pos(p), p.team.as_deref().filter(|t| !t.is_empty()).unwrap_or("-"), status(p)));
        for n in its {
            let tags = if n.tags.is_empty() { String::new() } else { format!(" [{}]", n.tags.join(", ")) };
            say(format!("  {} {}{tags} {}", age_text(n.published.and_then(|p| p.to_utc()), now), n.source, n.headline));
            if !n.blurb.is_empty() {
                let chars: Vec<char> = n.blurb.chars().collect();
                let blurb = if chars.len() <= 240 { n.blurb.clone() } else { format!("{}...", chars[..237].iter().collect::<String>().trim_end()) };
                say(format!("    {blurb}"));
            }
        }
    }
    footer(lc, &errors);
    Ok(())
}

// --------------------------------------------------------------------------- ask

#[derive(Args, Clone, Debug)]
pub struct AskArgs {
    /// Your question, e.g. "should I trade X for Y?"
    pub question: String,
    #[command(flatten)]
    pub lo: LeagueOpts,
}

pub fn ask_cmd(cli: &mut Cli, a: &AskArgs) -> Result<()> {
    use fm_sources::llm::context::NewsInput;
    let (league, as_json) = cli.opts(&a.lo);
    let client = llm_client(&cli.settings);
    if !client.available() {
        return Err(fail("`fm ask` needs an LLM: set OPENROUTER_API_KEY in .env (get a key at openrouter.ai; FM_LLM_MODEL=openrouter/free works without credits)."));
    }
    let d = cli.load_all(&league, false, None)?;
    let lc = &d.lc;
    let mut errors = d.errors.clone();
    let history = cli.history()?;
    let recs = run_advise(cli, &d, &History::Store(&history), &mut errors, Some(15))?;
    drop(history);
    let items = fetch_news(cli, &d.cache, &mut errors);
    let answer = match fm_sources::llm::openrouter::ask(&a.question, lc, &d.values, &recs, NewsInput::Items(&items), Some(&client), 12000) {
        Ok(x) => x,
        Err(e) if e.downcast_ref::<fm_sources::llm::openrouter::LLMError>().is_some() => return Err(fail(e.to_string())),
        Err(e) => return Err(e),
    };
    if as_json {
        json_out(json!({"question": a.question, "answer": answer, "model": client.model}), Some(lc), &errors);
        return Ok(());
    }
    say(&answer);
    say(format!("model: {}", client.model));
    footer(lc, &errors);
    Ok(())
}

// --------------------------------------------------------------------------- report

#[derive(Args, Clone, Debug)]
pub struct ReportArgs {
    /// Output folder (default: <FM_DATA_DIR>/reports).
    #[arg(long = "out", short = 'o')]
    pub out: Option<PathBuf>,
    /// Post the summary to Discord/Slack webhooks.
    #[arg(long = "notify")]
    pub notify: bool,
    /// Add short LLM explanations (needs OPENROUTER_API_KEY).
    #[arg(long = "explain")]
    pub explain: bool,
    /// Save today's injury statuses as the baseline for the next run.
    #[arg(long = "record", overrides_with = "no_record")]
    pub record: bool,
    /// Do not save today's injury statuses.
    #[arg(long = "no-record", overrides_with = "record")]
    pub no_record: bool,
    /// Max recommendations in the digest.
    #[arg(long = "limit", short = 'n', default_value_t = 25)]
    pub limit: usize,
    /// Also fetch NHL game logs for recent form.
    #[arg(long = "deep")]
    pub deep: bool,
    #[command(flatten)]
    pub mode: ModeOpt,
    #[command(flatten)]
    pub lo: LeagueOpts,
}

fn webhooks_configured(s: &fm_core::config::Settings) -> bool {
    let set = |x: &Option<fm_core::config::Secret>| x.as_ref().is_some_and(|v| !v.expose().is_empty());
    set(&s.discord_webhook_url) || set(&s.slack_webhook_url)
}

pub fn report(cli: &mut Cli, a: &ReportArgs) -> Result<()> {
    use fm_report::digest::{build_digest, model_headline, model_health, write_digest};
    use fm_report::notify::{any_failed, notify_all};

    let (league, as_json) = cli.opts(&a.lo);
    let record = !a.no_record;
    let d = cli.load_all(&league, a.deep, a.mode.val())?;
    let lc = &d.lc;
    let mut errors = d.errors.clone();
    let history = cli.history()?;
    let mut recs = run_advise(cli, &d, &History::Store(&history), &mut errors, Some(a.limit))?;
    if record {
        let _ = safe(&mut errors, "injury snapshot", || history.record(lc.teams.iter().flat_map(|t| t.players_iter()), Some(cli.clock.now)))?;
    }
    drop(history);
    let items = fetch_news(cli, &d.cache, &mut errors);
    let mut notes: Vec<String> = Vec::new();
    if a.explain {
        explain(cli, &mut recs, &d, &mut errors, Some(items.clone()), Some(&mut notes));
    }
    let all_players = lc.all_players();
    let by_cid = match_news(&items, &all_players);
    let data_dir = cli.data_dir().to_path_buf();
    let week_plan = safe(&mut errors, "weekly plan", || {
        let opts = PlanOpts {
            dynasty_values: d.dyn_values.as_ref(),
            recs: Some(&recs),
            provider: Some(d.provider.as_ref()),
            data_dir: Some(&data_dir),
            ..PlanOpts::default()
        };
        let wp = build_week_plan(lc, &d.values, &opts, &cli.now(), &cli.memo());
        let lines = plan_summary(&wp, lc.as_of);
        Ok(if lines.is_empty() { None } else { Some(lines) })
    })?
    .flatten();
    let headline = model_headline(&data_dir, &lc.provider);
    let health = model_health(&data_dir, &lc.provider);
    // Python: generated_at = datetime.now() (naive local): the stamp, title date and file name
    let digest = build_digest(lc, &d.values, &recs, Some(&by_cid), &cli.now_local(), headline.as_deref(), health.as_deref(), week_plan.as_deref());
    let out_dir = a.out.clone().unwrap_or_else(|| data_dir.join("reports"));
    let (md_path, html_path) = match write_digest(&digest, &out_dir) {
        Ok(p) => p,
        Err(e) => return Err(fail(format!("could not write the report to {}: {e}", out_dir.display()))),
    };
    let results: Vec<String> = if a.notify { notify_all(&cli.settings, &digest.summary, None) } else { Vec::new() };
    let failed = a.notify && (any_failed(&results) || !webhooks_configured(&cli.settings));
    if as_json {
        json_out(
            json!({"markdown": md_path.to_string_lossy(), "html": html_path.to_string_lossy(), "summary": digest.summary,
                   "recommendations": recs.len(), "notify": results, "notes": notes}),
            Some(lc),
            &errors,
        );
    } else {
        say(format!("Wrote {}", md_path.display()));
        say(format!("Wrote {}", html_path.display()));
        say(format!("{} recommendation(s), {} matched news item(s).", recs.len(), by_cid.values().map(|v| v.len()).sum::<usize>()));
        for r in &results {
            say(r);
        }
        for n in &notes {
            say(n);
        }
        footer(lc, &errors);
    }
    if failed {
        return Err(Exit(1).into());
    }
    Ok(())
}

// --------------------------------------------------------------------------- notify

#[derive(Args, Clone, Debug)]
pub struct NotifyArgs {
    /// Message to send (useful for testing webhooks).
    pub text: String,
    /// Print JSON instead of tables.
    #[arg(long = "json")]
    pub json: bool,
}

pub fn notify(cli: &mut Cli, a: &NotifyArgs) -> Result<()> {
    use fm_report::notify::{any_failed, notify_all};
    let as_json = cli.as_json(a.json);
    let results = notify_all(&cli.settings, &a.text, None);
    let configured = webhooks_configured(&cli.settings);
    if as_json {
        dump(&json!({"results": results, "configured": configured}));
    } else {
        for r in &results {
            say(r);
        }
    }
    if !configured || any_failed(&results) {
        return Err(Exit(1).into());
    }
    Ok(())
}

impl crate::app::Recs {
    pub fn commands(&self) -> Vec<crate::app::CommandSpec> {
        vec![
            crate::command_spec!("trades", 80, TradesArgs, "Trade proposals ranked by expected value: your lineup gain x the chance they accept, plus the sweet spot and exploits.", trades),
            crate::command_spec!("trade-eval", 90, TradeEvalArgs, "Evaluate a specific trade offer: your lineup gain, theirs, fairness, dynasty value, market check and a verdict.", trade_eval),
            crate::command_spec!("flags", 100, FlagsArgs, "Sell-high (your hot, lucky players) and buy-low (cold players with intact shot rates).", flags),
            crate::command_spec!("advise", 110, AdviseArgs, "Everything at once: injuries, lineup, waivers, trades and flags, ranked.", advise_cmd),
            crate::command_spec!("alerts", 120, AlertsArgs, "Every alert engine: line / PP-unit changes, confirmed goalie starts, role changes and rising free agents.", alerts_cmd),
            crate::command_spec!("trending", 130, TrendingArgs, "ESPN-wide % rostered risers (or fallers) this week, tagged free agent / rostered when in the chosen league.", trending_cmd),
            crate::command_spec!("lines", 140, LinesArgs, "Daily Faceoff line combinations and power-play units: one NHL team's lines, or all of my players'.", lines_cmd),
            crate::command_spec!("goalies", 150, GoaliesArgs, "Tonight's starting goalies (Daily Faceoff, cached 3h) and whether my goalies start.", goalies_cmd),
            crate::command_spec!("news", 160, NewsArgs, "Player news (RotoWire, ESPN) matched to your roster and free agents.", news_cmd),
            crate::command_spec!("ask", 170, AskArgs, "Ask the LLM a question about your league (needs OPENROUTER_API_KEY).", ask_cmd),
            crate::command_spec!("report", 180, ReportArgs, "Write the daily digest (markdown + HTML) and optionally post a summary to webhooks.", report),
            crate::command_spec!("notify", 190, NotifyArgs, "Send a message to the configured Discord/Slack webhooks.", notify),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[derive(Parser)]
    struct T<A: Args + Send + Sync + 'static> {
        #[command(flatten)]
        a: A,
    }

    #[test]
    fn split_names_handles_commas_and_blanks() {
        let v = split_names(&["A, B".to_string(), " ".to_string(), "C".to_string()]);
        assert_eq!(v, vec!["A", "B", "C"]);
    }

    #[test]
    fn args_parse() {
        T::<TradesArgs>::command().debug_assert();
        T::<TradeEvalArgs>::command().debug_assert();
        T::<NewsArgs>::command().debug_assert();
        T::<ReportArgs>::command().debug_assert();
        let t = T::<TradesArgs>::parse_from(["t", "--need", "-n", "4", "--exploits", "0"]);
        assert!(t.a.need && t.a.limit == 4 && t.a.exploit_limit == 0);
        assert!(!T::<NewsArgs>::parse_from(["t"]).a.all);
        assert!(T::<NewsArgs>::parse_from(["t", "--all"]).a.all);
        assert!(!T::<NewsArgs>::parse_from(["t", "--all", "--mine"]).a.all);
        assert!(T::<ReportArgs>::parse_from(["t", "--no-record"]).a.no_record);
    }

    #[test]
    fn gain_text_parts() {
        let r = Recommendation::new(fm_core::models::RecKind::Trade, 1.0, "t");
        assert_eq!(trade_gain_text(&r), "-");
    }
}
