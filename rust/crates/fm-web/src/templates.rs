//! The dashboard's Jinja2 templates (`fantasy_manager/web/templates/*.html`, used in place and
//! never modified) rendered through minijinja, with the Python-isms they rely on bridged here.
//! Owner: work package `W10-web-app`.
//!
//! What Jinja2 (+ the FastAPI app's `env.filters` / `env.globals`) gave the templates, and how it
//! is provided:
//! * printing: a custom formatter prints values like Python `str()` (`None`, `True`, float
//!   `repr`) and escapes like markupsafe (`&<>"'` -> `&amp; &lt; &gt; &#34; &#39;`, minijinja would
//!   also escape `/`). Markup (`overview_summary`, chart SVGs) is a safe string.
//! * methods: an unknown-method callback adds `.strftime(fmt)` / `.isoformat()` on ISO date /
//!   datetime strings, `'{:,}'.format(n)`, `player.gp(split)`, and falls back to
//!   minijinja-contrib's pycompat (`.get`, `.items`, `.strip`, `.replace`, `.startswith`, ...).
//! * filters with Python semantics: `format` (%-formatting via `fm_pycompat::fmt`), `round`
//!   (ties to even, ints stay ints), `sum` (Python 3.12 float sum, `attribute=`), `int`,
//!   `truncate` (Jinja2's algorithm), `urlencode` / `q` (`urllib.parse.quote`), `capitalize`,
//!   and the app's own (`num`, `n1`, `rate`, `pos`, `ago`, `pct`, `action`, ...).
//! * engine objects: `res` is a [`ResObj`] over the `Arc<LoadResult>`; `res.ctx`, `res.recs`
//!   (each a [`RecObj`] that knows its index, so `strength_info(r, res.recs)` keeps Python's
//!   identity semantics), `res.values` / `res.dynasty` are lazy views cached on the LoadResult.
//!   View-model JSON (`fm_web::views`) is converted with [`to_mj`]: player maps get the
//!   `is_goalie` / `nhl_id` properties, team maps `players`, and recommendation maps found in
//!   `res.recs` become [`RecObj`]s.
//! * `{% set _ = sweet.append(x) %}` (recommendations.html) mutates a list, which minijinja
//!   cannot: [`preprocess`] rewrites it at load time to a `namespace()` accumulator.
//! * minijinja ignores `with context` on imports, and macros see the template context anyway.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use chrono::{NaiveDate, NaiveDateTime};
use fm_core::models::{LeagueContext, Recommendation};
use fm_pycompat::fmt::{fixed, percent_format, repr_f64, thousands};
use fm_pycompat::num::{py_sum_start, round_to};
use minijinja::value::{Enumerator, Kwargs, Object, ObjectRepr, Rest, ValueKind};
use minijinja::{AutoEscape, Environment, Error, ErrorKind, State, Value};
use regex::Regex;
use serde_json::Value as J;

use crate::views::{self, LoadResult};



// --------------------------------------------------------------------------- locations

/// `fantasy_manager/web/templates` (override: `FM_TEMPLATES_DIR`).
pub fn template_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("FM_TEMPLATES_DIR") {
        return PathBuf::from(d);
    }
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = here.parent().and_then(Path::parent).and_then(Path::parent).map(Path::to_path_buf).unwrap_or(here);
    root.join("fantasy_manager").join("web").join("templates")
}

// --------------------------------------------------------------------------- source rewrite

/// Load-time rewrite of Jinja2 code minijinja cannot run: a list `.append` inside
/// `{% set _ = NAME.append(ARGS) %}` becomes a `namespace()` accumulator:
/// `{% set NAME = [] %}` -> `{% set __ns_NAME = namespace(items=[]) %}`, every `NAME` up to the
/// `{% endfor %}` after the last append -> `__ns_NAME.items`, the append ->
/// `{% set __ns_NAME.items = __ns_NAME.items + [ARGS] %}`, and `{% set NAME = __ns_NAME.items %}`
/// after that `endfor`. Templates without an append are returned unchanged.
///
/// Also: a printed `{{ x.day }}` / `.month` / `.year` becomes `{{ x|__date_part('day') }}`, which
/// reads the attribute of a `date` (an ISO string here) or falls back to the plain attribute.
pub fn preprocess(src: &str) -> String {
    static APPEND: OnceLock<Regex> = OnceLock::new();
    static DATE_PART: OnceLock<Regex> = OnceLock::new();
    let append = APPEND.get_or_init(|| Regex::new(r"\{%-?\s*set\s+_\s*=\s*(\w+)\.append\((.*?)\)\s*-?%\}").unwrap());
    let date_part = DATE_PART.get_or_init(|| Regex::new(r"\{\{(\s*)([\w\.]+)\.(day|month|year)(\s*)\}\}").unwrap());
    let mut out = date_part.replace_all(src, "{{${1}${2}|__date_part('${3}')${4}}}").into_owned();
    let names: Vec<String> = {
        let mut v: Vec<String> = append.captures_iter(src).map(|c| c[1].to_string()).collect();
        v.dedup();
        v
    };
    for name in names {
        let def = Regex::new(&format!(r"\{{%-?\s*set\s+{}\s*=\s*\[\]\s*-?%\}}", regex::escape(&name))).unwrap();
        let Some(d) = def.find(&out) else { continue };
        let (d_start, d_end) = (d.start(), d.end());
        let last_append = append
            .captures_iter(&out)
            .filter(|c| c[1] == *name)
            .filter_map(|c| c.get(0).map(|m| m.end()))
            .filter(|e| *e > d_end)
            .last();
        let Some(la) = last_append else { continue };
        let endfor = Regex::new(r"\{%-?\s*endfor\s*-?%\}").unwrap();
        let Some(ef) = endfor.find_at(&out, la) else { continue };
        let seg = &out[d_end..ef.end()];
        let ns = format!("__ns_{name}");
        let seg = append
            .replace_all(seg, |c: &regex::Captures<'_>| {
                if c[1] == *name {
                    format!("{{% set \u{0}NS\u{0}.items = \u{0}NS\u{0}.items + [{}] %}}", &c[2])
                } else {
                    c[0].to_string()
                }
            })
            .into_owned();
        // a bare NAME (not an attribute like `g.trade.NAME`)
        let word = Regex::new(&format!(r"(^|[^.\w]){}\b", regex::escape(&name))).unwrap();
        let seg = word.replace_all(&seg, "${1}\u{0}NS\u{0}.items").into_owned().replace("\u{0}NS\u{0}", &ns);
        let rebuilt = format!(
            "{}{{% set {ns} = namespace(items=[]) %}}{}{{% set {name} = {ns}.items %}}{}",
            &out[..d_start],
            seg,
            &out[ef.end()..]
        );
        out = rebuilt;
    }
    out
}

// --------------------------------------------------------------------------- Python str()/escape

/// markupsafe `escape`.
pub fn escape_html(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&#34;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

fn f64_of(v: &Value) -> Option<f64> {
    match v.kind() {
        ValueKind::Number => f64::try_from(v.clone()).ok(),
        ValueKind::Bool => Some(if v.is_true() { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Python `repr()` of a template value (inside printed lists / dicts).
pub fn py_repr(v: &Value) -> String {
    match v.kind() {
        ValueKind::String => {
            let s = v.as_str().unwrap_or("");
            if s.contains('\'') && !s.contains('"') {
                format!("\"{s}\"")
            } else {
                format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
            }
        }
        _ => py_str(v),
    }
}

/// Python `str()` of a template value.
pub fn py_str(v: &Value) -> String {
    match v.kind() {
        ValueKind::Undefined => String::new(),
        ValueKind::None => "None".into(),
        ValueKind::Bool => if v.is_true() { "True" } else { "False" }.into(),
        ValueKind::Number => {
            if v.is_integer() {
                v.to_string()
            } else {
                f64::try_from(v.clone()).map(repr_f64).unwrap_or_else(|_| v.to_string())
            }
        }
        ValueKind::String => v.as_str().unwrap_or("").to_string(),
        ValueKind::Seq => {
            let items: Vec<String> = v.try_iter().map(|it| it.map(|x| py_repr(&x)).collect()).unwrap_or_default();
            format!("[{}]", items.join(", "))
        }
        ValueKind::Map if v.as_object().map(|o| o.repr() == ObjectRepr::Plain).unwrap_or(false) => v.to_string(),
        ValueKind::Map => {
            let mut parts = Vec::new();
            if let Ok(it) = v.try_iter() {
                for k in it {
                    let x = v.get_item(&k).unwrap_or_default();
                    parts.push(format!("{}: {}", py_repr(&k), py_repr(&x)));
                }
            }
            format!("{{{}}}", parts.join(", "))
        }
        _ => v.to_string(),
    }
}

// --------------------------------------------------------------------------- JSON -> minijinja

/// `Value` from view-model JSON (see the module docs for the added properties).
pub fn to_mj(v: &J, res: Option<&Arc<LoadResult>>) -> Value {
    match v {
        J::Null => Value::from(()),
        J::Bool(b) => Value::from(*b),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::from(i)
            } else if let Some(u) = n.as_u64() {
                Value::from(u)
            } else {
                Value::from(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        J::String(s) => Value::from(s.as_str()),
        J::Array(a) => Value::from(a.iter().map(|x| to_mj(x, res)).collect::<Vec<_>>()),
        J::Object(m) => {
            if let Some(r) = res {
                if is_rec_map(m) {
                    if let Some(idx) = rec_index(r, m) {
                        return RecObj::value(r, idx);
                    }
                }
            }
            let mut pairs: Vec<(Value, Value)> = m.iter().map(|(k, x)| (Value::from(k.as_str()), to_mj(x, res))).collect();
            if m.contains_key("cid") && m.contains_key("positions") && m.contains_key("name") {
                let goalie = m["positions"].as_array().is_some_and(|ps| ps.iter().any(|p| p == "G"));
                if !m.contains_key("is_goalie") {
                    pairs.push((Value::from("is_goalie"), Value::from(goalie)));
                }
                if !m.contains_key("nhl_id") {
                    let nid = m.get("ids").and_then(|i| i.get("nhl")).and_then(|x| x.as_str()).and_then(|s| s.trim().parse::<i64>().ok());
                    pairs.push((Value::from("nhl_id"), nid.map(Value::from).unwrap_or(Value::from(()))));
                }
            }
            if m.contains_key("team_id") && m.contains_key("slots") && !m.contains_key("players") {
                let players: Vec<Value> = m["slots"]
                    .as_array()
                    .map(|ss| ss.iter().filter_map(|s| s.get("player")).filter(|p| !p.is_null()).map(|p| to_mj(p, res)).collect())
                    .unwrap_or_default();
                pairs.push((Value::from("players"), Value::from(players)));
            }
            Value::from_iter(pairs)
        }
    }
}

fn is_rec_map(m: &serde_json::Map<String, J>) -> bool {
    m.contains_key("kind") && m.contains_key("title") && m.contains_key("add") && m.contains_key("drop") && m.contains_key("reasons")
}

fn cids_json(v: Option<&J>) -> String {
    v.and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|p| p.get("cid").and_then(|c| c.as_str())).collect::<Vec<_>>().join(","))
        .unwrap_or_default()
}

fn rec_ident(kind: &str, title: &str, cp: Option<&str>, add: &str, drop: &str, score: f64) -> String {
    format!("{kind}\u{1}{title}\u{1}{}\u{1}{add}\u{1}{drop}\u{1}{}", cp.unwrap_or(""), repr_f64(score))
}

fn rec_index(res: &Arc<LoadResult>, m: &serde_json::Map<String, J>) -> Option<usize> {
    let cache = mj_cache(res);
    let idx = cache.rec_ids.get_or_init(|| {
        let mut h = HashMap::new();
        for (i, r) in res.recs.iter().enumerate() {
            let add = r.add.iter().map(|p| p.cid.as_str()).collect::<Vec<_>>().join(",");
            let drop = r.drop.iter().map(|p| p.cid.as_str()).collect::<Vec<_>>().join(",");
            h.entry(rec_ident(r.kind.as_str(), &r.title, r.counterparty.as_deref(), &add, &drop, r.score)).or_insert(i);
        }
        h
    });
    let key = rec_ident(
        m.get("kind")?.as_str()?,
        m.get("title")?.as_str()?,
        m.get("counterparty").and_then(|c| c.as_str()),
        &cids_json(m.get("add")),
        &cids_json(m.get("drop")),
        m.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0),
    );
    idx.get(&key).copied()
}

/// Template values derived from a LoadResult, built once per result (kept in its memo).
#[derive(Default)]
pub struct MjCache {
    ctx: OnceLock<Value>,
    recs: OnceLock<Vec<Value>>,
    recs_v: OnceLock<Value>,
    rec_ids: OnceLock<HashMap<String, usize>>,
    values: Mutex<HashMap<String, Value>>,
    dynasty: Mutex<HashMap<String, Value>>,
}

pub fn mj_cache(res: &LoadResult) -> Arc<MjCache> {
    res.memo.get_or("w10:mj", MjCache::default)
}

fn ser<T: serde::Serialize>(t: &T) -> J {
    serde_json::to_value(t).unwrap_or(J::Null)
}

/// `res.values[cid]` as a template value (with the serde-skipped fields Python's object has).
pub fn pv_value(res: &LoadResult, cid: &str) -> Value {
    let cache = mj_cache(res);
    if let Some(v) = cache.values.lock().unwrap_or_else(|e| e.into_inner()).get(cid) {
        return v.clone();
    }
    let v = match res.values.get(cid) {
        None => Value::from(()),
        Some(pv) => {
            let mut j = ser(pv);
            if let J::Object(m) = &mut j {
                m.insert("rookie".into(), ser(&pv.rookie));
                m.insert("share_source".into(), ser(&pv.share_source));
                m.insert("share_starts".into(), ser(&pv.share_starts));
                m.insert("share_team_games".into(), ser(&pv.share_team_games));
            }
            to_mj(&j, None)
        }
    };
    cache.values.lock().unwrap_or_else(|e| e.into_inner()).insert(cid.to_string(), v.clone());
    v
}

/// `res.dynasty[cid]` as a template value (None when missing).
pub fn dyn_value_of(res: &LoadResult, cid: &str) -> Value {
    let cache = mj_cache(res);
    if let Some(v) = cache.dynasty.lock().unwrap_or_else(|e| e.into_inner()).get(cid) {
        return v.clone();
    }
    let v = match res.dynasty.as_ref().and_then(|d| d.get(cid)) {
        None => Value::from(()),
        Some(d) => to_mj(&ser(d), None),
    };
    cache.dynasty.lock().unwrap_or_else(|e| e.into_inner()).insert(cid.to_string(), v.clone());
    v
}

// --------------------------------------------------------------------------- objects

/// `res` in the templates.
#[derive(Debug)]
pub struct ResObj {
    pub res: Arc<LoadResult>,
}

impl std::fmt::Debug for LoadResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LoadResult({})", self.ctx.league_id)
    }
}

const RES_KEYS: [&str; 12] = [
    "ctx", "values", "dynasty", "recs", "news_by_cid", "loaded_at", "warnings", "mode", "mode_source", "compute_seconds",
    "recalc_only", "explanations",
];

impl ResObj {
    pub fn value(res: Arc<LoadResult>) -> Value {
        Value::from_object(ResObj { res })
    }
}

fn naive_iso(d: NaiveDateTime) -> String {
    if d.and_utc().timestamp_subsec_micros() == 0 {
        d.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        d.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
    }
}

impl Object for ResObj {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let r = &self.res;
        Some(match key.as_str()? {
            "ctx" => Value::from_object(CtxObj { res: r.clone() }),
            "values" => Value::from_object(ValuesObj { res: r.clone(), dynasty: false }),
            "dynasty" => {
                if r.dynasty.is_some() {
                    Value::from_object(ValuesObj { res: r.clone(), dynasty: true })
                } else {
                    Value::from(())
                }
            }
            "recs" => mj_cache(r).recs_v.get_or_init(|| Value::from(rec_values(r))).clone(),
            "news_by_cid" => to_mj(&J::Object(r.news_by_cid.iter().map(|(k, v)| (k.clone(), J::Array(v.clone()))).collect()), None),
            "loaded_at" => Value::from(naive_iso(r.loaded_at)),
            "warnings" => Value::from(r.warnings()),
            "mode" => r.mode.clone().map(Value::from).unwrap_or(Value::from(())),
            "mode_source" => r.mode_source.clone().map(Value::from).unwrap_or(Value::from(())),
            "compute_seconds" => r.compute_seconds.map(Value::from).unwrap_or(Value::from(())),
            "recalc_only" => Value::from(r.recalc_only),
            "explanations" => {
                let e = r.explanations.lock().unwrap_or_else(|e| e.into_inner()).clone();
                to_mj(&J::Object(e.into_iter().collect()), None)
            }
            _ => return None,
        })
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Str(&RES_KEYS)
    }
}

/// `res.ctx`: the serialized LeagueContext (cached) that also hands the real one to globals.
#[derive(Debug)]
pub struct CtxObj {
    pub res: Arc<LoadResult>,
}

impl CtxObj {
    fn inner(&self) -> Value {
        let c = mj_cache(&self.res);
        c.ctx
            .get_or_init(|| {
                let mut j = ser(&self.res.ctx);
                if let J::Object(m) = &mut j {
                    // Python property: ctx.my_team
                    if let Ok(t) = self.res.ctx.my_team() {
                        m.insert("my_team".into(), ser(t));
                    }
                }
                to_mj(&j, None)
            })
            .clone()
    }
}

impl Object for CtxObj {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        item(&self.inner(), key)
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        let inner = self.inner();
        match inner.try_iter() {
            Ok(it) => Enumerator::Values(it.collect()),
            Err(_) => Enumerator::Empty,
        }
    }
}

/// `res.values` / `res.dynasty`: lookups by cid.
#[derive(Debug)]
pub struct ValuesObj {
    pub res: Arc<LoadResult>,
    pub dynasty: bool,
}

impl Object for ValuesObj {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let cid = key.as_str()?;
        let v = if self.dynasty { dyn_value_of(&self.res, cid) } else { pv_value(&self.res, cid) };
        (!v.is_none()).then_some(v)
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        let keys: Vec<Value> = if self.dynasty {
            self.res.dynasty.as_ref().map(|d| d.keys().map(|k| Value::from(k.as_str())).collect()).unwrap_or_default()
        } else {
            self.res.values.keys().map(|k| Value::from(k.as_str())).collect()
        };
        Enumerator::Values(keys)
    }

    fn call_method(self: &Arc<Self>, _state: &State, method: &str, args: &[Value]) -> Result<Value, Error> {
        if method == "get" {
            let k = args.first().cloned().unwrap_or_default();
            return Ok(self.get_value(&k).unwrap_or_else(|| args.get(1).cloned().unwrap_or(Value::from(()))));
        }
        Err(Error::from(ErrorKind::UnknownMethod))
    }
}

/// One of `res.recs` (by index), so the globals get the real Recommendation back. Holds a weak
/// reference: these values are memoised on the LoadResult itself (no reference cycle).
#[derive(Debug)]
pub struct RecObj {
    pub res: Weak<LoadResult>,
    pub idx: usize,
    inner: Value,
}

impl RecObj {
    pub fn value(res: &Arc<LoadResult>, idx: usize) -> Value {
        let inner = to_mj(&ser(&res.recs[idx]), None);
        Value::from_object(RecObj { res: Arc::downgrade(res), idx, inner })
    }

    pub fn rec(&self) -> Option<RecRef> {
        self.res.upgrade().map(|r| RecRef::Shared(r, self.idx))
    }
}

/// A Recommendation from a template value: one of `res.recs` or a deserialized copy.
pub enum RecRef {
    Shared(Arc<LoadResult>, usize),
    Owned(Box<Recommendation>),
}

impl std::ops::Deref for RecRef {
    type Target = Recommendation;
    fn deref(&self) -> &Recommendation {
        match self {
            RecRef::Shared(r, i) => &r.recs[*i],
            RecRef::Owned(b) => b,
        }
    }
}

impl Object for RecObj {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        item(&self.inner, key)
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        match self.inner.try_iter() {
            Ok(it) => Enumerator::Values(it.collect()),
            Err(_) => Enumerator::Empty,
        }
    }
}

/// `hv` (the `health_view` module) on /health.
#[derive(Debug)]
pub struct HealthViewObj;

impl Object for HealthViewObj {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        use crate::health_view as hv;
        let table: &[(&str, &str)] = match key.as_str()? {
            "POOL_TITLE" => &hv::POOL_TITLE,
            "BASELINE_LABEL" => &hv::BASELINE_LABEL,
            "KIND_TITLE" => &hv::KIND_TITLE,
            "ORIGIN_LABEL" => &hv::ORIGIN_LABEL,
            "METRIC_LABEL" => &hv::METRIC_LABEL,
            _ => return None,
        };
        Some(Value::from_iter(table.iter().map(|(k, v)| (Value::from(*k), Value::from(*v)))))
    }

    fn call_method(self: &Arc<Self>, _state: &State, method: &str, args: &[Value]) -> Result<Value, Error> {
        let v = args.first().and_then(f64_of);
        Ok(Value::from(match method {
            "num" => {
                let nd = args.get(1).and_then(f64_of).map(|n| n as usize).unwrap_or(2);
                crate::health_view::num(v, nd)
            }
            "pct" => crate::health_view::pct(v),
            "signed_pct" => crate::health_view::signed_pct(v),
            "kind_title" => crate::health_view::kind_title(args.first().and_then(|a| a.as_str()).unwrap_or("")),
            _ => return Err(Error::from(ErrorKind::UnknownMethod)),
        }))
    }
}

// --------------------------------------------------------------------------- argument helpers

fn res_from(v: &Value) -> Option<Arc<LoadResult>> {
    if let Some(o) = v.downcast_object_ref::<ResObj>() {
        return Some(o.res.clone());
    }
    if let Some(o) = v.downcast_object_ref::<CtxObj>() {
        return Some(o.res.clone());
    }
    if let Some(o) = v.downcast_object_ref::<RecObj>() {
        return o.res.upgrade();
    }
    if let Some(o) = v.downcast_object_ref::<ValuesObj>() {
        return Some(o.res.clone());
    }
    None
}

/// The LoadResult behind any of `args`, else the context's `res`.
fn res_of(state: &State, args: &[Value]) -> Option<Arc<LoadResult>> {
    args.iter().find_map(res_from).or_else(|| state.lookup("res").as_ref().and_then(res_from))
}

/// The Recommendation behind a template value (a [`RecObj`] or a rec map).
pub fn rec_of(v: &Value) -> Option<RecRef> {
    if let Some(o) = v.downcast_object_ref::<RecObj>() {
        if let Some(r) = o.rec() {
            return Some(r);
        }
    }
    let j = serde_json::to_value(v).ok()?;
    serde_json::from_value::<Recommendation>(j).ok().map(|r| RecRef::Owned(Box::new(r)))
}

fn ctx_from<'a>(v: &'a Value, res: Option<&'a Arc<LoadResult>>) -> Option<Cow<'a, LeagueContext>> {
    if let Some(o) = v.downcast_object_ref::<CtxObj>() {
        return Some(Cow::Borrowed(&o.res.ctx));
    }
    if let Some(r) = res {
        return Some(Cow::Borrowed(&r.ctx));
    }
    let j = serde_json::to_value(v).ok()?;
    serde_json::from_value::<LeagueContext>(j).ok().map(Cow::Owned)
}

fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).cloned().unwrap_or_default()
}

fn none() -> Value {
    Value::from(())
}

fn opt_str(v: Option<String>) -> Value {
    v.map(Value::from).unwrap_or_default_none()
}

trait OrNone {
    fn unwrap_or_default_none(self) -> Value;
}

impl OrNone for Option<Value> {
    fn unwrap_or_default_none(self) -> Value {
        self.unwrap_or_else(none)
    }
}

fn data_dir_today(state: &State, res: Option<&Arc<LoadResult>>) -> (PathBuf, NaiveDate) {
    if let Some(r) = res {
        return (r.data_dir.clone(), r.today);
    }
    let dd = state.lookup("__fm_data_dir").and_then(|v| v.as_str().map(PathBuf::from)).unwrap_or_else(|| PathBuf::from("data"));
    let today = state
        .lookup("__fm_today")
        .and_then(|v| v.as_str().and_then(fm_pycompat::time::date_fromisoformat))
        .unwrap_or_else(|| chrono::Local::now().date_naive());
    (dd, today)
}

fn err(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidOperation, msg.into())
}

// --------------------------------------------------------------------------- methods

fn parse_dt(s: &str) -> Option<(NaiveDateTime, Option<i32>, bool)> {
    let t = s.trim();
    if t.len() == 10 {
        if let Some(d) = fm_pycompat::time::date_fromisoformat(t) {
            return Some((d.and_hms_opt(0, 0, 0)?, None, true));
        }
    }
    let t2 = t.replace(' ', "T");
    fm_pycompat::time::fromisoformat(&t2).map(|(d, off)| (d, off, false))
}

fn py_isoformat(s: &str, seconds_only: bool) -> Option<String> {
    let (d, off, date_only) = parse_dt(s)?;
    if date_only {
        return Some(d.date().format("%Y-%m-%d").to_string());
    }
    let mut out = if seconds_only { d.format("%Y-%m-%dT%H:%M:%S").to_string() } else { naive_iso(d) };
    if let Some(o) = off {
        let sign = if o < 0 { '-' } else { '+' };
        let a = o.abs();
        let _ = write!(out, "{sign}{:02}:{:02}", a / 3600, (a % 3600) / 60);
        if a % 60 != 0 {
            let _ = write!(out, ":{:02}", a % 60);
        }
    }
    Some(out)
}

fn method_callback(state: &State, value: &Value, method: &str, args: &[Value]) -> Result<Value, Error> {
    if value.kind() == ValueKind::String {
        let s = value.as_str().unwrap_or("");
        match method {
            "strftime" => {
                let fmt = args.first().and_then(|a| a.as_str()).unwrap_or("");
                let (d, _, _) = parse_dt(s).ok_or_else(|| err(format!("not a date: {s:?}")))?;
                return Ok(Value::from(fm_pycompat::time::strftime(d, fmt)));
            }
            "isoformat" => {
                let seconds = args.iter().any(|a| a.kind() == ValueKind::Map);
                return Ok(Value::from(py_isoformat(s, seconds).ok_or_else(|| err(format!("not a date: {s:?}")))?));
            }
            "format" if s == "{:,}" => {
                if let Some(n) = args.first().and_then(f64_of) {
                    return Ok(Value::from(thousands(n as i64)));
                }
            }
            "capitalize" => return Ok(Value::from(fm_pycompat::text::capitalize(s))),
            "strip" if args.is_empty() => return Ok(Value::from(fm_pycompat::text::py_strip(s))),
            _ => {}
        }
    }
    if value.kind() == ValueKind::Map && method == "gp" {
        let split = args.first().and_then(|a| a.as_str()).unwrap_or("season").to_string();
        let gp = value
            .get_attr("lines")
            .ok()
            .and_then(|l| item(&l, &Value::from(split)))
            .and_then(|line| line.get_attr("gp").ok())
            .filter(|g| g.kind() == ValueKind::Number)
            .unwrap_or(Value::from(0));
        return Ok(gp);
    }
    minijinja_contrib::pycompat::unknown_method_callback(state, value, method, args)
}

// --------------------------------------------------------------------------- filters

/// `fmt_num(v, digits=2, signed=False)`.
fn f_num(v: Value, digits: Option<Value>, signed: Option<Value>) -> String {
    let Some(x) = f64_of(&v) else { return "-".into() };
    let d = digits.as_ref().and_then(f64_of).map(|d| d as usize).unwrap_or(2);
    fixed(x, d, signed.is_some_and(|s| s.is_true()))
}

/// `fmt1(v, signed=False)`.
fn f_n1(v: Value, signed: Option<Value>) -> String {
    if v.kind() != ValueKind::Number {
        return "-".into();
    }
    let x = f64_of(&v).unwrap_or(0.0);
    let d = if x != 0.0 && x.abs() < 0.1 { 2 } else { 1 };
    fixed(x, d, signed.is_some_and(|s| s.is_true()))
}

/// `fmt_rate(v, stat="")`.
fn f_rate(v: Value, stat: Option<String>) -> String {
    if v.kind() != ValueKind::Number {
        return "-".into();
    }
    let x = f64_of(&v).unwrap_or(0.0);
    fixed(x, if stat.as_deref() == Some("SVPCT") { 3 } else { 2 }, false)
}

/// Python `'<fmt>' % args` (the Jinja `format` filter).
fn f_format(fmt: String, args: Rest<Value>) -> Result<String, Error> {
    let mut out = String::new();
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    let mut ai = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '%' {
            out.push(c);
            i += 1;
            continue;
        }
        if chars.get(i + 1) == Some(&'%') {
            out.push('%');
            i += 2;
            continue;
        }
        let mut j = i + 1;
        while j < chars.len() && !"diouxXeEfFgGcrsa".contains(chars[j]) {
            j += 1;
        }
        if j >= chars.len() {
            return Err(err("incomplete format"));
        }
        let spec: String = chars[i + 1..=j].iter().collect();
        let a = args.get(ai).cloned().ok_or_else(|| err("not enough arguments for format string"))?;
        ai += 1;
        match chars[j] {
            's' => out.push_str(&py_str(&a)),
            'r' | 'a' => out.push_str(&py_repr(&a)),
            _ => {
                let x = f64_of(&a).ok_or_else(|| err(format!("a real number is required, not {}", a.kind())))?;
                out.push_str(&percent_format(&spec, x));
            }
        }
        i = j + 1;
    }
    Ok(out)
}

/// Jinja `round(value, precision=0)`: Python `round` (ties to even); ints stay ints.
fn f_round(v: Value, precision: Option<i32>) -> Result<Value, Error> {
    if v.is_integer() {
        return Ok(v);
    }
    let x = f64_of(&v).ok_or_else(|| err(format!("cannot round {}", v.kind())))?;
    Ok(Value::from(round_to(x, precision.unwrap_or(0))))
}

/// Jinja `sum(iterable, attribute=None, start=0)` with Python's float summation.
fn f_sum(values: Value, kwargs: Kwargs) -> Result<Value, Error> {
    let attr: Option<String> = kwargs.get("attribute")?;
    kwargs.assert_all_used()?;
    let mut ints: i64 = 0;
    let mut floats: Vec<f64> = Vec::new();
    let mut any_float = false;
    for item in values.try_iter()? {
        let x = match &attr {
            Some(a) => item.get_attr(a)?,
            None => item,
        };
        if x.is_integer() {
            ints += i64::try_from(x.clone()).unwrap_or(0);
            floats.push(f64_of(&x).unwrap_or(0.0));
        } else if let Some(f) = f64_of(&x) {
            any_float = true;
            floats.push(f);
        } else if x.is_undefined() {
            continue;
        } else {
            return Err(err(format!("unsupported operand type(s) for +: 'int' and '{}'", x.kind())));
        }
    }
    Ok(if any_float { Value::from(py_sum_start(0.0, floats)) } else { Value::from(ints) })
}

/// Jinja `int(value, default=0)`.
fn f_int(v: Value) -> Value {
    match v.kind() {
        ValueKind::Number => {
            if v.is_integer() {
                v
            } else {
                Value::from(f64_of(&v).unwrap_or(0.0).trunc() as i64)
            }
        }
        ValueKind::Bool => Value::from(v.is_true() as i64),
        ValueKind::String => {
            let s = v.as_str().unwrap_or("").trim();
            s.parse::<i64>().map(Value::from).unwrap_or_else(|_| Value::from(s.parse::<f64>().map(|f| f.trunc() as i64).unwrap_or(0)))
        }
        _ => Value::from(0),
    }
}

/// Jinja `truncate(s, length=255, killwords=False, end='...', leeway=5)`.
fn f_truncate(s: Value, length: Option<usize>, killwords: Option<bool>, end: Option<String>, leeway: Option<usize>) -> String {
    let s = py_str(&s);
    let length = length.unwrap_or(255);
    let end = end.unwrap_or_else(|| "...".into());
    let leeway = leeway.unwrap_or(5);
    let n = s.chars().count();
    if n <= length + leeway {
        return s;
    }
    let keep = length.saturating_sub(end.chars().count());
    let head: String = s.chars().take(keep).collect();
    if killwords.unwrap_or(false) {
        return head + &end;
    }
    let cut = match head.rfind(' ') {
        Some(i) => head[..i].to_string(),
        None => head,
    };
    cut + &end
}

/// `urllib.parse.quote(s, safe)`.
pub fn quote(s: &str, safe: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        let c = b as char;
        if b.is_ascii_alphanumeric() || "_.-~".contains(c) || (b.is_ascii() && safe.contains(c)) {
            o.push(c);
        } else {
            let _ = write!(o, "%{b:02X}");
        }
    }
    o
}

/// Jinja `urlencode` on a string (`quote(s, safe="/")`) or a mapping (query string).
fn f_urlencode(v: Value) -> Result<String, Error> {
    if v.kind() == ValueKind::Map {
        let mut parts = Vec::new();
        for k in v.try_iter()? {
            let x = v.get_item(&k)?;
            parts.push(format!("{}={}", quote_plus(&py_str(&k)), quote_plus(&py_str(&x))));
        }
        return Ok(parts.join("&"));
    }
    Ok(quote(&py_str(&v), "/"))
}

fn quote_plus(s: &str) -> String {
    quote(s, " ").replace(' ', "+")
}

/// `ago(when, now)`.
fn f_ago(when: Value, now: Option<Value>) -> String {
    let Some(w) = when.as_str().and_then(parse_dt) else { return "never".into() };
    let now = now.and_then(|n| n.as_str().and_then(parse_dt)).map(|x| x.0).unwrap_or_else(|| chrono::Local::now().naive_local());
    let secs = (now - w.0).num_microseconds().map(|u| u as f64 / 1e6).unwrap_or(0.0);
    crate::app::ago_text(secs)
}

fn f_pct(score: Value) -> i64 {
    let x = f64_of(&score).unwrap_or(0.0);
    fm_pycompat::num::py_round_i64(x * 10.0).clamp(2, 100)
}

fn f_kind_group(kind: String) -> String {
    if kind == "sell_high" || kind == "buy_low" { "flags".into() } else { kind }
}

fn f_safe_url(url: Value) -> Value {
    match url.as_str() {
        Some(u) if u.to_lowercase().starts_with("https://") || u.to_lowercase().starts_with("http://") => Value::from(u),
        _ => none(),
    }
}

fn f_pos(p: Value) -> String {
    let ps: Vec<String> = p
        .get_attr("positions")
        .ok()
        .and_then(|x| x.try_iter().ok().map(|it| it.filter_map(|v| v.as_str().map(str::to_string)).collect()))
        .unwrap_or_default();
    let non_f: Vec<&str> = ps.iter().map(String::as_str).filter(|x| *x != "F").collect();
    if !non_f.is_empty() {
        non_f.join("/")
    } else if !ps.is_empty() {
        ps.join("/")
    } else {
        "-".into()
    }
}

fn f_action(r: Value) -> Result<String, Error> {
    let rec = rec_of(&r).ok_or_else(|| err("action: not a recommendation"))?;
    Ok(crate::app::action_line(&rec))
}

fn f_action_note(r: Value) -> Result<Value, Error> {
    let rec = rec_of(&r).ok_or_else(|| err("action_note: not a recommendation"))?;
    Ok(crate::app::action_note(&rec).map(Value::from).unwrap_or_else(none))
}

// --------------------------------------------------------------------------- globals

fn g_val(state: &State, args: Rest<Value>) -> Value {
    let Some(res) = res_of(state, &args[..1.min(args.len())]).or_else(|| res_of(state, &[])) else { return none() };
    let cid = arg(&args, 1);
    let attr = arg(&args, 2);
    let (Some(cid), Some(attr)) = (cid.as_str(), attr.as_str()) else { return none() };
    let pv = pv_value(&res, cid);
    if pv.is_none() {
        return none();
    }
    item(&pv, &Value::from(attr)).unwrap_or_else(none)
}

fn g_dyn_value(state: &State, args: Rest<Value>) -> Value {
    let d = arg(&args, 0);
    if d.is_none() || d.is_undefined() {
        return none();
    }
    let Some(res) = res_of(state, &args[..1.min(args.len())]) else { return none() };
    let cid = arg(&args, 1);
    match res.dynasty.as_ref().and_then(|m| m.get(cid.as_str().unwrap_or(""))) {
        Some(dv) => Value::from(dv.value),
        _ => none(),
    }
}

fn g_compare(state: &State, args: Rest<Value>) -> Result<Value, Error> {
    let res = res_of(state, &args).ok_or_else(|| err("compare: no result"))?;
    let rv = &args[1.min(args.len().saturating_sub(1))];
    if let Some(o) = rv.downcast_object_ref::<RecObj>() {
        if o.res.upgrade().is_some_and(|x| Arc::ptr_eq(&x, &res)) {
            return Ok(memo_value(&res, &format!("compare:{}", o.idx), || views::compare(&res, &res.recs[o.idx])));
        }
    }
    let r = rec_of(rv).ok_or_else(|| err("compare: not a recommendation"))?;
    Ok(to_mj(&views::compare(&res, &r), Some(&res)))
}

fn g_gain_info(state: &State, args: Rest<Value>) -> Result<Value, Error> {
    let r = rec_of(&arg(&args, 0)).ok_or_else(|| err("gain_info: not a recommendation"))?;
    let res = args.get(1).filter(|v| !v.is_none() && !v.is_undefined()).and_then(res_from);
    let res_any = res.clone().or_else(|| res_of(state, &[]));
    Ok(to_mj(&views::gain_info(&r, res.as_deref()), res_any.as_ref()))
}

fn g_strength_info(state: &State, args: Rest<Value>) -> Result<Value, Error> {
    let rv = arg(&args, 0);
    let recs = arg(&args, 1);
    let with_recs = recs.kind() == ValueKind::Seq && recs.len().unwrap_or(0) > 0;
    if let Some(o) = rv.downcast_object_ref::<RecObj>() {
        if let Some(res) = o.res.upgrade() {
            let all: &[Recommendation] = if with_recs { &res.recs } else { &[] };
            return Ok(to_mj(&views::strength_info(&res.recs[o.idx], all), Some(&res)));
        }
    }
    let r = rec_of(&rv).ok_or_else(|| err("strength_info: not a recommendation"))?;
    let res = res_of(state, &[]);
    Ok(to_mj(&views::strength_info(&r, &[]), res.as_ref()))
}

fn g_market_info(state: &State, args: Rest<Value>) -> Result<Value, Error> {
    let r = rec_of(&arg(&args, 0)).ok_or_else(|| err("market_info: not a recommendation"))?;
    Ok(to_mj(&views::market_info(&r), res_of(state, &[]).as_ref()))
}

fn g_counterparty(state: &State, args: Rest<Value>) -> Result<Value, Error> {
    let res = res_of(state, &args).ok_or_else(|| err("counterparty: no result"))?;
    let rv = arg(&args, 1);
    if let Some(o) = rv.downcast_object_ref::<RecObj>() {
        if o.res.upgrade().is_some_and(|x| Arc::ptr_eq(&x, &res)) {
            return Ok(memo_value(&res, &format!("counterparty:{}", o.idx), || views::counterparty(&res, &res.recs[o.idx])));
        }
    }
    let r = rec_of(&rv).ok_or_else(|| err("counterparty: not a recommendation"))?;
    Ok(to_mj(&views::counterparty(&res, &r), Some(&res)))
}

fn g_rec_key(args: Rest<Value>) -> Result<Value, Error> {
    let league = arg(&args, 0);
    let r = rec_of(&arg(&args, 1)).ok_or_else(|| err("rec_key: not a recommendation"))?;
    Ok(Value::from(crate::app::rec_key(league.as_str().unwrap_or(""), &r)))
}

fn g_ctx_fn(state: &State, args: &[Value], f: impl Fn(&LeagueContext) -> J) -> Value {
    let res = res_of(state, args);
    let a = arg(args, 0);
    match ctx_from(&a, res.as_ref()) {
        Some(ctx) => to_mj(&f(&ctx), None),
        None => none(),
    }
}

fn g_track_info(state: &State, args: Rest<Value>) -> Result<Value, Error> {
    let r = rec_of(&arg(&args, 0)).ok_or_else(|| err("track_info: not a recommendation"))?;
    let league = arg(&args, 1);
    let res = res_of(state, &args);
    let (dd, today) = data_dir_today(state, res.as_ref());
    Ok(to_mj(&views::track_info(&r, league.as_str().unwrap_or(""), &dd, today), None))
}

fn g_track_strip(state: &State, args: Rest<Value>) -> Value {
    let league = arg(&args, 0);
    let res = res_of(state, &[]);
    let (dd, today) = data_dir_today(state, res.as_ref());
    to_mj(&views::track_strip(league.as_str().unwrap_or(""), &dd, today), None)
}

fn g_exploits(state: &State, args: Rest<Value>) -> Result<Value, Error> {
    let res = res_of(state, &args).ok_or_else(|| err("exploits: no result"))?;
    let limit = args.get(1).and_then(f64_of).map(|n| n as usize).unwrap_or(8);
    Ok(memo_value(&res, &format!("exploits:{limit}"), || J::Array((*views::exploits(&res, limit)).clone())))
}

fn g_need_trades(state: &State, args: Rest<Value>) -> Result<Value, Error> {
    let res = res_of(state, &args).ok_or_else(|| err("need_trades: no result"))?;
    let limit = args.get(1).and_then(f64_of).map(|n| n as usize).unwrap_or(5);
    Ok(memo_value(&res, &format!("need_trades:{limit}"), || J::Array((*views::need_trades(&res, limit)).clone())))
}

fn g_team_color(abbr: Value) -> Value {
    opt_str(views::team_color(abbr.as_str()).map(str::to_string))
}

fn g_mode_source_label(source: Value) -> String {
    fm_core::prefs::mode_source_label(source.as_str().unwrap_or(""))
}

/// Constant globals shared by every page (Python `env.globals`).
fn constant_globals() -> Vec<(&'static str, J)> {
    use serde_json::json;
    let status_label = json!({"healthy": "Healthy", "dtd": "Day-to-day", "out": "Out", "ir": "IR", "ltir": "LTIR",
        "suspended": "Suspended", "unknown": "Unknown"});
    let status_class = json!({"healthy": "ok", "dtd": "dtd", "out": "out", "ir": "ir", "ltir": "ir",
        "suspended": "susp", "unknown": "unk"});
    let kind_label = json!({"lineup": "Lineup", "waiver": "Waiver", "trade": "Trade", "sell_high": "Sell high",
        "buy_low": "Buy low", "injury": "Injury", "alert": "Alert"});
    let move_label = json!({"waiver": ["Add", "Drop"], "injury": ["Add", "Drop"], "trade": ["Get", "Give"],
        "lineup": ["Start", "Sit"], "sell_high": ["Buy", "Sell"], "buy_low": ["Buy", "Sell"], "alert": ["Add", "Drop"]});
    let kind_filters: serde_json::Map<String, J> =
        crate::app::KIND_FILTERS.iter().map(|(k, label, kinds)| (k.to_string(), json!([label, kinds]))).collect();
    let rate_label: serde_json::Map<String, J> = views::RATE_LABEL.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
    let fa_sorts: serde_json::Map<String, J> = views::FA_SORTS.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
    vec![
        ("STATUS_LABEL", status_label),
        ("STATUS_CLASS", status_class),
        ("KIND_LABEL", kind_label),
        ("MOVE_LABEL", move_label),
        ("KIND_FILTERS", J::Object(kind_filters)),
        ("LEAGUES", json!(crate::app::LEAGUES)),
        ("HIDDEN_REASONS", json!(["RAW_SCORE"])),
        ("REASONS_SHOWN", json!(crate::app::REASONS_SHOWN)),
        ("MODES", json!(["contend", "balanced", "rebuild"])),
        ("SKATER_RATES", json!(views::SKATER_RATES)),
        ("GOALIE_RATES", json!(views::GOALIE_RATES)),
        ("RATE_LABEL", J::Object(rate_label)),
        ("POSITION_FILTERS", json!(views::POSITION_FILTERS)),
        ("FA_SORTS", J::Object(fa_sorts)),
        ("EXPLAIN_FAILED", json!(crate::app::EXPLAIN_FAILED)),
    ]
}

/// Options of [`build_env`] (Python `create_app` arguments that reach the templates).
#[derive(Clone, Debug)]
pub struct EnvOptions {
    pub dir: PathBuf,
    /// "plan" or "overview": what "/" shows
    pub home: String,
    pub auth_enabled: bool,
}

/// The template environment of the dashboard.
pub fn build_env(opts: &EnvOptions) -> Environment<'static> {
    let mut env = Environment::new();
    let dir = opts.dir.clone();
    env.set_loader(move |name| {
        if name.contains("..") {
            return Ok(None);
        }
        match std::fs::read_to_string(dir.join(name)) {
            // Jinja2 reads templates with universal newlines (a Windows checkout has CRLF)
            Ok(src) => Ok(Some(preprocess(&src.replace("\r\n", "\n")))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::new(ErrorKind::InvalidOperation, format!("reading template {name}: {e}"))),
        }
    });
    env.set_auto_escape_callback(|name| if name.ends_with(".html") || name.ends_with(".xml") { AutoEscape::Html } else { AutoEscape::None });
    env.set_formatter(|out, state, value| {
        if value.is_safe() {
            return out.write_str(value.as_str().unwrap_or(&value.to_string())).map_err(|_| Error::from(ErrorKind::WriteFailure));
        }
        let s = py_str(value);
        let r = if matches!(state.auto_escape(), AutoEscape::None) { out.write_str(&s) } else { out.write_str(&escape_html(&s)) };
        r.map_err(|_| Error::from(ErrorKind::WriteFailure))
    });
    env.set_unknown_method_callback(method_callback);

    env.add_filter("num", f_num);
    env.add_filter("pos", f_pos);
    env.add_filter("ago", f_ago);
    env.add_filter("reason_label", |code: String| code.replace('_', " ").to_lowercase());
    env.add_filter("pct", f_pct);
    env.add_filter("kind_group", f_kind_group);
    env.add_filter("safe_url", f_safe_url);
    env.add_filter("q", |s: Value| quote(&py_str(&s), ":"));
    env.add_filter("action", f_action);
    env.add_filter("action_note", f_action_note);
    env.add_filter("n1", f_n1);
    env.add_filter("rate", f_rate);
    env.add_filter("last", |name: String| crate::app::last_name(&name));
    env.add_filter("format", f_format);
    env.add_filter("round", f_round);
    env.add_filter("sum", f_sum);
    env.add_filter("int", f_int);
    env.add_filter("truncate", f_truncate);
    env.add_filter("urlencode", f_urlencode);
    env.add_filter("capitalize", |s: Value| fm_pycompat::text::capitalize(&py_str(&s)));
    env.add_filter("__date_part", |v: Value, part: String| -> Value {
        if let Some((d, _, _)) = v.as_str().and_then(parse_dt) {
            use chrono::Datelike;
            return Value::from(match part.as_str() {
                "day" => d.day() as i64,
                "month" => d.month() as i64,
                _ => d.year() as i64,
            });
        }
        v.get_attr(&part).unwrap_or_default()
    });
    env.add_filter("string",|s: Value| if s.is_safe() { s } else { Value::from(py_str(&s)) });

    for (k, v) in constant_globals() {
        env.add_global(k, to_mj(&v, None));
    }
    env.add_global("auth_enabled", Value::from(opts.auth_enabled));
    env.add_global("HOME", Value::from(opts.home.as_str()));
    let plan_home = opts.home == "plan";
    env.add_global("OVERVIEW_HREF", Value::from(if plan_home { "/overview" } else { "/" }));
    env.add_global("PLAN_HREF", Value::from(if plan_home { "/" } else { "/plan" }));
    env.add_function("val", g_val);
    env.add_function("dyn_value", g_dyn_value);
    env.add_function("mode_source_label", g_mode_source_label);
    env.add_function("compare", g_compare);
    env.add_function("gain_info", g_gain_info);
    env.add_function("strength_info", g_strength_info);
    env.add_function("market_info", g_market_info);
    env.add_function("counterparty", g_counterparty);
    env.add_function("rec_key", g_rec_key);
    env.add_function("credits", |state: &State, args: Rest<Value>| g_ctx_fn(state, &args, |c| serde_json::json!(views::credits(c))));
    env.add_function("limits_text", |state: &State, args: Rest<Value>| {
        g_ctx_fn(state, &args, |c| views::limits_text(c).map(J::from).unwrap_or(J::Null))
    });
    env.add_function("exploits", g_exploits);
    env.add_function("moves_text", |state: &State, args: Rest<Value>| {
        g_ctx_fn(state, &args, |c| crate::app::moves_footer(c).map(J::from).unwrap_or(J::Null))
    });
    env.add_function("sources_text", |state: &State, args: Rest<Value>| {
        g_ctx_fn(state, &args, |c| match fm_report::health::footer_summary(c) {
            Some(f) => serde_json::json!({"level": f.level, "text": f.text}),
            None => J::Null,
        })
    });
    env.add_function("track_info", g_track_info);
    env.add_function("track_strip", g_track_strip);
    env.add_function("need_trades", g_need_trades);
    env.add_function("team_color", g_team_color);
    env
}

/// `res.recs` as template values (built once per LoadResult).
pub fn rec_values(res: &Arc<LoadResult>) -> Vec<Value> {
    mj_cache(res).recs.get_or_init(|| (0..res.recs.len()).map(|i| RecObj::value(res, i)).collect()).clone()
}

/// One of `res.recs` as a template value.
pub fn rec_value_at(res: &Arc<LoadResult>, idx: usize) -> Value {
    mj_cache(res).recs.get_or_init(|| (0..res.recs.len()).map(|i| RecObj::value(res, i)).collect())[idx].clone()
}

/// A view model converted once per LoadResult (memoised on it).
pub fn memo_value(res: &Arc<LoadResult>, key: &str, f: impl FnOnce() -> J) -> Value {
    let v: Arc<Value> = res.memo.get_or(&format!("w10:mj:{key}"), || to_mj(&f(), Some(res)));
    (*v).clone()
}

/// A safe (pre-escaped) HTML string.
pub fn safe(s: impl Into<String>) -> Value {
    Value::from_safe_string(s.into())
}

/// Mark the `svg` / `legend` strings of the /health chart specs as safe HTML.
pub fn safe_charts(charts: &J) -> Value {
    match charts {
        J::Array(a) => Value::from(a.iter().map(safe_charts).collect::<Vec<_>>()),
        J::Object(m) => Value::from_iter(m.iter().map(|(k, x)| {
            let val = match (k.as_str(), x.as_str()) {
                ("svg" | "legend", Some(s)) => safe(s),
                _ => safe_charts(x),
            };
            (Value::from(k.as_str()), val)
        })),
        other => to_mj(other, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_append() {
        let src = "{% set sweet = [] %}\n{% for r in xs if r %}{% if sweet|length < 5 %}{% set _ = sweet.append((r, 1)) %}{% endif %}{% endfor %}\n{% if sweet %}{{ sweet|length }}{% endif %}";
        let out = preprocess(src);
        assert!(!out.contains(".append("), "{out}");
        let mut env = Environment::new();
        env.add_template_owned("t", out).unwrap();
        let s = env.get_template("t").unwrap().render(minijinja::context! { xs => vec![1, 0, 2, 3] }).unwrap();
        assert_eq!(s.trim(), "3");
    }

    #[test]
    fn quoting_and_truncate() {
        assert_eq!(quote("/plan?league=espn&x=a b", "/"), "/plan%3Fleague%3Despn%26x%3Da%20b");
        assert_eq!(quote("espn:123", ":"), "espn:123");
        assert_eq!(f_truncate(Value::from("a b c"), Some(3), None, None, Some(0)), "...");
        assert_eq!(f_truncate(Value::from("hello world foo"), Some(11), None, None, Some(0)), "hello...");
        assert_eq!(f_truncate(Value::from("short"), Some(70), None, None, None), "short");
    }

    #[test]
    fn python_printing() {
        assert_eq!(py_str(&Value::from(())), "None");
        assert_eq!(py_str(&Value::from(true)), "True");
        assert_eq!(py_str(&Value::from(1.0)), "1.0");
        assert_eq!(py_str(&Value::from(1e-5)), "1e-05");
        assert_eq!(py_str(&Value::from(vec![Value::from("a"), Value::from(2)])), "['a', 2]");
        assert_eq!(escape_html("<a href=\"/x\">it's & /</a>"), "&lt;a href=&#34;/x&#34;&gt;it&#39;s &amp; /&lt;/a&gt;");
        assert_eq!(py_isoformat("2026-11-04T10:00:00Z", false).unwrap(), "2026-11-04T10:00:00+00:00");
        assert_eq!(py_isoformat("2026-11-04T10:00:00.123456", true).unwrap(), "2026-11-04T10:00:00");
        assert_eq!(py_isoformat("2026-11-04", false).unwrap(), "2026-11-04");
    }

    #[test]
    fn format_filter() {
        let f = |fmt: &str, v: Value| f_format(fmt.into(), Rest(vec![v])).unwrap();
        assert_eq!(f("%+.1f", Value::from(2.25)), "+2.2");
        assert_eq!(f("%02d", Value::from(7)), "07");
        assert_eq!(f("%.0f%%", Value::from(45.5)), "46%");
        assert_eq!(f("%g", Value::from(1.5)), "1.5");
        assert_eq!(f_round(Value::from(2.5), None).unwrap(), Value::from(2.0));
        assert_eq!(f_round(Value::from(3), None).unwrap(), Value::from(3));
    }
}

/// `v[key]`, None when missing / undefined.
fn item(v: &Value, key: &Value) -> Option<Value> {
    v.get_item(key).ok().filter(|x| !x.is_undefined())
}
