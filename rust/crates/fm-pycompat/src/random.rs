//! CPython's `random.Random` (MT19937) so seeded simulations match draw for draw.
//! Implementation: `W1-pycompat`. Reference: CPython `Modules/_randommodule.c` + `Lib/random.py`.
//!
//! * `Random(seed)` for an int seed: `init_by_array` with the absolute value split into 32-bit
//!   words (little-endian; `0` -> `[0]`).
//! * `random()`: `(a >> 5, b >> 6)` -> `(a * 67108864 + b) / 9007199254740992`.
//! * `getrandbits(k)`, `_randbelow(n)` (getrandbits(n.bit_length()) rejection loop),
//!   `randrange`, `randint`, `shuffle` (Fisher-Yates from the end with `_randbelow(i + 1)`),
//!   `choices(population, k=)` without weights (`floor(random() * n)`), `choice`.
//! * `gauss(mu, sigma)`: Box-Muller pair with the cached `gauss_next` (NOT normalvariate).
//!   Python 3.12: `x2pi = random() * TWOPI; g2rad = sqrt(-2 * log(1 - random()))`.

pub struct PyRandom {
    _state: [u32; 624],
    _index: usize,
    _gauss_next: Option<f64>,
}

impl PyRandom {
    /// `random.Random(seed)` for a non-negative or negative int seed.
    pub fn new(_seed: i64) -> Self {
        todo!("W1-pycompat")
    }
    /// `random.random()`.
    pub fn random(&mut self) -> f64 {
        todo!("W1-pycompat")
    }
    /// `random.getrandbits(k)` for k <= 64.
    pub fn getrandbits(&mut self, _k: u32) -> u64 {
        todo!("W1-pycompat")
    }
    /// `random._randbelow(n)`.
    pub fn randbelow(&mut self, _n: u64) -> u64 {
        todo!("W1-pycompat")
    }
    /// `random.randrange(start, stop)` (step 1).
    pub fn randrange(&mut self, _start: i64, _stop: i64) -> i64 {
        todo!("W1-pycompat")
    }
    /// `random.shuffle(xs)`.
    pub fn shuffle<T>(&mut self, _xs: &mut [T]) {
        todo!("W1-pycompat")
    }
    /// `random.choices(range(n), k=k)` (no weights): indices.
    pub fn choices_idx(&mut self, _n: usize, _k: usize) -> Vec<usize> {
        todo!("W1-pycompat")
    }
    /// `random.gauss(mu, sigma)`.
    pub fn gauss(&mut self, _mu: f64, _sigma: f64) -> f64 {
        todo!("W1-pycompat")
    }
}
