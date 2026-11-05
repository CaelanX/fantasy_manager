//! Python float semantics. The architect implemented the core functions so every wave can use
//! them; work package `W1-pycompat` owns golden tests against CPython for all of them.

/// `sum(iterable_of_floats)` exactly as CPython 3.12+ computes it (compensated Neumaier
/// summation, `Python/bltinmodule.c` builtin_sum_impl). An explicit `total += x` loop in Python
/// must stay a plain `+=` loop in Rust - only the builtin `sum()` is compensated.
/// Note `sum(ints)` stays integer arithmetic: use `i64` sums for those.
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

/// `round(x)` -> int (as f64), ties to even (`round(2.5) == 2`, `round(-0.5) == -0`).
pub fn py_round(x: f64) -> f64 {
    x.round_ties_even()
}

/// `int(round(x))`.
pub fn py_round_i64(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// `round(x, ndigits)`: correctly rounded on the exact binary value, ties to even
/// (`round(2.675, 2) == 2.67`, `round(0.125, 2) == 0.12`). Rust's precision formatting performs
/// exactly that rounding (exact decimal expansion, half-even), so format-then-parse is exact.
pub fn round_to(x: f64, ndigits: i32) -> f64 {
    if !x.is_finite() {
        return x;
    }
    if ndigits >= 0 {
        if ndigits > 22 {
            return x;
        }
        let s = format!("{:.*}", ndigits as usize, x);
        let r: f64 = s.parse().unwrap_or(x);
        // keep Python's signed zero behaviour: round(-0.001, 2) == -0.0
        if r == 0.0 && x.is_sign_negative() {
            -0.0
        } else {
            r
        }
    } else {
        let pow = 10f64.powi(-ndigits);
        let y = x / pow;
        let z = y.round_ties_even();
        z * pow
    }
}

/// Python `a // b` for floats (floor division; `-7.0 // 2 == -4.0`).
pub fn floordiv(a: f64, b: f64) -> f64 {
    (a / b).floor()
}

/// Python `a % b` for floats (result has the sign of `b`).
pub fn pymod(a: f64, b: f64) -> f64 {
    let r = a % b;
    if r != 0.0 && ((r < 0.0) != (b < 0.0)) {
        r + b
    } else {
        r
    }
}

/// Python `a // b` for ints.
pub fn floordiv_i64(a: i64, b: i64) -> i64 {
    let q = a / b;
    if a % b != 0 && ((a < 0) != (b < 0)) {
        q - 1
    } else {
        q
    }
}

/// `statistics.median` (mean of the two middle values for an even count). Panics on empty input
/// like Python raises.
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
    }

    #[test]
    fn neumaier() {
        // sum([0.1] * 10) == 1.0 in Python 3.12 (0.9999999999999999 with naive addition)
        assert_eq!(py_sum(std::iter::repeat(0.1).take(10)), 1.0);
        assert_eq!(py_sum([1e100, 1.0, -1e100, 1.0]), 2.0);
    }
}
