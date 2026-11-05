//! Ordering helpers that reproduce Python's `max`/`min`/`sorted` tie-breaking.
//! Implementations: work package `W1-pycompat` (the trivial ones are written here already).

use std::cmp::Ordering;

/// Total order on f64 for sort keys, matching Python comparisons for non-NaN floats.
/// (Python with NaN keys is ill-defined; the port treats NaN as equal to everything, which
/// keeps a stable sort's input order - assert in debug builds that keys are not NaN.)
pub fn cmp_f64(a: f64, b: f64) -> Ordering {
    debug_assert!(!a.is_nan() && !b.is_nan(), "NaN sort key");
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

/// Python `max(items, key=f)`: the FIRST item whose key is maximal (Rust's `max_by` returns the
/// last). `None` for an empty input (Python raises; callers decide).
pub fn first_max_by<T, I, F>(items: I, mut cmp: F) -> Option<T>
where
    I: IntoIterator<Item = T>,
    F: FnMut(&T, &T) -> Ordering,
{
    let mut best: Option<T> = None;
    for x in items {
        match &best {
            None => best = Some(x),
            Some(b) => {
                if cmp(&x, b) == Ordering::Greater {
                    best = Some(x);
                }
            }
        }
    }
    best
}

/// Python `min(items, key=f)`: the FIRST item whose key is minimal.
pub fn first_min_by<T, I, F>(items: I, mut cmp: F) -> Option<T>
where
    I: IntoIterator<Item = T>,
    F: FnMut(&T, &T) -> Ordering,
{
    let mut best: Option<T> = None;
    for x in items {
        match &best {
            None => best = Some(x),
            Some(b) => {
                if cmp(&x, b) == Ordering::Less {
                    best = Some(x);
                }
            }
        }
    }
    best
}

/// `xs.sort(key=f, reverse=True)`: stable descending (equal keys keep their input order -
/// Python's reverse sort is stable, so it is NOT `sort` followed by `reverse`).
pub fn sort_desc_by<T, F>(xs: &mut [T], mut cmp: F)
where
    F: FnMut(&T, &T) -> Ordering,
{
    xs.sort_by(|a, b| cmp(b, a));
}

/// Python tuple-key comparison helper: compares `Option<f64>` the way a key using
/// `(x is None, x)` would (None last). Python itself raises on comparing None with float, so a
/// translator must look at how the key was written; this covers the common idiom.
pub fn cmp_opt_f64_none_last(a: Option<f64>, b: Option<f64>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => cmp_f64(x, y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// `bisect.bisect_left(sorted, x)`.
pub fn bisect_left(sorted: &[f64], x: f64) -> usize {
    sorted.partition_point(|v| *v < x)
}

/// `bisect.bisect_right(sorted, x)`.
pub fn bisect_right(sorted: &[f64], x: f64) -> usize {
    sorted.partition_point(|v| *v <= x)
}

/// `itertools.combinations(range(n), k)` in Python's (lexicographic index) order.
pub fn combinations(_n: usize, _k: usize) -> Vec<Vec<usize>> {
    todo!("W1-pycompat")
}
