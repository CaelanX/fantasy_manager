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
/// `k > n` gives no combinations, `k == 0` gives one empty combination (like Python).
pub fn combinations(n: usize, k: usize) -> Vec<Vec<usize>> {
    let mut out = Vec::new();
    if k > n {
        return out;
    }
    let mut idx: Vec<usize> = (0..k).collect();
    loop {
        out.push(idx.clone());
        // rightmost index that can still move (Python's itertools.combinations algorithm)
        let mut i = k;
        loop {
            if i == 0 {
                return out;
            }
            i -= 1;
            if idx[i] != i + n - k {
                break;
            }
        }
        idx[i] += 1;
        for j in i + 1..k {
            idx[j] = idx[j - 1] + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos() {
        assert_eq!(combinations(4, 2), vec![vec![0, 1], vec![0, 2], vec![0, 3], vec![1, 2], vec![1, 3], vec![2, 3]]);
        assert_eq!(combinations(3, 0), vec![Vec::<usize>::new()]);
        assert!(combinations(2, 3).is_empty());
        assert_eq!(combinations(5, 5).len(), 1);
        assert_eq!(combinations(10, 3).len(), 120);
    }

    #[test]
    fn first_extremes_and_stable_desc() {
        let xs = [(1, 3.0), (2, 5.0), (3, 5.0), (4, 1.0), (5, 1.0)];
        assert_eq!(first_max_by(xs.iter(), |a, b| cmp_f64(a.1, b.1)).unwrap().0, 2);
        assert_eq!(first_min_by(xs.iter(), |a, b| cmp_f64(a.1, b.1)).unwrap().0, 4);
        let mut v = xs.to_vec();
        sort_desc_by(&mut v, |a, b| cmp_f64(a.1, b.1));
        assert_eq!(v.iter().map(|x| x.0).collect::<Vec<_>>(), vec![2, 3, 1, 4, 5]);
        let s = [1.0, 2.0, 2.0, 3.0];
        assert_eq!((bisect_left(&s, 2.0), bisect_right(&s, 2.0)), (1, 3));
    }
}
