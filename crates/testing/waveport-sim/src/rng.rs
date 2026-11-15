//! Seeds and the random number generator.

use core::fmt;
use core::num::ParseIntError;
use core::ops::Range;
use core::str::FromStr;

/// Identifies one simulated run: the same seed always gives the same run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seed(pub u64);

impl fmt::Display for Seed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#018x}", self.0)
    }
}

impl FromStr for Seed {
    type Err = ParseIntError;

    /// Parses a decimal number, or a hexadecimal one starting with `0x`.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            Some(hex) => u64::from_str_radix(hex, 16),
            None => text.parse(),
        }
        .map(Self)
    }
}

/// One step of `SplitMix64`: advances `state` and returns the next output.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// 64-bit FNV-1a, used to turn a label into a number.
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// A deterministic random number generator: `xoshiro256++`, seeded through
/// `SplitMix64`.
///
/// The sequence a seed produces is frozen by tests and will not change.
/// It is not suitable for cryptography.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimRng {
    /// The seed this generator started from; forks derive from it.
    origin: u64,
    state: [u64; 4],
}

impl SimRng {
    /// Starts the sequence that belongs to `seed`.
    #[must_use]
    pub fn new(seed: Seed) -> Self {
        let mut counter = seed.0;
        let state = [
            splitmix(&mut counter),
            splitmix(&mut counter),
            splitmix(&mut counter),
            splitmix(&mut counter),
        ];
        Self {
            origin: seed.0,
            state,
        }
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let [s0, s1, s2, s3] = self.state;
        let result = s0.wrapping_add(s3).rotate_left(23).wrapping_add(s0);
        let t = s1 << 17;
        let s2 = s2 ^ s0;
        let s3 = s3 ^ s1;
        let s1 = s1 ^ s2;
        let s0 = s0 ^ s3;
        self.state = [s0, s1, s2 ^ t, s3.rotate_left(45)];
        result
    }

    /// A number below `bound`, every value equally likely; 0 when `bound` is 0.
    fn below(&mut self, bound: u64) -> u64 {
        if bound <= 1 {
            return 0;
        }
        // Draw as many bits as `bound - 1` has and retry until the value
        // fits. This is unbiased and needs fewer than two draws on average.
        let mask = u64::MAX >> (bound - 1).leading_zeros();
        loop {
            let value = self.next_u64() & mask;
            if value < bound {
                return value;
            }
        }
    }

    /// A number in `range`, every value equally likely.
    ///
    /// Returns `range.start` when the range is empty.
    pub fn range(&mut self, range: Range<u64>) -> u64 {
        let width = range.end.saturating_sub(range.start);
        range.start.saturating_add(self.below(width))
    }

    /// True `numerator` times out of `denominator`.
    ///
    /// Never true when `denominator` is 0.
    pub fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        denominator != 0 && self.below(denominator) < numerator
    }

    /// An index below `len`, or 0 when `len` is 0.
    fn index(&mut self, len: usize) -> usize {
        let bound = u64::try_from(len).unwrap_or(u64::MAX);
        usize::try_from(self.below(bound)).unwrap_or(0)
    }

    /// One of `items`, each equally likely; `None` when there are none.
    pub fn choose<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        items.get(self.index(items.len()))
    }

    /// Puts `items` in a random order, every order equally likely.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for last in (1..items.len()).rev() {
            let other = self.index(last + 1);
            items.swap(last, other);
        }
    }

    /// A separate generator for one purpose, such as `"network"` or `"faults"`.
    ///
    /// The fork depends only on the seed and the label, not on how many
    /// numbers were drawn so far. A scenario that gives each source of
    /// randomness its own fork can add a new one without changing what the
    /// others produce.
    #[must_use]
    pub fn fork(&self, label: &str) -> Self {
        let mut counter = self.origin ^ fnv1a(label);
        Self::new(Seed(splitmix(&mut counter)))
    }
}

#[cfg(test)]
mod tests {
    use super::{Seed, SimRng, fnv1a, splitmix};

    fn take(rng: &mut SimRng, count: usize) -> Vec<u64> {
        (0..count).map(|_| rng.next_u64()).collect()
    }

    #[test]
    fn splitmix_matches_the_reference_implementation() {
        let mut state = 0;
        let outputs: Vec<u64> = (0..4).map(|_| splitmix(&mut state)).collect();
        assert_eq!(
            outputs,
            [
                0xe220_a839_7b1d_cdaf,
                0x6e78_9e6a_a1b9_65f4,
                0x06c4_5d18_8009_454f,
                0xf88b_b8a8_724c_81ec,
            ]
        );
    }

    #[test]
    fn xoshiro_matches_the_reference_implementation() {
        let mut rng = SimRng {
            origin: 0,
            state: [1, 2, 3, 4],
        };
        assert_eq!(
            take(&mut rng, 6),
            [
                41_943_041,
                58_720_359,
                3_588_806_011_781_223,
                3_591_011_842_654_386,
                9_228_616_714_210_784_205,
                9_973_669_472_204_895_162,
            ]
        );
    }

    /// Freezes what each seed produces. If this test has to change, every
    /// seed recorded in a bug report stops reproducing its failure.
    #[test]
    fn seeded_sequences_are_frozen() {
        assert_eq!(
            take(&mut SimRng::new(Seed(0)), 4),
            [
                0x5317_5d61_490b_23df,
                0x61da_6f3d_c380_d507,
                0x5c0f_df91_ec9a_7bfc,
                0x02ee_bf8c_3bbe_5e1a,
            ]
        );
        assert_eq!(
            take(&mut SimRng::new(Seed(42)), 4),
            [
                0xd076_4d4f_4476_689f,
                0x519e_4174_576f_3791,
                0xfbe0_7cfb_0c24_ed8c,
                0xb37d_9f60_0cd8_35b8,
            ]
        );
    }

    #[test]
    fn forked_sequences_are_frozen() {
        assert_eq!(fnv1a("network"), 0x1e8e_57b2_909b_2a85);
        let rng = SimRng::new(Seed(42));
        assert_eq!(
            take(&mut rng.fork("network"), 2),
            [0x4876_652a_1970_aa2d, 0xbe9a_dcc7_a2b4_7e02]
        );
        assert_eq!(
            take(&mut rng.fork("faults"), 2),
            [0xb794_3f82_bbbe_8f61, 0x27d6_6084_5b19_bdda]
        );
    }

    #[test]
    fn fork_ignores_how_much_was_drawn() {
        let mut rng = SimRng::new(Seed(7));
        let before = rng.fork("network");
        let _ = take(&mut rng, 100);
        assert_eq!(rng.fork("network"), before);
        assert_ne!(rng.fork("network"), rng.fork("faults"));
        // Forking twice with one label does not lead back to the parent.
        assert_ne!(before.fork("network"), SimRng::new(Seed(7)));
    }

    #[test]
    fn range_stays_inside_and_reaches_every_value() {
        let mut rng = SimRng::new(Seed(1));
        let mut seen = [false; 6];
        for _ in 0..1_000 {
            let value = rng.range(10..16);
            assert!((10..16).contains(&value));
            seen[usize::try_from(value - 10).unwrap()] = true;
        }
        assert_eq!(seen, [true; 6]);
        assert_eq!(rng.range(5..5), 5);
        // A range that ends before it starts is empty too.
        let (start, end) = (9, 3);
        assert_eq!(rng.range(start..end), 9);
        assert_eq!(rng.range(u64::MAX - 1..u64::MAX), u64::MAX - 1);
    }

    #[test]
    fn range_covers_the_full_width() {
        let mut rng = SimRng::new(Seed(2));
        let high = (0..64).any(|_| rng.range(0..u64::MAX) > u64::MAX / 2);
        assert!(high);
    }

    #[test]
    fn chance_follows_the_ratio() {
        let mut rng = SimRng::new(Seed(3));
        assert!((0..100).all(|_| rng.chance(1, 1)));
        assert!((0..100).all(|_| !rng.chance(0, 5)));
        assert!((0..100).all(|_| !rng.chance(1, 0)));
        let hits = (0..10_000).filter(|_| rng.chance(1, 4)).count();
        assert!((2_200..2_800).contains(&hits), "{hits}");
    }

    #[test]
    fn choose_and_shuffle_keep_the_items() {
        let mut rng = SimRng::new(Seed(4));
        assert_eq!(rng.choose::<u8>(&[]), None);
        assert_eq!(rng.choose(&[9]), Some(&9));
        let mut items: Vec<u32> = (0..32).collect();
        rng.shuffle(&mut items);
        assert_ne!(items, (0..32).collect::<Vec<_>>());
        items.sort_unstable();
        assert_eq!(items, (0..32).collect::<Vec<_>>());
    }

    #[test]
    fn seeds_print_and_parse() {
        assert_eq!(Seed(42).to_string(), "0x000000000000002a");
        assert_eq!("0x2a".parse(), Ok(Seed(42)));
        assert_eq!(" 42 ".parse(), Ok(Seed(42)));
        assert_eq!(Seed(u64::MAX).to_string().parse(), Ok(Seed(u64::MAX)));
        assert!("forty-two".parse::<Seed>().is_err());
        assert!("-1".parse::<Seed>().is_err());
    }
}
