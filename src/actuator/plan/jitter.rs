//! The one deterministic randomness source in `plan`, and the salt that keeps
//! its two consumers — [`timings`](super::timings)'s waits and the
//! [`jobs`](super::jobs) builders' positions — on independent streams.

use super::geometry::{DesignPoint, Zone};

/// Both streams seed from the same `now_ms`, and one shared sequence would make
/// click coordinates depend on the timing config; `XOR`ing keeps positions
/// byte-stable whatever the ranges.
pub(super) const DELAY_SEED_SALT: u64 = 0xD31A_7000_D31A_7000;

/// Deterministic per-seed click randomizer (xorshift64*): no two clicks look
/// alike, while tests stay reproducible.
pub struct Jitter(u64);

impl Jitter {
    pub fn new(seed: u64) -> Self {
        // xorshift never leaves state 0: remap to an arbitrary odd constant.
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    /// The raw draw, for `DelayRange::draw`; every other caller wants a shaped
    /// one below.
    pub(super) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Uniform within the central 75% of the zone: every click stays well
    /// inside the button.
    pub fn point_in(&mut self, zone: Zone) -> DesignPoint {
        DesignPoint {
            x: zone.cx + (self.unit() - 0.5) * 0.75 * zone.w,
            y: zone.cy + (self.unit() - 0.5) * 0.75 * zone.h,
            anchor: zone.anchor,
        }
    }

    /// Mouse-button hold before release.
    pub fn press_ms(&mut self) -> u64 {
        40 + self.next() % 51
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actuator::plan::fixtures::assert_within;
    use crate::actuator::plan::{CONFIRM_BUY, REFRESH};

    #[test]
    fn jitter_is_deterministic_per_seed() {
        let mut a = Jitter::new(7);
        let mut b = Jitter::new(7);
        for _ in 0..5 {
            assert_eq!(a.point_in(REFRESH), b.point_in(REFRESH));
            assert_eq!(a.press_ms(), b.press_ms());
        }
    }

    #[test]
    fn jitter_seeds_diverge() {
        let mut c = Jitter::new(8);
        assert_ne!(Jitter::new(7).point_in(REFRESH), c.point_in(REFRESH));
    }

    #[test]
    fn jitter_stays_in_the_central_band_and_hold_range() {
        let mut jitter = Jitter::new(1234);
        for _ in 0..200 {
            assert_within(jitter.point_in(CONFIRM_BUY), CONFIRM_BUY);
            let hold = jitter.press_ms();
            assert!((40..=90).contains(&hold), "hold {hold} out of range");
        }
    }

    #[test]
    fn jitter_seed_zero_is_not_degenerate() {
        let mut jitter = Jitter::new(0);
        let first = jitter.point_in(REFRESH);
        let second = jitter.point_in(REFRESH);
        assert_ne!(first, second);
    }
}
