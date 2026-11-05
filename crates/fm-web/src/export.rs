//! Rust port of `fantasy_manager/web/export.py`: static snapshot of the dashboard (`fm web-export`).
//!
//! Crawls an axum `Router` in-process (`tower::ServiceExt::oneshot`, no socket, no login: pass a
//! router built without auth) from the top pages of each league, rewrites same-site links to flat
//! `.html` files and copies `static/`. Independent of the routes' content.
//!
//! Deviations: `export_site` is async (the router is a tower service); the static dir is passed in
//! (`default_static_dir()`: `FM_STATIC_DIR` or None = the files compiled into the binary); redirects
//! are followed like Starlette's `TestClient` (up to 10 hops, same-site `Location` only).

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{header, Request};
use axum::Router;
use http_body_util::BodyExt;
use regex::{Captures, Regex};
use tower::ServiceExt;

pub const LEAGUES: [&str; 2] = ["espn", "fantrax"];
pub const PAGE_LIMIT: usize = 2500;
pub const SEED_PATHS: [&str; 8] =
    ["/plan", "/overview", "/roster", "/recommendations", "/news", "/schedule", "/matchup", "/health"];
/// GET pages worth a file; anything else on the site (API, login, POST targets) becomes "#".
pub const PAGE_PREFIXES: [&str; 9] =
    ["/plan", "/overview", "/roster", "/recommendations", "/news", "/schedule", "/matchup", "/health", "/player/"];
/// Query parameters that page through time without end (the schedule's week arrows).
pub const SKIP_PARAMS: [&str; 1] = ["week"];
pub const SNAPSHOT_CSS: &str = "<style>form[method=\"post\"],form[method=post]{display:none!important}\
.snapshot-note{font-size:.8rem;opacity:.75;text-align:center;padding:.4rem}</style>";

#[derive(Debug, Clone, Default)]
pub struct ExportResult {
    pub pages: usize,
    pub errors: Vec<String>,
    pub truncated: bool,
}

/// `FM_STATIC_DIR`, or None for the static files compiled into the binary.
pub fn default_static_dir() -> Option<PathBuf> {
    crate::assets::static_override()
}

fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_bad = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
            in_bad = false;
        } else if !in_bad {
            out.push('-');
            in_bad = true;
        }
    }
    out
}

/// `parse_qsl(query, keep_blank_values=True)`.
fn parse_qsl(query: &str) -> Vec<(String, String)> {
    form_urlencoded::parse(query.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

/// File name of a site URL: "/roster?league=espn&team=3" -> "roster__league-espn_team-3.html".
pub fn page_file(path: &str, query: &str) -> String {
    let stripped = path.trim_matches('/');
    let mut name = sanitize(if stripped.is_empty() { "index" } else { stripped });
    let mut params = parse_qsl(query);
    params.sort();
    if !params.is_empty() {
        let joined: Vec<String> = params.iter().map(|(k, v)| format!("{k}-{v}")).collect();
        name.push_str("__");
        name.push_str(&sanitize(&joined.join("_")));
    }
    let name: String = name.chars().take(180).collect();
    format!("{name}.html")
}

fn is_page(path: &str) -> bool {
    path == "/" || PAGE_PREFIXES.iter().any(|p| if p.ends_with('/') { path.starts_with(p) } else { path == *p })
}

/// Python `html.escape(s, quote=True)`.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#x27;")
}

/// (path, query, fragment) of a site-relative URL (`urlsplit` for strings starting with one "/").
fn urlsplit(raw: &str) -> (&str, &str, &str) {
    let (rest, frag) = raw.split_once('#').unwrap_or((raw, ""));
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    (path, query, frag)
}

/// (page with site links pointing at the exported files, site URLs it links to).
pub fn rewrite(body: &str, stamp: &str) -> (String, Vec<String>) {
    let attr = Regex::new(r#"(\s(?:href|src|action)=)"([^"]*)""#).expect("static regex");
    let htmx = Regex::new(r"(?i)<script[^>]*htmx[^>]*></script>\s*").expect("static regex");
    let body_tag = Regex::new(r"(<body[^>]*>)").expect("static regex");
    let mut found: Vec<String> = Vec::new();

    let out = attr.replace_all(body, |m: &Captures| {
        let raw = htmlize::unescape(&m[2]).into_owned();
        if !raw.starts_with('/') || raw.starts_with("//") {
            return m[0].to_string();
        }
        let (path, query, frag) = urlsplit(&raw);
        let frag_s = if frag.is_empty() { String::new() } else { format!("#{frag}") };
        if path.starts_with("/static/") {
            return format!("{}\"{}\"", &m[1], html_escape(&path[1..]));
        }
        if m[1].trim().starts_with("action")
            || !is_page(path)
            || parse_qsl(query).iter().any(|(k, _)| SKIP_PARAMS.contains(&k.as_str()))
        {
            return format!("{}\"#\"", &m[1]);
        }
        found.push(if query.is_empty() { path.to_string() } else { format!("{path}?{query}") });
        format!("{}\"{}\"", &m[1], html_escape(&(page_file(path, query) + &frag_s)))
    });
    let out = htmx.replace_all(&out, "").replace("hx-boost=\"true\"", "");
    let out = out.replacen("</head>", &format!("{SNAPSHOT_CSS}</head>"), 1);
    let note = format!("<div class=\"snapshot-note\">Read-only snapshot, updated {}</div>", html_escape(stamp));
    let out = body_tag.replacen(&out, 1, |c: &Captures| format!("{}{note}", &c[1])).into_owned();
    (out, found)
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

struct Fetched {
    status: u16,
    content_type: String,
    text: String,
}

async fn get(router: &Router, url: &str) -> anyhow::Result<Fetched> {
    let mut url = url.to_string();
    for _ in 0..10 {
        let req = Request::builder().method("GET").uri(&url).body(Body::empty())?;
        let resp = router.clone().oneshot(req).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        let status = resp.status();
        if status.is_redirection() {
            if let Some(loc) = resp.headers().get(header::LOCATION).and_then(|v| v.to_str().ok()) {
                if loc.starts_with('/') && !loc.starts_with("//") {
                    url = loc.to_string();
                    continue;
                }
            }
        }
        let content_type = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let bytes = resp.into_body().collect().await.map_err(|e| anyhow::anyhow!("{e}"))?.to_bytes();
        return Ok(Fetched { status: status.as_u16(), content_type, text: String::from_utf8_lossy(&bytes).into_owned() });
    }
    anyhow::bail!("too many redirects")
}

/// Python `datetime.now().strftime("%a %b %d, %I:%M %p").replace(" 0", " ")`.
pub fn snapshot_stamp() -> String {
    chrono::Local::now().format("%a %b %d, %I:%M %p").to_string().replace(" 0", " ")
}

/// Crawl the dashboard into `out_dir` (emptied first) using `static_dir` for the assets.
pub async fn export_site_with(
    router: Router,
    out_dir: &Path,
    static_dir: Option<&Path>,
    leagues: &[&str],
    limit: usize,
    stamp: &str,
) -> anyhow::Result<ExportResult> {
    if out_dir.exists() {
        std::fs::remove_dir_all(out_dir)?;
    }
    std::fs::create_dir_all(out_dir)?;
    match static_dir {
        Some(dir) => copy_dir(dir, &out_dir.join("static"))?,
        None => crate::assets::write_static(&out_dir.join("static"))?,
    }
    // a project site lives under /<repo>/: no absolute paths
    let css = out_dir.join("static").join("style.css");
    if let Ok(text) = std::fs::read_to_string(&css) {
        std::fs::write(&css, text.replace("url(\"/static/", "url(\""))?;
    }
    let mut res = ExportResult::default();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut seen: HashSet<String> = HashSet::new();
    for lg in leagues {
        let q = form_urlencoded::Serializer::new(String::new()).append_pair("league", lg).finish();
        for p in std::iter::once("/").chain(SEED_PATHS.iter().copied()) {
            queue.push_back(format!("{p}?{q}"));
        }
    }
    let mut first: Option<String> = None;
    while let Some(url) = queue.pop_front() {
        let (path, query, _) = urlsplit(&url);
        let fname = page_file(path, query);
        if seen.contains(&fname) {
            continue;
        }
        if seen.len() >= limit {
            res.truncated = true;
            break;
        }
        seen.insert(fname.clone());
        let r = match get(&router, &url).await {
            Ok(r) => r,
            Err(e) => {
                // one bad page must not sink the snapshot
                res.errors.push(format!("{url}: {e}"));
                continue;
            }
        };
        if r.status >= 400 {
            res.errors.push(format!("{url}: HTTP {}", r.status));
        }
        if !r.content_type.contains("text/html") {
            continue;
        }
        let (body, links) = rewrite(&r.text, stamp);
        std::fs::write(out_dir.join(&fname), body)?;
        res.pages += 1;
        if first.is_none() {
            first = Some(fname);
        }
        queue.extend(links);
    }
    if let Some(first) = first {
        std::fs::copy(out_dir.join(&first), out_dir.join("index.html"))?;
    }
    Ok(res)
}

/// `export_site(router, out, leagues, limit)`: the Python entry point (static dir =
/// [`default_static_dir`], stamp = now).
pub async fn export_site(router: Router, out_dir: &Path, leagues: &[&str], limit: usize) -> anyhow::Result<ExportResult> {
    export_site_with(router, out_dir, default_static_dir().as_deref(), leagues, limit, &snapshot_stamp()).await
}
