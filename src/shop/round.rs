use image::{GrayImage, RgbaImage};
use tracing::{debug, info, warn};

use crate::error::Result;
use crate::layout;

use super::ShopRunner;
use super::scan::{crop_icon_patch, scan_shop_raw, strip_hash};

/// 4% of window height ≈ one row at 1080p — wide enough for sub-pixel
/// scroll drift, narrow enough to never leak into the adjacent row.
pub const BUY_COLUMN_ROW_BAND_RATIO: f32 = 0.04;

/// Only x/w of `column` are used; the row Y comes from the icon match.
pub fn buy_column_row_rect_for(
    column: [f32; 4],
    item_y: i32,
    frame_h: u32,
    window_w: u32,
    window_h: u32,
    y_offset_ratio: f32,
    band_h_ratio: f32,
) -> [i32; 4] {
    let (w, h) = (window_w as i32, window_h as i32);
    let bx = (column[0] * w as f32).round() as i32;
    let bw = (column[2] * w as f32).round() as i32;
    let band_h = ((frame_h as f32) * band_h_ratio).round() as i32;
    let y_offset = ((frame_h as f32) * y_offset_ratio).round() as i32;
    let by = (item_y + y_offset - band_h / 2).clamp(0, (h - band_h.max(1)).max(0));
    [bx.max(0), by, bw.max(1), band_h.max(1)]
}

impl ShopRunner {
    pub(super) fn buy_round(&mut self) -> Result<u32> {
        let mut bought = 0u32;
        let max_scrolls = self.config.shop.max_scrolls_per_round;

        info!("scrolling to top of shop");
        self.scroll_to_top()?;
        self.clicker.human_pause();

        let mut prev_strip_hash: Option<u64> = None;

        for scroll_iter in 0..=max_scrolls {
            if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(bought);
            }

            let view_bought = self.process_current_view()?;
            bought += view_bought;
            if view_bought > 0 {
                info!(scroll_iter, view_bought, "items found in this view");
            } else {
                debug!(scroll_iter, "scroll position scanned, no targets");
            }

            // Two identical bottom-strip hashes = scroll didn't reveal
            // new content (at the bottom, or the scroll click missed).
            let strip = self.bottom_strip_hash(&self.snapshot()?);
            if let Some(prev) = prev_strip_hash
                && prev == strip
            {
                debug!(scroll_iter, "bottom of list reached — ending scan early");
                break;
            }
            prev_strip_hash = Some(strip);

            if scroll_iter < max_scrolls {
                self.scroll_one_step()?;
            }
        }
        Ok(bought)
    }

    /// Hashes the bottom quarter of the item grid to detect "scroll
    /// didn't move anything".
    pub(super) fn bottom_strip_hash(&self, gray: &GrayImage) -> u64 {
        let [gx, gy, gw, gh] = self.config.regions.shop_grid.unwrap_or(layout::SHOP_GRID);
        let strip = [gx, gy + gh * 0.75, gw, gh * 0.25];
        strip_hash(gray, strip)
    }

    /// Searching the full column rather than a fixed slot patch absorbs
    /// Y-drift across resolutions and patches.
    pub(super) fn process_current_view(&mut self) -> Result<u32> {
        let mut bought = 0u32;
        let shop_grid = self.config.regions.shop_grid.unwrap_or(layout::SHOP_GRID);

        let mut pending: Vec<&'static str> = self
            .enabled_targets
            .iter()
            .copied()
            .filter(|a| !self.bought_types.contains(a))
            .collect();
        if pending.is_empty() {
            return Ok(0);
        }

        // After a buy the sold overlay / row relayout makes the previous
        // hit.y stale, so refresh the frame before evaluating remaining
        // aliases. Each iteration shares one capture between NCC and the
        // colour verify so they sample the same pixels.
        while !pending.is_empty() {
            if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(bought);
            }

            let scan = scan_shop_raw(&*self.capture, &self.detector, &pending, shop_grid)?;

            let mut any_bought = false;
            let mut still_pending: Vec<&'static str> = Vec::with_capacity(pending.len());

            for (alias_name, hit) in scan.hits {
                if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
                    return Ok(bought);
                }
                let Some(hit) = hit else {
                    continue;
                };
                debug!(
                    alias = alias_name,
                    score = hit.score,
                    margin = hit.margin,
                    x = hit.x,
                    y = hit.y,
                    "item matched in shop_grid"
                );

                if !self.verify_colour(alias_name, &hit, &scan.rgba) {
                    continue;
                }

                // Already bought one this frame — defer remaining buys
                // to the next iteration with a fresh snapshot so the
                // click Y isn't computed against pre-buy pixels.
                if any_bought {
                    still_pending.push(alias_name);
                    continue;
                }

                let success = self.try_buy_at_pixel_y(alias_name, hit.y, scan.rgba.height())?;
                // Mark regardless: a failed click (modal didn't open)
                // shouldn't re-fire on the next scroll view in the same
                // round either — the button isn't going to wake up.
                self.bought_types.insert(alias_name);
                if success {
                    bought += 1;
                    self.progress.item_bought(alias_name);
                    any_bought = true;
                }
            }

            if !any_bought {
                break;
            }
            pending = still_pending;
        }
        Ok(bought)
    }

    /// Returns `false` on colour mismatch — caller treats it as "no
    /// match". `rgba` MUST be the frame the NCC hit was computed from
    /// so the patch sampled here matches the matched pixels.
    pub(super) fn verify_colour(
        &self,
        alias_name: &'static str,
        hit: &crate::detector::Hit,
        rgba: &RgbaImage,
    ) -> bool {
        let patch = crop_icon_patch(rgba, hit);
        let Some(report) = self.color_check.evaluate(alias_name, &patch) else {
            return true;
        };
        if report.passed {
            return true;
        }
        warn!(
            alias = alias_name,
            score = hit.score,
            x = hit.x,
            y = hit.y,
            colour_distance = report.distance,
            coloured_fraction = report.coloured_fraction,
            "NCC hit rejected by colour check — likely cross-colour false positive"
        );
        false
    }

    /// Pre/post hash on `buy_confirm` gates the confirm click so a
    /// missed first click can't blindly trigger an unrelated buy.
    pub(super) fn try_buy_at_pixel_y(
        &mut self,
        alias_name: &'static str,
        icon_y_px: i32,
        frame_h: u32,
    ) -> Result<bool> {
        let buy_confirm = self.config.zones.buy_confirm.unwrap_or(layout::BUY_CONFIRM);

        let before_gray = self.snapshot()?;
        let before = strip_hash(&before_gray, buy_confirm);

        let row_rect = self.buy_button_local_rect(icon_y_px, frame_h)?;
        self.clicker.click_local_in_rect(&*self.capture, row_rect)?;
        self.clicker.human_pause();
        if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(false);
        }

        self.clicker
            .pause_ms(self.config.timing.modal_open_pause_ms);
        if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(false);
        }

        // Unchanged confirm-zone = modal didn't open (greyed-out buy
        // button, animation hiccup). Bail before clicking blind.
        let after_gray = self.snapshot()?;
        let after = strip_hash(&after_gray, buy_confirm);
        if before == after {
            warn!(
                alias = alias_name,
                "buy modal did not open — skipping confirm click"
            );
            return Ok(false);
        }

        let confirm_rect = self.ratio_rect_to_local(buy_confirm)?;
        self.clicker
            .click_local_in_rect(&*self.capture, confirm_rect)?;
        self.clicker.human_pause();
        // Symmetric to `refresh_shop`: let the close animation finish
        // before subsequent input. Skipping this lets `scroll_one_step`
        // fire while the modal still has focus, the scroll event gets
        // eaten, and the next iteration sees the unchanged top view —
        // round buys one item then refreshes without scanning the rest.
        self.clicker
            .pause_ms(self.config.timing.modal_open_pause_ms);
        Ok(true)
    }

    /// Delegates to the pure `buy_column_row_rect_for` helper — also
    /// used by the GUI debug overlay so both stay in lockstep.
    pub(super) fn buy_button_local_rect(&self, icon_y_px: i32, frame_h: u32) -> Result<[i32; 4]> {
        let r = self.capture.rect()?;
        // Only column[0] / column[2] (X / W) are used at click time.
        let column = self.config.zones.buy_column.unwrap_or([
            layout::BUY_COLUMN_X,
            0.0,
            layout::BUY_COLUMN_W,
            0.0,
        ]);
        Ok(buy_column_row_rect_for(
            column,
            icon_y_px,
            frame_h,
            r.width,
            r.height,
            self.config.shop.buy_button_y_offset_ratio,
            self.config.shop.buy_button_band_h_ratio,
        ))
    }

    pub(super) fn ratio_rect_to_local(&self, zone: [f32; 4]) -> Result<[i32; 4]> {
        let r = self.capture.rect()?;
        let (w, h) = (r.width as i32, r.height as i32);
        let x = (zone[0] * w as f32).round() as i32;
        let y = (zone[1] * h as f32).round() as i32;
        let zw = (zone[2] * w as f32).round() as i32;
        let zh = (zone[3] * h as f32).round() as i32;
        Ok([x.clamp(0, w), y.clamp(0, h), zw.max(1), zh.max(1)])
    }

    pub(super) fn scroll_one_step(&mut self) -> Result<()> {
        let (mx, my) = self.scroll_point()?;
        self.clicker
            .scroll_at(&*self.capture, mx, my, self.config.timing.scroll_amount)?;
        self.clicker.pause_ms(self.config.timing.scroll_pause_ms);
        Ok(())
    }

    pub(super) fn scroll_to_top(&mut self) -> Result<()> {
        let (mx, my) = self.scroll_point()?;
        let total = self.config.shop.max_scrolls_per_round + 2;
        for _ in 0..total {
            if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(());
            }
            self.clicker
                .scroll_at(&*self.capture, mx, my, -self.config.timing.scroll_amount)?;
            self.clicker.pause_ms(80);
        }
        Ok(())
    }

    pub(super) fn scroll_point(&self) -> Result<(i32, i32)> {
        let r = self.capture.rect()?;
        let (w, h) = (r.width as f32, r.height as f32);
        let [gx, gy, gw, gh] = self.config.regions.shop_grid.unwrap_or(layout::SHOP_GRID);
        let cx = gx + gw / 2.0;
        let cy = gy + gh / 2.0;
        Ok(((cx * w).round() as i32, (cy * h).round() as i32))
    }

    /// `true` only when the refresh modal opened, confirm was clicked,
    /// AND the items rerolled. Failed rounds count toward
    /// `consecutive_failures` so the bot eventually bails.
    pub(super) fn refresh_shop(&mut self) -> Result<bool> {
        let refresh = self.config.zones.refresh.unwrap_or(layout::REFRESH);
        let refresh_confirm = self
            .config
            .zones
            .refresh_confirm
            .unwrap_or(layout::REFRESH_CONFIRM);
        let shop_grid = self.config.regions.shop_grid.unwrap_or(layout::SHOP_GRID);

        let before_gray = self.snapshot()?;
        let before_confirm = strip_hash(&before_gray, refresh_confirm);
        let before_grid = strip_hash(&before_gray, shop_grid);

        info!("clicking refresh");
        let refresh_rect = self.ratio_rect_to_local(refresh)?;
        debug!(local_rect = ?refresh_rect, "refresh zone resolved");
        self.clicker
            .click_local_in_rect(&*self.capture, refresh_rect)?;
        self.clicker.human_pause();
        if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(false);
        }

        self.clicker
            .pause_ms(self.config.timing.modal_open_pause_ms);
        if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(false);
        }

        let after_gray = self.snapshot()?;
        let after_confirm = strip_hash(&after_gray, refresh_confirm);
        debug!(
            before_hash = before_confirm,
            after_hash = after_confirm,
            "refresh modal hash check"
        );
        if before_confirm == after_confirm {
            warn!("refresh modal did not open — skipping confirm click (this round won't refresh)");
            return Ok(false);
        }

        let confirm_rect = self.ratio_rect_to_local(refresh_confirm)?;
        self.clicker
            .click_local_in_rect(&*self.capture, confirm_rect)?;
        self.clicker.human_pause();

        // After modal close + items re-render, verify the items grid
        // actually changed — catches confirm-click missed / game lagged
        // / never on a real shop modal in the first place.
        self.clicker
            .pause_ms(self.config.timing.modal_open_pause_ms);
        let post_gray = self.snapshot()?;
        let post_grid = strip_hash(&post_gray, shop_grid);
        if post_grid == before_grid {
            warn!("shop items unchanged after refresh — counting round as failed");
            return Ok(false);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::test_support::{
        FakeEvent, REFRESH_CONFIRM, SHOP_GRID, gray_frame, paint_zone, runner_for_loop_tests,
    };

    #[test]
    fn buy_column_rect_centers_band_on_item_y_with_offset() {
        // 100×1000 window, item at y=500, band ratio 0.04 → 40 px band centred on 500.
        let r = buy_column_row_rect_for([0.8, 0.0, 0.1, 1.0], 500, 1000, 100, 1000, 0.0, 0.04);
        assert_eq!(r[0], 80);
        assert!(r[1] >= 478 && r[1] <= 482);
        assert_eq!(r[2], 10);
        assert_eq!(r[3], 40);
    }

    #[test]
    fn buy_column_rect_shifts_down_by_y_offset_ratio() {
        let no_off = buy_column_row_rect_for([0.8, 0.0, 0.1, 1.0], 500, 1000, 100, 1000, 0.0, 0.04);
        let with_off =
            buy_column_row_rect_for([0.8, 0.0, 0.1, 1.0], 500, 1000, 100, 1000, 0.04, 0.04);
        assert_eq!(with_off[1] - no_off[1], 40);
    }

    #[test]
    fn buy_column_rect_clamps_band_to_window_bounds() {
        let r = buy_column_row_rect_for([0.8, 0.0, 0.1, 1.0], 0, 1000, 100, 1000, 0.0, 0.04);
        assert!(r[1] >= 0);
        let r = buy_column_row_rect_for([0.8, 0.0, 0.1, 1.0], 1000, 1000, 100, 1000, 0.0, 0.04);
        assert!(i64::from(r[1] + r[3]) <= 1000);
    }

    #[test]
    fn buy_column_rect_y_h_of_zone_are_ignored() {
        let a = buy_column_row_rect_for([0.8, 0.1, 0.1, 0.2], 500, 1000, 100, 1000, 0.0, 0.04);
        let b = buy_column_row_rect_for([0.8, 0.9, 0.1, 0.05], 500, 1000, 100, 1000, 0.0, 0.04);
        assert_eq!(a, b);
    }

    #[test]
    fn buy_column_rect_band_height_scales_with_ratio() {
        let thin = buy_column_row_rect_for([0.8, 0.0, 0.1, 1.0], 500, 1000, 100, 1000, 0.0, 0.02);
        let thick = buy_column_row_rect_for([0.8, 0.0, 0.1, 1.0], 500, 1000, 100, 1000, 0.0, 0.08);
        assert_eq!(thin[3], 20);
        assert_eq!(thick[3], 80);
    }

    #[test]
    fn refresh_shop_returns_false_when_modal_does_not_open() {
        let a = gray_frame(200, 200, 100);
        let b = a.clone();
        let (mut runner, events) = runner_for_loop_tests(vec![a, b]);
        let result = runner.refresh_shop().unwrap();
        assert!(!result);
        let clicks = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, FakeEvent::Click(_)))
            .count();
        assert_eq!(clicks, 1);
    }

    #[test]
    fn refresh_shop_returns_false_when_grid_does_not_reroll() {
        let a = gray_frame(200, 200, 100);
        let mut b = a.clone();
        paint_zone(&mut b, REFRESH_CONFIRM, 200);
        let c = a.clone();
        let (mut runner, events) = runner_for_loop_tests(vec![a, b, c]);
        let result = runner.refresh_shop().unwrap();
        assert!(!result);
        let clicks = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, FakeEvent::Click(_)))
            .count();
        assert_eq!(clicks, 2);
    }

    #[test]
    fn refresh_shop_returns_true_when_modal_opens_and_grid_changes() {
        let a = gray_frame(200, 200, 100);
        let mut b = a.clone();
        paint_zone(&mut b, REFRESH_CONFIRM, 200);
        let mut c = a.clone();
        paint_zone(&mut c, SHOP_GRID, 30);
        let (mut runner, events) = runner_for_loop_tests(vec![a, b, c]);
        let result = runner.refresh_shop().unwrap();
        assert!(result);
        let clicks = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, FakeEvent::Click(_)))
            .count();
        assert_eq!(clicks, 2);
    }

    #[test]
    fn try_buy_at_pixel_y_confirms_when_modal_opens() {
        let before = gray_frame(200, 200, 100);
        let mut after = before.clone();
        paint_zone(&mut after, [0.4, 0.5, 0.1, 0.1], 200);
        let (mut runner, events) = runner_for_loop_tests(vec![before, after]);

        let result = runner
            .try_buy_at_pixel_y(crate::detector::alias::MYSTIC_MEDAL, 100, 200)
            .unwrap();

        assert!(result, "modal opened — should report a successful buy");
        let clicks = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, FakeEvent::Click(_)))
            .count();
        assert_eq!(clicks, 2, "buy-button click + confirm click");
    }

    #[test]
    fn try_buy_at_pixel_y_skips_confirm_when_modal_stays_shut() {
        let before = gray_frame(200, 200, 100);
        let after = before.clone();
        let (mut runner, events) = runner_for_loop_tests(vec![before, after]);

        let result = runner
            .try_buy_at_pixel_y(crate::detector::alias::MYSTIC_MEDAL, 100, 200)
            .unwrap();

        assert!(!result, "modal never opened — must not report a buy");
        let clicks = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, FakeEvent::Click(_)))
            .count();
        assert_eq!(clicks, 1, "only the buy-button click; confirm suppressed");
    }

    #[test]
    fn buy_round_breaks_early_when_bottom_strip_repeats() {
        let a = gray_frame(200, 200, 100);
        let b = a.clone();
        let (mut runner, _events) = runner_for_loop_tests(vec![a, b]);
        runner.config.shop.max_scrolls_per_round = 5;

        let bought = runner.buy_round().unwrap();
        assert_eq!(bought, 0, "no targets enabled — nothing bought");
    }
}
