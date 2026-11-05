//! The embedded asset lists (`src/assets.rs`) cover every file in `templates/` and `static/` and
//! are byte-identical to them, so a new template or static file can't be forgotten.

use std::path::{Path, PathBuf};

fn files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            files(root, &p, out);
        } else {
            out.push(p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
        }
    }
}

fn listing(sub: &str) -> (PathBuf, Vec<String>) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(sub);
    let mut v = Vec::new();
    files(&root, &root, &mut v);
    v.sort();
    (root, v)
}

#[test]
fn every_template_is_embedded() {
    let (root, on_disk) = listing("templates");
    let mut embedded: Vec<String> = fm_web::assets::TEMPLATES.iter().map(|(n, _)| n.to_string()).collect();
    embedded.sort();
    assert_eq!(embedded, on_disk, "update TEMPLATES in crates/fm-web/src/assets.rs");
    for (n, src) in fm_web::assets::TEMPLATES {
        assert_eq!(*src, std::fs::read_to_string(root.join(n)).unwrap(), "{n}");
    }
}

#[test]
fn every_static_file_is_embedded() {
    let (root, on_disk) = listing("static");
    let mut embedded: Vec<String> = fm_web::assets::STATIC.iter().map(|(n, _)| n.to_string()).collect();
    embedded.sort();
    assert_eq!(embedded, on_disk, "update STATIC in crates/fm-web/src/assets.rs");
    for (n, bytes) in fm_web::assets::STATIC {
        assert_eq!(*bytes, std::fs::read(root.join(n)).unwrap().as_slice(), "{n}");
    }
}

#[test]
fn write_static_round_trips() {
    let t = tempfile::tempdir().unwrap();
    fm_web::assets::write_static(t.path()).unwrap();
    assert!(t.path().join("fonts/barlow-400.woff2").is_file());
    assert_eq!(std::fs::read(t.path().join("style.css")).unwrap(), fm_web::assets::static_file("style.css").unwrap());
}
