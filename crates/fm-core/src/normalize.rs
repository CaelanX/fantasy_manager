//! `matching/normalize.py`: the RICH name normalizer (nicknames, suffixes, initials) used for
//! cross-source matching, plus team / position normalization. Distinct from
//! `models::normalize_name` (Player.name_norm).

use unicode_normalization::char::canonical_combining_class;
use unicode_normalization::UnicodeNormalization;

use crate::Set;

/// Python `str.isspace` (what `\S` / `str.split()` use): Unicode White_Space plus U+001C..U+001F.
fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// The `_SPECIAL` translation table (characters NFKD does not decompose).
fn special(c: char) -> Option<&'static str> {
    Some(match c {
        'ø' | 'Ø' => "o",
        'æ' | 'Æ' => "ae",
        'œ' | 'Œ' => "oe",
        'ß' => "ss",
        'ł' | 'Ł' => "l",
        'đ' | 'Đ' | 'ð' => "d",
        'þ' => "th",
        'ı' => "i",
        _ => return None,
    })
}

fn is_suffix(t: &str) -> bool {
    matches!(t, "jr" | "sr" | "ii" | "iii" | "iv")
}

fn nickname(t: &str) -> Option<&'static str> {
    Some(match t {
        "alex" => "alexander",
        "mitch" => "mitchell",
        "matt" => "matthew",
        "nick" => "nicholas",
        "mike" => "michael",
        "chris" => "christopher",
        "zach" => "zachary",
        "josh" => "joshua",
        "will" => "william",
        _ => return None,
    })
}

/// `strip_accents(s)`: translation table, NFKD, drop combining marks.
pub fn strip_accents(s: &str) -> String {
    let mut translated = String::with_capacity(s.len());
    for c in s.chars() {
        match special(c) {
            Some(r) => translated.push_str(r),
            None => translated.push(c),
        }
    }
    translated.nfkd().filter(|&c| canonical_combining_class(c) == 0).collect()
}

/// `normalize_name(name)` (matching flavour).
pub fn normalize_name(name: Option<&str>) -> String {
    let Some(name) = name else { return String::new() };
    if name.is_empty() {
        return String::new();
    }
    let lowered = strip_accents(name).to_lowercase();
    // drop apostrophes; hyphen / dash variants become spaces
    let mut s = String::with_capacity(lowered.len() + 4);
    for c in lowered.chars() {
        match c {
            '\'' | '’' | '‘' | '`' | '´' => {}
            '-' | '‐' | '–' => s.push(' '),
            _ => s.push(c),
        }
    }
    // re.sub(r"\.(?=\S)", ". ", s) then drop every period
    let chars: Vec<char> = s.chars().collect();
    let mut dotted = String::with_capacity(s.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        if c == '.' {
            // period removed; a following non-space char gets a token boundary
            if chars.get(i + 1).is_some_and(|&n| !py_isspace(n)) {
                dotted.push(' ');
            }
        } else {
            dotted.push(c);
        }
    }
    // [^a-z0-9 ]+ -> " "
    let cleaned: String = dotted.chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == ' ' { c } else { ' ' }).collect();
    let mut tokens: Vec<String> = cleaned.split_whitespace().map(str::to_string).collect();
    // merge a leading run of single letters: "t j oshie" -> "tj oshie"
    if tokens.len() >= 3 && tokens[0].len() == 1 {
        let mut i = 0;
        while i < tokens.len() - 1 && tokens[i].len() == 1 {
            i += 1;
        }
        if i >= 2 {
            let merged: String = tokens[..i].concat();
            let mut rest = vec![merged];
            rest.extend(tokens.drain(i..));
            tokens = rest;
        }
    }
    let filtered: Vec<String> = tokens.iter().filter(|t| !is_suffix(t)).cloned().collect();
    if !filtered.is_empty() {
        tokens = filtered;
    }
    if tokens.len() > 1 {
        if let Some(n) = nickname(&tokens[0]) {
            tokens[0] = n.to_string();
        }
    }
    tokens.join(" ")
}

fn team_alias(t: &str) -> Option<&'static str> {
    Some(match t {
        "NJ" => "NJD",
        "SJ" => "SJS",
        "TB" => "TBL",
        "LA" => "LAK",
        "LV" | "VEG" => "VGK",
        "UTAH" | "UHC" | "ARI" => "UTA",
        "WAS" => "WSH",
        "MON" => "MTL",
        "CLS" | "CLB" => "CBJ",
        "NAS" => "NSH",
        "CAL" => "CGY",
        "WIN" => "WPG",
        "ANH" => "ANA",
        _ => return None,
    })
}

/// `normalize_team(code)`: canonical NHL team abbreviation via TEAM_ALIASES (None / "" -> None;
/// unknown codes uppercased).
pub fn normalize_team(team: Option<&str>) -> Option<String> {
    let team = team?;
    if team.is_empty() {
        return None;
    }
    let t = team.trim().to_uppercase();
    Some(team_alias(&t).map(str::to_string).unwrap_or(t))
}

/// `normalize_positions("C/LW")` -> {"C","LW"} (a position STRING; iteration order is irrelevant).
pub fn normalize_positions(pos: Option<&str>) -> Set<String> {
    let mut out = Set::new();
    let Some(pos) = pos else { return out };
    if pos.is_empty() {
        return out;
    }
    let up = pos.trim().to_uppercase();
    for part in up.split(|c: char| c == '/' || c == ',' || py_isspace(c)).filter(|p| !p.is_empty()) {
        match part {
            "L" | "LW" => {
                out.insert("LW".to_string());
            }
            "R" | "RW" => {
                out.insert("RW".to_string());
            }
            "C" | "D" | "G" => {
                out.insert(part.to_string());
            }
            "F" => {
                out.extend(["C", "LW", "RW"].map(String::from));
            }
            "W" => {
                out.extend(["LW", "RW"].map(String::from));
            }
            _ => {}
        }
    }
    out
}

fn is_forward(p: &String) -> bool {
    matches!(p.as_str(), "C" | "LW" | "RW")
}

/// `positions_agree(a, b)`.
pub fn positions_agree(a: Option<&str>, b: Option<&str>) -> bool {
    let (pa, pb) = (normalize_positions(a), normalize_positions(b));
    if pa.is_empty() || pb.is_empty() {
        return false;
    }
    if pa.iter().any(|x| pb.contains(x)) {
        return true;
    }
    pa.iter().any(is_forward) && pb.iter().any(is_forward)
}
