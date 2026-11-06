//! The dashboard's templates and static files, compiled into the binary so an installed `fm`
//! (Docker image, `/usr/local/bin`) needs nothing beside it. Sources: `crates/fm-web/templates`
//! and `crates/fm-web/static`; `tests/assets.rs` fails when a file there is missing from these
//! lists. `FM_TEMPLATES_DIR` / `FM_STATIC_DIR` serve a directory instead (template editing
//! without a rebuild).

use std::path::{Path, PathBuf};

macro_rules! embed_str {
    ($($name:literal),* $(,)?) => { &[$(($name, include_str!(concat!("../templates/", $name)))),*] };
}
macro_rules! embed_bytes {
    ($($name:literal),* $(,)?) => { &[$(($name, include_bytes!(concat!("../static/", $name)) as &[u8])),*] };
}

/// `(name, source)` of every template.
pub const TEMPLATES: &[(&str, &str)] = embed_str!(
    "_macros.html",
    "base.html",
    "error.html",
    "health.html",
    "keepers.html",
    "login.html",
    "matchup.html",
    "news.html",
    "overview.html",
    "plan.html",
    "player.html",
    "player_card.html",
    "recommendations.html",
    "roster.html",
    "schedule.html",
    "setup.html",
    "trade_eval.html",
);

/// `(path below static/, bytes)` of every static file.
pub const STATIC: &[(&str, &[u8])] = embed_bytes!(
    "style.css",
    "moved.js",
    "card.js",
    "fonts/OFL.txt",
    "fonts/barlow-400.woff2",
    "fonts/barlow-500.woff2",
    "fonts/barlow-600.woff2",
    "fonts/barlow-condensed-600.woff2",
    "fonts/barlow-condensed-700.woff2",
);

pub fn template(name: &str) -> Option<&'static str> {
    TEMPLATES.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
}

pub fn static_file(path: &str) -> Option<&'static [u8]> {
    STATIC.iter().find(|(n, _)| *n == path).map(|(_, b)| *b)
}

/// Content type of a static file by extension.
pub fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "css" => "text/css",
        "js" => "text/javascript",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// `FM_TEMPLATES_DIR` (None: the embedded templates).
pub fn templates_override() -> Option<PathBuf> {
    std::env::var_os("FM_TEMPLATES_DIR").filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// `FM_STATIC_DIR` (None: the embedded static files).
pub fn static_override() -> Option<PathBuf> {
    std::env::var_os("FM_STATIC_DIR").filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Write the embedded static files below `dst`.
pub fn write_static(dst: &Path) -> std::io::Result<()> {
    for (name, bytes) in STATIC {
        let p = dst.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, bytes)?;
    }
    Ok(())
}
