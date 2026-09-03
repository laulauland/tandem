//! A seeded generator, small enough to read in one sitting.
//!
//! The simulation needs a random number stream that is identical on every
//! machine and every run for a given seed. A crate would give that too, but it
//! would also give a version that can change under the suite, and a seed that
//! only reproduces against one lockfile is not a reproducer. Sixteen lines of
//! SplitMix64 are cheaper than that promise.

pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n`. `n` must not be zero.
    pub fn below(&mut self, n: usize) -> usize {
        assert!(n > 0, "below(0) has no answer");
        (self.next_u64() % n as u64) as usize
    }

    /// A number in `lo..=hi`.
    pub fn between(&mut self, lo: usize, hi: usize) -> usize {
        assert!(lo <= hi);
        lo + self.below(hi - lo + 1)
    }

    /// True `percent` times in a hundred.
    pub fn chance(&mut self, percent: u64) -> bool {
        self.next_u64() % 100 < percent
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| (self.next_u64() & 0xff) as u8).collect()
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// A seed for a run nobody pinned — printed so that a failure becomes a
/// regression seed.
pub fn arbitrary_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before the epoch")
        .as_nanos() as u64;
    let pid = std::process::id() as u64;
    nanos ^ pid.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}
