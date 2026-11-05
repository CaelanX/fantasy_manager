//! Parity tapes: `parity/fixtures/<league>/tape.json.gz` written by `dump_fixtures.py`:
//! `{"schema": 1, "http": {cache_key: {url, params, key_headers, status, final_url, body}},
//!   "fxpa": {fxpa_key: {league, msgs, body}}}`. Replaying a tape through [`crate::HttpCache`] /
//! the Fantrax client lets providers + enrichment run with zero network and no cache.db, and it
//! proves the Rust cache-key derivation (a wrong key = a tape miss).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use crate::cache::{ParamValue, Params};

#[derive(Clone, Debug, Deserialize)]
pub struct TapeHttp {
    pub url: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub key_headers: HashMap<String, String>,
    pub status: u16,
    pub final_url: String,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TapeFxpa {
    pub league: String,
    pub msgs: Vec<Value>,
    pub body: Value,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct TapeData {
    #[serde(default)]
    pub http: HashMap<String, TapeHttp>,
    #[serde(default)]
    pub fxpa: HashMap<String, TapeFxpa>,
}

/// Shared, read-only tape.
#[derive(Clone, Debug, Default)]
pub struct Tape(pub Arc<TapeData>);

/// Rebuild [`Params`] from the tape's JSON form (`jsonable(params)` of the Python dict):
/// `str(v)` per Python value (True/False, ints, float repr), lists element-wise, null stays null.
pub fn params_from_json(v: &Value) -> Params {
    fn py_str(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            Value::Bool(true) => "True".into(),
            Value::Bool(false) => "False".into(),
            Value::Null => "None".into(),
            Value::Number(n) => match (n.as_i64(), n.as_u64()) {
                (Some(i), _) => i.to_string(),
                (_, Some(u)) => u.to_string(),
                _ => fm_pycompat::fmt::repr_f64(n.as_f64().unwrap_or(f64::NAN)),
            },
            other => other.to_string(),
        }
    }
    let Some(m) = v.as_object() else { return Vec::new() };
    m.iter()
        .map(|(k, v)| {
            let pv = match v {
                Value::Null => ParamValue::None,
                Value::Array(a) => ParamValue::List(a.iter().map(py_str).collect()),
                other => ParamValue::Str(py_str(other)),
            };
            (k.clone(), pv)
        })
        .collect()
}

impl TapeHttp {
    /// `key_headers` as a header list for [`crate::cache_key`].
    pub fn headers(&self) -> Vec<(String, String)> {
        self.key_headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
    /// Rebuilt request params.
    pub fn params_vec(&self) -> Params {
        params_from_json(&self.params)
    }
}

impl Tape {
    /// Load a `.json.gz` (or plain `.json`) tape.
    pub fn load(path: &Path) -> anyhow::Result<Tape> {
        use std::io::Read;
        let mut raw = Vec::new();
        std::fs::File::open(path)?.read_to_end(&mut raw)?;
        let data: TapeData = if raw.starts_with(&[0x1f, 0x8b]) {
            serde_json::from_reader(std::io::BufReader::new(flate2::read::GzDecoder::new(&raw[..])))?
        } else {
            serde_json::from_slice(&raw)?
        };
        Ok(Tape(Arc::new(data)))
    }

    pub fn http(&self, key: &str) -> Option<&TapeHttp> {
        self.0.http.get(key)
    }

    pub fn fxpa(&self, key: &str) -> Option<&TapeFxpa> {
        self.0.fxpa.get(key)
    }
}
