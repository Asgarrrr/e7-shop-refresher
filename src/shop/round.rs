use image::GrayImage;
use tracing::{debug, info, warn};

use crate::error::Result;
use crate::layout;

use super::scan::{crop_ratio_rect, scan_shop_rows, strip_hash};
use super::{ShopRunner, wait};

/// 4% of window height ≈ one row at 1080p — wide enough for sub-pixel
/// scroll drift, narrow enough to never leak into the adjacent row.
/// Kept as the GUI calibration overlay's default band height.
pub const BUY_COLUMN_ROW_BAND_RATIO: f32 = 0.04;

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

    /// Row-inventory pass: anchor every visible row on its buy button,
    /// classify each row's icon cell, buy the first pending target.
    /// One buy per scan — the sold overlay relayouts rows, so hits from
    /// a pre-buy frame are stale afterwards.
    pub(super) fn process_current_view(&mut self) -> Result<u32> {
        let mut bought = 0u32;
        loop {
            if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(bought);
            }
            if self
                .enabled_targets
                .iter()
                .all(|a| self.bought_types.contains(a))
            {
                return Ok(bought);
            }

            let rows = scan_shop_rows(
                &*self.capture,
                &self.detector,
                &self.color_check,
                self.buy_column(),
                self.shop_grid(),
                self.config.shop.buy_button_y_offset_ratio,
            )?;
            debug!(
                rows = rows.len(),
                classified = ?rows.iter().filter_map(|r| r.klass).collect::<Vec<_>>(),
                "row inventory scanned"
            );

            let target = rows.iter().find_map(|row| {
                row.klass
                    .filter(|k| self.enabled_targets.contains(k) && !self.bought_types.contains(k))
                    .map(|k| (k, row.anchor))
            });
            let Some((alias_name, anchor)) = target else {
                return Ok(bought);
            };

            let success = self.try_buy_row(alias_name, &anchor)?;
            // Mark regardless: a failed buy (modal didn't open, wrong
            // item shown in the modal) shouldn't re-fire on the next
            // scan in the same round — the row won't change until the
            // next refresh.
            self.bought_types.insert(alias_name);
            if success {
                bought += 1;
                self.progress.item_bought(alias_name);
            }
        }
    }

    fn buy_column(&self) -> [f32; 4] {
        self.config.zones.buy_column.unwrap_or([
            layout::BUY_COLUMN_X,
            0.0,
            layout::BUY_COLUMN_W,
            1.0,
        ])
    }

    /// Modal state is observed (grid dimming), never assumed: the buy
    /// click must dim the grid before confirm fires, and confirm must
    /// un-dim it before the loop moves on — a missed click can't
    /// cascade into a blind buy. The modal also shows the item at full
    /// size; it is reclassified there and cancelled on mismatch, so a
    /// drifted row click can never buy the wrong item.
    pub(super) fn try_buy_row(
        &mut self,
        alias_name: &'static str,
        anchor: &crate::detector::Hit,
    ) -> Result<bool> {
        let buy_confirm = self.config.zones.buy_confirm.unwrap_or(layout::BUY_CONFIRM);

        let before_gray = self.snapshot()?;
        let baseline = wait::mean_luma(&before_gray, self.shop_grid());

        let row_rect = self.anchor_click_rect(anchor);
        self.clicker.click_local_in_rect(&*self.capture, row_rect)?;
        self.clicker.human_pause();
        if self.stop.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(false);
        }

        if !self.wait_grid_dimmed(baseline)? {
            warn!(
                alias = alias_name,
                "buy modal did not open — skipping confirm click"
            );
            return Ok(false);
        }

        // Let the slide-in finish so the modal's item icon is fully
        // drawn, then make sure it is the item we clicked for.
        self.wait_settled(layout::BUY_MODAL_ICON, wait::SETTLE_TIMEOUT_MS)?;
        let modal = self.capture.snapshot_rgba()?;
        let icon = crop_ratio_rect(&modal, layout::BUY_MODAL_ICON);
        if !self.color_check.accepts(alias_name, &icon) {
            warn!(
                alias = alias_name,
                "buy modal shows a different item — cancelling"
            );
            self.click_zone(layout::BUY_CANCEL)?;
            self.wait_grid_undimmed(baseline)?;
            return Ok(false);
        }

        let confirm_rect = self.ratio_rect_to_local(buy_confirm)?;
        self.clicker
            .click_local_in_rect(&*self.capture, confirm_rect)?;
        self.clicker.human_pause();

        // Grid still dimmed = the confirm click missed and the modal is
        // still up. Cancel out rather than leave a live modal eating the
        // next scroll/click.
        if !self.wait_grid_undimmed(baseline)? {
            warn!(
                alias = alias_name,
                "buy modal still open after confirm — clicking cancel"
            );
            self.click_zone(layout::BUY_CANCEL)?;
            self.wait_grid_undimmed(baseline)?;
            return Ok(false);
        }

        // Close animation + sold-overlay relayout must finish before the
        // caller re-scans or scrolls, or the input gets eaten.
        self.wait_settled(self.shop_grid(), wait::SETTLE_TIMEOUT_MS)?;
        Ok(true)
    }

    fn click_zone(&mut self, zone: [f32; 4]) -> Result<()> {
        let rect = self.ratio_rect_to_local(zone)?;
        self.clicker.click_local_in_rect(&*self.capture, rect)?;
        self.clicker.human_pause();
        Ok(())
    }

    /// Click box centred on the matched buy-button anchor, shrunk so a
    /// uniform random point stays well inside the pill.
    fn anchor_click_rect(&self, anchor: &crate::detector::Hit) -> [i32; 4] {
        // Dimensions are always present in prod (bundled fallback); the
        // default only serves detector-less tests.
        let (tw, th) = self
            .detector
            .template_dimensions(crate::detector::alias::BUY_BUTTON)
            .unwrap_or((40, 20));
        let w = (((tw as f32) * anchor.scale).round() as i32 * 3 / 5).max(1);
        let h = (((th as f32) * anchor.scale).round() as i32 * 3 / 5).max(1);
        [anchor.x - w / 2, anchor.y - h / 2, w, h]
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
        // Scroll glide done = two identical grid hashes. A scroll that
        // moved nothing (already at the bottom) settles immediately.
        self.wait_settled(self.shop_grid(), wait::SETTLE_TIMEOUT_MS)?;
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
        let shop_grid = self.shop_grid();

        let before_gray = self.snapshot()?;
        let baseline = wait::mean_luma(&before_gray, shop_grid);
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

        if !self.wait_grid_dimmed(baseline)? {
            warn!("refresh modal did not open — skipping confirm click (this round won't refresh)");
            return Ok(false);
        }

        let confirm_rect = self.ratio_rect_to_local(refresh_confirm)?;
        self.clicker
            .click_local_in_rect(&*self.capture, confirm_rect)?;
        self.clicker.human_pause();

        // Grid still dimmed = confirm click missed, modal still up.
        // Cancel out so the next round doesn't fight a live modal.
        if !self.wait_grid_undimmed(baseline)? {
            warn!("refresh modal still open after confirm — clicking cancel");
            self.click_zone(layout::REFRESH_CANCEL)?;
            self.wait_grid_undimmed(baseline)?;
            return Ok(false);
        }

        // After modal close + items re-render, verify the items grid
        // actually changed — catches confirm-click missed / game lagged
        // / never on a real shop modal in the first place.
        self.wait_settled(shop_grid, wait::SETTLE_TIMEOUT_MS)?;
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
    use super::super::test_support::{
        FakeEvent, SHOP_GRID, gray_frame, paint_zone, rgba_frame_with_mystic_in,
        runner_for_loop_tests, runner_with_frames,
    };

    fn anchor_at(x: i32, y: i32) -> crate::detector::Hit {
        crate::detector::Hit {
            x,
            y,
            score: 1.0,
            scale: 1.0,
            margin: 0.0,
        }
    }

    fn mystic_modal_rgba() -> image::RgbaImage {
        rgba_frame_with_mystic_in(200, 200, crate::layout::BUY_MODAL_ICON)
    }

    /// Enough identical frames to exhaust a full predicate-wait
    /// timeout (attempt-count = timeout / poll cadence).
    fn timeout_frames(frame: &image::GrayImage) -> Vec<image::GrayImage> {
        let attempts =
            (super::super::wait::STATE_TIMEOUT_MS / super::super::wait::STATE_POLL_MS) as usize;
        vec![frame.clone(); attempts]
    }

    fn click_count(events: &std::sync::Mutex<Vec<FakeEvent>>) -> usize {
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, FakeEvent::Click(_)))
            .count()
    }

    #[test]
    fn refresh_shop_returns_false_when_modal_does_not_open() {
        // Baseline + a full timeout of never-dimming frames.
        let a = gray_frame(200, 200, 100);
        let mut frames = vec![a.clone()];
        frames.extend(timeout_frames(&a));
        let (mut runner, events) = runner_for_loop_tests(frames);
        let result = runner.refresh_shop().unwrap();
        assert!(!result);
        assert_eq!(click_count(&events), 1);
    }

    #[test]
    fn refresh_shop_returns_false_when_grid_does_not_reroll() {
        let a = gray_frame(200, 200, 100);
        let mut dim = a.clone();
        paint_zone(&mut dim, SHOP_GRID, 30);
        // baseline, dimmed (modal), undimmed, settle ×2, post — post
        // grid hash equals the baseline's, so no reroll happened.
        let frames = vec![a.clone(), dim, a.clone(), a.clone(), a.clone(), a.clone()];
        let (mut runner, events) = runner_for_loop_tests(frames);
        let result = runner.refresh_shop().unwrap();
        assert!(!result);
        assert_eq!(click_count(&events), 2);
    }

    #[test]
    fn refresh_shop_returns_true_when_modal_opens_and_grid_changes() {
        let a = gray_frame(200, 200, 100);
        let mut dim = a.clone();
        paint_zone(&mut dim, SHOP_GRID, 30);
        // Rerolled grid: bright enough to count as undimmed, different
        // pixels so the pre/post hash comparison sees the change.
        let mut rerolled = a.clone();
        paint_zone(&mut rerolled, SHOP_GRID, 90);
        let frames = vec![
            a,
            dim,
            rerolled.clone(),
            rerolled.clone(),
            rerolled.clone(),
            rerolled,
        ];
        let (mut runner, events) = runner_for_loop_tests(frames);
        let result = runner.refresh_shop().unwrap();
        assert!(result);
        assert_eq!(click_count(&events), 2);
    }

    #[test]
    fn refresh_shop_clicks_cancel_when_confirm_leaves_modal_open() {
        let a = gray_frame(200, 200, 100);
        let mut dim = a.clone();
        paint_zone(&mut dim, SHOP_GRID, 30);
        // baseline, modal opens, then the grid never un-dims (confirm
        // missed) until after the cancel click.
        let mut frames = vec![a.clone(), dim.clone()];
        frames.extend(timeout_frames(&dim));
        frames.push(a);
        let (mut runner, events) = runner_for_loop_tests(frames);
        let result = runner.refresh_shop().unwrap();
        assert!(!result, "a cancelled refresh must count as failed");
        assert_eq!(click_count(&events), 3, "refresh + confirm + cancel");
    }

    #[test]
    fn try_buy_row_confirms_when_modal_opens_with_the_right_item() {
        let before = gray_frame(200, 200, 100);
        let mut dim = before.clone();
        paint_zone(&mut dim, SHOP_GRID, 30);
        // baseline, dimmed (modal), modal-icon settle ×2, then the rgba
        // modal check (separate queue), undimmed, grid settle ×2.
        let frames = vec![
            before.clone(),
            dim.clone(),
            dim.clone(),
            dim,
            before.clone(),
            before.clone(),
            before,
        ];
        let (mut runner, events) = runner_with_frames(frames, vec![mystic_modal_rgba()]);

        let result = runner
            .try_buy_row(crate::detector::alias::MYSTIC_MEDAL, &anchor_at(160, 100))
            .unwrap();

        assert!(result, "modal opened — should report a successful buy");
        assert_eq!(click_count(&events), 2, "buy-button click + confirm click");
    }

    #[test]
    fn try_buy_row_skips_confirm_when_modal_stays_shut() {
        let before = gray_frame(200, 200, 100);
        let mut frames = vec![before.clone()];
        frames.extend(timeout_frames(&before));
        let (mut runner, events) = runner_for_loop_tests(frames);

        let result = runner
            .try_buy_row(crate::detector::alias::MYSTIC_MEDAL, &anchor_at(160, 100))
            .unwrap();

        assert!(!result, "modal never opened — must not report a buy");
        assert_eq!(
            click_count(&events),
            1,
            "only the buy-button click; confirm suppressed"
        );
    }

    #[test]
    fn try_buy_row_cancels_when_modal_shows_a_different_item() {
        let before = gray_frame(200, 200, 100);
        let mut dim = before.clone();
        paint_zone(&mut dim, SHOP_GRID, 30);
        // No rgba override: the modal check sees a synthesized gray
        // frame — zero saturation, so it cannot be the clicked item.
        let frames = vec![
            before.clone(),
            dim.clone(),
            dim.clone(),
            dim.clone(),
            dim,
            before,
        ];
        let (mut runner, events) = runner_for_loop_tests(frames);

        let result = runner
            .try_buy_row(crate::detector::alias::MYSTIC_MEDAL, &anchor_at(160, 100))
            .unwrap();

        assert!(!result, "wrong item in the modal must not be bought");
        assert_eq!(click_count(&events), 2, "buy + cancel, no confirm");
    }

    #[test]
    fn try_buy_row_clicks_cancel_when_confirm_leaves_modal_open() {
        let before = gray_frame(200, 200, 100);
        let mut dim = before.clone();
        paint_zone(&mut dim, SHOP_GRID, 30);
        let mut frames = vec![before.clone(), dim.clone(), dim.clone(), dim.clone()];
        frames.extend(timeout_frames(&dim));
        frames.push(before);
        let (mut runner, events) = runner_with_frames(frames, vec![mystic_modal_rgba()]);

        let result = runner
            .try_buy_row(crate::detector::alias::MYSTIC_MEDAL, &anchor_at(160, 100))
            .unwrap();

        assert!(!result, "a cancelled buy must not count");
        assert_eq!(click_count(&events), 3, "buy + confirm + cancel");
    }

    #[test]
    fn buy_round_breaks_early_when_bottom_strip_repeats() {
        // strip snapshot, scroll settle ×2, second strip snapshot
        // (identical → early break).
        let a = gray_frame(200, 200, 100);
        let (mut runner, _events) = runner_for_loop_tests(vec![a.clone(), a.clone(), a.clone(), a]);
        runner.config.shop.max_scrolls_per_round = 5;

        let bought = runner.buy_round().unwrap();
        assert_eq!(bought, 0, "no targets enabled — nothing bought");
    }
}
