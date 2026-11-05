//! Rust port of `fantasy_manager/report/digest.py`: the daily digest, markdown + self-contained
//! HTML + short plain-text summary for chat webhooks. Owner: work package `W9-report-pregame`.
//!
//! Deviations: `generated_at` is an explicit parameter (Python defaulted to `datetime.now()`);
//! `values` is the real `Map<Cid, PlayerValue>` (only `fpg` is read); `write_digest` writes LF
//! (Python's `write_text` on Windows wrote CRLF). `build_digest`'s `data_health` call has no
//! try/except (the Rust port cannot raise there).

use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use fm_core::models::{LeagueContext, Player, Recommendation, RecKind};
use fm_core::{Cid, Map, PyDateTime};
use fm_pycompat::fmt::fixed;
use fm_pycompat::order::cmp_f64;
use fm_pycompat::text::{py_len, py_rstrip, py_slice, py_strip};
use fm_sources::news::NewsItem;
use fm_sources::news_match::player_news_summary;

use crate::health::{data_health, should_alert, DataHealth, Overall};

pub const SUMMARY_MAX: usize = 1500;
pub const ALERT_STATUSES: [&str; 5] = ["dtd", "out", "ir", "ltir", "suspended"];
pub const MONEYPUCK_CREDIT: &str = "Expected goals: MoneyPuck.com";
pub const DFO_CREDIT: &str = "Lines, power-play units and starting goalies: Daily Faceoff (dailyfaceoff.com)";
pub const SUMMARY_PROBLEM_LINES: usize = 4;

/// (section title, rec kinds) in display order after the headline and injury sections
pub const REC_SECTIONS: [(&str, &[RecKind]); 5] = [
    ("Lineup", &[RecKind::Lineup]),
    ("Waivers", &[RecKind::Waiver]),
    ("Trades", &[RecKind::Trade]),
    ("Flags", &[RecKind::SellHigh, RecKind::BuyLow]),
    ("Alerts", &[RecKind::Alert]),
];

/// Trade lines: the reasons a reader needs (recommend.trades); the solver detail stays in the app.
pub const TRADE_REASONS: [&str; 8] = [
    "MY_EDGE",
    "MARKET_VIEW",
    "THEIR_NEED",
    "TRADE_BLOCK",
    "ROSTER_CONSEQUENCE",
    "SWEET_SPOT",
    "WIN_NOW_COST",
    "POSITION_CAP",
];

pub fn status_label(status: &str) -> &str {
    match status {
        "dtd" => "day-to-day",
        "out" => "out",
        "ir" => "IR",
        "ltir" => "LTIR",
        "suspended" => "suspended",
        other => other,
    }
}

pub fn kind_label(kind: RecKind) -> &'static str {
    match kind {
        RecKind::Lineup => "Lineup",
        RecKind::Waiver => "Waiver",
        RecKind::Trade => "Trade",
        RecKind::SellHigh => "Sell high",
        RecKind::BuyLow => "Buy low",
        RecKind::Injury => "Injury",
        RecKind::Alert => "Alert",
    }
}

#[derive(Clone, Debug)]
pub struct Digest {
    pub markdown: String,
    pub html: String,
    pub summary: String,
    pub generated_at: Option<PyDateTime>,
    /// data health of this run (report.health) and whether it warrants an alert (a failed source,
    /// or a stale source that feeds valuation); callers may notify on `alert`
    pub health: Option<DataHealth>,
    pub alert: bool,
    /// provider name, used in the output filename
    pub league: Option<String>,
}

impl Digest {
    /// `Digest.day` (Python fell back to `datetime.now()` without `generated_at`; callers always
    /// pass it here).
    pub fn day(&self) -> Option<NaiveDate> {
        self.generated_at.map(|g| g.date())
    }
}

// --------------------------------------------------------------------------- data gathering

struct Alert<'a> {
    player: &'a Player,
    slot: Option<&'a str>,
    fpg: Option<f64>,
}

fn ranked(recs: &[Recommendation]) -> Vec<&Recommendation> {
    let mut v: Vec<&Recommendation> = recs.iter().collect();
    v.sort_by(|a, b| cmp_f64(-a.score, -b.score));
    v
}

fn fpg(values: &Map<Cid, fm_core::values::PlayerValue>, cid: &str) -> Option<f64> {
    values.get(cid).map(|v| v.fpg)
}

fn injury_alerts<'a>(
    ctx: &'a LeagueContext,
    values: &Map<Cid, fm_core::values::PlayerValue>,
    recs: &[&'a Recommendation],
) -> (Vec<&'a Recommendation>, Vec<Alert<'a>>) {
    let inj_recs: Vec<&Recommendation> = recs.iter().copied().filter(|r| r.kind == RecKind::Injury).collect();
    let covered: std::collections::HashSet<&str> =
        inj_recs.iter().flat_map(|r| r.add.iter().chain(r.drop.iter())).map(|p| p.cid.as_str()).collect();
    let mut alerts: Vec<Alert> = Vec::new();
    if let Ok(team) = ctx.my_team() {
        for s in &team.slots {
            if let Some(p) = s.player.as_ref() {
                if ALERT_STATUSES.contains(&p.status.as_str()) && !covered.contains(p.cid.as_str()) {
                    alerts.push(Alert { player: p, slot: Some(s.slot.as_str()), fpg: fpg(values, &p.cid) });
                }
            }
        }
    }
    alerts.sort_by(|a, b| {
        cmp_f64(-a.fpg.unwrap_or(0.0), -b.fpg.unwrap_or(0.0)).then_with(|| a.player.name.cmp(&b.player.name))
    });
    (inj_recs, alerts)
}

type NewsRow<'a> = (&'a Player, Vec<NewsItem>);

fn news_for_me<'a>(ctx: &'a LeagueContext, news_by_cid: &Map<String, Vec<NewsItem>>) -> Vec<NewsRow<'a>> {
    let mut rows: Vec<NewsRow> = match ctx.my_team() {
        Ok(t) => t
            .players_iter()
            .map(|p| (p, news_by_cid.get(&p.cid).cloned().unwrap_or_default()))
            .filter(|(_, items)| !items.is_empty())
            .collect(),
        Err(_) => vec![],
    };
    rows.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    rows
}

fn team_or_fa(p: &Player) -> &str {
    p.team.as_deref().filter(|t| !t.is_empty()).unwrap_or("FA")
}

fn alert_text(a: &Alert) -> String {
    let st = a.player.status.as_str();
    let mut second = status_label(st).to_string();
    if let Some(n) = a.player.status_note.as_deref().filter(|n| !n.is_empty()) {
        second.push_str(&format!(": {n}"));
    }
    let first = format!("{} ({}, {})", a.player.name, a.player.positions.join("/"), team_or_fa(a.player));
    let mut extra: Vec<String> = Vec::new();
    if let Some(s) = a.slot.filter(|s| !s.is_empty()) {
        extra.push(format!("slot {s}"));
    }
    if let Some(f) = a.fpg {
        extra.push(format!("{} FPG healthy", fixed(f, 2, false)));
    }
    let base = format!("{first} - {second}");
    if extra.is_empty() {
        base
    } else {
        format!("{base} ({})", extra.join(", "))
    }
}

/// My goalies with a Daily Faceoff start report for today: (player, "starting" / "not starting").
fn starts(ctx: &LeagueContext) -> Vec<(&Player, &'static str)> {
    let mut out: Vec<(&Player, &'static str)> = Vec::new();
    if let Ok(t) = ctx.my_team() {
        for p in t.players_iter() {
            if p.is_goalie() {
                if let Some(cs) = p.confirmed_start {
                    out.push((p, if cs { "starting" } else { "not starting" }));
                }
            }
        }
    }
    out.sort_by(|a, b| (a.1 != "starting").cmp(&(b.1 != "starting")).then_with(|| a.0.name.cmp(&b.0.name)));
    out
}

fn start_text(p: &Player, state: &str) -> String {
    let src = match p.start_source.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!(" - {s}"),
        None => String::new(),
    };
    format!("{} ({}): {state}{src}", p.name, team_or_fa(p))
}

/// Credits owed for data used in this digest (MoneyPuck xG, Daily Faceoff lines / starts).
pub fn credits(ctx: &LeagueContext) -> Vec<String> {
    let players = ctx.all_players();
    let mut out = Vec::new();
    if players.iter().any(|p| p.ixg_per_game.is_some()) {
        out.push(MONEYPUCK_CREDIT.to_string());
    }
    if players.iter().any(|p| {
        p.line.as_deref().is_some_and(|s| !s.is_empty())
            || p.pp_unit.as_deref().is_some_and(|s| !s.is_empty())
            || p.confirmed_start.is_some()
    }) {
        out.push(DFO_CREDIT.to_string());
    }
    out
}

// --------------------------------------------------------------------------- markdown

/// Reasons to print: for trades scored by acceptance (a MY_EDGE reason), only `TRADE_REASONS`.
fn reasons(r: &Recommendation) -> Vec<&fm_core::models::Reason> {
    if r.kind == RecKind::Trade && r.reasons.iter().any(|x| x.code == "MY_EDGE") {
        return r.reasons.iter().filter(|x| TRADE_REASONS.contains(&x.code.as_str())).collect();
    }
    r.reasons.iter().collect()
}

fn md_rec(r: &Recommendation, level: &str) -> Vec<String> {
    let cp = match r.counterparty.as_deref().filter(|c| !c.is_empty()) {
        Some(c) => format!(" (with {c})"),
        None => String::new(),
    };
    let mut out = vec![
        format!("{level} {}{cp}", r.title),
        String::new(),
        format!("*{} - score {}*", kind_label(r.kind), fixed(r.score, 2, false)),
        String::new(),
    ];
    if let Some(n) = r.narrative.as_deref().filter(|n| !n.is_empty()) {
        out.push(format!("> {n}"));
        out.push(String::new());
    }
    out.extend(reasons(r).into_iter().map(|x| format!("- {}", x.text)));
    out.push(String::new());
    out
}

fn why(r: &Recommendation) -> String {
    match r.narrative.as_deref().filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => r.reasons.first().map(|x| x.text.clone()).unwrap_or_default(),
    }
}

fn md_headline(r: &Recommendation, i: usize) -> String {
    let w = why(r);
    format!("{i}. **{}** ({}, score {})", r.title, kind_label(r.kind), fixed(r.score, 2, false))
        + &if w.is_empty() { String::new() } else { format!(" - {w}") }
}

/// The harness headline ("Model: ..."), or None while nothing is trustworthy.
fn model_line(model: Option<&str>) -> Option<String> {
    let m = model.filter(|m| !m.is_empty())?;
    let m = py_strip(m);
    Some(if m.starts_with("Model:") { m.to_string() } else { format!("Model: {m}") })
}

fn problems(data: Option<&DataHealth>) -> bool {
    data.is_some_and(|d| d.overall != Overall::Ok)
}

fn md_data_health(data: &DataHealth) -> Vec<String> {
    let mut md = vec!["## \u{26a0} Data health".to_string(), String::new(), format!("**{}**", data.headline.as_deref().unwrap_or("")), String::new()];
    md.extend(data.problem_lines.iter().map(|l| format!("- {l}")));
    md.push(String::new());
    if let Some(f) = data.footer.as_deref().filter(|f| !f.is_empty()) {
        md.push(format!("*{f}*"));
        md.push(String::new());
    }
    md
}

struct Parts<'a> {
    ctx: &'a LeagueContext,
    ranked: &'a [&'a Recommendation],
    inj_recs: &'a [&'a Recommendation],
    alerts: &'a [Alert<'a>],
    news_rows: &'a [NewsRow<'a>],
    generated_at: &'a PyDateTime,
    model: Option<&'a str>,
    health: Option<&'a [String]>,
    data: Option<&'a DataHealth>,
    plan: Option<&'a [String]>,
}

fn stamp(g: &PyDateTime) -> String {
    g.local.format("%Y-%m-%d %H:%M").to_string()
}

fn datestamp(g: &PyDateTime) -> String {
    g.local.format("%Y-%m-%d").to_string()
}

fn markdown(a: &Parts) -> String {
    let ctx = a.ctx;
    let my_name = ctx.my_team().map(|t| t.name.as_str()).unwrap_or("");
    let mut md: Vec<String> = vec![
        format!("# Fantasy digest - {}", ctx.name),
        String::new(),
        format!("*{} league {} - team {} - generated {}*", ctx.provider, ctx.league_id, my_name, stamp(a.generated_at)),
        String::new(),
    ];
    if let Some(d) = a.data.filter(|d| problems(Some(d))) {
        md.extend(md_data_health(d));
    }
    if let Some(m) = model_line(a.model) {
        md.push(format!("*{m}*"));
        md.push(String::new());
    }
    if let Some(plan) = a.plan.filter(|p| !p.is_empty()) {
        md.extend(["## This week".to_string(), String::new(), format!("**{}**", plan[0]), String::new()]);
        md.extend(plan[1..].iter().map(|l| format!("- {l}")));
        md.push(String::new());
    }
    md.push("## Headline".to_string());
    md.push(String::new());
    if a.ranked.is_empty() {
        md.push("No moves recommended today.".to_string());
    } else {
        md.extend(a.ranked.iter().take(3).enumerate().map(|(i, r)| md_headline(r, i + 1)));
    }
    md.extend(["".to_string(), "## Injury alerts".to_string(), String::new()]);
    if a.inj_recs.is_empty() && a.alerts.is_empty() {
        md.push("_No injury concerns on your roster._".to_string());
        md.push(String::new());
    }
    for r in a.inj_recs {
        md.extend(md_rec(r, "###"));
    }
    if !a.alerts.is_empty() {
        md.extend(a.alerts.iter().map(|x| format!("- {}", alert_text(x))));
        md.push(String::new());
    }
    let st = starts(ctx);
    if !st.is_empty() {
        md.extend(["## Confirmed starts tonight".to_string(), String::new()]);
        md.extend(st.iter().map(|(p, s)| format!("- {}", start_text(p, s))));
        md.push(String::new());
    }
    for (title, kinds) in REC_SECTIONS {
        md.push(format!("## {title}"));
        md.push(String::new());
        let items: Vec<&&Recommendation> = a.ranked.iter().filter(|r| kinds.contains(&r.kind)).collect();
        if items.is_empty() {
            md.push("_None today._".to_string());
            md.push(String::new());
        }
        for r in items {
            md.extend(md_rec(r, "###"));
        }
    }
    md.push("## News for my players".to_string());
    md.push(String::new());
    if a.news_rows.is_empty() {
        md.push("_No recent news for your players._".to_string());
        md.push(String::new());
    }
    for (p, items) in a.news_rows {
        md.push(format!("**{}**", p.name));
        md.push(String::new());
        md.extend(player_news_summary(items, 3, 200).into_iter().map(|l| format!("- {l}")));
        let url = items.iter().take(3).filter_map(|n| n.url.as_deref()).find(|u| !u.is_empty());
        if let Some(u) = url {
            md.push(format!("  - source: <{u}>"));
        }
        md.push(String::new());
    }
    if let Some(h) = a.health.filter(|h| !h.is_empty()) {
        md.extend(["## Model health".to_string(), String::new()]);
        md.extend(h.iter().map(|l| format!("- {l}")));
        md.push(String::new());
    }
    if let Some(d) = a.data {
        if !problems(Some(d)) {
            if let Some(f) = d.footer.as_deref().filter(|f| !f.is_empty()) {
                md.push(format!("*{f}*"));
                md.push(String::new());
            }
        }
    }
    let cr = credits(ctx);
    if !cr.is_empty() {
        md.push(format!("*Data: {}*", cr.join("; ")));
        md.push(String::new());
    }
    format!("{}\n", py_rstrip(&md.join("\n")))
}

// --------------------------------------------------------------------------- html

pub const CSS: &str = r#"
:root{--bg:#f7f7f8;--card:#fff;--fg:#1b1d21;--muted:#5f6670;--accent:#1a5fb4;--border:#dcdfe4;
--warn:#b54708;--warn-bg:#fff4e5;--bad:#b42318;--bad-bg:#fdeceb}
@media (prefers-color-scheme: dark){:root{--bg:#121417;--card:#1c1f24;--fg:#e8eaed;--muted:#9aa1ab;
--accent:#78aeed;--border:#2e333a;--warn:#f5a454;--warn-bg:#2a2016;--bad:#ff8a7e;--bad-bg:#331714}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--fg);
font:16px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif}
main{max-width:720px;margin:0 auto;padding:16px}
h1{font-size:1.4rem;margin:.2em 0}
h2{font-size:1.1rem;margin:1.6em 0 .6em;padding-bottom:.25em;border-bottom:1px solid var(--border)}
.meta,.empty,.score{color:var(--muted);font-size:.9rem}
.card{background:var(--card);border:1px solid var(--border);border-radius:10px;padding:12px 14px;margin:10px 0}
.card h3{font-size:1rem;margin:0 0 4px}
.narr{margin:6px 0;font-style:italic}
ul{margin:6px 0;padding-left:1.2em}
li{margin:2px 0}
.tag{display:inline-block;font-size:.75rem;padding:1px 8px;border-radius:999px;border:1px solid var(--border);
color:var(--muted);margin-right:6px}
.alert{background:var(--warn-bg);border-color:var(--warn)}
.alert strong{color:var(--warn)}
ol.head{padding-left:1.4em}
.data-health{background:var(--bad-bg);border:2px solid var(--bad)}
.data-health h2{color:var(--bad);border:0;margin:0 0 6px;padding:0}
.data-health .dh-lead{font-weight:600;margin:0 0 4px}
ol.head li{margin:6px 0}
"#;

/// `html.escape(str(s), quote=True)`.
pub fn e(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
    out
}

fn html_rec(r: &Recommendation, cls: &str) -> String {
    let cp = match r.counterparty.as_deref().filter(|c| !c.is_empty()) {
        Some(c) => format!(" <span class=\"score\">with {}</span>", e(c)),
        None => String::new(),
    };
    let mut parts = vec![
        format!("<div class=\"{cls}\">"),
        format!("<h3>{}{cp}</h3>", e(&r.title)),
        format!("<div class=\"score\"><span class=\"tag\">{}</span>score {}</div>", e(kind_label(r.kind)), fixed(r.score, 2, false)),
    ];
    if let Some(n) = r.narrative.as_deref().filter(|n| !n.is_empty()) {
        parts.push(format!("<p class=\"narr\">{}</p>", e(n)));
    }
    if !r.reasons.is_empty() {
        let lis: String = reasons(r).into_iter().map(|x| format!("<li>{}</li>", e(&x.text))).collect();
        parts.push(format!("<ul>{lis}</ul>"));
    }
    parts.push("</div>".to_string());
    parts.concat()
}

fn html_data_health(data: &DataHealth) -> String {
    let headline = data.headline.as_deref().unwrap_or("");
    let lead = headline.replacen("\u{26a0} Data problems: ", "", 1);
    let lis: String = data.problem_lines.iter().map(|l| format!("<li>{}</li>", e(l))).collect();
    let mut parts = vec![
        "<section class=\"card data-health\" role=\"alert\">".to_string(),
        "<h2>\u{26a0} Data problems</h2>".to_string(),
        format!("<p class=\"dh-lead\">{}</p>", e(&lead)),
        format!("<ul>{lis}</ul>"),
    ];
    if let Some(f) = data.footer.as_deref().filter(|f| !f.is_empty()) {
        parts.push(format!("<p class=\"meta\">{}</p>", e(f)));
    }
    parts.concat() + "</section>"
}

fn html(a: &Parts) -> String {
    let ctx = a.ctx;
    let my_name = ctx.my_team().map(|t| t.name.as_str()).unwrap_or("");
    let mut b: Vec<String> = vec![
        format!("<h1>Fantasy digest - {}</h1>", e(&ctx.name)),
        format!(
            "<div class=\"meta\">{} league {} - team {} - generated {}</div>",
            e(&ctx.provider),
            e(&ctx.league_id),
            e(my_name),
            stamp(a.generated_at)
        ),
    ];
    if let Some(d) = a.data.filter(|d| problems(Some(d))) {
        b.push(html_data_health(d));
    }
    if let Some(m) = model_line(a.model) {
        b.push(format!("<div class=\"meta model\">{}</div>", e(&m)));
    }
    if let Some(plan) = a.plan.filter(|p| !p.is_empty()) {
        b.push("<h2>This week</h2>".to_string());
        let lis: String = plan[1..].iter().map(|l| format!("<li>{}</li>", e(l))).collect();
        b.push(format!("<div class=\"card\"><h3>{}</h3><ul>{lis}</ul></div>", e(&plan[0])));
    }
    b.push("<h2>Headline</h2>".to_string());
    if !a.ranked.is_empty() {
        b.push("<ol class=\"head\">".to_string());
        for r in a.ranked.iter().take(3) {
            let w = why(r);
            b.push(format!(
                "<li><strong>{}</strong> <span class=\"score\">({}, score {})</span>{}</li>",
                e(&r.title),
                e(kind_label(r.kind)),
                fixed(r.score, 2, false),
                if w.is_empty() { String::new() } else { format!("<br>{}", e(&w)) }
            ));
        }
        b.push("</ol>".to_string());
    } else {
        b.push("<p class=\"empty\">No moves recommended today.</p>".to_string());
    }
    b.push("<h2>Injury alerts</h2>".to_string());
    if a.inj_recs.is_empty() && a.alerts.is_empty() {
        b.push("<p class=\"empty\">No injury concerns on your roster.</p>".to_string());
    }
    b.extend(a.inj_recs.iter().map(|r| html_rec(r, "card alert")));
    if !a.alerts.is_empty() {
        let lis: String = a.alerts.iter().map(|x| format!("<li>{}</li>", e(&alert_text(x)))).collect();
        b.push(format!("<div class=\"card alert\"><ul>{lis}</ul></div>"));
    }
    let st = starts(ctx);
    if !st.is_empty() {
        b.push("<h2>Confirmed starts tonight</h2>".to_string());
        let lis: String = st.iter().map(|(p, s)| format!("<li>{}</li>", e(&start_text(p, s)))).collect();
        b.push(format!("<div class=\"card\"><ul>{lis}</ul></div>"));
    }
    for (title, kinds) in REC_SECTIONS {
        b.push(format!("<h2>{}</h2>", e(title)));
        let items: Vec<String> = a.ranked.iter().filter(|r| kinds.contains(&r.kind)).map(|r| html_rec(r, "card")).collect();
        if items.is_empty() {
            b.push("<p class=\"empty\">None today.</p>".to_string());
        } else {
            b.extend(items);
        }
    }
    b.push("<h2>News for my players</h2>".to_string());
    if a.news_rows.is_empty() {
        b.push("<p class=\"empty\">No recent news for your players.</p>".to_string());
    }
    for (p, items) in a.news_rows {
        let lis: String = player_news_summary(items, 3, 200).iter().map(|l| format!("<li>{}</li>", e(l))).collect();
        b.push(format!("<div class=\"card\"><h3>{}</h3><ul>{lis}</ul></div>", e(&p.name)));
    }
    if let Some(h) = a.health.filter(|h| !h.is_empty()) {
        b.push("<h2>Model health</h2>".to_string());
        let lis: String = h.iter().map(|l| format!("<li>{}</li>", e(l))).collect();
        b.push(format!("<ul class=\"model-health\">{lis}</ul>"));
    }
    if let Some(d) = a.data {
        if !problems(Some(d)) {
            if let Some(f) = d.footer.as_deref().filter(|f| !f.is_empty()) {
                b.push(format!("<p class=\"meta data-fresh\">{}</p>", e(f)));
            }
        }
    }
    let cr = credits(ctx);
    if !cr.is_empty() {
        b.push(format!("<p class=\"meta credits\">Data: {}</p>", e(&cr.join("; "))));
    }
    let title = format!("Fantasy digest - {} - {}", ctx.name, datestamp(a.generated_at));
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<meta name=\"color-scheme\" content=\"light dark\">\
<title>{}</title><style>{CSS}</style></head>\
<body><main>\n{}\n</main></body></html>\n",
        e(&title),
        b.join("\n")
    )
}

// --------------------------------------------------------------------------- summary

fn summary(a: &Parts) -> String {
    let ctx = a.ctx;
    let mut lines: Vec<String> = Vec::new();
    if let Some(d) = a.data.filter(|d| problems(Some(d))) {
        // lead with the failures: phones preview the first lines
        lines.push(d.headline.clone().unwrap_or_default());
        let shown = &d.problem_lines[..d.problem_lines.len().min(SUMMARY_PROBLEM_LINES)];
        lines.extend(shown.iter().map(|l| format!("- {l}")));
        if d.problem_lines.len() > shown.len() {
            lines.push(format!("- ... and {} more (see the digest)", d.problem_lines.len() - shown.len()));
        }
    }
    lines.push(format!("Fantasy digest: {} ({})", ctx.name, datestamp(a.generated_at)));
    if let Some(p) = a.plan {
        // the week plan leads: win prob, moves left, today's actions
        lines.extend(p.iter().cloned());
    }
    if let Some(m) = model_line(a.model) {
        lines.push(m);
    }
    if !a.ranked.is_empty() {
        lines.push("Top moves:".to_string());
        for (i, r) in a.ranked.iter().take(3).enumerate() {
            let w = why(r);
            lines.push(
                format!("{}. {} (score {})", i + 1, r.title, fixed(r.score, 2, false))
                    + &if w.is_empty() { String::new() } else { format!(" - {w}") },
            );
        }
    } else {
        lines.push("No moves recommended today.".to_string());
    }
    let mut injured: Vec<String> = a.inj_recs.iter().map(|r| r.title.clone()).collect();
    injured.extend(a.alerts.iter().map(|x| format!("{} ({})", x.player.name, status_label(x.player.status.as_str()))));
    if !injured.is_empty() {
        let mut seen = std::collections::HashSet::new();
        let uniq: Vec<&String> = injured.iter().filter(|s| seen.insert(s.as_str())).collect();
        lines.push(format!("Injury alerts ({}): {}", injured.len(), uniq.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("; ")));
    }
    let st = starts(ctx);
    if !st.is_empty() {
        lines.push(format!("Goalies tonight: {}", st.iter().map(|(p, s)| format!("{} {s}", p.name)).collect::<Vec<_>>().join("; ")));
    }
    let counts: Vec<String> = REC_SECTIONS
        .iter()
        .map(|(title, kinds)| format!("{title} {}", a.ranked.iter().filter(|r| kinds.contains(&r.kind)).count()))
        .collect();
    lines.push(format!("Counts: {}", counts.join(" | ")));
    if !a.news_rows.is_empty() {
        lines.push(format!("News: {}", a.news_rows.iter().map(|(p, _)| p.name.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if let Some(d) = a.data {
        if !problems(Some(d)) {
            if let Some(f) = d.footer.as_deref().filter(|f| !f.is_empty()) {
                let mut cs = f.chars();
                let first = cs.next().map(|c| c.to_lowercase().collect::<String>()).unwrap_or_default();
                lines.push(format!("Data: {first}{}", cs.as_str()));
            }
        }
    }
    let text = lines.join("\n");
    if py_len(&text) > SUMMARY_MAX {
        return format!("{}\u{2026}", py_rstrip(&py_slice(&text, None, Some(SUMMARY_MAX as isize - 1))));
    }
    text
}

// --------------------------------------------------------------------------- public API

/// Render the daily digest. `values` maps cid -> PlayerValue (only `fpg` is read).
/// `model_headline` is the harness line (`harness::metrics::headline`); None hides it.
/// `model_health` is the 3-line "Model health" block (`harness::health::health_block`) for the
/// markdown / HTML digest (not the webhook summary); None hides it.
///
/// Data health (`report::health::data_health` over `ctx.sources`): when a source failed or went
/// stale, a red "Data problems" block opens the markdown / HTML and the summary leads with it;
/// otherwise a one-line freshness footer. `Digest.alert` is `report::health::should_alert`.
pub fn build_digest(
    ctx: &LeagueContext,
    values: &Map<Cid, fm_core::values::PlayerValue>,
    recs: &[Recommendation],
    news_by_cid: Option<&Map<String, Vec<NewsItem>>>,
    generated_at: &PyDateTime,
    model_headline: Option<&str>,
    model_health: Option<&[String]>,
    week_plan: Option<&[String]>,
) -> Digest {
    let empty = Map::new();
    let news_by_cid = news_by_cid.unwrap_or(&empty);
    let ranked = ranked(recs);
    let (inj_recs, alerts) = injury_alerts(ctx, values, &ranked);
    let news_rows = news_for_me(ctx, news_by_cid);
    let health = model_health.filter(|h| !h.is_empty());
    let data = data_health(ctx, None);
    let plan = week_plan.filter(|p| !p.is_empty());
    let parts = Parts {
        ctx,
        ranked: &ranked,
        inj_recs: &inj_recs,
        alerts: &alerts,
        news_rows: &news_rows,
        generated_at,
        model: model_headline,
        health,
        data: Some(&data),
        plan,
    };
    let (md, ht, sm) = (markdown(&parts), html(&parts), summary(&parts));
    let alert = should_alert(&data);
    Digest {
        markdown: md,
        html: ht,
        summary: sm,
        generated_at: Some(*generated_at),
        health: Some(data),
        alert,
        league: Some(ctx.provider.clone()),
    }
}

/// The harness headline for `league` from `<data_dir>/harness.db` (None when there is no ledger
/// yet, nothing is trustworthy, or reading fails: the digest never breaks on it).
pub fn model_headline(data_dir: impl AsRef<Path>, league: &str) -> Option<String> {
    use fm_harness::ledger::{Ledger, DB_NAME};
    if !data_dir.as_ref().join(DB_NAME).exists() {
        return None;
    }
    let led = Ledger::open(data_dir.as_ref()).ok()?;
    fm_harness::metrics::headline(&led, league).ok().flatten()
}

/// The 3-line "Model health" block for `league` (MAE trend, hit rate, params version), or None
/// while nothing is trustworthy / there is no ledger / reading fails.
pub fn model_health(data_dir: impl AsRef<Path>, league: &str) -> Option<Vec<String>> {
    use fm_harness::ledger::{Ledger, DB_NAME};
    if !data_dir.as_ref().join(DB_NAME).exists() {
        return None;
    }
    let led = Ledger::open(data_dir.as_ref()).ok()?;
    fm_harness::health::health_block(&led, league).ok().flatten()
}

/// Write `digest-<league>-YYYY-MM-DD.md` and `.html` into `out_dir` (created if needed). The
/// league is part of the name so ESPN and Fantrax digests on the same day don't overwrite each other.
pub fn write_digest(digest: &Digest, out_dir: impl AsRef<Path>) -> anyhow::Result<(PathBuf, PathBuf)> {
    let out = out_dir.as_ref();
    std::fs::create_dir_all(out)?;
    let day = digest.day().map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
    let stem = match digest.league.as_deref().filter(|l| !l.is_empty()) {
        Some(l) => format!("digest-{l}-{day}"),
        None => format!("digest-{day}"),
    };
    let (md_path, html_path) = (out.join(format!("{stem}.md")), out.join(format!("{stem}.html")));
    std::fs::write(&md_path, &digest.markdown)?;
    std::fs::write(&html_path, &digest.html)?;
    Ok((md_path, html_path))
}
