//! Screen-state waits: poll a predicate on fresh captures instead of
//! sleeping a fixed duration. Fixed pauses tuned on one machine either
//! under-wait on slow PCs (modal not open yet at check time) or burn
//! dead time on fast ones; polling self-adapts to both.

use image::GrayImage;

use crate::error::Result;

use super::ShopRunner;
use super::scan::strip_hash;

/// Poll cadence for screen-state waits. Universal, not config — fast
/// enough to react within a frame or two of the game, slow enough that
/// the capture cost stays negligible.
pub(super) const STATE_POLL_MS: u64 = 60;

/// Ceiling on waiting for a modal to open or close. Generous for slow
/// machines; past this the click definitely missed.
pub(super) const STATE_TIMEOUT_MS: u64 = 2_500;

/// Ceiling on waiting for an animation (modal close, row relayout,
/// scroll glide) to stop mutating the frame.
pub(super) const SETTLE_TIMEOUT_MS: u64 = 1_500;

/// Floor before settle sampling starts. Without it, two polls that
/// both land before the game begins rendering the animation (input
/// latency + slide-in start) hash identically and "settle" on the
/// pre-action frame. Covers the ~150 ms modal slide-in and typical
/// input latency; the adaptive polling handles everything longer.
const SETTLE_GRACE_MS: u64 = 250;

/// A modal dims the whole background. Measured on live captures the
/// shop grid drops to 0.44–0.72 of its pre-click luminance; 0.85 splits
/// dimmed from undimmed with margin on both sides. Ratio-based, so a
/// global screen tint (Night Light, ICC, HDR) cancels out.
const MODAL_DIM_RATIO: f32 = 0.85;

/// Mean luminance over a `[x, y, w, h]` ratio rect of `gray`.
pub(super) fn mean_luma(gray: &GrayImage, [x, y, w, h]: [f32; 4]) -> f32 {
    let (sw, sh) = (gray.width(), gray.height());
    let x0 = (x * sw as f32)
        .round()
        .clamp(0.0, sw.saturating_sub(1) as f32) as u32;
    let y0 = (y * sh as f32)
        .round()
        .clamp(0.0, sh.saturating_sub(1) as f32) as u32;
    let x1 = ((x + w) * sw as f32).round().clamp(0.0, sw as f32) as u32;
    let y1 = ((y + h) * sh as f32).round().clamp(0.0, sh as f32) as u32;
    let mut sum = 0u64;
    let mut n = 0u64;
    for yy in y0..y1 {
        for xx in x0..x1 {
            sum += u64::from(gray.get_pixel(xx, yy)[0]);
            n += 1;
        }
    }
    if n == 0 { 0.0 } else { sum as f32 / n as f32 }
}

impl ShopRunner {
    /// Polls `extract` on fresh snapshots every `STATE_POLL_MS` until it
    /// returns `Some` or `timeout_ms` elapses. Attempt-count based
    /// rather than wall-clock so tests with instant fake pauses
    /// terminate.
    pub(super) fn wait_map<T>(
        &self,
        timeout_ms: u64,
        mut extract: impl FnMut(&GrayImage) -> Option<T>,
    ) -> Result<Option<T>> {
        let attempts = (timeout_ms / STATE_POLL_MS).max(1);
        for i in 0..attempts {
            if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(None);
            }
            let gray = self.snapshot()?;
            if let Some(v) = extract(&gray) {
                return Ok(Some(v));
            }
            if i + 1 < attempts {
                self.clicker.pause_ms(STATE_POLL_MS);
            }
        }
        Ok(None)
    }

    pub(super) fn wait_for(
        &self,
        timeout_ms: u64,
        mut pred: impl FnMut(&GrayImage) -> bool,
    ) -> Result<bool> {
        Ok(self
            .wait_map(timeout_ms, |g| pred(g).then_some(()))?
            .is_some())
    }

    /// `Some(dimmed mean)` once the dim-zone luminance drops below the
    /// dim ratio of `baseline` — i.e. a modal opened over it. The
    /// returned reading feeds `wait_grid_undimmed`.
    pub(super) fn wait_grid_dimmed(&self, baseline: f32) -> Result<Option<f32>> {
        let zone = dim_zone();
        let cut = baseline.max(1.0) * MODAL_DIM_RATIO;
        self.wait_map(STATE_TIMEOUT_MS, |g| {
            let m = mean_luma(g, zone);
            (m < cut).then_some(m)
        })
    }

    /// `true` once the dim-zone luminance is back above the midpoint
    /// between the observed dimmed level and the pre-click baseline.
    /// Midpoint, not the dim cut itself: a successful buy/refresh
    /// legitimately changes grid content brightness, and darker
    /// rerolled items must not read as "modal still open".
    pub(super) fn wait_grid_undimmed(&self, baseline: f32, dimmed: f32) -> Result<bool> {
        let zone = dim_zone();
        let cut = dimmed + (baseline.max(1.0) - dimmed) * 0.5;
        self.wait_for(STATE_TIMEOUT_MS, |g| mean_luma(g, zone) >= cut)
    }

    /// `true` once two consecutive snapshots hash identically over
    /// `zone` — the animation touching it has finished. Sampling only
    /// starts after `SETTLE_GRACE_MS` so the pre-action frame can't
    /// satisfy the check before the animation begins.
    pub(super) fn wait_settled(&self, zone: [f32; 4], timeout_ms: u64) -> Result<bool> {
        self.clicker.pause_ms(SETTLE_GRACE_MS);
        let mut prev: Option<u64> = None;
        self.wait_for(timeout_ms, move |g| {
            let h = strip_hash(g, zone);
            let settled = prev == Some(h);
            prev = Some(h);
            settled
        })
    }

    pub(super) fn shop_grid(&self) -> [f32; 4] {
        self.config
            .regions
            .shop_grid
            .unwrap_or(crate::layout::SHOP_GRID)
    }
}

/// Luminance zone for modal-dim detection: always the bundled icon
/// column, NOT the user-calibratable `regions.shop_grid`. A user who
/// drew a wide grid region (valid for the hash checks) would otherwise
/// include the bright modal window itself and the ratio would never
/// trip.
fn dim_zone() -> [f32; 4] {
    crate::layout::SHOP_GRID
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{SHOP_GRID, gray_frame, paint_zone, runner_for_loop_tests};
    use super::*;

    #[test]
    fn mean_luma_of_uniform_zone_is_the_pixel_value() {
        let img = gray_frame(100, 100, 200);
        let m = mean_luma(&img, [0.1, 0.1, 0.5, 0.5]);
        assert!((m - 200.0).abs() < 0.01);
    }

    #[test]
    fn mean_luma_handles_degenerate_zone() {
        let img = gray_frame(10, 10, 50);
        // Zero-area rect must not divide by zero.
        let m = mean_luma(&img, [0.5, 0.5, 0.0, 0.0]);
        assert_eq!(m, 0.0);
    }

    #[test]
    fn wait_grid_dimmed_fires_on_darkened_grid_and_reports_the_level() {
        let base = gray_frame(200, 200, 100);
        let mut dim = base.clone();
        paint_zone(&mut dim, SHOP_GRID, 40);
        // First poll sees the still-bright frame, second the dimmed one.
        let (runner, _) = runner_for_loop_tests(vec![base, dim]);
        let dimmed = runner.wait_grid_dimmed(100.0).unwrap();
        assert!(dimmed.is_some_and(|m| m < 85.0));
    }

    #[test]
    fn wait_grid_dimmed_times_out_on_unchanged_grid() {
        let base = gray_frame(200, 200, 100);
        let attempts = (STATE_TIMEOUT_MS / STATE_POLL_MS) as usize;
        let (runner, _) = runner_for_loop_tests(vec![base; attempts]);
        assert!(runner.wait_grid_dimmed(100.0).unwrap().is_none());
    }

    #[test]
    fn wait_grid_undimmed_tolerates_darker_rerolled_content() {
        // Baseline 100, dimmed reading 40 → cut is the midpoint 70.
        // Rerolled content at 80 is darker than baseline but must still
        // count as "modal closed".
        let base = gray_frame(200, 200, 100);
        let mut rerolled = base.clone();
        paint_zone(&mut rerolled, SHOP_GRID, 78);
        let (runner, _) = runner_for_loop_tests(vec![rerolled]);
        assert!(runner.wait_grid_undimmed(100.0, 40.0).unwrap());
    }

    #[test]
    fn wait_settled_needs_two_identical_hashes() {
        let a = gray_frame(200, 200, 100);
        let mut b = a.clone();
        paint_zone(&mut b, SHOP_GRID, 60);
        let c = b.clone();
        let d = b.clone();
        // a → b differ, b → c identical: settles on the third snapshot.
        let (runner, _) = runner_for_loop_tests(vec![a, b, c, d]);
        assert!(runner.wait_settled(SHOP_GRID, SETTLE_TIMEOUT_MS).unwrap());
    }

    #[test]
    fn wait_settled_times_out_while_zone_keeps_changing() {
        let attempts = (SETTLE_TIMEOUT_MS / STATE_POLL_MS) as usize;
        let frames: Vec<_> = (0..attempts)
            .map(|i| gray_frame(200, 200, (i % 255) as u8))
            .collect();
        let (runner, _) = runner_for_loop_tests(frames);
        assert!(!runner.wait_settled(SHOP_GRID, SETTLE_TIMEOUT_MS).unwrap());
    }
}
