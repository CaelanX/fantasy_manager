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
//! | `f"{x:.2f}"`, `{x:+.1f}`, `{x:.0%}`, `{x:g}`, `{x:.2e}` | [`fmt::fixed`], [`fmt::percent`], [`fmt::general`], [`fmt::exponent`] |
//! | `format(x, any_spec)`          | [`fmt::format_float`]                  |
//! | `"%+.1f" % x` (Jinja `format`) | [`fmt::percent_format`] / [`fmt::try_percent_format`] |
//! | `f"{n:,}"`                     | [`fmt::thousands`]                     |
//! | `repr(float)` / `str(float)`   | [`fmt::repr_f64`]                      |
//! | `json.dumps(...)`              | [`pyjson::dumps`] (+ `dumps_str`, `dumps_f64`) |
//! | `random.Random(seed)`          | [`random::PyRandom`]                   |
//! | `divmod`, `//`, `%` on ints    | [`num::floordiv_i64`], [`num::pymod_i64`] |
//! | `len(s)`, `s[:n]` on str       | [`text::py_len`], [`text::py_slice`]   |
//! | `s.lower()/upper()/title()/capitalize()` | [`text::py_lower`], [`text::py_upper`], [`text::title`], [`text::capitalize`] |
//! | `s.split()`, `s.strip()`, `s.isdigit()` | [`text::split_ws`], [`text::py_strip`], [`text::is_digit`] |
//! | `itertools.combinations(range(n), k)` | [`order::combinations`]          |
//! | `date.strftime` (C locale)     | [`time::strftime`], [`time::strftime_date`] |
//! | `datetime.strptime`            | [`time::strptime`]                     |
//! | `datetime/date.fromisoformat`  | [`time::fromisoformat`], [`time::date_fromisoformat`] |
//!
//! Every helper is checked against CPython 3.12 output by `tests/golden.rs` (goldens generated
//! by `tests/golden/gen_golden.py`).

pub mod fmt;
pub mod num;
pub mod order;
pub mod pyjson;
pub mod random;
pub mod text;
pub mod time;
