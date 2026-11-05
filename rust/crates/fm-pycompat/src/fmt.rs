//! Python `format()` / f-string / `%` formatting of numbers, verified against CPython 3.12 by
//! `tests/golden.rs`. Rust's precision formatting (`{:.N}`, `{:.Ne}`) is exact and rounds the
//! binary value half-to-even, like CPython's `_Py_dg_dtoa`, so the digits come from it and only
//! the layout (exponent form, padding, signs) is Python-specific here.
//!
//! Deviation: CPython prints a NaN with its sign bit (`format(-nan, 'f') == '-nan'`) only in some
//! paths; here NaN is always unsigned (`nan` / `+nan`). NaN never reaches output in the port.

/// `f"{x:.{prec}f}"`; with `sign` it is `f"{x:+.{prec}f}"`.
pub fn fixed(x: f64, prec: usize, sign: bool) -> String {
    let body = f_body(x.abs(), prec, false, false);
    format!("{}{}", sign_str(x, if sign { '+' } else { '-' }), body)
}

/// `f"{x:.{prec}%}"` (x * 100, fixed, then '%'); with `sign` `f"{x:+.{prec}%}"`.
pub fn percent(x: f64, prec: usize, sign: bool) -> String {
    format!("{}%", fixed(x * 100.0, prec, sign))
}

/// `f"{x:g}"` / `f"{x:.{prec}g}"` (default precision 6, precision 0 counts as 1): general
/// format, trailing zeros removed, exponent form `1e-05` / `1e+06` when exp < -4 or
/// exp >= precision. Also `"%g" % x`. With `sign`: `f"{x:+g}"`.
pub fn general(x: f64, prec: Option<usize>, sign: bool) -> String {
    let body = g_body(x.abs(), prec.unwrap_or(6), false, false);
    format!("{}{}", sign_str(x, if sign { '+' } else { '-' }), body)
}

/// `f"{x:.{prec}e}"` (`1.50e+03`); `upper` for `E`, `sign` for `+`.
pub fn exponent(x: f64, prec: usize, sign: bool, upper: bool) -> String {
    let body = e_body(x.abs(), prec, false, upper);
    format!("{}{}", sign_str(x, if sign { '+' } else { '-' }), body)
}

/// `repr(x)` == `str(x)` == `f"{x}"` for a float: shortest round-trip digits, `1.0`, `0.0001`,
/// `1e-05`, `1e+16`, `inf`, `nan`. Used by `str(float)`, f-strings without a spec, and
/// `json.dumps`.
pub fn repr_f64(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    let (digits, e) = shortest_digits(x.abs());
    let sgn = if x < 0.0 { "-" } else { "" };
    let decpt = e + 1;
    if decpt <= -4 || decpt > 16 {
        format!("{sgn}{}{}", dotted(&digits, false), exp_suffix(e, false))
    } else if decpt <= 0 {
        format!("{sgn}0.{}{}", "0".repeat((-decpt) as usize), digits)
    } else if decpt as usize >= digits.len() {
        format!("{sgn}{}{}.0", digits, "0".repeat(decpt as usize - digits.len()))
    } else {
        let (a, b) = digits.split_at(decpt as usize);
        format!("{sgn}{a}.{b}")
    }
}

/// `format(x, spec)` / `f"{x:spec}"` for a float with the full format-spec mini-language:
/// `[[fill]align][sign][z][#][0][width][,|_][.precision][type]`, types `e E f F g G %` and none
/// (`""` is `repr`, `".3"` is the repr-like general format). `None` where Python raises
/// (`ValueError: Unknown format code` etc.).
pub fn format_float(x: f64, spec: &str) -> Option<String> {
    let s = parse_spec(spec)?;
    let neg_zero_to_pos = s.z;
    let xv = if neg_zero_to_pos && x == 0.0 { 0.0 } else { x };
    let (body, xs) = match s.ty {
        Some('f') | Some('F') => (f_body(xv.abs(), s.prec.unwrap_or(6), s.alt, s.ty == Some('F')), xv),
        Some('e') | Some('E') => (e_body(xv.abs(), s.prec.unwrap_or(6), s.alt, s.ty == Some('E')), xv),
        Some('g') | Some('G') => (g_body(xv.abs(), s.prec.unwrap_or(6), s.alt, s.ty == Some('G')), xv),
        Some('%') => {
            let y = xv * 100.0;
            (format!("{}%", f_body(y.abs(), s.prec.unwrap_or(6), s.alt, false)), y)
        }
        Some('n') => (g_body(xv.abs(), s.prec.unwrap_or(6), s.alt, false), xv),
        None => match s.prec {
            None => {
                let r = repr_f64(xv.abs());
                // repr has no sign when abs'd; alt '#' is a no-op for repr except it forces '.'
                (r, xv)
            }
            Some(p) => (r_body(xv.abs(), p, s.alt), xv),
        },
        _ => return None,
    };
    let body = if let Some(g) = s.grouping { group_body(&body, g) } else { body };
    let sign = sign_str(xs, s.sign);
    Some(pad(&sign, &body, s.width, s.fill, s.align))
}

/// Python `%`-formatting of one float: `("%" + spec) % x`, `spec` being the part after '%'
/// (e.g. "+.1f", "02d", "g", "s", "-8.2f"). Flags `- + space 0 #`, width, precision, types
/// `f F e E g G d i u s r a`. `None` where Python raises (`%d` of inf/nan, `%x` of a float,
/// unknown codes). Used for Jinja's `'%.2f'|format(x)` etc. Note `%s`/`%r` print `str(x)` of a
/// FLOAT; an int argument must be formatted as an int by the caller.
pub fn try_percent_format(spec: &str, x: f64) -> Option<String> {
    let mut chars = spec.chars().peekable();
    let (mut left, mut plus, mut space, mut zero, mut alt) = (false, false, false, false, false);
    while let Some(&c) = chars.peek() {
        match c {
            '-' => left = true,
            '+' => plus = true,
            ' ' => space = true,
            '0' => zero = true,
            '#' => alt = true,
            _ => break,
        }
        chars.next();
    }
    let mut width = 0usize;
    while let Some(&c) = chars.peek() {
        if let Some(d) = c.to_digit(10) {
            width = width * 10 + d as usize;
            chars.next();
        } else {
            break;
        }
    }
    let mut prec: Option<usize> = None;
    if chars.peek() == Some(&'.') {
        chars.next();
        let mut p = 0usize;
        while let Some(&c) = chars.peek() {
            if let Some(d) = c.to_digit(10) {
                p = p * 10 + d as usize;
                chars.next();
            } else {
                break;
            }
        }
        prec = Some(p);
    }
    let ty = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let sign_flag = if plus { '+' } else if space { ' ' } else { '-' };
    let (sign, body, numeric) = match ty {
        'f' | 'F' => (sign_str(x, sign_flag), f_body(x.abs(), prec.unwrap_or(6), alt, ty == 'F'), true),
        'e' | 'E' => (sign_str(x, sign_flag), e_body(x.abs(), prec.unwrap_or(6), alt, ty == 'E'), true),
        'g' | 'G' => (sign_str(x, sign_flag), g_body(x.abs(), prec.unwrap_or(6), alt, ty == 'G'), true),
        'd' | 'i' | 'u' => {
            if !x.is_finite() {
                return None;
            }
            let t = x.trunc();
            let digits = format!("{:.0}", t.abs());
            let digits = match prec {
                Some(p) if digits.len() < p => format!("{}{}", "0".repeat(p - digits.len()), digits),
                _ => digits,
            };
            let s = if t < 0.0 { "-".to_string() } else { sign_str(1.0, sign_flag) };
            (s, digits, true)
        }
        's' | 'r' | 'a' => {
            let mut r = repr_f64(x);
            if let Some(p) = prec {
                r = r.chars().take(p).collect();
            }
            (String::new(), r, false)
        }
        _ => return None,
    };
    let len = sign.chars().count() + body.chars().count();
    if len >= width {
        return Some(format!("{sign}{body}"));
    }
    let fill = width - len;
    Some(if left {
        format!("{sign}{body}{}", " ".repeat(fill))
    } else if zero && numeric {
        format!("{sign}{}{body}", "0".repeat(fill))
    } else {
        format!("{}{sign}{body}", " ".repeat(fill))
    })
}

/// [`try_percent_format`], falling back to `str(x)` where Python would raise.
pub fn percent_format(spec: &str, x: f64) -> String {
    try_percent_format(spec, x).unwrap_or_else(|| repr_f64(x))
}

/// `f"{n:,}"` thousands separators for integers (`-1,234,567`).
pub fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let grouped = group_int(&digits, ',');
    if n < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

// ------------------------------------------------------------------------------------------
// internals

/// Split Rust's `{:e}` / `{:.Ne}` output of a NON-NEGATIVE value into (digits without dot, exp).
fn sci_parts(s: &str) -> (String, i32) {
    let (mant, exp) = s.split_once('e').unwrap_or((s, "0"));
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    (digits, exp.parse().unwrap_or(0))
}

/// Shortest round-trip digits of a positive finite value, as `_Py_dg_dtoa` mode 0 picks them:
/// among the shortest strings that round-trip, the one closest to the exact value, ties to even
/// (`repr(982928183363351.25) == '982928183363351.2'`; Rust's shortest gives `...351.3`). The
/// correctly rounded n-digit string IS the closest n-digit decimal, so use it when it
/// round-trips.
fn shortest_digits(a: f64) -> (String, i32) {
    let (digits, e) = sci_parts(&format!("{:e}", a));
    if digits.len() > 1 {
        let exact = format!("{:.*e}", digits.len() - 1, a);
        if exact.parse::<f64>().ok() == Some(a) {
            return sci_parts(&exact);
        }
    }
    (digits, e)
}

/// "12345" -> "1.2345" (alt: "1" -> "1.").
fn dotted(digits: &str, alt: bool) -> String {
    if digits.len() > 1 {
        format!("{}.{}", &digits[..1], &digits[1..])
    } else if alt {
        format!("{digits}.")
    } else {
        digits.to_string()
    }
}

fn exp_suffix(e: i32, upper: bool) -> String {
    format!("{}{}{:02}", if upper { 'E' } else { 'e' }, if e < 0 { '-' } else { '+' }, e.abs())
}

fn nonfinite(x: f64, upper: bool) -> Option<String> {
    let s = if x.is_nan() {
        "nan"
    } else if x.is_infinite() {
        "inf"
    } else {
        return None;
    };
    Some(if upper { s.to_uppercase() } else { s.to_string() })
}

/// Sign prefix for a value: '-' if negative (incl. -0.0, never NaN); else "+"/" "/"" per flag.
fn sign_str(x: f64, flag: char) -> String {
    if x.is_sign_negative() && !x.is_nan() {
        "-".into()
    } else {
        match flag {
            '+' => "+".into(),
            ' ' => " ".into(),
            _ => String::new(),
        }
    }
}

/// Fixed-point digits of a non-negative value.
fn f_body(a: f64, prec: usize, alt: bool, upper: bool) -> String {
    if let Some(s) = nonfinite(a, upper) {
        return s;
    }
    let s = format!("{:.*}", prec, a);
    if alt && prec == 0 {
        format!("{s}.")
    } else {
        s
    }
}

/// Exponent-form digits of a non-negative value (`1.500000e+03`).
fn e_body(a: f64, prec: usize, alt: bool, upper: bool) -> String {
    if let Some(s) = nonfinite(a, upper) {
        return s;
    }
    let (digits, e) = sci_parts(&format!("{:.*e}", prec, a));
    format!("{}{}", dotted(&digits, alt), exp_suffix(e, upper))
}

/// General-format digits of a non-negative value.
fn g_body(a: f64, prec: usize, alt: bool, upper: bool) -> String {
    if let Some(s) = nonfinite(a, upper) {
        return s;
    }
    let p = prec.max(1);
    if a == 0.0 {
        return if alt { format!("0.{}", "0".repeat(p - 1)) } else { "0".into() };
    }
    let (digits, e) = sci_parts(&format!("{:.*e}", p - 1, a));
    if e < -4 || e >= p as i32 {
        let d = if alt { digits.as_str() } else { digits.trim_end_matches('0') };
        format!("{}{}", dotted(d, alt), exp_suffix(e, upper))
    } else {
        let decimals = (p as i32 - 1 - e).max(0) as usize;
        let s = format!("{:.*}", decimals, a);
        if alt {
            if s.contains('.') { s } else { format!("{s}.") }
        } else if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s
        }
    }
}

/// Format spec with a precision but no type (`format(x, ".3")`): like `g`, but the exponent
/// threshold is the precision and fixed notation keeps at least one digit after the point.
fn r_body(a: f64, prec: usize, alt: bool) -> String {
    if let Some(s) = nonfinite(a, false) {
        return s;
    }
    let p = prec.max(1);
    if a == 0.0 {
        return "0.0".into();
    }
    let (digits, e) = sci_parts(&format!("{:.*e}", p - 1, a));
    let d = digits.trim_end_matches('0');
    let d = if d.is_empty() { "0" } else { d };
    if e < -4 || e >= p as i32 {
        format!("{}{}", dotted(d, alt), exp_suffix(e, false))
    } else if e < 0 {
        format!("0.{}{}", "0".repeat((-e - 1) as usize), d)
    } else {
        let ip = (e + 1) as usize;
        if d.len() > ip {
            format!("{}.{}", &d[..ip], &d[ip..])
        } else {
            format!("{}{}.0", d, "0".repeat(ip - d.len()))
        }
    }
}

fn group_int(digits: &str, sep: char) -> String {
    let n = digits.len();
    let mut out = String::with_capacity(n + n / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (n - i).is_multiple_of(3) {
            out.push(sep);
        }
        out.push(c);
    }
    out
}

/// Insert grouping separators into the integer part of a formatted body.
fn group_body(body: &str, sep: char) -> String {
    let end = body.find(|c: char| !c.is_ascii_digit()).unwrap_or(body.len());
    if end == 0 {
        return body.to_string();
    }
    format!("{}{}", group_int(&body[..end], sep), &body[end..])
}

struct Spec {
    fill: char,
    align: Option<char>,
    sign: char,
    z: bool,
    alt: bool,
    width: usize,
    grouping: Option<char>,
    prec: Option<usize>,
    ty: Option<char>,
}

fn parse_spec(spec: &str) -> Option<Spec> {
    let cs: Vec<char> = spec.chars().collect();
    let mut i = 0;
    let mut fill = ' ';
    let mut align = None;
    let is_align = |c: char| matches!(c, '<' | '>' | '^' | '=');
    if cs.len() >= 2 && is_align(cs[1]) {
        fill = cs[0];
        align = Some(cs[1]);
        i = 2;
    } else if !cs.is_empty() && is_align(cs[0]) {
        align = Some(cs[0]);
        i = 1;
    }
    let mut sign = '-';
    if i < cs.len() && matches!(cs[i], '+' | '-' | ' ') {
        sign = cs[i];
        i += 1;
    }
    let mut z = false;
    if i < cs.len() && cs[i] == 'z' {
        z = true;
        i += 1;
    }
    let mut alt = false;
    if i < cs.len() && cs[i] == '#' {
        alt = true;
        i += 1;
    }
    if i < cs.len() && cs[i] == '0' {
        if align.is_none() {
            fill = '0';
            align = Some('=');
        }
        i += 1;
    }
    let mut width = 0usize;
    while i < cs.len() && cs[i].is_ascii_digit() {
        width = width * 10 + cs[i].to_digit(10)? as usize;
        i += 1;
    }
    let mut grouping = None;
    if i < cs.len() && matches!(cs[i], ',' | '_') {
        grouping = Some(cs[i]);
        i += 1;
    }
    let mut prec = None;
    if i < cs.len() && cs[i] == '.' {
        i += 1;
        let st = i;
        let mut p = 0usize;
        while i < cs.len() && cs[i].is_ascii_digit() {
            p = p * 10 + cs[i].to_digit(10)? as usize;
            i += 1;
        }
        if i == st {
            return None;
        }
        prec = Some(p);
    }
    let ty = if i < cs.len() {
        i += 1;
        Some(cs[i - 1])
    } else {
        None
    };
    if i != cs.len() {
        return None;
    }
    Some(Spec { fill, align, sign, z, alt, width, grouping, prec, ty })
}

/// Pad `sign + body` to `width`. Numbers align right by default; '=' puts the fill between the
/// sign and the digits (also for inf/nan, like CPython: `format(inf, '010f') == '0000000inf'`).
fn pad(sign: &str, body: &str, width: usize, fill: char, align: Option<char>) -> String {
    let len = sign.chars().count() + body.chars().count();
    if len >= width {
        return format!("{sign}{body}");
    }
    let n = width - len;
    let f = |k: usize| std::iter::repeat_n(fill, k).collect::<String>();
    match align.unwrap_or('>') {
        '<' => format!("{sign}{body}{}", f(n)),
        '^' => format!("{}{sign}{body}{}", f(n / 2), f(n - n / 2)),
        '=' => format!("{sign}{}{body}", f(n)),
        _ => format!("{}{sign}{body}", f(n)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repr() {
        for (x, s) in [(1.0, "1.0"), (0.1, "0.1"), (1e-5, "1e-05"), (1e16, "1e+16"), (1e15, "1000000000000000.0"),
                       (0.0001, "0.0001"), (-2.5, "-2.5"), (123.456, "123.456"), (1.5e-7, "1.5e-07"),
                       (1.0541809484618251e-16, "1.0541809484618251e-16")] {
            assert_eq!(repr_f64(x), s, "{x}");
        }
    }

    #[test]
    fn general_fmt() {
        for (x, s) in [(1.0, "1"), (0.5, "0.5"), (1e-5, "1e-05"), (123456.0, "123456"), (1234567.0, "1.23457e+06"),
                       (0.0001, "0.0001"), (2.50, "2.5"), (-0.0, "-0"), (100.0, "100")] {
            assert_eq!(general(x, None, false), s, "{x}");
        }
        assert_eq!(general(0.123456789, Some(4), false), "0.1235");
    }

    #[test]
    fn fixed_fmt() {
        assert_eq!(fixed(2.675, 2, false), "2.67");
        assert_eq!(fixed(-0.001, 2, false), "-0.00");
        assert_eq!(fixed(1.25, 1, true), "+1.2");
        assert_eq!(percent(0.125, 0, false), "12%");
        assert_eq!(percent(0.4567, 1, true), "+45.7%");
        assert_eq!(thousands(-1234567), "-1,234,567");
        assert_eq!(percent_format("+.1f", 2.25), "+2.2");
        assert_eq!(percent_format("02d", 7.9), "07");
    }
}
