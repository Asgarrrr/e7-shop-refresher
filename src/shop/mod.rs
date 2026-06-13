use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use image::GrayImage;
use tracing::{debug, info, warn};

use crate::capture::Capture;
use crate::color_check::ColorVerifier;
use crate::config::{Config, ShopConfig};
use crate::detector::{Detector, alias};
use crate::error::{Error, Result};
use crate::input::Input;

mod round;
mod scan;
mod stop;

#[cfg(test)]
mod test_support;

pub use round::{BUY_COLUMN_ROW_BAND_RATIO, buy_column_row_rect_for};
pub(crate) use scan::{crop_icon_patch, scan_shop_raw};
pub use stop::{COVENANT_DROP_PER_SLOT, MYSTIC_DROP_PER_SLOT, SHOP_SLOTS_PER_REFRESH, prices};
pub(crate) use stop::{gold_spent_for, stop_condition_for};

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
    pub(super) capture: Arc<dyn Capture>,
    pub(super) detector: Arc<Detector>,
    pub(super) color_check: ColorVerifier,
    pub(super) clicker: Box<dyn Input>,
    pub(super) config: Config,
    /// Runner re-reads at every round boundary so mid-run UI edits to
    /// `[shop]` apply at the next boundary. Timing/matching stay
    /// captured by value in `config` since they're in use right now.
    pub(super) live_shop: Arc<RwLock<ShopConfig>>,
    pub(super) stop: Arc<AtomicBool>,
    pub(super) progress: Arc<dyn ProgressSink>,
    pub(super) enabled_targets: Vec<&'static str>,
    /// Shop holds one of each per refresh — a later match for the same
    /// alias is a re-detection of the sold-out item. Also dedupes a
    /// failed buy attempt (modal didn't open) so subsequent scroll
    /// views don't re-click an inactive button. Cleared per round.
    pub(super) bought_types: HashSet<&'static str>,
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

    pub(super) fn run_inner(&mut self, started: Instant) -> Result<RunInnerOutcome> {
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

    pub(super) fn snapshot(&self) -> Result<GrayImage> {
        self.capture.snapshot_gray()
    }
}

#[derive(Debug, Clone, Copy)]
struct RoundOutcome {
    bought: u32,
    refreshed: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct RunInnerOutcome {
    /// `None` = clean exit, `Some(_)` = stop condition fired.
    pub(super) reason: Option<&'static str>,
    pub(super) refreshes: u32,
}

const MAX_CONSECUTIVE_FAILURES: u32 = 5;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::WindowRect;
    use crate::config::Config;
    use crate::detector::Detector;
    use crate::input::Input;

    use super::test_support::{
        FakeCapture, FakeEvent, FakeInput, REFRESH_CONFIRM, SHOP_GRID, gray_frame, paint_zone,
        runner_for_loop_tests,
    };

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

    #[test]
    fn run_inner_bails_with_too_many_failures_when_capture_keeps_dying() {
        let (mut runner, _events) = runner_for_loop_tests(vec![]);
        let err = runner.run_inner(std::time::Instant::now()).unwrap_err();
        assert!(matches!(err, Error::TooManyFailures(n) if n == 5));
    }

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
}
