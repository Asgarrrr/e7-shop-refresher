//! Pure click plans: design-space zones, the design→screen transform, and the
//! timed input sequences the executor replays. No I/O — everything is computed
//! from a client rect, an epoch and a seed, in the game's 1280×720 design space,
//! origin top-left.
//!
//! [`geometry`] → [`jitter`] → [`timings`] → [`jobs`] is the dependency order,
//! and nothing reaches back down. Every item is re-exported here.

mod geometry;
mod jitter;
mod jobs;
mod timings;

pub use geometry::{
    Anchor, CONFIRM_BUY, CONFIRM_REFRESH, ClientRect, DesignPoint, MAX_ASPECT, REFRESH, Row,
    ScreenError, Slot, Viewport, Zone, buy_zone, to_screen,
};
pub use jitter::Jitter;
pub use jobs::{Epoch, Input, Job, TimedStep, buy_job, confirm_retry_job, refresh_job};
pub use timings::{
    DelayRange, DelayRangeError, MAX_TIMING_MS, TimingPreset, Timings, Trigger,
    WAIT_BETWEEN_BUYS_MS, WAIT_BUY_MODAL_MS, WAIT_CONFIRM_REFRESH_MODAL_MS,
    WAIT_PURCHASE_RESUMED_MS, WAIT_RECOVERY_MS, WAIT_REFRESHED_MS, WAIT_SCROLL_SETTLE_MS,
    WAIT_SHOP_OPENED_MS,
};

/// Shared by two test modules each. They panic rather than return a `Result` so
/// a fixture cannot smuggle in a value the type itself would refuse.
#[cfg(test)]
mod fixtures {
    use super::{DelayRange, DesignPoint, Row, Zone};

    pub(super) fn row(index: u8) -> Row {
        Row::new(index).expect("the fixture must name a real row")
    }

    pub(super) fn range(min_ms: u64, max_ms: u64) -> DelayRange {
        DelayRange::try_new(min_ms, max_ms).expect("the fixture range must be valid")
    }

    /// Within the central 75% of the zone, correct anchor.
    pub(super) fn assert_within(at: DesignPoint, zone: Zone) {
        assert_eq!(at.anchor, zone.anchor);
        assert!(
            (at.x - zone.cx).abs() <= 0.375 * zone.w,
            "x {} escapes zone {zone:?}",
            at.x
        );
        assert!(
            (at.y - zone.cy).abs() <= 0.375 * zone.h,
            "y {} escapes zone {zone:?}",
            at.y
        );
    }
}
