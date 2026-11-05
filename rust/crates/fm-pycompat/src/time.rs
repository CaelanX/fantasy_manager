//! Python `datetime` formatting/parsing in the C locale. Implementations: `W1-pycompat`.

use chrono::{NaiveDate, NaiveDateTime};

/// `d.strftime(fmt)` for a date (English day/month names, `%d` zero padded, `%-d` unsupported
/// on Windows Python so it never appears). chrono's formatter is compatible for
/// `%a %A %b %B %d %m %Y %y %H %M %S %I %p %j`; this wrapper exists so the choice is central.
pub fn strftime_date(d: NaiveDate, fmt: &str) -> String {
    d.format(fmt).to_string()
}

/// `dt.strftime(fmt)` for a naive/local datetime.
pub fn strftime(dt: NaiveDateTime, fmt: &str) -> String {
    dt.format(fmt).to_string()
}

/// `datetime.fromisoformat(s)` (Python 3.11+ lenient: `Z`, offsets, fractional seconds, date
/// only). Returns the naive local part and the offset in seconds if present.
pub fn fromisoformat(_s: &str) -> Option<(NaiveDateTime, Option<i32>)> {
    todo!("W1-pycompat")
}

/// `datetime.strptime(s, fmt)` for the formats the providers use (`%a %b %d, %Y, %I:%M%p`,
/// `%b %d, %Y`, `%Y-%m-%d`, ...). English names, case-insensitive like Python.
pub fn strptime(_s: &str, _fmt: &str) -> Option<NaiveDateTime> {
    todo!("W1-pycompat")
}
