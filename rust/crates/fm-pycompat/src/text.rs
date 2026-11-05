//! Python `str` semantics (code-point indexing). Implementations: `W1-pycompat`.

/// `len(s)`: number of code points (NOT bytes).
pub fn py_len(s: &str) -> usize {
    s.chars().count()
}

/// `s[start:end]` with Python slice semantics on code points (negative indices count from the
/// end, out-of-range clamps). `None` = open end.
pub fn py_slice(_s: &str, _start: Option<isize>, _end: Option<isize>) -> String {
    todo!("W1-pycompat")
}

/// `s.capitalize()` (first char upper, REST LOWER).
pub fn capitalize(_s: &str) -> String {
    todo!("W1-pycompat")
}

/// `s.title()`.
pub fn title(_s: &str) -> String {
    todo!("W1-pycompat")
}

/// `s.strip(chars)` / `lstrip` / `rstrip` with a character SET (not a prefix).
pub fn strip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
    s.trim_matches(|c| chars.contains(c))
}
pub fn lstrip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
    s.trim_start_matches(|c| chars.contains(c))
}
pub fn rstrip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
    s.trim_end_matches(|c| chars.contains(c))
}

/// `s.split()` (no argument): splits on runs of Unicode whitespace, drops empties.
pub fn split_ws(s: &str) -> Vec<&str> {
    s.split_whitespace().collect()
}

/// `str.isdigit()` for ASCII-or-Unicode decimal digits (approximation: `char::is_numeric` is
/// broader; use this helper so the choice is made in one place).
pub fn is_digit(_s: &str) -> bool {
    todo!("W1-pycompat")
}
