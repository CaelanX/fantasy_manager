//! Rust port of `fantasy_manager/recommend/headline.py`: one plain-English sentence per
//! recommendation, the deciding reason shown before the numbers. Owner: `W5-recommend-base`.
//!
//! Python lets `IndexError` escape `headline_for` when a BREAK_EVEN reason has empty text (and
//! `apply_headlines` then leaves the headline None); here `headline_for` returns None in that
//! one case.

use fm_core::models::{GainUnits, Player, RecKind, Reason, Recommendation};
use fm_pycompat::fmt::{fixed, percent};
use fm_pycompat::text::{lstrip_chars, py_len, py_lower, py_rstrip, py_slice, py_slice_str, py_upper, strip_chars};
use fm_pycompat::time::strftime_date;

pub const MAX_LEN: usize = 140;

/// `IndexError` stand-in (see module docs).
struct Raised;

fn find<'a>(rec: &'a Recommendation, code: &str) -> Option<&'a Reason> {
    rec.reasons.iter().find(|x| x.code == code)
}

fn val(rec: &Recommendation, code: &str) -> Option<f64> {
    find(rec, code).and_then(|x| x.value)
}

fn player_reason<'a>(rec: &'a Recommendation, code: &str, p: &Player) -> Option<&'a Reason> {
    let prefix = format!("{}:", p.name);
    rec.reasons.iter().find(|x| x.code == code && x.text.starts_with(&prefix))
}

fn cut(text: &str) -> String {
    if py_len(text) <= MAX_LEN {
        return text.to_string();
    }
    let head = py_slice(text, None, Some(MAX_LEN as isize - 1));
    format!("{}\u{2026}", py_rstrip(&head))
}

/// " · market agrees" / " · model disagrees with market" / " · market says you overpay" / "".
pub fn market_suffix(rec: &Recommendation) -> &'static str {
    let checks: Vec<&Reason> = rec.reasons.iter().filter(|x| x.code == "MARKET_CHECK").collect();
    if checks.is_empty() {
        return "";
    }
    if checks.iter().any(|x| x.value.is_none() && x.text.starts_with("Model disagrees")) {
        return " \u{b7} market says you overpay";
    }
    if checks.iter().any(|x| x.text.to_lowercase().contains("disagrees")) {
        return " \u{b7} model disagrees with market";
    }
    " \u{b7} market agrees"
}

/// Daily start/sit: the deciding fact first (no game / not expected to play / more points).
pub fn day_swap_headline(
    pin: Option<&Player>,
    pout: Option<&Player>,
    pin_pts: f64,
    pout_pts: f64,
    pout_plays: bool,
    slot: Option<&str>,
    opp: Option<&str>,
) -> String {
    let where_ = match opp.filter(|o| !o.is_empty()) {
        Some(o) => format!("plays {o}"),
        None => "has a game".to_string(),
    };
    let Some(pin) = pin else {
        return match pout {
            Some(o) => format!("{} isn't expected to score today; leave the slot empty", o.name),
            None => String::new(),
        };
    };
    let Some(pout) = pout else {
        let slot = slot.filter(|s| !s.is_empty()).map_or(String::new(), |s| format!("{s} "));
        return format!("Empty {slot}slot today: {} {where_}", pin.name);
    };
    if !pout_plays {
        return format!("{} has no game today; {} {where_}", pout.name, pin.name);
    }
    if pout_pts <= 0.0 {
        return format!("{} isn't expected to play today; {} {where_}", pout.name, pin.name);
    }
    format!("{} projects {} pts today vs {} for {}", pin.name, fixed(pin_pts, 1, false), fixed(pout_pts, 1, false), pout.name)
}

fn lineup(rec: &Recommendation) -> String {
    let has = |c: &str| rec.reasons.iter().any(|x| x.code == c);
    if has("IR_SWAP") && !rec.add.is_empty() && !rec.subjects.is_empty() {
        let (a, s) = (&rec.add[0], &rec.subjects[0]);
        return format!("{} is back ({}); {} ({}) takes his IR slot", a.name, a.status.as_str(), s.name, s.status.as_str());
    }
    if has("RETURN_DUE") {
        let pts = val(rec, "RETURN_DUE");
        let who = rec.subjects.first().map_or("He", |s| s.name.as_str());
        let tail = match pts.filter(|p| *p != 0.0) {
            Some(p) => format!(": ~{} pts once activated", fixed(p, 1, false)),
            None => String::new(),
        };
        return format!("{who} is due back this matchup{tail}");
    }
    let pin = rec.add.first();
    let pout = rec.drop.first();
    if pin.is_some() || pout.is_some() {
        let di = pin.and_then(|p| player_reason(rec, "DAY_VALUE", p));
        let do_ = pout.and_then(|p| player_reason(rec, "DAY_VALUE", p));
        if di.is_some() || do_.is_some() {
            return day_swap_headline(
                pin,
                pout,
                di.map_or(0.0, |r| r.value.unwrap_or(0.0)),
                do_.map_or(0.0, |r| r.value.unwrap_or(0.0)),
                do_.is_some_and(|r| !r.text.contains("no game today")),
                None,
                None,
            );
        }
        let gi = pin.and_then(|p| player_reason(rec, "GAMES_NEXT7", p));
        let go = pout.and_then(|p| player_reason(rec, "GAMES_NEXT7", p));
        if let (Some(pin), Some(pout), Some(gi), Some(go)) = (pin, pout, gi, go) {
            if let (Some(a), Some(b)) = (gi.value, go.value) {
                if a != b {
                    return format!("{}: {} games this week vs {} for {}", pin.name, fixed(a, 0, false), fixed(b, 0, false), pout.name);
                }
            }
        }
    }
    if has("STATUS") {
        if let (Some(pout), None) = (pout, pin) {
            return format!("{} is {}: he scores nothing from a starting slot", pout.name, pout.status.as_str());
        }
        if let (Some(pin), None) = (pin, pout) {
            if rec.title.contains("IR") {
                let h = format!("{} is {} and can play: he is wasting an IR slot", pin.name, pin.status.as_str());
                return if has("ROSTER_FULL") { h + "; roster is full, so it needs a drop or an IR swap" } else { h };
            }
        }
    }
    match val(rec, "LINEUP_GAIN") {
        Some(g) => format!("{} from this lineup change", fixed(g, 2, true)),
        None => String::new(),
    }
}

fn waiver(rec: &Recommendation) -> String {
    let drop = rec.drop.first();
    let vd = find(rec, "VORP_DELTA");
    let mp = val(rec, "MATCHUP_GAIN");
    let mut h = if let Some(mp) = mp {
        format!("{} pts over the rest of this matchup with your best daily lineup", fixed(mp, 1, true))
    } else if let Some(vd) = vd.filter(|v| v.text.starts_with("fills empty")) {
        let head = vd.text.split(" (").next().unwrap_or("");
        format!("Fills your {}", head.replace("fills ", ""))
    } else if let Some(v) = vd.and_then(|v| v.value) {
        let over = drop.map_or(String::new(), |d| format!(" over {}", d.name));
        format!("{} FPG{over} for the rest of the season", fixed(v, 2, true))
    } else {
        String::new()
    };
    if find(rec, "OPEN_SPOT").is_some() {
        h += "; no drop needed";
    } else if find(rec, "IR_MOVE").is_some() && drop.is_none() {
        let who = rec.subjects.first().map_or("an injured player", |s| s.name.as_str());
        h += &format!("; {who} goes to IR, no drop");
    }
    if find(rec, "WAIVER_CLAIM").is_some() {
        if let Some(until) = rec.add.first().and_then(|a| a.waiver_until) {
            h += &format!("; claim (on waivers until {})", strftime_date(until, "%a"));
        }
    }
    lstrip_chars(&h, "; ").to_string()
}

/// Leads with the lineup number (DELTA_ME); a dynasty blend that differs is named.
fn trade(rec: &Recommendation) -> String {
    let (edge, me, p) = (val(rec, "MY_EDGE"), val(rec, "DELTA_ME"), val(rec, "P_ACCEPT"));
    let mut h = if let (Some(g), true) = (rec.predicted_gain, rec.gain_units == Some(GainUnits::Dynasty)) {
        format!("Dynasty value {} for you", fixed(g, 2, true))
    } else if let Some(me) = me {
        let mut h = format!("Your lineup {} {} FPG", if me >= 0.0 { "gains" } else { "changes" }, fixed(me, 2, true));
        if let Some(edge) = edge {
            if (edge - me).abs() >= 0.05 {
                h += &format!(" ({} with long-term value)", fixed(edge, 2, true));
            }
        }
        h
    } else if let Some(edge) = edge {
        format!("Your lineup gains {} FPG", fixed(edge, 2, true))
    } else {
        String::new()
    };
    if let Some(p) = p {
        h += &format!("; ~{} they accept", percent(p, 0, false));
    }
    lstrip_chars(&h, "; ").to_string()
}

fn injury(rec: &Recommendation) -> Result<String, Raised> {
    if let Some(hv) = find(rec, "HOLD_VERDICT") {
        return Ok(hv.text.clone());
    }
    let (cut_pts, hold, be) = (val(rec, "CUT_PTS"), val(rec, "HOLD_PTS"), find(rec, "BREAK_EVEN"));
    if let (Some(c), Some(h)) = (cut_pts, hold) {
        let verdict = if rec.drop.is_empty() { "Holding" } else { "Cutting" };
        let diff = (c - h).abs();
        let mut s = format!("{verdict} wins by ~{} pts rest of season", fixed(diff, 0, false));
        if let Some(be) = be {
            if be.text.is_empty() {
                return Err(Raised);
            }
            s += &format!("; {}{}", py_lower(py_slice_str(&be.text, None, Some(1))), py_slice_str(&be.text, Some(1), None));
        }
        return Ok(s);
    }
    if let Some(sc) = find(rec, "STATUS_CHANGE") {
        return Ok(sc.text.clone());
    }
    if find(rec, "IR_SLOT").is_some() {
        if let Some(s) = rec.subjects.first() {
            return Ok(format!("An IR slot is free: {} costs no roster spot there", s.name));
        }
    }
    Ok(find(rec, "STATUS").map_or(String::new(), |s| s.text.clone()))
}

fn flag(rec: &Recommendation) -> String {
    let (ratio, luck) = (val(rec, "FORM_RATIO"), val(rec, "XG_LUCK"));
    if rec.kind == RecKind::SellHigh {
        let mut h = match ratio {
            Some(r) => format!("Running hot: last 15 days {}x his baseline", fixed(r, 2, false)),
            None => "Running hot".to_string(),
        };
        if luck.is_some_and(|l| l > 0.0) {
            h += ", scoring above his expected goals";
        }
        h
    } else {
        let mut h = match ratio {
            Some(r) => format!("Cold stretch: last 15 days {}x his baseline", fixed(r, 2, false)),
            None => "Cold stretch".to_string(),
        };
        if find(rec, "SOG_RATE").is_some() {
            h += "; his shots held up";
        }
        if luck.is_some_and(|l| l < 0.0) {
            h += ", finishing below expected";
        }
        h
    }
}

/// The deciding reason of `rec` in one sentence (None when nothing says more than the title).
pub fn headline_for(rec: &Recommendation) -> Option<String> {
    let h = match rec.kind {
        RecKind::Lineup => lineup(rec),
        RecKind::Waiver => waiver(rec) + market_suffix(rec),
        RecKind::Trade => trade(rec) + market_suffix(rec),
        RecKind::Injury => injury(rec).ok()?,
        RecKind::SellHigh | RecKind::BuyLow => flag(rec),
        _ => rec.reasons.first().map_or(String::new(), |r| r.text.clone()),
    };
    let h = strip_chars(&h, " \u{b7};");
    if h.is_empty() {
        return None;
    }
    let first = py_slice_str(h, None, Some(1));
    let text = format!("{}{}", py_upper(first), py_slice_str(h, Some(1), None));
    Some(cut(&text))
}

/// Set `headline` (in place) on every rec that has none; a failure leaves it None.
pub fn apply_headlines(recs: &mut [Recommendation]) {
    for r in recs.iter_mut() {
        if r.headline.is_none() {
            r.headline = headline_for(r);
        }
    }
}
