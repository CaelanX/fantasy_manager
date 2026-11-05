//! Shared contract every translator codes against. The types in `models`, `values`, `snapshot`,
//! `provider` and `pydt` are COMPLETE (written by the architect, verified by deserializing and
//! re-serializing the parity fixtures - see `tests/snapshot_roundtrip.rs`). Do not change their
//! fields without the orchestrator; additive helper methods are fine.
//!
//! Python mapping: `models.py` -> [`models`], `config.py` -> [`config`], `prefs.py` -> [`prefs`],
//! `matching/normalize.py` -> [`normalize`], `valuation.valuate.PlayerValue` /
//! `valuation.dynasty.DynastyValue` / `valuation.picks.PickAsset` /
//! `valuation.rookie.{RookiePrior,RookieEstimate}` -> [`values`], the enrichment registries and
//! provider side data -> [`snapshot`], `providers/base.py` -> [`provider`].

pub mod activity;
pub mod calendar;
pub mod config;
pub mod error;
pub mod models;
pub mod normalize;
pub mod prefs;
pub mod provider;
pub mod pydt;
pub mod snapshot;
pub mod unproven;
pub mod values;

pub use error::{CoreError, ProviderError};
pub use models::*;
pub use pydt::PyDateTime;
pub use values::{DynastyValue, Horizon, PickAsset, PlayerValue, RookieEstimate, RookiePrior};

/// Insertion-ordered map: the default for every Python `dict` whose iteration order can reach an
/// output (most of them). Use `HashMap` only for pure lookups that are never iterated.
pub type Map<K, V> = indexmap::IndexMap<K, V>;
/// Insertion-ordered set (Python sets are NOT ordered: see ARCHITECTURE.md "Set iteration").
pub type Set<T> = indexmap::IndexSet<T>;
/// Player canonical id, e.g. "espn:4024854", "fantrax:04an5".
pub type Cid = String;
