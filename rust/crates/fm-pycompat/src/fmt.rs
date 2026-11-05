//! Python `format()` / f-string / `%` formatting of numbers. `fixed`, `percent`, `general` and
//! `repr_f64` are implemented (architect); `W1-pycompat` owns golden tests against CPython and
//! the rest. Rust's precision formatting rounds the exact binary value half-to-even like CPython.

/// `f"{x:.{prec}f}"`; with `sign` it is `f"{x:+.{prec}f}"`.
pub fn fixed(x: f64, prec: usize, sign: bool) -> String {
    if x.is_nan() {
        return if sign { "+nan".into() } else { "nan".into() };
    }
    if x.is_infinite() {
        return match (x > 0.0, sign) {
            (true, true) => "+inf".into(),
            (true, false) => "inf".into(),
            (false, _) => "-inf".into(),
        };
    }
    if sign {
        format!("{:+.*}", prec, x)
    } else {
        format!("{:.*}", prec, x)
    }
}

/// `f"{x:.{prec}%}"` (x * 100, fixed, then '%'); with `sign` `f"{x:+.{prec}%}"`.
pub fn percent(x: f64, prec: usize, sign: bool) -> String {
    format!("{}%", fixed(x * 100.0, prec, sign))
}

/// Split Rust's shortest/precision `{:e}` output into (negative, digits without dot, exponent).
fn sci_parts(s: &str) -> (bool, String, i32) {
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (mant, exp) = s.split_once('e').expect("exponent");
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    (neg, digits, exp.parse().expect("exp"))
}

fn exp_suffix(e: i32) -> String {
    format!("e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs())
}

/// `f"{x:g}"` / `f"{x:.{prec}g}"` (default precision 6): general format, trailing zeros removed,
/// exponent form `1e-05` / `1e+06` when exp < -4 or exp >= precision. Also used for `%g`.
pub fn general(x: f64, prec: Option<usize>, sign: bool) -> String {
    let p = prec.unwrap_or(6).max(1);
    let plus = if sign && !x.is_sign_negative() { "+" } else { "" };
    if x.is_nan() {
        return format!("{plus}nan");
    }
    if x.is_infinite() {
        return format!("{}inf", if x < 0.0 { "-" } else { plus });
    }
    if x == 0.0 {
        return format!("{}0", if x.is_sign_negative() { "-" } else { plus });
    }
    let (neg, digits, e) = sci_parts(&format!("{:.*e}", p - 1, x));
    let sgn = if neg { "-" } else { plus };
    if e < -4 || e >= p as i32 {
        let d = digits.trim_end_matches('0');
        let mant = if d.len() > 1 { format!("{}.{}", &d[..1], &d[1..]) } else { d.to_string() };
        format!("{sgn}{mant}{}", exp_suffix(e))
    } else {
        let decimals = (p as i32 - 1 - e).max(0) as usize;
        let s = format!("{:.*}", decimals, x.abs());
        let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
        format!("{sgn}{s}")
    }
}

/// `repr(x)` == `str(x)` for a float: shortest round-trip digits, `1.0`, `0.0001`, `1e-05`,
/// `1e+16`, `inf`, `nan`. Used by `str(float)`, f-strings without a spec, and `json.dumps`.
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
    let (neg, digits, e) = sci_parts(&format!("{:e}", x));
    let sgn = if neg { "-" } else { "" };
    let decpt = e + 1;
    if decpt <= -4 || decpt > 16 {
        let mant = if digits.len() > 1 { format!("{}.{}", &digits[..1], &digits[1..]) } else { digits.clone() };
        format!("{sgn}{mant}{}", exp_suffix(e))
    } else if decpt <= 0 {
        format!("{sgn}0.{}{}", "0".repeat((-decpt) as usize), digits)
    } else if decpt as usize >= digits.len() {
        format!("{sgn}{}{}.0", digits, "0".repeat(decpt as usize - digits.len()))
    } else {
        let (a, b) = digits.split_at(decpt as usize);
        format!("{sgn}{a}.{b}")
    }
}

/// Python `%`-style / `format` spec mini-language subset used by templates (`'%.2f'|format(x)`,
/// `%+d`, `%02d`, `%g`, `%s`): `spec` is the part after '%' (e.g. "+.1f").
pub fn percent_format(_spec: &str, _x: f64) -> String {
    todo!("W1-pycompat")
}

/// `f"{n:,}"` thousands separators for integers.
pub fn thousands(_n: i64) -> String {
    todo!("W1-pycompat")
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
    }
}
