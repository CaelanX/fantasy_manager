//! Python `str` semantics (code-point indexing, Python's whitespace/digit/case rules). Verified
//! against CPython 3.12 (Unicode 15.0) by `tests/golden.rs`. The tables at the bottom are
//! generated from CPython (`tests/golden/gen_golden.py` writes them into `text.json`; the golden
//! test checks the tables against it).
//!
//! Case mappings come from Rust's `char::to_lowercase/to_uppercase`, with the 110 code points
//! whose mapping changed in Unicode 16 pinned back to CPython's Unicode 15.0 behaviour
//! ([`py_lower`] / [`py_upper`]; plain `str::to_lowercase` differs from Python only there).
//! Deviation: the final-sigma rule inside `title`/`capitalize` uses an approximate
//! "case-ignorable" set.

/// `len(s)`: number of code points (NOT bytes).
pub fn py_len(s: &str) -> usize {
    s.chars().count()
}

/// Normalize Python slice bounds (step 1) to `(start, end)` code-point indices with
/// `start <= end <= len`.
fn slice_bounds(len: usize, start: Option<isize>, end: Option<isize>) -> (usize, usize) {
    let len_i = len as isize;
    let norm = |v: isize| -> usize {
        if v < 0 {
            (v + len_i).max(0) as usize
        } else {
            (v.min(len_i)) as usize
        }
    };
    let a = start.map_or(0, norm);
    let b = end.map_or(len, norm);
    (a, b.max(a))
}

/// `s[start:end]` with Python slice semantics on code points (negative indices count from the
/// end, out-of-range clamps). `None` = open end.
pub fn py_slice(s: &str, start: Option<isize>, end: Option<isize>) -> String {
    py_slice_str(s, start, end).to_string()
}

/// [`py_slice`] without allocating (a sub-slice of `s`).
pub fn py_slice_str(s: &str, start: Option<isize>, end: Option<isize>) -> &str {
    if s.is_ascii() {
        let (a, b) = slice_bounds(s.len(), start, end);
        return &s[a..b];
    }
    let (a, b) = slice_bounds(py_len(s), start, end);
    let byte = |n: usize| s.char_indices().nth(n).map_or(s.len(), |(i, _)| i);
    &s[byte(a)..byte(b)]
}

/// Code points whose case mapping in Rust's (newer, Unicode 16) tables differs from CPython 3.12
/// (Unicode 15.0): CPython maps them to themselves. Found by `golden_case_maps`.
fn unicode16_case_change(c: char) -> bool {
    matches!(c as u32, 0x19b | 0x264 | 0x1c89..=0x1c8a | 0xa7cb..=0xa7cf | 0xa7d2..=0xa7d5 | 0xa7da..=0xa7dc
        | 0x10d50..=0x10d65 | 0x10d70..=0x10d85 | 0x16ea0..=0x16eb8 | 0x16ebb..=0x16ed3)
}

/// Python's titlecase mapping of one character (`_PyUnicode_ToTitleFull`).
fn push_title(c: char, out: &mut String) {
    match TITLE_SPECIAL.binary_search_by(|(k, _)| k.cmp(&c)) {
        Ok(i) => out.push_str(TITLE_SPECIAL[i].1),
        Err(_) if unicode16_case_change(c) => out.push(c),
        Err(_) => out.extend(c.to_uppercase()),
    }
}

/// Titlecase letters (category Lt).
fn is_titlecase_letter(c: char) -> bool {
    matches!(c as u32, 0x1c5 | 0x1c8 | 0x1cb | 0x1f2 | 0x1f88..=0x1f8f | 0x1f98..=0x1f9f | 0x1fa8..=0x1faf
        | 0x1fbc | 0x1fcc | 0x1ffc)
}

/// `_PyUnicode_IsCased`: Lu, Ll, Lt, Other_Lowercase, Other_Uppercase.
pub fn is_cased(c: char) -> bool {
    if unicode16_case_change(c) && !matches!(c as u32, 0x19b | 0x264) {
        return false; // unassigned in Unicode 15.0
    }
    c.is_lowercase() || c.is_uppercase() || is_titlecase_letter(c)
}

/// `s.lower()` (full mappings, final sigma; CPython's Unicode 15.0 tables).
pub fn py_lower(s: &str) -> String {
    if !s.chars().any(unicode16_case_change) {
        return s.to_lowercase();
    }
    let cs: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for i in 0..cs.len() {
        push_lower(&cs, i, &mut out);
    }
    out
}

/// `s.upper()` (full mappings: `"ß".upper() == "SS"`; CPython's Unicode 15.0 tables).
pub fn py_upper(s: &str) -> String {
    if !s.chars().any(unicode16_case_change) {
        return s.to_uppercase();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if unicode16_case_change(c) {
            out.push(c);
        } else {
            out.extend(c.to_uppercase());
        }
    }
    out
}

/// Approximation of Unicode `Case_Ignorable` (only used by the final-sigma rule).
fn is_case_ignorable(c: char) -> bool {
    matches!(c as u32, 0x27 | 0x2e | 0x3a | 0x5e | 0x60 | 0xa8 | 0xad | 0xaf | 0xb4 | 0xb7 | 0xb8
        | 0x2b0..=0x36f | 0x483..=0x489 | 0x2018 | 0x2019 | 0x2024 | 0x2027 | 0x1ab0..=0x1aff
        | 0x1dc0..=0x1dff | 0x20d0..=0x20ff | 0xfe00..=0xfe0f | 0xfe20..=0xfe2f)
}

/// CPython `handle_capital_sigma`: lowercase of U+03A3 at index `i` of `cs`.
fn lower_sigma(cs: &[char], i: usize) -> char {
    let mut j = i;
    let mut final_sigma = false;
    while j > 0 {
        j -= 1;
        if !is_case_ignorable(cs[j]) {
            final_sigma = is_cased(cs[j]);
            break;
        }
    }
    if final_sigma {
        let mut j = i + 1;
        while j < cs.len() && is_case_ignorable(cs[j]) {
            j += 1;
        }
        final_sigma = j == cs.len() || !is_cased(cs[j]);
    }
    if final_sigma { '\u{3c2}' } else { '\u{3c3}' }
}

fn push_lower(cs: &[char], i: usize, out: &mut String) {
    if cs[i] == '\u{3a3}' {
        out.push(lower_sigma(cs, i));
    } else if unicode16_case_change(cs[i]) {
        out.push(cs[i]);
    } else {
        out.extend(cs[i].to_lowercase());
    }
}

/// `s.capitalize()` (Python 3.8+: first char TITLE-cased, rest lower-cased).
pub fn capitalize(s: &str) -> String {
    let cs: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for i in 0..cs.len() {
        if i == 0 {
            push_title(cs[0], &mut out);
        } else {
            push_lower(&cs, i, &mut out);
        }
    }
    out
}

/// `s.title()`: a character following a cased character is lower-cased, any other is
/// title-cased (so `"they're".title() == "They'Re"`).
pub fn title(s: &str) -> String {
    let cs: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut prev_cased = false;
    for i in 0..cs.len() {
        let c = cs[i];
        if prev_cased {
            push_lower(&cs, i, &mut out);
        } else {
            push_title(c, &mut out);
        }
        prev_cased = is_cased(c);
    }
    out
}

/// `s.strip(chars)` / `lstrip` / `rstrip` with a character SET (not a prefix).
pub fn strip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
    s.trim_matches(|c| chars.contains(c))
}
/// `s.lstrip(chars)`.
pub fn lstrip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
    s.trim_start_matches(|c| chars.contains(c))
}
/// `s.rstrip(chars)`.
pub fn rstrip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
    s.trim_end_matches(|c| chars.contains(c))
}

/// `c.isspace()` (Python's definition: also `\x1c`..`\x1f`, which Rust's `is_whitespace` lacks).
pub fn is_space(c: char) -> bool {
    matches!(c as u32, 0x09..=0x0d | 0x1c..=0x20 | 0x85 | 0xa0 | 0x1680 | 0x2000..=0x200a | 0x2028 | 0x2029
        | 0x202f | 0x205f | 0x3000)
}

/// `s.strip()` (no argument: Python whitespace).
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(is_space)
}
/// `s.lstrip()`.
pub fn py_lstrip(s: &str) -> &str {
    s.trim_start_matches(is_space)
}
/// `s.rstrip()`.
pub fn py_rstrip(s: &str) -> &str {
    s.trim_end_matches(is_space)
}

/// `s.split()` (no argument): splits on runs of Python whitespace, drops empties.
pub fn split_ws(s: &str) -> Vec<&str> {
    s.split(is_space).filter(|p| !p.is_empty()).collect()
}

/// `c.isdigit()` (Numeric_Type Digit or Decimal: ASCII digits, other scripts' decimal digits,
/// superscripts, circled digits...).
pub fn is_digit_char(c: char) -> bool {
    let cp = c as u32;
    ISDIGIT_RANGES
        .binary_search_by(|&(a, b)| {
            if cp < a {
                std::cmp::Ordering::Greater
            } else if cp > b {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// `s.isdigit()`: non-empty and every character is a digit (Python's Unicode definition, not
/// just ASCII: `"²".isdigit()` is True - then `int(s)` would fail in Python too).
pub fn is_digit(s: &str) -> bool {
    !s.is_empty() && s.chars().all(is_digit_char)
}

/// Title-case special mappings (char -> CPython `chr(c).title()` where it differs from upper).
pub type TitleTable = &'static [(char, &'static str)];
/// Inclusive code-point ranges.
pub type RangeTable = &'static [(u32, u32)];

/// The tables (title-case specials, isdigit ranges) for the golden test.
#[doc(hidden)]
pub fn tables() -> (TitleTable, RangeTable) {
    (TITLE_SPECIAL, ISDIGIT_RANGES)
}

// ---- generated from CPython 3.12 / Unicode 15.0 (see module docs) ----
const TITLE_SPECIAL: &[(char, &str)] = &[
    ('\u{df}', "Ss"),
    ('\u{1c4}', "\u{1c5}"),
    ('\u{1c5}', "\u{1c5}"),
    ('\u{1c6}', "\u{1c5}"),
    ('\u{1c7}', "\u{1c8}"),
    ('\u{1c8}', "\u{1c8}"),
    ('\u{1c9}', "\u{1c8}"),
    ('\u{1ca}', "\u{1cb}"),
    ('\u{1cb}', "\u{1cb}"),
    ('\u{1cc}', "\u{1cb}"),
    ('\u{1f1}', "\u{1f2}"),
    ('\u{1f2}', "\u{1f2}"),
    ('\u{1f3}', "\u{1f2}"),
    ('\u{587}', "\u{535}\u{582}"),
    ('\u{10d0}', "\u{10d0}"),
    ('\u{10d1}', "\u{10d1}"),
    ('\u{10d2}', "\u{10d2}"),
    ('\u{10d3}', "\u{10d3}"),
    ('\u{10d4}', "\u{10d4}"),
    ('\u{10d5}', "\u{10d5}"),
    ('\u{10d6}', "\u{10d6}"),
    ('\u{10d7}', "\u{10d7}"),
    ('\u{10d8}', "\u{10d8}"),
    ('\u{10d9}', "\u{10d9}"),
    ('\u{10da}', "\u{10da}"),
    ('\u{10db}', "\u{10db}"),
    ('\u{10dc}', "\u{10dc}"),
    ('\u{10dd}', "\u{10dd}"),
    ('\u{10de}', "\u{10de}"),
    ('\u{10df}', "\u{10df}"),
    ('\u{10e0}', "\u{10e0}"),
    ('\u{10e1}', "\u{10e1}"),
    ('\u{10e2}', "\u{10e2}"),
    ('\u{10e3}', "\u{10e3}"),
    ('\u{10e4}', "\u{10e4}"),
    ('\u{10e5}', "\u{10e5}"),
    ('\u{10e6}', "\u{10e6}"),
    ('\u{10e7}', "\u{10e7}"),
    ('\u{10e8}', "\u{10e8}"),
    ('\u{10e9}', "\u{10e9}"),
    ('\u{10ea}', "\u{10ea}"),
    ('\u{10eb}', "\u{10eb}"),
    ('\u{10ec}', "\u{10ec}"),
    ('\u{10ed}', "\u{10ed}"),
    ('\u{10ee}', "\u{10ee}"),
    ('\u{10ef}', "\u{10ef}"),
    ('\u{10f0}', "\u{10f0}"),
    ('\u{10f1}', "\u{10f1}"),
    ('\u{10f2}', "\u{10f2}"),
    ('\u{10f3}', "\u{10f3}"),
    ('\u{10f4}', "\u{10f4}"),
    ('\u{10f5}', "\u{10f5}"),
    ('\u{10f6}', "\u{10f6}"),
    ('\u{10f7}', "\u{10f7}"),
    ('\u{10f8}', "\u{10f8}"),
    ('\u{10f9}', "\u{10f9}"),
    ('\u{10fa}', "\u{10fa}"),
    ('\u{10fd}', "\u{10fd}"),
    ('\u{10fe}', "\u{10fe}"),
    ('\u{10ff}', "\u{10ff}"),
    ('\u{1f80}', "\u{1f88}"),
    ('\u{1f81}', "\u{1f89}"),
    ('\u{1f82}', "\u{1f8a}"),
    ('\u{1f83}', "\u{1f8b}"),
    ('\u{1f84}', "\u{1f8c}"),
    ('\u{1f85}', "\u{1f8d}"),
    ('\u{1f86}', "\u{1f8e}"),
    ('\u{1f87}', "\u{1f8f}"),
    ('\u{1f88}', "\u{1f88}"),
    ('\u{1f89}', "\u{1f89}"),
    ('\u{1f8a}', "\u{1f8a}"),
    ('\u{1f8b}', "\u{1f8b}"),
    ('\u{1f8c}', "\u{1f8c}"),
    ('\u{1f8d}', "\u{1f8d}"),
    ('\u{1f8e}', "\u{1f8e}"),
    ('\u{1f8f}', "\u{1f8f}"),
    ('\u{1f90}', "\u{1f98}"),
    ('\u{1f91}', "\u{1f99}"),
    ('\u{1f92}', "\u{1f9a}"),
    ('\u{1f93}', "\u{1f9b}"),
    ('\u{1f94}', "\u{1f9c}"),
    ('\u{1f95}', "\u{1f9d}"),
    ('\u{1f96}', "\u{1f9e}"),
    ('\u{1f97}', "\u{1f9f}"),
    ('\u{1f98}', "\u{1f98}"),
    ('\u{1f99}', "\u{1f99}"),
    ('\u{1f9a}', "\u{1f9a}"),
    ('\u{1f9b}', "\u{1f9b}"),
    ('\u{1f9c}', "\u{1f9c}"),
    ('\u{1f9d}', "\u{1f9d}"),
    ('\u{1f9e}', "\u{1f9e}"),
    ('\u{1f9f}', "\u{1f9f}"),
    ('\u{1fa0}', "\u{1fa8}"),
    ('\u{1fa1}', "\u{1fa9}"),
    ('\u{1fa2}', "\u{1faa}"),
    ('\u{1fa3}', "\u{1fab}"),
    ('\u{1fa4}', "\u{1fac}"),
    ('\u{1fa5}', "\u{1fad}"),
    ('\u{1fa6}', "\u{1fae}"),
    ('\u{1fa7}', "\u{1faf}"),
    ('\u{1fa8}', "\u{1fa8}"),
    ('\u{1fa9}', "\u{1fa9}"),
    ('\u{1faa}', "\u{1faa}"),
    ('\u{1fab}', "\u{1fab}"),
    ('\u{1fac}', "\u{1fac}"),
    ('\u{1fad}', "\u{1fad}"),
    ('\u{1fae}', "\u{1fae}"),
    ('\u{1faf}', "\u{1faf}"),
    ('\u{1fb2}', "\u{1fba}\u{345}"),
    ('\u{1fb3}', "\u{1fbc}"),
    ('\u{1fb4}', "\u{386}\u{345}"),
    ('\u{1fb7}', "\u{391}\u{342}\u{345}"),
    ('\u{1fbc}', "\u{1fbc}"),
    ('\u{1fc2}', "\u{1fca}\u{345}"),
    ('\u{1fc3}', "\u{1fcc}"),
    ('\u{1fc4}', "\u{389}\u{345}"),
    ('\u{1fc7}', "\u{397}\u{342}\u{345}"),
    ('\u{1fcc}', "\u{1fcc}"),
    ('\u{1ff2}', "\u{1ffa}\u{345}"),
    ('\u{1ff3}', "\u{1ffc}"),
    ('\u{1ff4}', "\u{38f}\u{345}"),
    ('\u{1ff7}', "\u{3a9}\u{342}\u{345}"),
    ('\u{1ffc}', "\u{1ffc}"),
    ('\u{fb00}', "Ff"),
    ('\u{fb01}', "Fi"),
    ('\u{fb02}', "Fl"),
    ('\u{fb03}', "Ffi"),
    ('\u{fb04}', "Ffl"),
    ('\u{fb05}', "St"),
    ('\u{fb06}', "St"),
    ('\u{fb13}', "\u{544}\u{576}"),
    ('\u{fb14}', "\u{544}\u{565}"),
    ('\u{fb15}', "\u{544}\u{56b}"),
    ('\u{fb16}', "\u{54e}\u{576}"),
    ('\u{fb17}', "\u{544}\u{56d}"),
];
const ISDIGIT_RANGES: &[(u32, u32)] = &[
    (0x30, 0x39), (0xb2, 0xb3), (0xb9, 0xb9), (0x660, 0x669), (0x6f0, 0x6f9), (0x7c0, 0x7c9),
    (0x966, 0x96f), (0x9e6, 0x9ef), (0xa66, 0xa6f), (0xae6, 0xaef), (0xb66, 0xb6f), (0xbe6, 0xbef),
    (0xc66, 0xc6f), (0xce6, 0xcef), (0xd66, 0xd6f), (0xde6, 0xdef), (0xe50, 0xe59), (0xed0, 0xed9),
    (0xf20, 0xf29), (0x1040, 0x1049), (0x1090, 0x1099), (0x1369, 0x1371), (0x17e0, 0x17e9), (0x1810, 0x1819),
    (0x1946, 0x194f), (0x19d0, 0x19da), (0x1a80, 0x1a89), (0x1a90, 0x1a99), (0x1b50, 0x1b59), (0x1bb0, 0x1bb9),
    (0x1c40, 0x1c49), (0x1c50, 0x1c59), (0x2070, 0x2070), (0x2074, 0x2079), (0x2080, 0x2089), (0x2460, 0x2468),
    (0x2474, 0x247c), (0x2488, 0x2490), (0x24ea, 0x24ea), (0x24f5, 0x24fd), (0x24ff, 0x24ff), (0x2776, 0x277e),
    (0x2780, 0x2788), (0x278a, 0x2792), (0xa620, 0xa629), (0xa8d0, 0xa8d9), (0xa900, 0xa909), (0xa9d0, 0xa9d9),
    (0xa9f0, 0xa9f9), (0xaa50, 0xaa59), (0xabf0, 0xabf9), (0xff10, 0xff19), (0x104a0, 0x104a9), (0x10a40, 0x10a43),
    (0x10d30, 0x10d39), (0x10e60, 0x10e68), (0x11052, 0x1105a), (0x11066, 0x1106f), (0x110f0, 0x110f9), (0x11136, 0x1113f),
    (0x111d0, 0x111d9), (0x112f0, 0x112f9), (0x11450, 0x11459), (0x114d0, 0x114d9), (0x11650, 0x11659), (0x116c0, 0x116c9),
    (0x11730, 0x11739), (0x118e0, 0x118e9), (0x11950, 0x11959), (0x11c50, 0x11c59), (0x11d50, 0x11d59), (0x11da0, 0x11da9),
    (0x11f50, 0x11f59), (0x16a60, 0x16a69), (0x16ac0, 0x16ac9), (0x16b50, 0x16b59), (0x1d7ce, 0x1d7ff), (0x1e140, 0x1e149),
    (0x1e2f0, 0x1e2f9), (0x1e4f0, 0x1e4f9), (0x1e950, 0x1e959), (0x1f100, 0x1f10a), (0x1fbf0, 0x1fbf9),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        assert_eq!(py_slice("h\u{e9}llo", Some(1), Some(3)), "\u{e9}l");
        assert_eq!(py_slice("h\u{e9}llo", Some(-2), None), "lo");
        assert_eq!(py_slice("h\u{e9}llo", Some(3), Some(1)), "");
        assert_eq!(py_slice("h\u{e9}llo", None, Some(99)), "h\u{e9}llo");
        assert_eq!(capitalize("hELLO world"), "Hello world");
        assert_eq!(title("they're bill's"), "They'Re Bill'S");
        assert_eq!(split_ws(" a\u{1c}b  c "), vec!["a", "b", "c"]);
        assert!(is_digit("123") && !is_digit("") && !is_digit("1.5") && is_digit("\u{b2}"));
    }
}
