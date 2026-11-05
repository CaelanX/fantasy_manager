//! CPython's `random.Random` (MT19937) so seeded simulations match draw for draw. Verified
//! against CPython 3.12 by `tests/golden.rs`. Reference: CPython `Modules/_randommodule.c` +
//! `Lib/random.py`.
//!
//! * `Random(seed)` for an int seed: `init_by_array` with the absolute value split into 32-bit
//!   words (little-endian; `0` -> `[0]`).
//! * `random()`: `(a >> 5, b >> 6)` -> `(a * 67108864 + b) / 9007199254740992`.
//! * `getrandbits(k)`, `_randbelow(n)` (getrandbits(n.bit_length()) rejection loop),
//!   `randrange`, `randint`, `shuffle` (Fisher-Yates from the end with `_randbelow(i + 1)`),
//!   `choices(population, k=)` without weights (`floor(random() * n)`), `choice`.
//! * `gauss(mu, sigma)`: Box-Muller pair with the cached `gauss_next` (NOT normalvariate).
//!   Python 3.12: `x2pi = random() * TWOPI; g2rad = sqrt(-2 * log(1 - random()))`.
//!   `cos`/`sin`/`log` come from the platform libm like in CPython; the goldens check that they
//!   agree bit for bit on this toolchain.

const N: usize = 624;
const M: usize = 397;
const MATRIX_A: u32 = 0x9908_b0df;
const UPPER_MASK: u32 = 0x8000_0000;
const LOWER_MASK: u32 = 0x7fff_ffff;

#[derive(Clone)]
pub struct PyRandom {
    state: [u32; N],
    index: usize,
    gauss_next: Option<f64>,
}

impl std::fmt::Debug for PyRandom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PyRandom").field("index", &self.index).field("gauss_next", &self.gauss_next).finish()
    }
}

impl PyRandom {
    /// `random.Random(seed)` for a non-negative or negative int seed (Python uses `abs(seed)`).
    pub fn new(seed: i64) -> Self {
        let a = seed.unsigned_abs();
        let key: Vec<u32> = if a >> 32 == 0 { vec![a as u32] } else { vec![a as u32, (a >> 32) as u32] };
        Self::from_key(&key)
    }

    /// `random.Random(seed)` for an arbitrary-size non-negative int given as little-endian 32-bit
    /// words (`init_by_array`). An empty key behaves like `[0]`.
    pub fn from_key(key: &[u32]) -> Self {
        let key: &[u32] = if key.is_empty() { &[0] } else { key };
        let mut r = PyRandom { state: [0; N], index: N + 1, gauss_next: None };
        r.init_by_array(key);
        r
    }

    fn init_genrand(&mut self, s: u32) {
        let mt = &mut self.state;
        mt[0] = s;
        for i in 1..N {
            mt[i] = 1_812_433_253u32.wrapping_mul(mt[i - 1] ^ (mt[i - 1] >> 30)).wrapping_add(i as u32);
        }
        self.index = N;
    }

    fn init_by_array(&mut self, key: &[u32]) {
        self.init_genrand(19_650_218);
        let mt = &mut self.state;
        let (mut i, mut j) = (1usize, 0usize);
        let mut k = N.max(key.len());
        while k > 0 {
            mt[i] = (mt[i] ^ (mt[i - 1] ^ (mt[i - 1] >> 30)).wrapping_mul(1_664_525))
                .wrapping_add(key[j])
                .wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= N {
                mt[0] = mt[N - 1];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
            k -= 1;
        }
        k = N - 1;
        while k > 0 {
            mt[i] = (mt[i] ^ (mt[i - 1] ^ (mt[i - 1] >> 30)).wrapping_mul(1_566_083_941)).wrapping_sub(i as u32);
            i += 1;
            if i >= N {
                mt[0] = mt[N - 1];
                i = 1;
            }
            k -= 1;
        }
        mt[0] = 0x8000_0000;
        self.index = N;
    }

    /// `genrand_uint32` (one 32-bit MT output).
    pub fn genrand_u32(&mut self) -> u32 {
        if self.index >= N {
            let mt = &mut self.state;
            let mag = |y: u32| if y & 1 == 0 { 0 } else { MATRIX_A };
            for kk in 0..N - M {
                let y = (mt[kk] & UPPER_MASK) | (mt[kk + 1] & LOWER_MASK);
                mt[kk] = mt[kk + M] ^ (y >> 1) ^ mag(y);
            }
            for kk in N - M..N - 1 {
                let y = (mt[kk] & UPPER_MASK) | (mt[kk + 1] & LOWER_MASK);
                mt[kk] = mt[kk + M - N] ^ (y >> 1) ^ mag(y);
            }
            let y = (mt[N - 1] & UPPER_MASK) | (mt[0] & LOWER_MASK);
            mt[N - 1] = mt[M - 1] ^ (y >> 1) ^ mag(y);
            self.index = 0;
        }
        let mut y = self.state[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^= y >> 18;
        y
    }

    /// `random.random()`.
    pub fn random(&mut self) -> f64 {
        let a = self.genrand_u32() >> 5;
        let b = self.genrand_u32() >> 6;
        (a as f64 * 67_108_864.0 + b as f64) * (1.0 / 9_007_199_254_740_992.0)
    }

    /// `random.getrandbits(k)` for k <= 64 (`getrandbits(0) == 0`). Panics for k > 64.
    pub fn getrandbits(&mut self, k: u32) -> u64 {
        assert!(k <= 64, "getrandbits: k > 64 not supported");
        if k == 0 {
            return 0;
        }
        if k <= 32 {
            return (self.genrand_u32() >> (32 - k)) as u64;
        }
        // little-endian 32-bit words, the last one shifted down to the remaining bits
        let lo = self.genrand_u32() as u64;
        let rest = k - 32;
        let hi = (self.genrand_u32() >> (32 - rest)) as u64;
        lo | (hi << 32)
    }

    /// `random._randbelow(n)` (`_randbelow_with_getrandbits`). Python loops forever for n == 0;
    /// this returns 0.
    pub fn randbelow(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let k = 64 - n.leading_zeros();
        let mut r = self.getrandbits(k);
        while r >= n {
            r = self.getrandbits(k);
        }
        r
    }

    /// `random.randrange(start, stop)` (step 1). Panics on an empty range like Python raises.
    pub fn randrange(&mut self, start: i64, stop: i64) -> i64 {
        let width = stop as i128 - start as i128;
        assert!(width > 0, "empty range in randrange({start}, {stop})");
        (start as i128 + self.randbelow(width as u64) as i128) as i64
    }

    /// `random.randrange(stop)`.
    pub fn randrange_to(&mut self, stop: i64) -> i64 {
        self.randrange(0, stop)
    }

    /// `random.randint(a, b)` (inclusive).
    pub fn randint(&mut self, a: i64, b: i64) -> i64 {
        self.randrange(a, b + 1)
    }

    /// `random.choice(seq)` as an index into a sequence of length `n` (`_randbelow(len(seq))`).
    /// Panics on an empty sequence like Python raises IndexError.
    pub fn choice_idx(&mut self, n: usize) -> usize {
        assert!(n > 0, "Cannot choose from an empty sequence");
        self.randbelow(n as u64) as usize
    }

    /// `random.shuffle(xs)`.
    pub fn shuffle<T>(&mut self, xs: &mut [T]) {
        for i in (1..xs.len()).rev() {
            let j = self.randbelow(i as u64 + 1) as usize;
            xs.swap(i, j);
        }
    }

    /// `random.choices(range(n), k=k)` (no weights): indices (`floor(random() * n)`).
    pub fn choices_idx(&mut self, n: usize, k: usize) -> Vec<usize> {
        let nf = n as f64;
        (0..k).map(|_| (self.random() * nf).floor() as usize).collect()
    }

    /// `random.uniform(a, b)` (`a + (b - a) * random()`).
    pub fn uniform(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * self.random()
    }

    /// `random.gauss(mu, sigma)` (keeps the second Box-Muller value in `gauss_next`).
    pub fn gauss(&mut self, mu: f64, sigma: f64) -> f64 {
        let z = match self.gauss_next.take() {
            Some(z) => z,
            None => {
                let x2pi = self.random() * std::f64::consts::TAU;
                let g2rad = (-2.0 * (1.0 - self.random()).ln()).sqrt();
                self.gauss_next = Some(x2pi.sin() * g2rad);
                x2pi.cos() * g2rad
            }
        };
        mu + z * sigma
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        // python -c "import random; r=random.Random(7); print(r.random())"
        let mut r = PyRandom::new(7);
        assert_eq!(r.random(), 0.32383276483316237);
        let mut r = PyRandom::new(0);
        assert_eq!(r.random(), 0.8444218515250481);
    }
}
