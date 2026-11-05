//! `parity-diff EXPECTED.json ACTUAL.json [--tol REL] [--abs ABS] [--unordered PAT]...
//! [--ignore PAT]... [--tol-path PAT=REL,ABS]... [--no-key-order] [--set-order] [--cli-meta]
//! [--limit N]`: diff two JSON files with the parity comparator rules. Exit 0 = identical under
//! the rules, 1 = differences (printed with JSON paths), 2 = usage / IO error.

use fm_parity::compare::{compare, Tolerance};
use fm_parity::normalize::{cli_meta, set_order_fields};

fn usage() -> ! {
    eprintln!(
        "usage: parity-diff EXPECTED.json ACTUAL.json [--tol REL] [--abs ABS] [--unordered PAT]... \
         [--ignore PAT]... [--tol-path PAT=REL,ABS]... [--no-key-order] [--set-order] [--cli-meta] [--limit N]"
    );
    std::process::exit(2);
}

fn load(p: &str) -> serde_json::Value {
    let text = std::fs::read_to_string(p).unwrap_or_else(|e| {
        eprintln!("parity-diff: cannot read {p}: {e}");
        std::process::exit(2)
    });
    serde_json::from_str(&text).unwrap_or_else(|e| {
        eprintln!("parity-diff: {p} is not valid JSON: {e}");
        std::process::exit(2)
    })
}

fn num(s: Option<String>) -> f64 {
    s.and_then(|s| s.parse().ok()).unwrap_or_else(|| usage())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut files = Vec::new();
    let mut tol = Tolerance::default();
    let (mut set_order, mut meta) = (false, false);
    let mut limit = 50usize;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--tol" => tol.rel = num(args.next()),
            "--abs" => tol.abs = num(args.next()),
            "--unordered" => tol.unordered.push(args.next().unwrap_or_else(|| usage())),
            "--ignore" => tol.ignore.push(args.next().unwrap_or_else(|| usage())),
            "--tol-path" => {
                let s = args.next().unwrap_or_else(|| usage());
                let (pat, rest) = s.split_once('=').unwrap_or_else(|| usage());
                let (r, ab) = rest.split_once(',').unwrap_or_else(|| usage());
                tol.per_path.push((
                    pat.to_string(),
                    r.parse().unwrap_or_else(|_| usage()),
                    ab.parse().unwrap_or_else(|_| usage()),
                ));
            }
            "--no-key-order" => tol.key_order = false,
            "--set-order" => set_order = true,
            "--cli-meta" => meta = true,
            "--limit" => limit = num(args.next()) as usize,
            "-h" | "--help" => usage(),
            s if s.starts_with("--") => usage(),
            _ => files.push(a),
        }
    }
    if files.len() != 2 {
        usage();
    }
    if set_order {
        tol = set_order_fields(tol);
    }
    if meta {
        tol = cli_meta(tol);
    }
    let (expected, actual) = (load(&files[0]), load(&files[1]));
    let diffs = compare(&expected, &actual, &tol);
    if diffs.is_empty() {
        println!("identical under the parity rules");
        return;
    }
    for d in diffs.iter().take(limit) {
        println!("{d}");
    }
    if diffs.len() > limit {
        println!("... and {} more", diffs.len() - limit);
    }
    println!("{} difference(s)", diffs.len());
    std::process::exit(1);
}
