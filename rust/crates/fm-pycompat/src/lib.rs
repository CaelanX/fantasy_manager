//! Python-exact building blocks. Every other crate must use these instead of the "obvious" Rust
//! equivalent wherever the Python source used the corresponding builtin (see ARCHITECTURE.md
//! "Numeric and ordering conventions"). Owned by work package `W1-pycompat`.
//!
//! | Python                         | Rust                                   |
//! |--------------------------------|----------------------------------------|
//! | `sum(floats)` (3.12 Neumaier)   | [`num::py_sum`]                        |
//! | `round(x)` (half-even -> int)  | [`num::py_round`] / [`num::py_round_i64`] |
//! | `round(x, n)`                  | [`num::round_to`]                      |
//! | `x ** y` (floats)              | `f64::powf` (never `powi`)             |
//! | `int(x)` (truncate)            | `x.trunc() as i64`                     |
//! | `x // y`, `x % y` (floor)      | [`num::floordiv`], [`num::pymod`]      |
//! | `max(xs, key=)` (first max)    | [`order::first_max_by`]                |
//! | `min(xs, key=)` (first min)    | [`order::first_min_by`]                |
//! | `sorted(xs, key=, reverse=True)` | [`order::sort_desc_by`] (stable, NOT sort+reverse) |
//! | `f"{x:.2f}"`, `{x:+.1f}`, `{x:.0%}`, `{x:g}` | [`fmt`]                    |
//! | `repr(float)` / `str(float)`   | [`fmt::repr_f64`]                      |
//! | `json.dumps(...)`              | [`pyjson::dumps`]                      |
//! | `random.Random(seed)`          | [`random::PyRandom`]                   |
//! | `len(s)`, `s[:n]` on str       | [`text::py_len`], [`text::py_slice`]   |
//! | `date.strftime` (C locale)     | [`time::strftime`]                     |

pub mod fmt;
pub mod num;
pub mod order;
pub mod pyjson;
pub mod random;
pub mod text;
pub mod time;
