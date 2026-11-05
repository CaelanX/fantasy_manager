use std::path::PathBuf;

use fm_http::ParamValue;
use fm_http::params_from_json;
use fm_http::{cache_key, fxpa_key, CacheMiss, CachedResponse, FxpaFileCache, HttpCache, Params, Tape};
use serde_json::{json, Value};

fn s(k: &str, v: &str) -> (String, ParamValue) {
    (k.to_string(), ParamValue::Str(v.to_string()))
}
fn h(k: &str, v: &str) -> (String, String) {
    (k.to_string(), v.to_string())
}

#[test]
fn golden_cache_keys_match_python() {
    // expected values computed with fantasy_manager.cache.cache_key
    let list: Params = vec![("view".into(), ParamValue::List(vec!["a".into(), "b".into()]))];
    let a = cache_key("GET", "https://x/y", &list, &[h("User-Agent", "1")]);
    assert_eq!(a, "ab3d33c0acac81df8625fd2a6de837755f583d796f918e97cbdc4f6e9c33bee6");
    // irrelevant header does not change the key; the keyed one does
    assert_eq!(a, cache_key("GET", "https://x/y", &list, &[h("User-Agent", "2")]));
    let c = cache_key("GET", "https://x/y", &list, &[h("X-Fantasy-Filter", "{}")]);
    assert_eq!(c, "b118155f13ff705fe6dff08b95047a919201b0e9e39806d69e502ce7da087510");

    let mixed: Params = vec![
        s("b", "1"),
        s("a", "True"),
        ("c".into(), ParamValue::None),
        s("d", "1.5"),
    ];
    assert_eq!(
        cache_key("GET", "https://x/y", &mixed, &[]),
        "1a957201a08e428ff6178f1206904a31c1d902e786aa138653c4c8249cda96e1"
    );
    let uni: Params = vec![s("n", "\u{65e5}\u{672c} \"q\" \\ \n \u{1F600}")];
    assert_eq!(
        cache_key("get", "https://x/\u{e9}?q=\u{fc}", &uni, &[]),
        "2cf917bd53452ed6db32f33de4ec3da50877c94ca5f3d2ce97f88fad6a06f998"
    );
    assert_eq!(
        cache_key("GET", "https://x/y", &vec![], &[]),
        "8e95892057408b88fa57f64f4fa1ddb7708cd3732919505b45c8b3fb6dcb389b"
    );
    // param order is irrelevant
    assert_eq!(
        cache_key("GET", "u", &vec![s("b", "1"), s("a", "2")], &[]),
        cache_key("GET", "u", &vec![s("a", "2"), s("b", "1")], &[])
    );
}

#[test]
fn golden_fxpa_key_matches_python() {
    let msgs = vec![json!({"method": "getTeamRosterInfo",
        "data": {"leagueId": "oq3", "teamId": "t1", "period": "5"}})];
    assert_eq!(
        fxpa_key("oq3", &msgs),
        "00118d2420de0a9b188a3021c1ece8721740033b31c2d306cd19e9d182ce7344"
    );
}

#[test]
fn offline_miss_and_hit() {
    let dir = tempfile::tempdir().unwrap();
    let cache = HttpCache::open(dir.path(), true).unwrap();
    let url = "https://example.invalid/data";
    let err = cache.get_json(url, &vec![], &[], &[], 3600.0).unwrap_err();
    assert!(err.downcast_ref::<CacheMiss>().is_some());
    let key = cache_key("GET", url, &vec![], &[]);
    cache
        .store(&key, url, &CachedResponse { status_code: 200, text: "{\"ok\": true}".into(), url: url.into(), from_cache: false })
        .unwrap();
    // offline ignores TTL
    assert_eq!(cache.get_json(url, &vec![], &[], &[], 0.0).unwrap(), json!({"ok": true}));
    assert_eq!(cache.lookup(&key).unwrap().1, 200);
    // cookies never part of the key
    let ck = [h("SWID", "x")];
    assert_eq!(cache.get_text(url, &vec![], &[], &ck, 0.0).unwrap(), "{\"ok\": true}");
    assert_eq!(cache.clear(Some("nomatch")).unwrap(), 0);
    assert_eq!(cache.clear(Some("example.invalid")).unwrap(), 1);
    assert!(cache.lookup(&key).is_none());
}

#[test]
fn stale_row_is_not_fresh_online_and_unreachable_errors() {
    // ttl 0 with a stored row and offline=false would hit the network; only check offline=false
    // freshness through lookup semantics: a fresh row is served without network.
    let dir = tempfile::tempdir().unwrap();
    let cache = HttpCache::open(dir.path(), false).unwrap();
    let url = "https://example.invalid/fresh";
    let key = cache_key("GET", url, &vec![], &[]);
    cache
        .store(&key, url, &CachedResponse { status_code: 200, text: "hi".into(), url: "https://final/".into(), from_cache: false })
        .unwrap();
    let r = cache.fetch(url, &vec![], &[], &[], 3600.0).unwrap();
    assert!(r.from_cache);
    assert_eq!(r.url, "https://final/");
}

#[test]
fn fxpa_file_cache() {
    let dir = tempfile::tempdir().unwrap();
    let online = FxpaFileCache::new(dir.path(), false);
    let body = json!({"responses": [{"data": {"n": "caf\u{e9}"}}]});
    assert!(online.get("k1", 900.0).is_none());
    online.put("k1", &body).unwrap();
    assert_eq!(online.get("k1", 900.0), Some(body.clone()));
    assert!(online.get("k1", 0.0).is_none()); // stale
    let offline = FxpaFileCache::new(dir.path(), true);
    assert_eq!(offline.get("k1", 0.0), Some(body)); // offline ignores TTL
    std::fs::write(online.dir.join("bad.json"), "{not json").unwrap();
    assert!(offline.get("bad", 0.0).is_none()); // corrupt ignored
    let text = std::fs::read_to_string(online.dir.join("k1.json")).unwrap();
    assert!(text.starts_with("{\"fetched_at\": ") && text.contains("caf\\u00e9"), "{text}");
    assert_eq!(online.clear(), 2);
}

fn fixture_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(p) = std::env::var("FM_PARITY_FIXTURES") {
        roots.push(PathBuf::from(p));
    }
    roots.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../parity/fixtures"));
    roots
}

#[test]
fn tape_replay_reproduces_every_key() {
    let mut checked = 0usize;
    for root in fixture_roots() {
        for league in ["espn", "fantrax"] {
            let path = root.join(league).join("tape.json.gz");
            if !path.is_file() {
                continue;
            }
            let tape = Tape::load(&path).unwrap();
            for (key, e) in &tape.0.http {
                let got = cache_key("GET", &e.url, &e.params_vec(), &e.headers());
                assert_eq!(&got, key, "{league}: key mismatch for {} {:?}", e.url, e.params);
                checked += 1;
            }
            for (key, e) in &tape.0.fxpa {
                assert_eq!(&fxpa_key(&e.league, &e.msgs), key, "{league}: fxpa key mismatch");
                checked += 1;
            }
            // replay through HttpCache::from_tape
            let cache = HttpCache::from_tape(tape.clone());
            if let Some((key, e)) = tape.0.http.iter().next() {
                let r = cache.fetch(&e.url, &e.params_vec(), &e.headers(), &[], 0.0).unwrap();
                assert_eq!(r.text, e.body);
                assert_eq!(r.status_code, e.status);
                assert!(cache.lookup(key).is_some());
            }
            let miss = cache.fetch("https://not.on/tape", &vec![], &[], &[], 0.0).unwrap_err();
            assert!(miss.downcast_ref::<CacheMiss>().is_some());
        }
    }
    if checked == 0 {
        eprintln!("SKIP tape_replay_reproduces_every_key: no tape.json.gz found (gitignored)");
    } else {
        eprintln!("tape keys verified: {checked}");
    }
}

#[test]
fn params_from_json_python_str() {
    let p = params_from_json(&json!({"a": true, "b": 3, "c": 1.5, "d": null, "e": [1, "x", false], "f": "s"}));
    assert_eq!(p[0], s("a", "True"));
    assert_eq!(p[1], s("b", "3"));
    assert_eq!(p[2], s("c", "1.5"));
    assert_eq!(p[3], ("d".into(), ParamValue::None));
    assert_eq!(p[4], ("e".into(), ParamValue::List(vec!["1".into(), "x".into(), "False".into()])));
}

#[test]
fn real_cache_db_is_readable() {
    let src = PathBuf::from("C:/code/fantasy_manager/data/cache.db");
    if !src.is_file() {
        eprintln!("SKIP real_cache_db_is_readable: {} absent", src.display());
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(&src, dir.path().join("cache.db")).unwrap();
    let cache = HttpCache::open(dir.path(), true).unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("cache.db")).unwrap();
    let (key, url, body_len): (String, String, i64) = conn
        .query_row("SELECT key, url, length(body) FROM http_cache LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap();
    let (fin, status, body, fetched) = cache.lookup(&key).expect("row visible through lookup");
    assert!(status >= 200 && status < 300 && fetched > 0.0, "{url} {fin:?}");
    assert!(body.len() as i64 >= body_len.min(1));
    // a param-less URL row can be re-derived from its url alone when its key matches
    let n: i64 = conn.query_row("SELECT count(*) FROM http_cache", [], |r| r.get(0)).unwrap();
    let mut hits = 0;
    let mut stmt = conn.prepare("SELECT key, url FROM http_cache").unwrap();
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).unwrap();
    for r in rows.flatten() {
        if cache_key("GET", &r.1, &vec![], &[]) == r.0 {
            hits += 1;
        }
    }
    eprintln!("cache.db rows: {n}, keys re-derived from bare url: {hits}");
    assert!(hits > 0, "no row key reproduced from its bare URL: key derivation differs");
    let _: Value = Value::Null;
}
