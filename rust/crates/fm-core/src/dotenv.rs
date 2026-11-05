//! Port of python-dotenv's parser (`dotenv/parser.py`) and variable interpolation
//! (`dotenv/main.py::resolve_variables`, `dotenv/variables.py`) as pydantic-settings uses it:
//! `dotenv_values(path, encoding='utf8')` with the defaults `interpolate=True, override=True`.
//!
//! Unparseable statements are skipped (python-dotenv logs and moves on), never fatal.

use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

struct Res {
    multiline_ws: Regex,
    whitespace: Regex,
    export: Regex,
    single_key: Regex,
    unquoted_key: Regex,
    equal_sign: Regex,
    single_val: Regex,
    double_val: Regex,
    unquoted_val: Regex,
    comment: Regex,
    eol: Regex,
    rest_of_line: Regex,
    inline_comment: Regex,
    dq_escape: Regex,
    sq_escape: Regex,
    variable: Regex,
}

fn res() -> &'static Res {
    static R: OnceLock<Res> = OnceLock::new();
    R.get_or_init(|| {
        let r = |p: &str| Regex::new(p).unwrap();
        Res {
            multiline_ws: r(r"^\s*"),
            whitespace: r(r"^[^\S\r\n]*"),
            export: r(r"^(?:export[^\S\r\n]+)?"),
            single_key: r(r"^'([^']+)'"),
            unquoted_key: r(r"^([^=\#\s]+)"),
            equal_sign: r(r"^(=[^\S\r\n]*)"),
            single_val: r(r"^(?s)'((?:\\.|[^'\\])*)'"),
            double_val: r(r#"^(?s)"((?:\\.|[^"\\])*)""#),
            unquoted_val: r(r"^([^\r\n]*)"),
            comment: r(r"^(?:[^\S\r\n]*#[^\r\n]*)?"),
            eol: r(r"^[^\S\r\n]*(?:\r\n|\n|\r|$)"),
            rest_of_line: r(r"^[^\r\n]*(?:\r|\n|\r\n)?"),
            inline_comment: r(r"\s+#.*"),
            dq_escape: r(r#"\\[\\'"abfnrtv]"#),
            sq_escape: r(r"\\[\\']"),
            variable: r(r"\$\{([^}:]*)(?::-([^}]*))?\}"),
        }
    })
}

fn decode_escapes(re: &Regex, s: &str) -> String {
    re.replace_all(s, |c: &regex::Captures| {
        match c[0].chars().nth(1).unwrap() {
            'a' => "\x07",
            'b' => "\x08",
            'f' => "\x0c",
            'n' => "\n",
            'r' => "\r",
            't' => "\t",
            'v' => "\x0b",
            '\\' => "\\",
            '\'' => "'",
            _ => "\"",
        }
        .to_string()
    })
    .into_owned()
}

/// Match `re` (anchored with `^`) at `*pos`, advancing; returns capture group 1 (if any).
fn read<'a>(re: &Regex, s: &'a str, pos: &mut usize) -> Option<&'a str> {
    let caps = re.captures(&s[*pos..])?;
    let whole = caps.get(0).unwrap();
    let g1 = caps.get(1).map(|m| &s[*pos + m.start()..*pos + m.end()]);
    *pos += whole.end();
    Some(g1.unwrap_or(""))
}

/// Parse one binding. `Ok(None)` => no key (blank/comment/end), `Ok(Some((k, v)))`, `Err` => skipped.
fn parse_binding(s: &str, pos: &mut usize) -> Result<Option<(String, Option<String>)>, ()> {
    let r = res();
    let attempt = (|| -> Result<Option<(String, Option<String>)>, ()> {
        read(&r.multiline_ws, s, pos).ok_or(())?;
        if *pos >= s.len() {
            return Ok(None);
        }
        read(&r.export, s, pos).ok_or(())?;
        // parse_key
        let key: Option<String> = match s[*pos..].chars().next() {
            Some('#') => None,
            Some('\'') => Some(read(&r.single_key, s, pos).ok_or(())?.to_string()),
            _ => Some(read(&r.unquoted_key, s, pos).ok_or(())?.to_string()),
        };
        read(&r.whitespace, s, pos).ok_or(())?;
        let value = if s[*pos..].starts_with('=') {
            read(&r.equal_sign, s, pos).ok_or(())?;
            Some(match s[*pos..].chars().next() {
                Some('\'') => decode_escapes(&r.sq_escape, read(&r.single_val, s, pos).ok_or(())?),
                Some('"') => decode_escapes(&r.dq_escape, read(&r.double_val, s, pos).ok_or(())?),
                None | Some('\n') | Some('\r') => String::new(),
                _ => {
                    let part = read(&r.unquoted_val, s, pos).ok_or(())?;
                    let stripped = r.inline_comment.replace(part, "");
                    stripped.trim_end().to_string()
                }
            })
        } else {
            None
        };
        read(&r.comment, s, pos).ok_or(())?;
        read(&r.eol, s, pos).ok_or(())?;
        Ok(key.map(|k| (k, value)))
    })();
    if attempt.is_err() {
        let _ = read(&r.rest_of_line, s, pos);
    }
    attempt
}

/// Raw bindings in file order (duplicates kept), before interpolation.
pub fn parse(content: &str) -> Vec<(String, Option<String>)> {
    let text = content.strip_prefix('\u{feff}').unwrap_or(content);
    // Python opens the file in text mode with universal newlines.
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < text.len() {
        let before = pos;
        if let Ok(Some(kv)) = parse_binding(&text, &mut pos) {
            out.push(kv);
        }
        if pos == before {
            break; // cannot happen (regexes always progress or error), but never loop forever
        }
    }
    out
}

fn interpolate(value: &str, env: &HashMap<String, String>) -> String {
    let re = &res().variable;
    let mut out = String::new();
    let mut cursor = 0;
    for c in re.captures_iter(value) {
        let m = c.get(0).unwrap();
        out.push_str(&value[cursor..m.start()]);
        let name = c.get(1).map_or("", |m| m.as_str());
        let default = c.get(2).map_or("", |m| m.as_str());
        out.push_str(env.get(name).map(String::as_str).unwrap_or(default));
        cursor = m.end();
    }
    out.push_str(&value[cursor..]);
    out
}

/// `dotenv_values(...)`: parse then interpolate `${VAR}` / `${VAR:-default}` (later file values
/// override earlier ones, and file values override `process_env`). Keys with no `=` map to `None`.
pub fn dotenv_values(
    content: &str,
    process_env: &HashMap<String, String>,
) -> HashMap<String, Option<String>> {
    // On Windows Python's os.environ upper-cases names.
    let mut env: HashMap<String, String> = if cfg!(windows) {
        process_env.iter().map(|(k, v)| (k.to_uppercase(), v.clone())).collect()
    } else {
        process_env.clone()
    };
    let mut new_values: HashMap<String, Option<String>> = HashMap::new();
    for (k, v) in parse(content) {
        let resolved = v.map(|v| interpolate(&v, &env));
        // A None value shadows with "" (Variable.resolve maps None to "", skipping the default).
        env.insert(k.clone(), resolved.clone().unwrap_or_default());
        new_values.insert(k, resolved);
    }
    new_values
}
