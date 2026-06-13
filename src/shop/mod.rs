use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use image::{GenericImageView, GrayImage, RgbaImage};
use tracing::{debug, info, warn};

use crate::capture::Capture;
use crate::color_check::ColorVerifier;
use crate::config::{Config, ShopConfig};
use crate::detector::{Detector, alias};
use crate::error::{Error, Result};
use crate::input::Input;
use crate::layout;

/// 4% of window height ≈ one row at 1080p — wide enough for sub-pixel
/// scroll drift, narrow enough to never leak into the adjacent row.
pub const BUY_COLUMN_ROW_BAND_RATIO: f32 = 0.04;

/// 4 visible at once + 2 unlocked by the single in-bot scroll. Drop
/// rates below are per-slot; multiply by this for per-refresh.
pub const SHOP_SLOTS_PER_REFRESH: u32 = 6;

pub const MYSTIC_DROP_PER_SLOT: f64 = 0.001_700_646;
pub const COVENANT_DROP_PER_SLOT: f64 = 0.006_602_509;

/// Lets the freshly re-located game window finish painting before the
/// next round resumes.
const REATTACH_BACKOFF_MS: u64 = 800;

/// Errors recoverable by re-finding the game window: crash + relaunch,
/// alt-F4, minimize. Other errors propagate up.
fn is_recoverable(err: &Error) -> bool {
    matches!(
        err,
        Error::WindowGone
            | Error::WindowHandleInvalid
            | Error::WindowNotForeground
            | Error::Xcap(_)
    )
}

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

/// Observer hooks for embedding `ShopRunner` in a GUI or test harness.
pub trait ProgressSink: Send + Sync {
    fn round_started(&self, _round: u32, _total: u32) {}
    fn round_finished(&self, _round: u32, _bought: u32) {}
    fn item_bought(&self, _alias: &str) {}
    /// `reason = Some("stop_when_…")` when a goal fired, `None` on
    /// manual Stop or clean exit.
    fn finished(&self, _reason: Option<&str>) {}
    fn failed(&self, _err: &str) {}

    /// Lets the GUI sink give `sleep_when_done` one-shot semantics.
    fn sleep_consumed(&self) {}

    fn sub_status(&self, _text: Option<&str>) {}

    /// Default `0` means per-alias stop conditions never fire — fine
    /// for CLI/headless runs that rely on `max_refreshes`.
    fn bought_count(&self, _alias: &str) -> u32 {
        0
    }
}

pub struct NullSink;
impl ProgressSink for NullSink {}

pub struct ShopRunner {
    capture: Arc<dyn Capture>,
    detector: Arc<Detector>,
    color_check: ColorVerifier,
    clicker: Box<dyn Input>,
    config: Config,
    /// Runner re-reads at every round boundary so mid-run UI edits to
    /// `[shop]` apply at the next boundary. Timing/matching stay
    /// captured by value in `config` since they're in use right now.
    live_shop: Arc<RwLock<ShopConfig>>,
    stop: Arc<AtomicBool>,
    progress: Arc<dyn ProgressSink>,
    enabled_targets: Vec<&'static str>,
    /// Shop holds one of each per refresh — a later match for the same
    /// alias is a re-detection of the sold-out item. Also dedupes a
    /// failed buy attempt (modal didn't open) so subsequent scroll
    /// views don't re-click an inactive button. Cleared per round.
    bought_types: HashSet<&'static str>,
}

impl ShopRunner {
    pub fn new(
        capture: Arc<dyn Capture>,
        detector: Arc<Detector>,
        clicker: Box<dyn Input>,
        config: Config,
        live_shop: Arc<RwLock<ShopConfig>>,
        stop: Arc<AtomicBool>,
    ) -> Self {
        let enabled_targets = config.enabled_targets();
        Self {
            capture,
            detector,
            color_check: ColorVerifier::new(),
            clicker,
            config,
            live_shop,
            stop,
            progress: Arc::new(NullSink),
            enabled_targets,
            bought_types: HashSet::new(),
        }
    }

    /// Called at the round boundary so the round reads one consistent
    /// snapshot even if the user is dragging a slider mid-round.
    fn refresh_live_shop(&mut self) {
        if let Ok(latest) = self.live_shop.read() {
            self.config.shop = latest.clone();
            self.enabled_targets = self.config.enabled_targets();
        }
    }

    pub fn with_progress(mut self, sink: Arc<dyn ProgressSink>) -> Self {
        self.progress = sink;
        self
    }

    /// Bundled fallbacks are calibrated for `BUNDLED_TEMPLATE_NATIVE_HEIGHT`
    /// — surfacing which ones are in use lets the user correlate a missed
    /// click with "ah, I never set that zone in Setup".
    fn log_bundled_zones(&self) {
        let z = &self.config.zones;
        let mut bundled: Vec<&str> = Vec::new();
        if self.config.regions.shop_grid.is_none() {
            bundled.push("shop_grid");
        }
        if z.refresh.is_none() {
            bundled.push("refresh");
        }
        if z.refresh_confirm.is_none() {
            bundled.push("refresh_confirm");
        }
        if z.buy_confirm.is_none() {
            bundled.push("buy_confirm");
        }
        if z.buy_column.is_none() {
            bundled.push("buy_column");
        }
        if !bundled.is_empty() {
            info!(zones = ?bundled, "using bundled layout fallback for these zones");
        }
    }

    pub fn run(&mut self) -> Result<()> {
        let s = &self.config.shop;
        info!(
            max_refreshes = s.max_refreshes,
            stop_after_minutes = s.stop_after_minutes,
            stop_when_mystic_medals = s.stop_when_mystic_medals,
            stop_when_covenants = s.stop_when_covenants,
            sleep_when_done = s.sleep_when_done,
            "starting shop refresh loop"
        );
        self.log_bundled_zones();

        let started = Instant::now();
        let result = self.run_inner(started);
        // Read sleep_when_done AFTER the loop so a toggle made mid-run
        // (clicked "Sleep PC" right before bed) is honoured.
        let sleep_after = self.config.shop.sleep_when_done;
        match &result {
            Ok(outcome) => {
                // Finished FIRST so the UI shows "Finished · Sending
                // Discord notification…" rather than "Running" for the
                // duration of the network round-trip.
                self.progress.finished(outcome.reason);
                if let Some(reason) = outcome.reason {
                    if sleep_after {
                        // Sync — fire-and-forget would race the OS
                        // suspending the network stack right after.
                        // WinHTTP has 5 s per-phase timeouts so worst
                        // case is bounded.
                        self.progress
                            .sub_status(Some("Sending Discord notification…"));
                        self.dispatch_completion_webhook(
                            reason,
                            outcome.refreshes,
                            started.elapsed(),
                        );
                        self.progress.sub_status(None);
                    } else {
                        // Detached — BotHandle::Drop joins the worker,
                        // so a slow webhook would freeze the GUI close
                        // for up to ~20 s.
                        self.spawn_completion_webhook(reason, outcome.refreshes, started.elapsed());
                    }
                }
                // Manual Stop = user is at the keyboard, never sleep.
                if outcome.reason.is_some() && sleep_after {
                    info!("goal reached — suspending system");
                    crate::power::suspend_to_sleep();
                    self.progress.sleep_consumed();
                }
            }
            Err(e) => self.progress.failed(&e.to_string()),
        }
        info!("shop refresh loop complete");
        result.map(|_| ())
    }

    fn dispatch_completion_webhook(&self, reason: &str, refreshes: u32, elapsed: Duration) {
        let url = self.config.notifications.webhook_url();
        if url.is_empty() {
            return;
        }
        crate::notifications::deliver_summary_blocking(
            url,
            self.build_summary(reason, refreshes, elapsed),
        );
    }

    fn spawn_completion_webhook(&self, reason: &str, refreshes: u32, elapsed: Duration) {
        let url = self.config.notifications.webhook_url();
        if url.is_empty() {
            return;
        }
        let url = url.to_string();
        let summary = self.build_summary(reason, refreshes, elapsed);
        let spawn = std::thread::Builder::new()
            .name("discord-webhook-completion".into())
            .spawn(move || {
                crate::notifications::deliver_summary_blocking(&url, summary);
            });
        if let Err(e) = spawn {
            warn!(error = %e, "failed to spawn completion webhook thread");
        }
    }

    fn build_summary(
        &self,
        reason: &str,
        refreshes: u32,
        elapsed: Duration,
    ) -> crate::notifications::RunSummary {
        crate::notifications::RunSummary {
            reason: reason.to_string(),
            elapsed,
            refreshes,
            mystic_bought: self.progress.bought_count(alias::MYSTIC_MEDAL),
            covenant_bought: self.progress.bought_count(alias::COVENANT),
            gold_spent: gold_spent_for(|a| self.progress.bought_count(a)),
        }
    }

    fn run_inner(&mut self, started: Instant) -> Result<RunInnerOutcome> {
        let long_every = self.config.timing.long_pause_every_n;
        // `refreshes_done` is user-facing — only a round that actually
        // fired the confirm click bumps it.
        let mut iteration: u32 = 0;
        let mut refreshes_done: u32 = 0;
        let mut consecutive_failures: u32 = 0;
        let mut goal_reason: Option<&'static str> = None;

        loop {
            iteration += 1;
            self.refresh_live_shop();
            let max_refreshes = self.config.shop.max_refreshes;

            if self.stop.load(Ordering::Relaxed) {
                info!("stop signal received");
                break;
            }
            if max_refreshes > 0 && refreshes_done >= max_refreshes {
                info!(max_refreshes, "max_refreshes reached — stopping");
                goal_reason = Some("max_refreshes");
                break;
            }
            if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                warn!(
                    consecutive_failures,
                    "too many consecutive failures — bailing"
                );
                return Err(Error::TooManyFailures(MAX_CONSECUTIVE_FAILURES));
            }
            if let Some(reason) = self.stop_condition_reached(started) {
                info!(stop_reason = reason, "stop condition reached");
                goal_reason = Some(reason);
                break;
            }

            // Drift is non-fatal — click positions resolve per-round
            // against `Capture::rect()`, the Detector carries ±5% scale
            // templates. Beyond that NCC misses and consecutive-failure
            // cap trips, so surface at warn so the GUI log panel shows it.
            if let Err(e) = self
                .capture
                .check_size_stable(self.config.window.resize_tolerance_px)
            {
                warn!(error = %e, "window size drifted — adapting in place");
            }

            self.progress.round_started(refreshes_done, max_refreshes);

            match self.run_round(iteration) {
                Ok(outcome) => {
                    if outcome.refreshed {
                        refreshes_done += 1;
                        consecutive_failures = 0;
                    } else {
                        consecutive_failures += 1;
                    }
                    self.progress.round_finished(refreshes_done, outcome.bought);
                }
                Err(e) if is_recoverable(&e) => {
                    // A failure raised after Stop is almost always a
                    // side-effect of the GUI grabbing focus, not a real
                    // fault — don't count it toward the failure cap and
                    // keep the shutdown log quiet.
                    let stop_in_flight = self.stop.load(Ordering::Relaxed);
                    if stop_in_flight {
                        debug!(
                            iteration,
                            error = %e,
                            "recoverable failure during shutdown — ignoring"
                        );
                    } else {
                        consecutive_failures += 1;
                        warn!(
                            iteration,
                            consecutive_failures,
                            error = %e,
                            "recoverable failure — attempting capture reattach"
                        );
                    }
                    if let Err(reattach_err) = self.capture.reattach() {
                        warn!(error = %reattach_err, "reattach failed — bailing");
                        return Err(e);
                    }
                    if !stop_in_flight {
                        info!("capture reattached, retrying next round");
                    }
                    self.clicker.pause_ms(REATTACH_BACKOFF_MS);
                }
                Err(e) => return Err(e),
            }

            // Skip the inter-round pause when we know the next iteration
            // will stop — avoids overshooting wall-clock by up to
            // long_pause_max_ms past the stop condition.
            let next_round_would_stop = self.stop.load(Ordering::Relaxed)
                || (max_refreshes > 0 && refreshes_done >= max_refreshes)
                || consecutive_failures >= MAX_CONSECUTIVE_FAILURES
                || self.stop_condition_reached(started).is_some();
            if !next_round_would_stop {
                if long_every > 0 && iteration.is_multiple_of(long_every) {
                    debug!(iteration, "taking a long human pause");
                    self.clicker.long_human_pause();
                } else {
                    self.clicker.inter_round_pause();
                }
            }
        }
        Ok(RunInnerOutcome {
            reason: goal_reason,
            refreshes: refreshes_done,
        })
    }

    fn stop_condition_reached(&self, started: Instant) -> Option<&'static str> {
        stop_condition_for(&self.config.shop, started.elapsed(), |alias| {
            self.progress.bought_count(alias)
        })
    }

    fn run_round(&mut self, round: u32) -> Result<RoundOutcome> {
        info!(round, "round starting");

        // Refresh re-rolls the inventory, so the per-round dedup set resets.
        self.bought_types.clear();

        // No "are we in the shop?" pre-check — too brittle across
        // languages/resolutions. IAP-redirect safety is covered downstream
        // by the modal-open hash checks in `refresh_shop` and
        // `try_buy_at_pixel_y` (a mis-clicked first click never triggers
        // a follow-up on a stale confirm zone).
        let bought = self.buy_round()?;
        info!(round, bought, "items bought");

        let refreshed = self.refresh_shop()?;
        Ok(RoundOutcome { bought, refreshed })
    }

    fn buy_round(&mut self) -> Result<u32> {
        let mut bought = 0u32;
        let max_scrolls = self.config.shop.max_scrolls_per_round;

        info!("scrolling to top of shop");
        self.scroll_to_top()?;
        self.clicker.human_pause();

        let mut prev_strip_hash: Option<u64> = None;

        for scroll_iter in 0..=max_scrolls {
            if self.stop.load(Ordering::Relaxed) {
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
    fn bottom_strip_hash(&self, gray: &GrayImage) -> u64 {
        let [gx, gy, gw, gh] = self.config.regions.shop_grid.unwrap_or(layout::SHOP_GRID);
        let strip = [gx, gy + gh * 0.75, gw, gh * 0.25];
        strip_hash(gray, strip)
    }

    /// Searching the full column rather than a fixed slot patch absorbs
    /// Y-drift across resolutions and patches.
    fn process_current_view(&mut self) -> Result<u32> {
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
            if self.stop.load(Ordering::Relaxed) {
                return Ok(bought);
            }

            let scan = scan_shop_raw(&*self.capture, &self.detector, &pending, shop_grid)?;

            let mut any_bought = false;
            let mut still_pending: Vec<&'static str> = Vec::with_capacity(pending.len());

            for (alias_name, hit) in scan.hits {
                if self.stop.load(Ordering::Relaxed) {
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
    fn verify_colour(
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
    fn try_buy_at_pixel_y(
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
        if self.stop.load(Ordering::Relaxed) {
            return Ok(false);
        }

        self.clicker
            .pause_ms(self.config.timing.modal_open_pause_ms);
        if self.stop.load(Ordering::Relaxed) {
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
    fn buy_button_local_rect(&self, icon_y_px: i32, frame_h: u32) -> Result<[i32; 4]> {
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

    fn ratio_rect_to_local(&self, zone: [f32; 4]) -> Result<[i32; 4]> {
        let r = self.capture.rect()?;
        let (w, h) = (r.width as i32, r.height as i32);
        let x = (zone[0] * w as f32).round() as i32;
        let y = (zone[1] * h as f32).round() as i32;
        let zw = (zone[2] * w as f32).round() as i32;
        let zh = (zone[3] * h as f32).round() as i32;
        Ok([x.clamp(0, w), y.clamp(0, h), zw.max(1), zh.max(1)])
    }

    fn scroll_one_step(&mut self) -> Result<()> {
        let (mx, my) = self.scroll_point()?;
        self.clicker
            .scroll_at(&*self.capture, mx, my, self.config.timing.scroll_amount)?;
        self.clicker.pause_ms(self.config.timing.scroll_pause_ms);
        Ok(())
    }

    fn scroll_to_top(&mut self) -> Result<()> {
        let (mx, my) = self.scroll_point()?;
        let total = self.config.shop.max_scrolls_per_round + 2;
        for _ in 0..total {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            self.clicker
                .scroll_at(&*self.capture, mx, my, -self.config.timing.scroll_amount)?;
            self.clicker.pause_ms(80);
        }
        Ok(())
    }

    fn scroll_point(&self) -> Result<(i32, i32)> {
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
    fn refresh_shop(&mut self) -> Result<bool> {
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
        if self.stop.load(Ordering::Relaxed) {
            return Ok(false);
        }

        self.clicker
            .pause_ms(self.config.timing.modal_open_pause_ms);
        if self.stop.load(Ordering::Relaxed) {
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

    fn snapshot(&self) -> Result<GrayImage> {
        self.capture.snapshot_gray()
    }
}

/// One capture + parallel NCC pass against `targets`. Single source of
/// truth for "what's on screen in the shop right now" — used both by the
/// bot loop and the Setup-tab live preview worker. The colour check is
/// kept at the call site so the bot can `warn!` on rejections while the
/// Setup preview drops them silently.
pub(crate) fn scan_shop_raw(
    capture: &dyn Capture,
    detector: &Detector,
    targets: &[&'static str],
    shop_grid: [f32; 4],
) -> Result<ShopScanRaw> {
    use rayon::prelude::*;

    let rgba = capture.snapshot_rgba()?;
    let gray = image::imageops::grayscale(&rgba);
    let ctx = detector.prepare_search(&gray, Some(shop_grid));
    let raw: Vec<(&'static str, Result<Option<crate::detector::Hit>>)> = targets
        .par_iter()
        .map(|alias_name| (*alias_name, detector.find_in(&ctx, alias_name)))
        .collect();
    // Propagate the first NCC error rather than masking it — matches the
    // bot's prior behaviour where a `find_in` failure stops the round.
    let hits: Vec<(&'static str, Option<crate::detector::Hit>)> = raw
        .into_iter()
        .map(|(alias, r)| r.map(|h| (alias, h)))
        .collect::<Result<Vec<_>>>()?;
    Ok(ShopScanRaw { rgba, hits })
}

pub(crate) struct ShopScanRaw {
    pub rgba: RgbaImage,
    /// `None` = template not found by NCC. The colour-check pass that
    /// turns a "raw" hit into an actionable one stays at the call site.
    pub hits: Vec<(&'static str, Option<crate::detector::Hit>)>,
}

/// Patch grows with `hit.scale` so the hue histogram covers the whole
/// rendered icon at non-native scales.
pub(crate) fn crop_icon_patch(rgba: &RgbaImage, hit: &crate::detector::Hit) -> RgbaImage {
    // 40×40 covers the largest bundled icon (covenant 47×45).
    const FALLBACK_SIDE: u32 = 40;
    let scale = hit.scale.max(0.1);
    let w = ((FALLBACK_SIDE as f32) * scale).round().max(8.0) as u32;
    let h = w;
    let (img_w, img_h) = rgba.dimensions();
    let x0 = (hit.x - (w / 2) as i32).clamp(0, (img_w.saturating_sub(1)) as i32) as u32;
    let y0 = (hit.y - (h / 2) as i32).clamp(0, (img_h.saturating_sub(1)) as i32) as u32;
    let cw = w.min(img_w - x0);
    let ch = h.min(img_h - y0);
    rgba.view(x0, y0, cw, ch).to_image()
}

#[derive(Debug, Clone, Copy)]
struct RoundOutcome {
    bought: u32,
    refreshed: bool,
}

#[derive(Debug, Clone, Copy)]
struct RunInnerOutcome {
    /// `None` = clean exit, `Some(_)` = stop condition fired.
    reason: Option<&'static str>,
    refreshes: u32,
}

const MAX_CONSECUTIVE_FAILURES: u32 = 5;

pub mod prices {
    pub const MYSTIC_MEDAL: u32 = 280_000;
    pub const COVENANT_BOOKMARK: u32 = 185_000;
}

pub(crate) fn gold_spent_for(bought: impl Fn(&str) -> u32) -> u64 {
    u64::from(bought(alias::MYSTIC_MEDAL)) * u64::from(prices::MYSTIC_MEDAL)
        + u64::from(bought(alias::COVENANT)) * u64::from(prices::COVENANT_BOOKMARK)
}

/// First condition in fixed priority order
/// (duration → mystic → covenant → gold) so the reason is deterministic.
pub(crate) fn stop_condition_for(
    shop: &crate::config::ShopConfig,
    elapsed: Duration,
    bought: impl Fn(&str) -> u32,
) -> Option<&'static str> {
    if shop.stop_after_minutes > 0
        && elapsed >= Duration::from_secs(u64::from(shop.stop_after_minutes) * 60)
    {
        return Some("stop_after_minutes");
    }
    if shop.stop_when_mystic_medals > 0
        && bought(alias::MYSTIC_MEDAL) >= shop.stop_when_mystic_medals
    {
        return Some("stop_when_mystic_medals");
    }
    if shop.stop_when_covenants > 0 && bought(alias::COVENANT) >= shop.stop_when_covenants {
        return Some("stop_when_covenants");
    }
    if shop.stop_when_gold_spent > 0
        && gold_spent_for(&bought) >= u64::from(shop.stop_when_gold_spent)
    {
        return Some("stop_when_gold_spent");
    }
    None
}

/// FxHasher because pixel data is non-adversarial — 2-3× faster than
/// std's SipHash on these tiny inputs.
fn strip_hash(gray: &GrayImage, [x, y, w, h]: [f32; 4]) -> u64 {
    use std::hash::Hasher;
    let (sw, sh) = (gray.width(), gray.height());
    let x0 = (x * sw as f32)
        .round()
        .clamp(0.0, sw.saturating_sub(1) as f32) as u32;
    let y0 = (y * sh as f32)
        .round()
        .clamp(0.0, sh.saturating_sub(1) as f32) as u32;
    let w_px = (w * sw as f32).round().clamp(1.0, sw as f32) as u32;
    let h_px = (h * sh as f32).round().clamp(1.0, sh as f32) as u32;
    let x1 = x0.saturating_add(w_px).min(sw);
    let y1 = y0.saturating_add(h_px).min(sh);

    let mut hasher = rustc_hash::FxHasher::default();
    for yy in (y0..y1).step_by(4) {
        for xx in (x0..x1).step_by(4) {
            hasher.write_u8(gray.get_pixel(xx, yy)[0]);
        }
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{Capture, WindowRect};
    use crate::config::{Config, ShopConfig};
    use crate::input::Input;
    use image::GrayImage;
    use std::sync::Mutex as StdMutex;

    /// Returns `WindowGone` once frames empty so tests can observe
    /// reattach / graceful exit.
    pub(super) struct FakeCapture {
        frames: StdMutex<Vec<GrayImage>>,
        rect: WindowRect,
    }

    impl FakeCapture {
        pub fn new(frames: Vec<GrayImage>, rect: WindowRect) -> Self {
            // Reversed so pop() returns frames in caller-supplied order.
            let frames: Vec<_> = frames.into_iter().rev().collect();
            Self {
                frames: StdMutex::new(frames),
                rect,
            }
        }
    }

    impl Capture for FakeCapture {
        fn snapshot_gray(&self) -> Result<GrayImage> {
            self.frames
                .lock()
                .expect("frames mutex poisoned")
                .pop()
                .ok_or(Error::WindowGone)
        }
        fn rect(&self) -> Result<WindowRect> {
            Ok(self.rect)
        }
        fn check_size_stable(&self, _: u32) -> Result<()> {
            Ok(())
        }
        fn restore_to_baseline(&self) -> Result<bool> {
            Ok(true)
        }
        fn local_to_screen(&self, x: i32, y: i32) -> Result<(i32, i32)> {
            Ok((self.rect.x + x, self.rect.y + y))
        }
        fn is_foreground(&self) -> bool {
            true
        }
        fn try_bring_foreground(&self) -> bool {
            true
        }
        fn hwnd_is_valid(&self) -> bool {
            true
        }
        fn reattach(&self) -> Result<()> {
            Ok(())
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    pub(super) enum FakeEvent {
        Click([i32; 4]),
        Scroll { x: i32, y: i32, lines: i32 },
        HumanPause,
        PauseMs(u64),
        InterRound,
        LongPause,
    }

    /// Event log behind Arc<Mutex<…>> so tests can inspect after the
    /// boxed input has been moved into a runner.
    #[derive(Default, Clone)]
    pub(super) struct FakeInput {
        pub events: Arc<StdMutex<Vec<FakeEvent>>>,
    }

    impl Input for FakeInput {
        fn click_local_in_rect(&mut self, _: &dyn Capture, rect: [i32; 4]) -> Result<()> {
            self.events.lock().unwrap().push(FakeEvent::Click(rect));
            Ok(())
        }
        fn scroll_at(&mut self, _: &dyn Capture, x: i32, y: i32, lines: i32) -> Result<()> {
            self.events
                .lock()
                .unwrap()
                .push(FakeEvent::Scroll { x, y, lines });
            Ok(())
        }
        fn human_pause(&mut self) {
            self.events.lock().unwrap().push(FakeEvent::HumanPause);
        }
        fn pause_ms(&self, ms: u64) {
            self.events.lock().unwrap().push(FakeEvent::PauseMs(ms));
        }
        fn inter_round_pause(&mut self) {
            self.events.lock().unwrap().push(FakeEvent::InterRound);
        }
        fn long_human_pause(&mut self) {
            self.events.lock().unwrap().push(FakeEvent::LongPause);
        }
    }

    #[test]
    fn fake_input_records_calls_through_trait_dispatch() {
        let cap = FakeCapture::new(
            vec![],
            WindowRect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
        );
        let cap_dyn: &dyn Capture = &cap;

        let mut input = FakeInput::default();
        let log = input.events.clone();

        let input_dyn: &mut dyn Input = &mut input;
        input_dyn
            .click_local_in_rect(cap_dyn, [10, 20, 30, 40])
            .unwrap();
        input_dyn.scroll_at(cap_dyn, 5, 6, 8).unwrap();
        input_dyn.human_pause();
        input_dyn.pause_ms(200);
        input_dyn.inter_round_pause();
        input_dyn.long_human_pause();

        let events = log.lock().unwrap();
        assert_eq!(
            events.as_slice(),
            &[
                FakeEvent::Click([10, 20, 30, 40]),
                FakeEvent::Scroll {
                    x: 5,
                    y: 6,
                    lines: 8
                },
                FakeEvent::HumanPause,
                FakeEvent::PauseMs(200),
                FakeEvent::InterRound,
                FakeEvent::LongPause,
            ]
        );
    }

    #[test]
    fn runner_with_fakes_respects_stop_flag_before_first_round() {
        let mut config: Config = toml::from_str(crate::config::DEFAULT_TOML).unwrap();
        config.zones.refresh = Some([0.1, 0.8, 0.1, 0.1]);
        config.zones.refresh_confirm = Some([0.4, 0.5, 0.1, 0.1]);
        config.zones.buy_confirm = Some([0.4, 0.5, 0.1, 0.1]);
        config.zones.buy_column = Some([0.8, 0.0, 0.1, 1.0]);
        config.shop.max_refreshes = 1;
        config.shop.sleep_when_done = false;

        let capture: Arc<dyn Capture> = Arc::new(FakeCapture::new(
            vec![],
            WindowRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
        ));
        let detector = Arc::new(Detector::from_test_images(std::collections::HashMap::new()));
        let fake_input = FakeInput::default();
        let events = fake_input.events.clone();
        let input: Box<dyn Input> = Box::new(fake_input);
        let stop = Arc::new(AtomicBool::new(true));

        let live_shop = Arc::new(RwLock::new(config.shop.clone()));
        let mut runner = ShopRunner::new(capture, detector, input, config, live_shop, stop);
        runner.run().expect("run with stop=true should be Ok");
        assert!(
            events.lock().unwrap().is_empty(),
            "no clicks/scrolls/pauses should fire when stop is set before round 1"
        );
    }

    #[test]
    fn refresh_live_shop_picks_up_external_mutation() {
        let mut config: Config = toml::from_str(crate::config::DEFAULT_TOML).unwrap();
        config.zones.refresh = Some([0.1, 0.8, 0.1, 0.1]);
        config.zones.refresh_confirm = Some([0.4, 0.5, 0.1, 0.1]);
        config.zones.buy_confirm = Some([0.4, 0.5, 0.1, 0.1]);
        config.zones.buy_column = Some([0.8, 0.0, 0.1, 1.0]);
        config.shop.max_refreshes = 1;
        config.shop.buy_mystic_medals = true;
        config.shop.buy_covenant = false;

        let capture: Arc<dyn Capture> = Arc::new(FakeCapture::new(
            vec![],
            WindowRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
        ));
        let detector = Arc::new(Detector::from_test_images(std::collections::HashMap::new()));
        let input: Box<dyn Input> = Box::new(FakeInput::default());
        let stop = Arc::new(AtomicBool::new(false));
        let live_shop = Arc::new(RwLock::new(config.shop.clone()));
        let mut runner = ShopRunner::new(
            capture,
            detector,
            input,
            config,
            Arc::clone(&live_shop),
            stop,
        );

        // Sanity: starting state matches the seed config.
        assert_eq!(runner.config.shop.max_refreshes, 1);
        assert!(runner.config.shop.buy_mystic_medals);
        assert!(!runner.config.shop.buy_covenant);
        assert_eq!(
            runner.enabled_targets,
            vec![crate::detector::alias::MYSTIC_MEDAL]
        );

        // Simulate the GUI mutating live_shop mid-run.
        {
            let mut shop = live_shop.write().unwrap();
            shop.max_refreshes = 42;
            shop.buy_mystic_medals = false;
            shop.buy_covenant = true;
            shop.stop_after_minutes = 30;
        }

        runner.refresh_live_shop();

        // All four fields propagated and enabled_targets was recomputed
        // from the new buy_* flags — this is the whole point of the
        // shared handle: ui edits visible to the worker without a
        // restart.
        assert_eq!(runner.config.shop.max_refreshes, 42);
        assert!(!runner.config.shop.buy_mystic_medals);
        assert!(runner.config.shop.buy_covenant);
        assert_eq!(runner.config.shop.stop_after_minutes, 30);
        assert_eq!(
            runner.enabled_targets,
            vec![crate::detector::alias::COVENANT]
        );
    }

    fn shop_with(max_refreshes: u32, minutes: u32, mystic: u32, covenants: u32) -> ShopConfig {
        ShopConfig {
            max_refreshes,
            buy_mystic_medals: true,
            buy_covenant: true,
            max_scrolls_per_round: 3,
            buy_button_y_offset_ratio: 0.04,
            buy_button_band_h_ratio: 0.04,
            buy_calibration_line_y_ratio: 0.55,
            stop_after_minutes: minutes,
            stop_when_mystic_medals: mystic,
            stop_when_covenants: covenants,
            stop_when_gold_spent: 0,
            sleep_when_done: false,
        }
    }

    #[test]
    fn stop_condition_none_when_all_zero() {
        let cfg = shop_with(0, 0, 0, 0);
        let reason = stop_condition_for(&cfg, Duration::from_secs(3600), |_| 999);
        assert_eq!(reason, None);
    }

    #[test]
    fn stop_condition_fires_on_minutes() {
        let cfg = shop_with(0, 5, 0, 0);
        let reason = stop_condition_for(&cfg, Duration::from_secs(300), |_| 0);
        assert_eq!(reason, Some("stop_after_minutes"));
        let reason = stop_condition_for(&cfg, Duration::from_secs(299), |_| 0);
        assert_eq!(reason, None);
    }

    #[test]
    fn stop_condition_fires_on_per_alias_counts() {
        let cfg = shop_with(0, 0, 3, 0);
        let count = |a: &str| {
            if a == alias::MYSTIC_MEDAL { 3 } else { 0 }
        };
        assert_eq!(
            stop_condition_for(&cfg, Duration::from_secs(0), count),
            Some("stop_when_mystic_medals")
        );
    }

    #[test]
    fn stop_condition_priority_is_duration_then_mystic_then_covenant() {
        let cfg = shop_with(0, 1, 5, 5);
        let reason = stop_condition_for(&cfg, Duration::from_secs(60), |_| 10);
        assert_eq!(reason, Some("stop_after_minutes"));
        let cfg = shop_with(0, 0, 5, 5);
        let reason = stop_condition_for(&cfg, Duration::from_secs(0), |_| 10);
        assert_eq!(reason, Some("stop_when_mystic_medals"));
        let cfg = shop_with(0, 0, 0, 5);
        let reason = stop_condition_for(&cfg, Duration::from_secs(0), |_| 10);
        assert_eq!(reason, Some("stop_when_covenants"));
    }

    #[test]
    fn stop_condition_ignores_count_when_threshold_is_zero() {
        let cfg = shop_with(0, 0, 0, 0);
        let reason = stop_condition_for(&cfg, Duration::from_secs(0), |_| 100);
        assert_eq!(reason, None);
    }

    #[test]
    fn stop_condition_fires_when_gold_spent_target_reached() {
        let mut cfg = shop_with(0, 0, 0, 0);
        cfg.stop_when_gold_spent = 1_000_000;
        // 3 mystic × 280k + 1 covenant × 185k = 1_025_000 ≥ 1_000_000.
        let count = |a: &str| match a {
            "mystic_medal" => 3,
            "covenant" => 1,
            _ => 0,
        };
        assert_eq!(
            stop_condition_for(&cfg, Duration::from_secs(0), count),
            Some("stop_when_gold_spent")
        );
    }

    #[test]
    fn stop_condition_gold_does_not_fire_below_threshold() {
        let mut cfg = shop_with(0, 0, 0, 0);
        cfg.stop_when_gold_spent = 1_000_000;
        let count = |a: &str| if a == "mystic_medal" { 3 } else { 0 }; // 840k
        assert_eq!(
            stop_condition_for(&cfg, Duration::from_secs(0), count),
            None
        );
    }

    #[test]
    fn stop_condition_fires_at_exact_threshold_not_below() {
        let cfg = shop_with(0, 0, 5, 0);
        assert_eq!(
            stop_condition_for(&cfg, Duration::from_secs(0), |_| 5),
            Some("stop_when_mystic_medals")
        );
        assert_eq!(
            stop_condition_for(&cfg, Duration::from_secs(0), |_| 4),
            None
        );
    }

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

    fn solid_gray(w: u32, h: u32, value: u8) -> GrayImage {
        GrayImage::from_pixel(w, h, image::Luma([value]))
    }

    fn checker(w: u32, h: u32, tile: u32) -> GrayImage {
        let mut img = GrayImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let v = if ((x / tile) + (y / tile)).is_multiple_of(2) {
                    0
                } else {
                    255
                };
                img.put_pixel(x, y, image::Luma([v]));
            }
        }
        img
    }

    #[test]
    fn strip_hash_handles_full_window_roi() {
        let img = checker(100, 100, 4);
        let _ = strip_hash(&img, [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn strip_hash_clamps_out_of_bounds_ratios() {
        let img = solid_gray(100, 100, 64);
        let _ = strip_hash(&img, [0.5, 0.5, 5.0, 5.0]);
    }

    // --- loop-test helpers ---

    fn gray_frame(w: u32, h: u32, base: u8) -> GrayImage {
        GrayImage::from_pixel(w, h, image::Luma([base]))
    }

    fn paint_zone(img: &mut GrayImage, [zx, zy, zw, zh]: [f32; 4], value: u8) {
        let (w, h) = (img.width() as f32, img.height() as f32);
        let x0 = (zx * w) as u32;
        let y0 = (zy * h) as u32;
        let x1 = ((zx + zw) * w).min(w) as u32;
        let y1 = ((zy + zh) * h).min(h) as u32;
        for y in y0..y1 {
            for x in x0..x1 {
                img.put_pixel(x, y, image::Luma([value]));
            }
        }
    }

    const REFRESH: [f32; 4] = [0.1, 0.8, 0.1, 0.1];
    const REFRESH_CONFIRM: [f32; 4] = [0.4, 0.5, 0.1, 0.1];
    const SHOP_GRID: [f32; 4] = [0.1, 0.1, 0.6, 0.6];

    fn runner_for_loop_tests(
        frames: Vec<GrayImage>,
    ) -> (ShopRunner, Arc<StdMutex<Vec<FakeEvent>>>) {
        let mut config: Config = toml::from_str(crate::config::DEFAULT_TOML).unwrap();
        config.zones.refresh = Some(REFRESH);
        config.zones.refresh_confirm = Some(REFRESH_CONFIRM);
        config.zones.buy_confirm = Some([0.4, 0.5, 0.1, 0.1]);
        config.zones.buy_column = Some([0.8, 0.0, 0.1, 1.0]);
        config.regions.shop_grid = Some(SHOP_GRID);
        config.shop.buy_mystic_medals = false;
        config.shop.buy_covenant = false;
        config.shop.max_scrolls_per_round = 0;
        config.shop.sleep_when_done = false;

        let capture: Arc<dyn Capture> = Arc::new(FakeCapture::new(
            frames,
            WindowRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
        ));
        let detector = Arc::new(Detector::from_test_images(std::collections::HashMap::new()));
        let fake_input = FakeInput::default();
        let events = fake_input.events.clone();
        let input: Box<dyn Input> = Box::new(fake_input);
        let stop = Arc::new(AtomicBool::new(false));
        let live_shop = Arc::new(RwLock::new(config.shop.clone()));
        (
            ShopRunner::new(capture, detector, input, config, live_shop, stop),
            events,
        )
    }

    // --- refresh_shop branch tests ---

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

    // --- run_inner failure-cap test ---

    #[test]
    fn run_inner_bails_with_too_many_failures_when_capture_keeps_dying() {
        let (mut runner, _events) = runner_for_loop_tests(vec![]);
        let err = runner.run_inner(std::time::Instant::now()).unwrap_err();
        assert!(matches!(err, Error::TooManyFailures(n) if n == 5));
    }

    // --- run_inner max_refreshes test ---

    #[test]
    fn run_inner_stops_with_max_refreshes_reason_after_successful_round() {
        let base = gray_frame(200, 200, 100);
        // Frame order for one full round:
        // 1. buy_round bottom-strip snapshot
        // 2. refresh_shop frame A
        // 3. refresh_shop frame B (modal opened)
        // 4. refresh_shop frame C (grid changed)
        let buy_strip = base.clone();
        let frame_a = base.clone();
        let mut frame_b = base.clone();
        paint_zone(&mut frame_b, REFRESH_CONFIRM, 200);
        let mut frame_c = base.clone();
        paint_zone(&mut frame_c, SHOP_GRID, 30);

        let (mut runner, events) =
            runner_for_loop_tests(vec![buy_strip, frame_a, frame_b, frame_c]);
        runner.config.shop.max_refreshes = 1;
        {
            let mut ls = runner.live_shop.write().unwrap();
            ls.max_refreshes = 1;
        }

        let outcome = runner.run_inner(std::time::Instant::now()).unwrap();
        assert_eq!(outcome.reason, Some("max_refreshes"));
        assert_eq!(outcome.refreshes, 1);
        let clicks = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, FakeEvent::Click(_)))
            .count();
        assert_eq!(clicks, 2);
    }

    // --- try_buy_at_pixel_y (buy-click modal guard) tests ---

    #[test]
    fn try_buy_at_pixel_y_confirms_when_modal_opens() {
        let before = gray_frame(200, 200, 100);
        let mut after = before.clone();
        paint_zone(&mut after, [0.4, 0.5, 0.1, 0.1], 200);
        let (mut runner, events) = runner_for_loop_tests(vec![before, after]);

        let result = runner
            .try_buy_at_pixel_y(alias::MYSTIC_MEDAL, 100, 200)
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
            .try_buy_at_pixel_y(alias::MYSTIC_MEDAL, 100, 200)
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

    // --- buy_round scroll-stop test ---

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
