//! Cross-check of the private Fantrax load against the official API (plan step 3). Nothing here changes
//! what was loaded: each kind of fact that both sources carry is compared, and a disagreement becomes one
//! warning starting "Fantrax official API disagrees:" that names what differs. A source that is
//! missing is skipped, never reported here (the caller reports a failed official read itself).

use std::collections::{BTreeMap, BTreeSet};

use fm_core::models::{FantasyTeam, LeagueContext};
use fm_core::Map;

use crate::fantrax::{official_weights, FPTS_KEY, NON_STARTING};
use crate::fantrax_official::{
    OfficialDraftPicks, OfficialLeagueInfo, OfficialRosterStatus, OfficialRosters, OfficialScoring, OfficialStanding,
};

const PREFIX: &str = "Fantrax official API disagrees:";
/// Examples named per warning; the rest is counted ("and 3 more").
const MAX_NAMED: usize = 5;

/// The private values that official data replaced in the loaded context (scoring weights, roster shape and
/// maximum), so they can still be compared with it. Without one the context's own values are compared.
#[derive(Clone, Debug, Default)]
pub struct PrivateBasis {
    /// The Rules page's (base weights, goalie overrides); None when that page gave no scoring.
    pub scoring: Option<(Map<String, f64>, Map<String, f64>)>,
    /// The roster shape from the roster and the Rules page, before the official limits were put on it.
    pub roster_shape: Map<String, i64>,
    pub max_roster_size: Option<i64>,
}

/// The official responses that could be read; None for one that failed or does not apply.
#[derive(Default)]
pub struct OfficialData<'a> {
    pub info: Option<&'a OfficialLeagueInfo>,
    pub standings: Option<&'a [OfficialStanding]>,
    pub picks: Option<&'a OfficialDraftPicks>,
    /// Rosters of the roster period holding the load's `as_of` date.
    pub rosters: Option<&'a OfficialRosters>,
    pub private: Option<&'a PrivateBasis>,
}

fn warning(kind: &str, items: &[String]) -> String {
    let shown = items.iter().take(MAX_NAMED).cloned().collect::<Vec<_>>().join("; ");
    let more = items.len().saturating_sub(MAX_NAMED);
    if more > 0 {
        format!("{PREFIX} {kind}: {shown}; and {more} more")
    } else {
        format!("{PREFIX} {kind}: {shown}")
    }
}

fn team_name(ctx: &LeagueContext, id: &str) -> String {
    ctx.team(id).map(|t| t.name.clone()).unwrap_or_else(|| id.to_string())
}

/// Every disagreement between the private load in `ctx` and the official data, one warning per kind
/// (team ids, roster membership, roster slots, records, draft picks, scoring weights, roster limits).
pub fn disagreements(ctx: &LeagueContext, o: &OfficialData) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |kind: &str, items: Vec<String>| {
        if !items.is_empty() {
            out.push(warning(kind, &items));
        }
    };
    if let Some(info) = o.info {
        push("team ids", team_ids(ctx, info));
        push("scoring weights", scoring(ctx, o.private, info.scoring.as_ref()));
        push("roster limits", limits(ctx, o.private, info));
    }
    if let Some(rosters) = o.rosters {
        let (membership, slots) = rosters_and_slots(ctx, rosters);
        push("roster membership", membership);
        push("roster slots", slots);
    }
    if let Some(st) = o.standings {
        push("records", records(ctx, st));
    }
    if let Some(picks) = o.picks {
        push("draft picks", draft_picks(ctx, picks));
    }
    out
}

fn team_ids(ctx: &LeagueContext, info: &OfficialLeagueInfo) -> Vec<String> {
    let private: BTreeSet<&str> = ctx.teams.iter().map(|t| t.team_id.as_str()).collect();
    let official: BTreeSet<&str> = info.teams.iter().map(|t| t.id.as_str()).collect();
    let mut items = Vec::new();
    for id in private.difference(&official) {
        items.push(format!("{} ({id}) is not in the official team list", team_name(ctx, id)));
    }
    for id in official.difference(&private) {
        let name = info.teams.iter().find(|t| t.id == *id).map(|t| t.name.as_str()).unwrap_or("");
        items.push(format!("{name} ({id}) is only in the official team list"));
    }
    items
}

#[derive(PartialEq)]
enum Class {
    Active,
    Bench,
    Injured,
    /// Minors or any status the API names differently; never compared further.
    Other,
}

impl Class {
    fn label(&self) -> &'static str {
        match self {
            Class::Active => "active",
            Class::Bench => "bench",
            Class::Injured => "IR",
            Class::Other => "other",
        }
    }
}

fn private_class(slot: &str) -> Class {
    match slot {
        "BN" => Class::Bench,
        "IR" => Class::Injured,
        s if NON_STARTING.contains(&s) => Class::Other,
        _ => Class::Active,
    }
}

fn official_class(s: &OfficialRosterStatus) -> Class {
    match s {
        OfficialRosterStatus::Active => Class::Active,
        OfficialRosterStatus::Reserve => Class::Bench,
        OfficialRosterStatus::InjuredReserve => Class::Injured,
        OfficialRosterStatus::Other(_) => Class::Other,
    }
}

/// (team id, class, name) of every private player, by Fantrax id.
fn private_players(teams: &[FantasyTeam]) -> BTreeMap<String, (String, Class, String)> {
    let mut out = BTreeMap::new();
    for t in teams {
        for sl in &t.slots {
            let Some(p) = &sl.player else { continue };
            if let Some(id) = p.ids.get("fantrax") {
                out.insert(id.clone(), (t.team_id.clone(), private_class(&sl.slot), p.name.clone()));
            }
        }
    }
    out
}

/// Membership (who is on which team) and slot (active, bench or IR) differences, matched by player id.
/// Only teams both sources list are compared; a team missing from one is the team-id warning's business.
fn rosters_and_slots(ctx: &LeagueContext, o: &OfficialRosters) -> (Vec<String>, Vec<String>) {
    let private = private_players(&ctx.teams);
    let common: BTreeSet<&str> =
        ctx.teams.iter().map(|t| t.team_id.as_str()).filter(|id| o.teams.iter().any(|t| t.team_id == *id)).collect();
    let mut official: BTreeMap<&str, (&str, Class)> = BTreeMap::new();
    for t in o.teams.iter().filter(|t| common.contains(t.team_id.as_str())) {
        for i in &t.items {
            official.insert(i.player_id.as_str(), (t.team_id.as_str(), official_class(&i.status)));
        }
    }
    let name_of = |id: &str| -> String {
        ctx.free_agents
            .iter()
            .find(|p| p.ids.get("fantrax").is_some_and(|x| x == id))
            .map(|p| p.name.clone())
            .unwrap_or_else(|| format!("official player id {id}"))
    };
    let mut membership = Vec::new();
    let mut slots = Vec::new();
    for (id, (team, class, name)) in &private {
        if !common.contains(team.as_str()) {
            continue;
        }
        let mine = team_name(ctx, team);
        match official.get(id.as_str()) {
            None => membership.push(format!("{name} is on {mine} in the private data but on no team in the official data")),
            Some((ot, _)) if *ot != team.as_str() => {
                membership.push(format!("{name} is on {mine} in the private data but on {} in the official data", team_name(ctx, ot)))
            }
            Some((_, oc)) if oc != class && *oc != Class::Other && *class != Class::Other => {
                slots.push(format!("{name} is {} in the private data but {} in the official data", class.label(), oc.label()))
            }
            _ => {}
        }
    }
    for (id, (ot, _)) in &official {
        if !private.contains_key(*id) {
            membership.push(format!("{} is on {} in the official data but not in the private data", name_of(id), team_name(ctx, ot)));
        }
    }
    (membership, slots)
}

fn records(ctx: &LeagueContext, st: &[OfficialStanding]) -> Vec<String> {
    let fmt = |r: (i64, i64, i64)| format!("{}-{}-{}", r.0, r.1, r.2);
    ctx.teams
        .iter()
        .filter_map(|t| {
            let private = t.record?;
            let official = st.iter().find(|s| s.team_id == t.team_id)?.record?;
            (private != official)
                .then(|| format!("{} is {} in the private data but {} in the official data", t.name, fmt(private), fmt(official)))
        })
        .collect()
}

type PickKey = (String, i64, i64, String);

fn draft_picks(ctx: &LeagueContext, o: &OfficialDraftPicks) -> Vec<String> {
    // (holder, year, round, original owner); a private pick without an original owner is the holder's own.
    let mut private: BTreeSet<PickKey> = BTreeSet::new();
    for (holder, years) in &ctx.draft_picks {
        for (year, picks) in years {
            for pk in picks {
                private.insert((holder.clone(), *year, pk.round, pk.original_owner.clone().unwrap_or_else(|| holder.clone())));
            }
        }
    }
    if private.is_empty() {
        return Vec::new();
    }
    let official: BTreeSet<PickKey> =
        o.future.iter().chain(&o.current).map(|p| (p.owner_team_id.clone(), p.year, p.round, p.original_owner_team_id.clone())).collect();
    let describe = |(holder, year, round, orig): &PickKey| {
        format!("{year} round {round} (originally {}) held by {}", team_name(ctx, orig), team_name(ctx, holder))
    };
    let mut items = Vec::new();
    items.extend(private.difference(&official).map(|p| format!("{} in the private data only", describe(p))));
    items.extend(official.difference(&private).map(|p| format!("{} in the official data only", describe(p))));
    items
}

fn same(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => (x - y).abs() < 1e-9,
        (None, None) => true,
        _ => false,
    }
}

fn scoring(ctx: &LeagueContext, private: Option<&PrivateBasis>, official: Option<&OfficialScoring>) -> Vec<String> {
    let Some(official) = official else { return Vec::new() };
    let (private_base, private_goalie) = match private.and_then(|p| p.scoring.as_ref()) {
        Some((b, g)) => (b, g),
        None => (&ctx.scoring.weights, &ctx.scoring.goalie_weights),
    };
    // The FP/G fallback has no per-stat weights to compare.
    if private_base.contains_key(FPTS_KEY) {
        return Vec::new();
    }
    let (base, goalie, _) = official_weights(official);
    let show = |v: Option<f64>| v.map_or("not scored".to_string(), |x| x.to_string());
    let mut items = Vec::new();
    let keys: BTreeSet<&String> = private_base.keys().chain(base.keys()).collect();
    for k in keys {
        let (a, b) = (private_base.get(k).copied(), base.get(k).copied());
        if !same(a, b) {
            items.push(format!("{k} is {} in the private data but {} in the official data", show(a), show(b)));
        }
    }
    let keys: BTreeSet<&String> = private_goalie.keys().chain(goalie.keys()).collect();
    for k in keys {
        let (a, b) = (private_goalie.get(k).copied(), goalie.get(k).copied());
        if !same(a, b) {
            items.push(format!("goalie {k} override is {} in the private data but {} in the official data", show(a), show(b)));
        }
    }
    items
}

fn limits(ctx: &LeagueContext, private: Option<&PrivateBasis>, info: &OfficialLeagueInfo) -> Vec<String> {
    let Some(l) = &info.roster_limits else { return Vec::new() };
    let (shape, max_roster) = match private {
        Some(p) => (&p.roster_shape, p.max_roster_size),
        None => (&ctx.roster_shape, ctx.max_roster_size),
    };
    let mut items = Vec::new();
    if let Some(n) = max_roster.filter(|n| *n != l.max_total) {
        items.push(format!("maximum roster size is {n} in the private data but {} in the official data", l.max_total));
    }
    for (pos, max) in &l.max_active_by_position {
        if let Some(n) = shape.get(pos).filter(|n| *n != max) {
            items.push(format!("active {pos} maximum is {n} in the private data but {max} in the official data"));
        }
    }
    if let Some(n) = shape.get("BN").filter(|n| **n != l.max_reserve) {
        items.push(format!("bench maximum is {n} in the private data but {} in the official data", l.max_reserve));
    }
    items
}
