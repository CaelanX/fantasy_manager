//! Python float semantics. Verified against CPython 3.12 by `tests/golden.rs` (goldens from
//! `tests/golden/gen_golden.py`).

/// `sum(iterable_of_floats)` exactly as CPython 3.12+ computes it (compensated Neumaier
/// summation, `Python/bltinmodule.c` builtin_sum_impl). An explicit `total += x` loop in Python
/// must stay a plain `+=` loop in Rust - only the builtin `sum()` is compensated.
/// Note `sum(ints)` stays integer arithmetic: use `i64` sums for those.
/// Deviation: Python's `sum()` of an EMPTY float list is the int `0`; this returns `0.0`.
pub fn py_sum<I: IntoIterator<Item = f64>>(xs: I) -> f64 {
    py_sum_start(0.0, xs)
}

/// `sum(xs, start)` with a float start value.
pub fn py_sum_start<I: IntoIterator<Item = f64>>(start: f64, xs: I) -> f64 {
    let mut f = start;
    let mut c = 0.0f64;
    for x in xs {
        let t = f + x;
        if f.abs() >= x.abs() {
            c += (f - t) + x;
        } else {
            c += (x - t) + f;
        }
        f = t;
    }
    // "Avoid losing the sign on a negative result, and don't let adding the compensation
    // convert an infinite or overflowed sum to a NaN."
    if c != 0.0 && c.is_finite() {
        f += c;
    }
    f
}

/// `float(round(x))`: Python `round(x)` -> int (returned as f64), ties to even
/// (`round(2.5) == 2`, `round(-0.5) == 0`; the f64 keeps the sign: `-0.0`).
pub fn py_round(x: f64) -> f64 {
    x.round_ties_even()
}

/// `round(x)` as an int (`int(round(x))`), ties to even. Saturates outside the i64 range
/// (Python ints are unbounded; Python raises for inf/nan, this gives i64::MAX/MIN/0).
pub fn py_round_i64(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// `round(x, ndigits)` (`float.__round__`): correctly rounded on the exact binary value, ties to
/// even (`round(2.675, 2) == 2.67`, `round(0.125, 2) == 0.12`, `round(1234.5, -1) == 1230.0`).
/// Mirrors CPython `double_round`: `ndigits > 323` returns `x`, `ndigits < -308` returns
/// `0.0 * x`, zero / non-finite `x` is returned unchanged, a zero result keeps the sign of `x`.
/// Python raises `OverflowError` when the rounded value is too large (`round(1.7e308, -308)`);
/// this returns the signed infinity instead.
pub fn round_to(x: f64, ndigits: i32) -> f64 {
    if ndigits > 323 {
        return x;
    }
    if ndigits < -308 {
        return 0.0 * x;
    }
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    let r = if ndigits >= 0 {
        // Rust's precision formatting is exact and rounds half to even, like `_Py_dg_dtoa`
        // mode 3; parsing back is correctly rounded like `_Py_dg_strtod`.
        let s = format!("{:.*}", ndigits as usize, x);
        s.parse::<f64>().unwrap_or(x)
    } else {
        round_neg_digits(x, (-ndigits) as usize)
    };
    if r == 0.0 {
        if x.is_sign_negative() {
            -0.0
        } else {
            0.0
        }
    } else {
        r
    }
}

/// Exact `round(x, -k)` for k >= 1: round the exact integer part at the 10^k digit, half to even,
/// with "has a fractional part" as sticky bit.
fn round_neg_digits(x: f64, k: usize) -> f64 {
    let neg = x < 0.0;
    let zero = if neg { -0.0 } else { 0.0 };
    let ip = x.abs().trunc();
    let sticky = x.abs() != ip;
    let d = format!("{:.0}", ip); // exact: ip is an integer-valued f64
    let d = if d == "0" { String::new() } else { d };
    if d.len() < k {
        // |x| < 10^len <= 10^(k-1) < 0.5 * 10^k
        return zero;
    }
    let (head, tail) = d.split_at(d.len() - k);
    let tb = tail.as_bytes();
    let up = match tb[0].cmp(&b'5') {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => {
            if sticky || tb[1..].iter().any(|c| *c != b'0') {
                true
            } else {
                // exact tie: half to even on the last kept digit (none kept = 0 = even)
                head.as_bytes().last().is_some_and(|c| (c - b'0') % 2 == 1)
            }
        }
    };
    let mut digits: Vec<u8> = head.as_bytes().to_vec();
    if up {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, b'1');
                break;
            }
            i -= 1;
            if digits[i] == b'9' {
                digits[i] = b'0';
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    if digits.is_empty() {
        return zero;
    }
    let s = format!("{}{}e{}", if neg { "-" } else { "" }, String::from_utf8_lossy(&digits), k);
    s.parse::<f64>().unwrap_or(if neg { f64::NEG_INFINITY } else { f64::INFINITY })
}

/// Python `a // b` for floats (`float.__floordiv__`, CPython `_float_div_mod`): `-7.0 // 2 ==
/// -4.0`, `1.0 // 0.1 == 9.0` (NOT `floor(a / b)`, which gives 10.0). Division by zero: Python
/// raises `ZeroDivisionError`; this returns `a / b` (inf/NaN).
pub fn floordiv(a: f64, b: f64) -> f64 {
    float_divmod(a, b).0
}

/// Python `a % b` for floats (result has the sign of `b`; `-0.0 % 2.0 == 0.0`,
/// `1.0 % -2.0 == -1.0`). Division by zero: Python raises; this returns NaN.
pub fn pymod(a: f64, b: f64) -> f64 {
    float_divmod(a, b).1
}

/// `divmod(a, b)` for floats, exactly CPython's `_float_div_mod`.
pub fn float_divmod(vx: f64, wx: f64) -> (f64, f64) {
    if wx == 0.0 {
        return (vx / wx, f64::NAN);
    }
    let mut m = vx % wx; // C fmod
    let mut div = (vx - m) / wx;
    if m != 0.0 {
        if (wx < 0.0) != (m < 0.0) {
            m += wx;
            div -= 1.0;
        }
    } else {
        m = 0f64.copysign(wx);
    }
    let floordiv = if div != 0.0 {
        let mut f = div.floor();
        if div - f > 0.5 {
            f += 1.0;
        }
        f
    } else {
        0f64.copysign(vx / wx)
    };
    (floordiv, m)
}

/// Python `a // b` for ints (floor division). Panics on `b == 0` like Python raises.
pub fn floordiv_i64(a: i64, b: i64) -> i64 {
    let q = a / b;
    if a % b != 0 && ((a < 0) != (b < 0)) {
        q - 1
    } else {
        q
    }
}

/// Python `a % b` for ints (result has the sign of `b`). Panics on `b == 0`.
pub fn pymod_i64(a: i64, b: i64) -> i64 {
    let r = a % b;
    if r != 0 && ((r < 0) != (b < 0)) {
        r + b
    } else {
        r
    }
}

/// `statistics.median` (mean of the two middle values for an even count:
/// `(data[i - 1] + data[i]) / 2`). Panics on empty input like Python raises.
pub fn median(xs: &[f64]) -> f64 {
    assert!(!xs.is_empty(), "no median for empty data");
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// `math.isclose(a, b)` with Python defaults (rel_tol=1e-09, abs_tol=0.0).
pub fn isclose(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= 1e-9 * a.abs().max(b.abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding() {
        assert_eq!(py_round(2.5), 2.0);
        assert_eq!(py_round(3.5), 4.0);
        assert_eq!(round_to(2.675, 2), 2.67);
        assert_eq!(round_to(0.125, 2), 0.12);
        assert_eq!(round_to(0.375, 2), 0.38);
        assert_eq!(round_to(1234.5, -1), 1230.0);
        assert_eq!(round_to(1250.0, -2), 1200.0);
        assert_eq!(round_to(1350.0, -2), 1400.0);
        assert_eq!(round_to(1250.0001, -2), 1300.0);
        assert!(round_to(-4.0, -1).is_sign_negative());
    }

    #[test]
    fn neumaier() {
        // sum([0.1] * 10) == 1.0 in Python 3.12 (0.9999999999999999 with naive addition)
        assert_eq!(py_sum(std::iter::repeat_n(0.1, 10)), 1.0);
        assert_eq!(py_sum([1e100, 1.0, -1e100, 1.0]), 2.0);
    }

    #[test]
    fn floor_div_mod() {
        assert_eq!(floordiv(1.0, 0.1), 9.0);
        assert_eq!(floordiv(-7.0, 2.0), -4.0);
        assert_eq!(pymod(-7.0, 2.0), 1.0);
        assert_eq!(floordiv_i64(-7, 2), -4);
        assert_eq!(pymod_i64(-7, 2), 1);
        assert_eq!(pymod_i64(7, -2), -1);
    }
}
