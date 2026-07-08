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
    /// Polls `pred` on fresh snapshots every `STATE_POLL_MS` until it
    /// holds or `timeout_ms` elapses. Attempt-count based rather than
    /// wall-clock so tests with instant fake pauses terminate.
    pub(super) fn wait_for(
        &self,
        timeout_ms: u64,
        mut pred: impl FnMut(&GrayImage) -> bool,
    ) -> Result<bool> {
        let attempts = (timeout_ms / STATE_POLL_MS).max(1);
        for i in 0..attempts {
            if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(false);
            }
            let gray = self.snapshot()?;
            if pred(&gray) {
                return Ok(true);
            }
            if i + 1 < attempts {
                self.clicker.pause_ms(STATE_POLL_MS);
            }
        }
        Ok(false)
    }

    /// `true` once the shop grid luminance drops below the dim ratio of
    /// `baseline` — i.e. a modal opened over it.
    pub(super) fn wait_grid_dimmed(&self, baseline: f32) -> Result<bool> {
        let grid = self.shop_grid();
        let cut = baseline.max(1.0) * MODAL_DIM_RATIO;
        self.wait_for(STATE_TIMEOUT_MS, |g| mean_luma(g, grid) < cut)
    }

    /// `true` once the shop grid luminance is back at (or above) the
    /// dim ratio of `baseline` — i.e. the modal closed.
    pub(super) fn wait_grid_undimmed(&self, baseline: f32) -> Result<bool> {
        let grid = self.shop_grid();
        let cut = baseline.max(1.0) * MODAL_DIM_RATIO;
        self.wait_for(STATE_TIMEOUT_MS, |g| mean_luma(g, grid) >= cut)
    }

    /// `true` once two consecutive snapshots hash identically over
    /// `zone` — the animation touching it has finished.
    pub(super) fn wait_settled(&self, zone: [f32; 4], timeout_ms: u64) -> Result<bool> {
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
    fn wait_grid_dimmed_fires_on_darkened_grid() {
        let base = gray_frame(200, 200, 100);
        let mut dim = base.clone();
        paint_zone(&mut dim, SHOP_GRID, 40);
        // First poll sees the still-bright frame, second the dimmed one.
        let (runner, _) = runner_for_loop_tests(vec![base, dim]);
        assert!(runner.wait_grid_dimmed(100.0).unwrap());
    }

    #[test]
    fn wait_grid_dimmed_times_out_on_unchanged_grid() {
        let base = gray_frame(200, 200, 100);
        let attempts = (STATE_TIMEOUT_MS / STATE_POLL_MS) as usize;
        let (runner, _) = runner_for_loop_tests(vec![base; attempts]);
        assert!(!runner.wait_grid_dimmed(100.0).unwrap());
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
