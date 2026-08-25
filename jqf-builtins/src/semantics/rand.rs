//! The shared non-cryptographic PRNG (xoshiro256**), its seeded draw, and the `--seed` state threading.
//!
//! Home of the `~rng` ENGINE BINDING (a non-tiered language surface): it draws through this law, so it cannot live in
//! the tiered extension family; this module sits where the engine and the rand/analytics builtins both reach it.

use jqf_data::{Number, Value};
use jqf_resource::ResourceContext;

/// `pub(crate)` because `~rng` draws through the SAME law — the pin is `~rng(S).next` == `rand(S)`, so the two sites
/// must share one spelling and never drift.
#[allow(
    clippy::cast_precision_loss,
    reason = "the draw keeps only the high 53 bits, so the u64-to-f64 cast is exact"
)]
pub fn rand_float(rng: &mut Prng) -> Value {
    let bits = rng.next_u64() >> 11;
    Value::Number(Number::float(jqf_data::Float::new(
        (bits as f64) * (1.0 / 9_007_199_254_740_992.0),
    )))
}

/// Draws through one [`Prng`], shared by every otherwise-impure rand-family and analytics-family call site.
///
/// Without a host `--seed` (the ordinary case), `draw` runs on a fresh [`Prng::seeded`] and the request's draw state
/// stays untouched forever — unchanged behavior. Once the CLI primes `--seed` (`ResourceContext::
/// with_rand_seed`), each call TAKES the request's current draw material, runs `draw` on the [`Prng`] it seeds, and
/// puts back ONE MORE word off the same generator as the next call's material — so successive calls in one request
/// draw different values while the whole sequence stays a pure function of the CLI seed, and a repeated RUN with the
/// same seed answers byte-identically. This is safe with no lock: every builtin that reaches here is `Effects::Impure`,
/// which keeps its program off the morsel/parallel relay entirely (`is_morsel_eligible`), so a request's draw state is
/// read and written on one thread only.
pub fn with_prng<R>(resources: &ResourceContext<'_>, draw: impl FnOnce(&mut Prng) -> R) -> R {
    match resources.take_rand_seed_state() {
        Some(material) => {
            let mut rng = Prng::from_seed(material);
            let result = draw(&mut rng);
            resources.put_rand_seed_state(rng.next_u64());
            result
        }
        None => draw(&mut Prng::seeded()),
    }
}

/// A small non-cryptographic PRNG (xoshiro256**), seeded from uuid v4 entropy.
///
/// Sampling and shuffling are impure effects by contract, and their draws need no cryptographic generator — uuid's v4
/// already carries the system entropy this tree trusts for the same purpose, and `SplitMix64` stretches its 128 bits
/// into the 256-bit state.
pub struct Prng {
    state: [u64; 4],
}

impl Prng {
    #[allow(clippy::cast_possible_truncation)]
    pub fn seeded() -> Self {
        let seed = uuid::Uuid::new_v4().as_u128();
        let mut rng = Self::from_seed(seed as u64);
        rng.state[0] ^= (seed >> 64) as u64;
        rng
    }

    /// Seeds the state from one u64 via `SplitMix64` — the same stretcher `seeded` applies to uuid v4 entropy — so
    /// `rand(seed)` is deterministic given the seed. The low bit is NOT forced: `SplitMix64`'s first additive constant
    /// already makes the all-zero state unreachable, and forcing it here would collapse adjacent seeds (`42 | 1 == 43 |
    /// 1`).
    pub fn from_seed(seed: u64) -> Self {
        let mut state = [0u64; 4];
        let mut mixed = seed;
        for slot in &mut state {
            mixed = mixed.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = mixed;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            *slot = z ^ (z >> 31);
        }
        Self { state }
    }

    fn next_u64(&mut self) -> u64 {
        let result = self.state[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.state[1] << 17;
        self.state[2] ^= self.state[0];
        self.state[3] ^= self.state[1];
        self.state[1] ^= self.state[2];
        self.state[0] ^= self.state[3];
        self.state[2] ^= t;
        self.state[3] = self.state[3].rotate_left(45);
        result
    }

    /// One uniform integer in `0..n`, by rejection. The `usize` cast is lossless: `n` is an array length, so it fits a
    /// u64 on every target.
    /// (Only the `ext-hash` rand/analytics families draw through it.)
    #[cfg(feature = "ext-hash")]
    #[allow(clippy::cast_possible_truncation)]
    pub fn below(&mut self, n: usize) -> usize {
        self.below_u64(n as u64) as usize
    }

    /// One uniform integer in `0..width`, by rejection (the full u64 range, so `randint`'s bounds are never narrowed to
    /// a platform pointer width).
    #[cfg(feature = "ext-hash")]
    pub fn below_u64(&mut self, width: u64) -> u64 {
        let limit = u64::MAX - (u64::MAX % width);
        loop {
            let value = self.next_u64();
            if value < limit {
                return value % width;
            }
        }
    }
}
