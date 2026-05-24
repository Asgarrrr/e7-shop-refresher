use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use image::GrayImage;
use tracing::{debug, info, warn};

use crate::capture::Capture;
use crate::config::{Config, RegionsConfig, ShopConfig};
use crate::detector::{Detector, alias};
use crate::error::{Error, Result};
use crate::input::Input;

/// 4% of window height ≈ one row at 1080p — wide enough for sub-pixel
/// scroll drift, narrow enough to never leak into the adjacent row.
pub const BUY_COLUMN_ROW_BAND_RATIO: f32 = 0.04;

/// Lets the freshly re-located game window finish painting before the
/// next anchor wait starts polling.
const REATTACH_BACKOFF_MS: u64 = 800;

/// Errors recoverable by re-finding the game window: crash + relaunch,
/// alt-F4, minimize. Unrecoverable errors (bad config, missing template,
/// unset zone) propagate up.
fn is_recoverable(err: &Error) -> bool {
    matches!(
        err,
        Error::WindowGone
            | Error::WindowHandleInvalid
            | Error::WindowNotForeground
            | Error::Xcap(_)
    )
}

/// Pure version of `ShopRunner::buy_column_row_rect` so the GUI debug
/// overlay can render the exact rect the runner would click. Only x/w of
/// `column` are used; the row Y comes from the matched icon.
pub fn buy_column_row_rect_for(
    column: [f32; 4],
    item_y: i32,
    frame_h: u32,
    window_w: u32,
    window_h: u32,
    y_offset_ratio: f32,
) -> [i32; 4] {
    let (w, h) = (window_w as i32, window_h as i32);
    let bx = (column[0] * w as f32).round() as i32;
    let bw = (column[2] * w as f32).round() as i32;
    let band_h = ((frame_h as f32) * BUY_COLUMN_ROW_BAND_RATIO).round() as i32;
    let y_offset = ((frame_h as f32) * y_offset_ratio).round() as i32;
    let by = (item_y + y_offset - band_h / 2).clamp(0, (h - band_h.max(1)).max(0));
    [bx.max(0), by, bw.max(1), band_h.max(1)]
}

/// Observer hooks for embedding `ShopRunner` in a GUI or test harness.
pub trait ProgressSink: Send + Sync {
    fn round_started(&self, _round: u32, _total: u32) {}
    fn round_finished(&self, _round: u32, _bought: u32) {}
    fn item_bought(&self, _alias: &str) {}
    fn finished(&self) {}
    fn failed(&self, _err: &str) {}

    /// Default `0` means per-alias stop conditions never fire under
    /// sinks that don't track counters — fine for CLI/headless runs
    /// that rely on `max_refreshes`.
    fn bought_count(&self, _alias: &str) -> u32 {
        0
    }
}

pub struct NullSink;
impl ProgressSink for NullSink {}

pub struct ShopRunner {
    capture: Arc<dyn Capture>,
    detector: Arc<Detector>,
    clicker: Box<dyn Input>,
    config: Config,
    /// Shared, live-editable view of `[shop]`. The GUI mutates the
    /// matching fields and pushes them here on every frame; the runner
    /// pulls a fresh snapshot at the start of every round so mid-run
    /// edits to stop conditions, targets, and sleep-on-done take effect
    /// at the next boundary. Setup-tab fields (timing / matching /
    /// templates / zones) stay captured by value in `config` since the
    /// bot is in the middle of using them.
    live_shop: Arc<RwLock<ShopConfig>>,
    stop: Arc<AtomicBool>,
    progress: Arc<dyn ProgressSink>,
    enabled_targets: Vec<&'static str>,
    /// Item types bought this round. The shop holds at most one of each
    /// per refresh, so any later match for the same alias is a re-detection
    /// of the now-sold-out item across scroll positions. Cleared per round.
    bought_types: HashSet<&'static str>,
    /// Icon-area hashes already clicked this round (success OR fail). A
    /// false NCC match that didn't open a modal at view N is skipped at
    /// views N+1, N+2. Cleared per round.
    attempted_icons: HashSet<u64>,
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
            clicker,
            config,
            live_shop,
            stop,
            progress: Arc::new(NullSink),
            enabled_targets,
            bought_types: HashSet::new(),
            attempted_icons: HashSet::new(),
        }
    }

    /// Pulls the latest `[shop]` section from `live_shop` into the
    /// owned `Config` snapshot the rest of the runner reads from. Also
    /// recomputes `enabled_targets` so a toggle of `buy_mystic_medals`
    /// / `buy_covenant` propagates without other bookkeeping.
    ///
    /// Called once per round at the iteration boundary — keeps every
    /// decision inside a round consistent against a single snapshot,
    /// even if the user is dragging a slider while the round runs.
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

    pub fn run(&mut self) -> Result<()> {
        self.config.ensure_zones_set()?;

        let s = &self.config.shop;
        info!(
            max_refreshes = s.max_refreshes,
            stop_after_minutes = s.stop_after_minutes,
            stop_when_mystic_medals = s.stop_when_mystic_medals,
            stop_when_covenants = s.stop_when_covenants,
            sleep_when_done = s.sleep_when_done,
            "starting shop refresh loop"
        );

        let result = self.run_inner();
        // Read sleep_when_done AFTER the loop so a toggle made mid-run
        // (e.g. user clicked "Sleep PC" right before going to bed) is
        // honoured. Matches the user's intuition that this is a final-
        // step decision, not a "must be set at start" config.
        let sleep_after = self.config.shop.sleep_when_done;
        match &result {
            Ok(reached_goal) => {
                self.progress.finished();
                // Manual Stop means the user is at the keyboard; never sleep on them.
                if *reached_goal && sleep_after {
                    info!("goal reached — suspending system");
                    crate::power::suspend_to_sleep();
                }
            }
            Err(e) => self.progress.failed(&e.to_string()),
        }
        info!("shop refresh loop complete");
        result.map(|_| ())
    }

    /// `Ok(true)` when stopped via a configured goal (drives sleep-on-finish),
    /// `Ok(false)` on manual Stop, `Err` on real failures.
    fn run_inner(&mut self) -> Result<bool> {
        let long_every = self.config.timing.long_pause_every_n;
        let started = Instant::now();
        let mut round: u32 = 0;
        let mut reached_goal = false;

        loop {
            round += 1;
            // Pull the latest live shop snapshot before reading any of
            // its fields — keeps the whole round consistent against one
            // version while still propagating UI edits at the boundary.
            self.refresh_live_shop();
            let max_refreshes = self.config.shop.max_refreshes;

            if self.stop.load(Ordering::Relaxed) {
                info!("stop signal received");
                break;
            }
            if max_refreshes > 0 && round > max_refreshes {
                info!(max_refreshes, "max_refreshes reached — stopping");
                reached_goal = true;
                break;
            }
            if let Some(reason) = self.stop_condition_reached(started) {
                info!(stop_reason = reason, "stop condition reached");
                reached_goal = true;
                break;
            }

            if let Err(e) = self
                .capture
                .check_size_stable(self.config.window.resize_tolerance_px)
            {
                warn!(error = %e, "window drifted — attempting to restore baseline size");
                match self.capture.restore_to_baseline() {
                    Ok(true)
                        if self
                            .capture
                            .check_size_stable(self.config.window.resize_tolerance_px)
                            .is_ok() =>
                    {
                        info!("window restored to baseline");
                    }
                    _ => {
                        warn!("restore did not bring window back to baseline — stopping");
                        return Err(e);
                    }
                }
            }

            self.progress.round_started(round, max_refreshes);

            match self.run_round(round) {
                Ok(bought) => self.progress.round_finished(round, bought),
                Err(e) if is_recoverable(&e) => {
                    // Burn this round (do NOT decrement) so a stuck
                    // unrecoverable state still hits max_refreshes.
                    warn!(
                        round,
                        error = %e,
                        "recoverable failure — attempting capture reattach"
                    );
                    if let Err(reattach_err) = self.capture.reattach() {
                        warn!(error = %reattach_err, "reattach failed — bailing");
                        return Err(e);
                    }
                    info!("capture reattached, retrying next round");
                    self.clicker.pause_ms(REATTACH_BACKOFF_MS);
                }
                Err(e) => return Err(e),
            }

            // Skip the inter-round pause if the next iteration would
            // immediately stop anyway (avoids overshooting wall-clock by
            // up to long_pause_max_ms).
            let next_round_would_stop = self.stop.load(Ordering::Relaxed)
                || (max_refreshes > 0 && round + 1 > max_refreshes)
                || self.stop_condition_reached(started).is_some();
            if !next_round_would_stop {
                if long_every > 0 && round.is_multiple_of(long_every) {
                    debug!(round, "taking a long human pause");
                    self.clicker.long_human_pause();
                } else {
                    self.clicker.inter_round_pause();
                }
            }
        }
        Ok(reached_goal)
    }

    fn stop_condition_reached(&self, started: Instant) -> Option<&'static str> {
        stop_condition_for(&self.config.shop, started.elapsed(), |alias| {
            self.progress.bought_count(alias)
        })
    }

    fn run_round(&mut self, round: u32) -> Result<u32> {
        info!(round, "round starting");

        // Refresh re-rolls the inventory, so both dedupe sets reset.
        self.bought_types.clear();
        self.attempted_icons.clear();

        // Anchor check is the safety net against the IAP redirect: if the
        // previous refresh ran out of skystones, the game opens a paid-pack
        // store. Anchor template doesn't match → bail without clicking.
        match self.wait_anchor() {
            Ok(()) => info!(round, "shop anchor confirmed"),
            Err(Error::AnchorTimeout(_)) => {
                warn!(
                    round,
                    "shop anchor not seen — skipping round (IAP redirect possible)"
                );
                return Ok(0);
            }
            Err(e) => return Err(e),
        }

        let bought = self.buy_round()?;
        info!(round, bought, "items bought");

        self.refresh_shop()?;
        Ok(bought)
    }

    fn wait_anchor(&mut self) -> Result<()> {
        let timeout = Duration::from_millis(self.config.timing.anchor_timeout_ms);
        let poll = Duration::from_millis(self.config.timing.poll_interval_ms);
        let roi = self.config.regions.anchor_shop;
        info!(
            timeout_ms = self.config.timing.anchor_timeout_ms,
            "waiting for shop anchor"
        );

        let stop = Arc::clone(&self.stop);
        let hit = self.detector.wait_for(
            &*self.capture,
            alias::ANCHOR_SHOP,
            roi,
            timeout,
            poll,
            &stop,
        )?;
        debug!(
            score = hit.score,
            margin = hit.margin,
            "shop anchor confirmed"
        );
        Ok(())
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

            let (view_bought, last_gray) = self.process_current_view()?;
            bought += view_bought;
            if view_bought > 0 {
                info!(scroll_iter, view_bought, "items found in this view");
            } else {
                debug!(scroll_iter, "scroll position scanned, no targets");
            }

            // Two identical bottom-strip hashes in a row = scroll didn't
            // reveal new content. Reuse the snapshot if process_current_view
            // already took one.
            let strip = match last_gray {
                Some(gray) => self.bottom_strip_hash(&gray),
                None => self.bottom_strip_hash(&self.snapshot()?),
            };
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

    fn bottom_strip_hash(&self, gray: &GrayImage) -> u64 {
        let [gx, gy, gw, gh] = self.regions().shop_grid.unwrap_or([0.0, 0.0, 1.0, 1.0]);
        let strip = [gx, gy + gh * 0.75, gw, gh * 0.25];
        strip_hash(gray, strip)
    }

    /// Returns `(items_bought, last_detection_snapshot)`. The snapshot is
    /// threaded across aliases: if alias N didn't buy, alias N+1 reuses
    /// the same pixels. After a buy, the buffer is dropped (modal in the way).
    fn process_current_view(&mut self) -> Result<(u32, Option<GrayImage>)> {
        let mut bought = 0u32;
        let mut cached_gray: Option<GrayImage> = None;
        // Clone to release the borrow on self.enabled_targets — the loop
        // body needs &mut self for try_buy_visible.
        let targets: Vec<&'static str> = self.enabled_targets.clone();
        for alias_name in targets {
            if self.stop.load(Ordering::Relaxed) {
                return Ok((bought, cached_gray));
            }
            let (outcome, gray) = self.try_buy_visible(alias_name, cached_gray.take())?;
            cached_gray = match outcome {
                BuyOutcome::Bought => {
                    bought += 1;
                    self.progress.item_bought(alias_name);
                    None
                }
                BuyOutcome::None => gray,
            };
        }
        Ok((bought, cached_gray))
    }

    /// Locate the alias's icon, click its row's buy button, verify the
    /// modal opened (hash of `buy_confirm` zone before/after), click the
    /// confirm pill. Pre/post hash gates the second click so a missed
    /// first click can't blindly trigger an unrelated buy.
    fn try_buy_visible(
        &mut self,
        alias_name: &'static str,
        cached_gray: Option<GrayImage>,
    ) -> Result<(BuyOutcome, Option<GrayImage>)> {
        if self.stop.load(Ordering::Relaxed) {
            return Ok((BuyOutcome::None, cached_gray));
        }
        // At most one of each type per refresh — skip cheaply without snapshotting.
        if self.bought_types.contains(alias_name) {
            debug!(
                alias = alias_name,
                "type already bought this round, skipping"
            );
            return Ok((BuyOutcome::None, cached_gray));
        }
        let gray = match cached_gray {
            Some(g) => g,
            None => self.snapshot()?,
        };
        let Some(item) = self
            .detector
            .find(&gray, alias_name, self.regions().shop_grid)?
        else {
            return Ok((BuyOutcome::None, Some(gray)));
        };

        // Same physical item at a different scroll position has the same
        // icon-area hash. Computed from the snapshot we already have.
        let icon_hash_val = self
            .detector
            .template_dimensions(alias_name)
            .map(|(tw, th)| icon_hash(&gray, item.x, item.y, tw, th));
        if let Some(h) = icon_hash_val
            && self.attempted_icons.contains(&h)
        {
            debug!(
                alias = alias_name,
                "icon already attempted this round, skipping"
            );
            return Ok((BuyOutcome::None, Some(gray)));
        }

        // buy_column: only X range is used — Y comes from the icon match.
        let buy_column = self
            .config
            .zones
            .buy_column
            .ok_or(Error::ZoneMissing { name: "buy_column" })?;
        let buy_confirm = self.config.zones.buy_confirm.ok_or(Error::ZoneMissing {
            name: "buy_confirm",
        })?;

        let before = strip_hash(&gray, buy_confirm);
        let row_rect = self.buy_column_row_rect(buy_column, item.y, gray.height())?;
        self.clicker.click_local_in_rect(&*self.capture, row_rect)?;
        self.clicker.human_pause();
        if self.stop.load(Ordering::Relaxed) {
            return Ok((BuyOutcome::None, Some(gray)));
        }

        self.clicker
            .pause_ms(self.config.timing.modal_open_pause_ms);
        if self.stop.load(Ordering::Relaxed) {
            return Ok((BuyOutcome::None, Some(gray)));
        }

        // Record before checking the modal: success OR fail, never retry
        // this icon in this round.
        if let Some(h) = icon_hash_val {
            self.attempted_icons.insert(h);
        }

        // Unchanged confirm-zone pixels = modal didn't open (greyed-out
        // buy button or NCC false positive). Bail before clicking
        // confirm into who-knows-what.
        let after_gray = self.snapshot()?;
        let after = strip_hash(&after_gray, buy_confirm);
        if before == after {
            warn!(
                alias = alias_name,
                "buy modal did not open — skipping confirm click"
            );
            return Ok((BuyOutcome::None, Some(after_gray)));
        }

        let confirm_rect = self.zone_to_local_rect(buy_confirm)?;
        self.clicker
            .click_local_in_rect(&*self.capture, confirm_rect)?;
        self.clicker.human_pause();
        self.bought_types.insert(alias_name);
        // Returned None: the modal is still on-screen, useless for the
        // next bottom-strip hash.
        Ok((BuyOutcome::Bought, None))
    }

    fn buy_column_row_rect(&self, column: [f32; 4], item_y: i32, frame_h: u32) -> Result<[i32; 4]> {
        let r = self.capture.rect()?;
        Ok(buy_column_row_rect_for(
            column,
            item_y,
            frame_h,
            r.width,
            r.height,
            self.config.shop.buy_button_y_offset_ratio,
        ))
    }

    fn zone_to_local_rect(&self, zone: [f32; 4]) -> Result<[i32; 4]> {
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

    /// Scroll wheel target = center of `regions.shop_grid` so the wheel
    /// event lands inside the scrollable list. Falls back to window
    /// center if no grid ROI is set.
    fn scroll_point(&self) -> Result<(i32, i32)> {
        let r = self.capture.rect()?;
        let (w, h) = (r.width as f32, r.height as f32);
        let (cx, cy) = match self.config.regions.shop_grid {
            Some([gx, gy, gw, gh]) => (gx + gw / 2.0, gy + gh / 2.0),
            None => (0.5, 0.5),
        };
        Ok(((cx * w).round() as i32, (cy * h).round() as i32))
    }

    fn refresh_shop(&mut self) -> Result<()> {
        let refresh = self
            .config
            .zones
            .refresh
            .ok_or(Error::ZoneMissing { name: "refresh" })?;
        let confirm = self
            .config
            .zones
            .refresh_confirm
            .ok_or(Error::ZoneMissing {
                name: "refresh_confirm",
            })?;

        // Same modal verification as try_buy_visible: a missed refresh
        // click would otherwise have refresh_confirm hit a random item.
        let before_gray = self.snapshot()?;
        let before = strip_hash(&before_gray, confirm);

        info!("clicking refresh");
        let refresh_rect = self.zone_to_local_rect(refresh)?;
        self.clicker
            .click_local_in_rect(&*self.capture, refresh_rect)?;
        self.clicker.human_pause();
        if self.stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        self.clicker
            .pause_ms(self.config.timing.modal_open_pause_ms);
        if self.stop.load(Ordering::Relaxed) {
            return Ok(());
        }

        let after_gray = self.snapshot()?;
        let after = strip_hash(&after_gray, confirm);
        if before == after {
            warn!("refresh modal did not open — skipping confirm click (this round won't refresh)");
            return Ok(());
        }

        let confirm_rect = self.zone_to_local_rect(confirm)?;
        self.clicker
            .click_local_in_rect(&*self.capture, confirm_rect)?;
        self.clicker.human_pause();
        Ok(())
    }

    fn snapshot(&self) -> Result<GrayImage> {
        // WGC captures are atomic, so we don't need to bracket every
        // snapshot with a rect() check — drift is caught up-front by
        // check_size_stable in run_inner.
        self.capture.snapshot_gray()
    }

    fn regions(&self) -> &RegionsConfig {
        &self.config.regions
    }
}

#[derive(Debug, Clone, Copy)]
enum BuyOutcome {
    Bought,
    None,
}

/// Pure stop-condition check (testable without standing up real IO).
/// Returns the first satisfied condition in fixed priority order
/// (duration → mystic → covenant) so the reason string is deterministic.
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
    None
}

/// Hash a region around a matched item icon, used to recognise the same
/// physical item across scroll positions. Region: 3× template width, 1×
/// template height, centred around the icon. Sampled every 4 px so
/// sub-pixel scroll drift doesn't flip the hash.
///
/// FxHasher instead of std SipHash: same equality semantics on our
/// non-adversarial pixel data, ~2–3× faster on inputs this small.
fn icon_hash(gray: &GrayImage, cx: i32, cy: i32, tw: u32, th: u32) -> u64 {
    use std::hash::Hasher;
    let (sw, sh) = (gray.width(), gray.height());
    let region_w = (tw as i32) * 3;
    let region_h = th as i32;
    let anchor_x = cx - (tw as i32);
    let x0 = (anchor_x - region_w / 2).max(0) as u32;
    let y0 = (cy - region_h / 2).max(0) as u32;
    let x1 = x0.saturating_add(region_w as u32).min(sw);
    let y1 = y0.saturating_add(region_h as u32).min(sh);
    let mut hasher = rustc_hash::FxHasher::default();
    for yy in (y0..y1).step_by(4) {
        for xx in (x0..x1).step_by(4) {
            hasher.write_u8(gray.get_pixel(xx, yy)[0]);
        }
    }
    hasher.finish()
}

/// Hash a sub-rectangle of a gray frame, sampled every 4th pixel.
/// Detects "frame didn't move" (top/bottom of scroll, modal settle).
/// Same hasher rationale as [`icon_hash`].
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

    /// Scripts gray frames + a fixed window rect. Returns `WindowGone`
    /// once the queue empties so tests can observe reattach or graceful
    /// exit.
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

    /// Event log exposed via Arc<Mutex<…>> so tests can inspect after
    /// the boxed input has been moved into a runner.
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
            stop_after_minutes: minutes,
            stop_when_mystic_medals: mystic,
            stop_when_covenants: covenants,
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
        let r = buy_column_row_rect_for([0.8, 0.0, 0.1, 1.0], 500, 1000, 100, 1000, 0.0);
        assert_eq!(r[0], 80);
        assert!(r[1] >= 478 && r[1] <= 482);
        assert_eq!(r[2], 10);
        assert_eq!(r[3], 40);
    }

    #[test]
    fn buy_column_rect_shifts_down_by_y_offset_ratio() {
        let no_off = buy_column_row_rect_for([0.8, 0.0, 0.1, 1.0], 500, 1000, 100, 1000, 0.0);
        let with_off = buy_column_row_rect_for([0.8, 0.0, 0.1, 1.0], 500, 1000, 100, 1000, 0.04);
        assert_eq!(with_off[1] - no_off[1], 40);
    }

    #[test]
    fn buy_column_rect_clamps_band_to_window_bounds() {
        let r = buy_column_row_rect_for([0.8, 0.0, 0.1, 1.0], 0, 1000, 100, 1000, 0.0);
        assert!(r[1] >= 0);
        let r = buy_column_row_rect_for([0.8, 0.0, 0.1, 1.0], 1000, 1000, 100, 1000, 0.0);
        assert!(i64::from(r[1] + r[3]) <= 1000);
    }

    #[test]
    fn buy_column_rect_y_h_of_zone_are_ignored() {
        let a = buy_column_row_rect_for([0.8, 0.1, 0.1, 0.2], 500, 1000, 100, 1000, 0.0);
        let b = buy_column_row_rect_for([0.8, 0.9, 0.1, 0.05], 500, 1000, 100, 1000, 0.0);
        assert_eq!(a, b);
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
    fn icon_hash_deterministic() {
        let img = checker(200, 200, 8);
        let a = icon_hash(&img, 100, 100, 50, 50);
        let b = icon_hash(&img, 100, 100, 50, 50);
        assert_eq!(a, b);
    }

    #[test]
    fn icon_hash_differs_for_different_content() {
        let a_img = checker(200, 200, 8);
        let b_img = solid_gray(200, 200, 128);
        let a = icon_hash(&a_img, 100, 100, 50, 50);
        let b = icon_hash(&b_img, 100, 100, 50, 50);
        assert_ne!(a, b);
    }

    #[test]
    fn icon_hash_handles_clamped_region_near_frame_edge() {
        // Match centre near left edge: 3×template region would extend negative.
        let img = checker(50, 50, 4);
        let h = icon_hash(&img, 5, 25, 30, 30);
        assert_eq!(h, icon_hash(&img, 5, 25, 30, 30));
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
}
