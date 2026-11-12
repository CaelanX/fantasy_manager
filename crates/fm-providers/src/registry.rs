//! Rust port of `fantasy_manager/providers/__init__.py` (the provider factory and the league /
//! status cache clearing) and `providers/base.py` (the interface lives in `fm_core::provider`).
//! Owner: work package `W5-provider-pipeline`.
//!
//! Deviations (API shape only):
//! * `get_provider` takes the shared `Arc<HttpCache>` and an optional pinned `today` (Python's
//!   `date.today()` inside the providers; `None` = the providers' own clock).
//! * `ProviderUnavailable` is `fm_core::ProviderError::Unavailable` (both Rust providers always
//!   exist, so the factory never raises it; kept for the CLI's exit-code mapping).

use std::sync::Arc;

use anyhow::Result;
use chrono::NaiveDate;
use fm_core::config::Settings;
use fm_core::provider::LeagueProvider;
use fm_core::ProviderError;
use fm_http::{FxpaFileCache, HttpCache};

use crate::espn::{EspnProvider, ESPN_HOST};
use crate::fantrax::FantraxProvider;

/// `PROVIDERS`
pub const PROVIDERS: [&str; 2] = ["espn", "fantrax"];

/// ESPN free-agent pool size (`EspnProvider(settings, cache, fa_size=75)`).
pub const ESPN_FA_SIZE: i64 = 75;

/// Python `repr(str)` for the short ASCII names this module formats.
pub(crate) fn py_str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `get_provider(name, settings, cache)`: instantiate the provider for `name` ("espn" or
/// "fantrax", case-insensitive).
pub fn get_provider(name: &str, settings: &Settings, cache: Arc<HttpCache>) -> Result<Box<dyn LeagueProvider>> {
    get_provider_at(name, settings, cache, None)
}

/// [`get_provider`] with the providers' `date.today()` pinned (FM_NOW / `--from-snapshot`).
pub fn get_provider_at(
    name: &str,
    settings: &Settings,
    cache: Arc<HttpCache>,
    today: Option<NaiveDate>,
) -> Result<Box<dyn LeagueProvider>> {
    match name.to_lowercase().as_str() {
        "espn" => {
            let mut p = EspnProvider::new(settings.clone(), cache, ESPN_FA_SIZE);
            if let Some(t) = today {
                p = p.with_today(t);
            }
            Ok(Box::new(p))
        }
        "fantrax" => {
            let mut p = FantraxProvider::new(settings.clone(), cache.offline).with_official(cache.clone());
            if let Some(t) = today {
                p = p.with_today(t);
            }
            Ok(Box::new(p))
        }
        _ => Err(ProviderError::Message(format!(
            "unknown league provider {}; expected one of {}",
            py_str_repr(name),
            PROVIDERS.join(", ")
        ))
        .into()),
    }
}

/// `clear_league_cache(name, settings, cache)`: drop every cached response that holds `name`'s
/// league state (rosters / lineups, settings, free agents), so the next load reads the league
/// live. NHL / news caches are left alone. Returns how many entries were removed.
pub fn clear_league_cache(name: &str, settings: &Settings, cache: Option<&HttpCache>) -> Result<usize> {
    match name.to_lowercase().as_str() {
        "espn" => match cache {
            Some(c) => c.clear(Some(ESPN_HOST)),
            None => Ok(0),
        },
        // fantrax.clear_cache(settings.fm_data_dir): every json file under <data_dir>/fantrax_cache, plus the
        // official-API responses, which live in the shared cache.db
        "fantrax" => {
            let files = FxpaFileCache::new(&settings.fm_data_dir, false).clear();
            let official = match cache {
                Some(c) => c.clear(Some(crate::fantrax_official::URL_MARK))?,
                None => 0,
            };
            Ok(files + official)
        }
        _ => Ok(0),
    }
}

/// `clear_status_cache(cache)`: drop the cached day-of status feeds (Daily Faceoff starting
/// goalies and the ESPN injury feed). Returns how many entries were removed.
pub fn clear_status_cache(cache: &HttpCache) -> Result<usize> {
    Ok(fm_sources::dailyfaceoff::clear_goalie_cache(cache)? + fm_sources::injuries::clear_cache(cache)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fm_http::CachedResponse;

    #[test]
    fn unknown_provider_message() {
        let s = Settings::default();
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(HttpCache::open(dir.path(), true).unwrap());
        let err = get_provider("yahoo", &s, cache).err().expect("error");
        assert_eq!(err.to_string(), "unknown league provider 'yahoo'; expected one of espn, fantrax");
        assert!(matches!(err.downcast_ref::<ProviderError>(), Some(ProviderError::Message(_))));
        assert_eq!(py_str_repr("it's"), "\"it's\"");
    }

    #[test]
    fn names_are_case_insensitive() {
        let s = Settings::default();
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(HttpCache::open(dir.path(), true).unwrap());
        assert_eq!(get_provider("ESPN", &s, cache.clone()).unwrap().name(), "espn");
        assert_eq!(get_provider("Fantrax", &s, cache).unwrap().name(), "fantrax");
    }

    /// test_league_cache.py::test_clear_league_cache_espn_keeps_nhl_rows
    #[test]
    fn clear_league_cache_espn_keeps_nhl_rows() {
        let dir = tempfile::tempdir().unwrap();
        let cache = HttpCache::open(dir.path(), false).unwrap();
        let urls = [
            "https://lm-api-reads.fantasy.espn.com/apis/v3/games/fhl/seasons/2027/segments/0/leagues/1",
            "https://api-web.nhle.com/v1/roster/EDM/current",
        ];
        for url in urls {
            let resp = CachedResponse { url: url.into(), status_code: 200, text: "{}".into(), from_cache: false };
            cache.store(url, url, &resp).unwrap();
        }
        let s = Settings { fm_data_dir: dir.path().to_path_buf(), ..Settings::default() };
        assert_eq!(clear_league_cache("espn", &s, Some(&cache)).unwrap(), 1);
        assert!(cache.lookup(urls[0]).is_none());
        assert!(cache.lookup(urls[1]).is_some());
        assert_eq!(clear_league_cache("yahoo", &s, Some(&cache)).unwrap(), 0);
    }

    /// test_league_cache.py::test_clear_league_cache_fantrax_drops_file_cache
    #[test]
    fn clear_league_cache_fantrax_drops_file_cache() {
        let dir = tempfile::tempdir().unwrap();
        let fx = FxpaFileCache::new(dir.path(), false);
        fx.put("abc", &serde_json::json!({"n": 1})).unwrap();
        assert!(fx.get("abc", 900.0).is_some());
        let s = Settings { fm_data_dir: dir.path().to_path_buf(), ..Settings::default() };
        assert_eq!(clear_league_cache("fantrax", &s, None).unwrap(), 1);
        assert!(fx.get("abc", 900.0).is_none()); // read live after the clear
        assert_eq!(clear_league_cache("fantrax", &s, None).unwrap(), 0);
    }

    /// The Fantrax clear also drops official-API rows from cache.db and leaves other hosts alone.
    #[test]
    fn clear_league_cache_fantrax_drops_official_rows() {
        let dir = tempfile::tempdir().unwrap();
        let cache = HttpCache::open(dir.path(), false).unwrap();
        let urls = [
            "https://www.fantrax.com/fxea/general/getLeagueInfo",
            "https://www.fantrax.com/fxea/general/getAdp",
            "https://api-web.nhle.com/v1/roster/EDM/current",
        ];
        for (i, url) in urls.iter().enumerate() {
            let resp = CachedResponse { url: (*url).into(), status_code: 200, text: "{}".into(), from_cache: false };
            cache.store(&format!("k{i}"), url, &resp).unwrap();
        }
        let s = Settings { fm_data_dir: dir.path().to_path_buf(), ..Settings::default() };
        assert_eq!(clear_league_cache("fantrax", &s, Some(&cache)).unwrap(), 2);
        assert!(cache.lookup("k0").is_none() && cache.lookup("k1").is_none());
        assert!(cache.lookup("k2").is_some());
    }

    /// Only the live factory attaches the official client, and only when a league id is set.
    #[test]
    fn live_factory_attaches_the_official_client() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(HttpCache::open(dir.path(), true).unwrap());
        let with_id = Settings { fantrax_league_id: Some("abc".into()), ..Settings::default() };
        let p = FantraxProvider::new(with_id.clone(), true);
        assert!(p.official().is_none(), "a plain provider has no official client");
        assert!(p.with_official(cache.clone()).official().is_some());
        assert!(FantraxProvider::new(Settings::default(), true).with_official(cache).official().is_none());
    }
}
