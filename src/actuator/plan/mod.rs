//! Pure click plans: design-space zones, the design→screen transform, and
//! the timed input sequences the executor replays. No I/O — everything here
//! is computed from a client rect, an epoch, and a seed.
//!
//! Coordinates are in the game's 1280×720 design space, origin top-left.
//!
//! The four submodules are the layers the file already had, in dependency order
//! — each one reads the ones above it and none of them reaches back down:
//!
//! - [`geometry`] — *where*: the design space, the zones, `Slot`/`Row`, and
//!   `to_screen`. Depends on nothing.
//! - [`jitter`] — the one deterministic stream, and the salt that keeps its two
//!   consumers from interfering. Its own module precisely because both of the
//!   next two draw from it and neither owns it.
//! - [`timings`] — *how long*: the tuned baselines, the player's ranges, the
//!   presets. The genuinely public half — `config` and `ui::editor` speak it.
//! - [`jobs`] — *what to click, in what order*: the three job builders and the
//!   `Input`/`TimedStep`/`Job` shapes the executor replays.
//!
//! Every item is re-exported here, so `plan::Timings`, `plan::to_screen` and the
//! rest are the paths they always were; nothing outside this directory names a
//! submodule.

mod geometry;
mod jitter;
mod jobs;
mod timings;

pub use geometry::{
    Anchor, CONFIRM_BUY, CONFIRM_REFRESH, ClientRect, DesignPoint, MAX_ASPECT, REFRESH, Row,
    ScreenError, Slot, Zone, buy_zone, to_screen,
};
pub use jitter::Jitter;
pub use jobs::{Epoch, Input, Job, TimedStep, buy_job, confirm_retry_job, refresh_job};
pub use timings::{
    DelayRange, DelayRangeError, MAX_TIMING_MS, TimingPreset, Timings, Trigger,
    WAIT_BETWEEN_BUYS_MS, WAIT_BUY_MODAL_MS, WAIT_CONFIRM_REFRESH_MODAL_MS,
    WAIT_PURCHASE_RESUMED_MS, WAIT_RECOVERY_MS, WAIT_REFRESHED_MS, WAIT_SCROLL_SETTLE_MS,
    WAIT_SHOP_OPENED_MS,
};

/// The three fixtures the split left with two readers each: they build values
/// the types refuse to hold when wrong, which is the whole point of them, and
/// duplicating that refusal per test module would be duplicating the assertion.
#[cfg(test)]
mod fixtures {
    use super::{DelayRange, DesignPoint, Row, Zone};

    /// A row the type system accepts, for the tests that plan clicks. Panics on
    /// an out-of-range index — the fixture must not smuggle in a row [`Row`]
    /// itself would refuse.
    pub(super) fn row(index: u8) -> Row {
        Row::new(index).expect("the fixture must name a real row")
    }

    /// A range the type accepts. Panics on a reversed or over-ceiling pair, for
    /// the same reason as [`row`].
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
