use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use enigo::{Axis, Button, Coordinate, Direction, Enigo, Mouse, Settings};
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use rand_distr::{Distribution, LogNormal};

use crate::capture::Capture;
use crate::config::TimingConfig;
use crate::error::{Error, Result};

/// Click / scroll / pause surface for `ShopRunner`. `&dyn Capture` (not
/// generic) so the trait stays object-safe.
pub trait Input: Send {
    fn click_local_in_rect(&mut self, capture: &dyn Capture, rect: [i32; 4]) -> Result<()>;
    fn scroll_at(
        &mut self,
        capture: &dyn Capture,
        local_x: i32,
        local_y: i32,
        lines: i32,
    ) -> Result<()>;
    fn human_pause(&mut self);
    fn pause_ms(&self, ms: u64);
    fn inter_round_pause(&mut self);
    fn long_human_pause(&mut self);
}

/// Sleep chunk for interruptible waits. Smaller = more responsive to Stop
/// at the cost of polling overhead.
const SLEEP_CHUNK_MS: u64 = 60;

/// Wait after `SetForegroundWindow` so the OS commits the focus change
/// and the game finishes painting. Clicks issued sooner race the
/// activation and land in dead time.
const FOREGROUND_SETTLE_MS: u64 = 150;

/// Cooperative mode poll cadence. Tight enough that resume feels
/// immediate; loose enough not to burn CPU when the user is typing.
const COOPERATIVE_POLL_MS: u64 = 100;

/// Tolerance between `GetLastInputInfo` (GetTickCount domain) and the
/// bot's own Instant readings. Without it, our own SendInput can look
/// like user activity due to clock-source jitter.
const BOT_INPUT_CLASSIFY_MARGIN_MS: u64 = 30;

pub struct Clicker {
    enigo: Enigo,
    timing: TimingConfig,
    rng: SmallRng,
    stop: Arc<AtomicBool>,
    /// Last time the bot itself called into enigo. Lets us tell apart
    /// "user moved the mouse" from "we moved the mouse" when reading
    /// `GetLastInputInfo`.
    last_bot_input_at: Instant,
}

impl Clicker {
    pub fn new(timing: TimingConfig, stop: Arc<AtomicBool>) -> Result<Self> {
        let enigo = Enigo::new(&Settings::default())?;
        let seed: u64 = rand::rng().random();
        Ok(Self {
            enigo,
            timing,
            rng: SmallRng::seed_from_u64(seed),
            stop,
            last_bot_input_at: Instant::now(),
        })
    }

    /// Update right after every SendInput so the user-activity check
    /// can subtract out our own actions.
    fn mark_bot_input(&mut self) {
        self.last_bot_input_at = Instant::now();
    }

    /// `Some(ms)` if the last OS-level input is more recent than our own
    /// last enigo call (user moved/typed). `None` if the last input was
    /// the bot itself.
    fn ms_since_user_input(&self) -> Option<u64> {
        use windows::Win32::System::SystemInformation::GetTickCount;
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
            return None;
        }
        let now_tick = unsafe { GetTickCount() };
        let since_input_ms = now_tick.wrapping_sub(info.dwTime) as u64;
        let since_bot_ms = self.last_bot_input_at.elapsed().as_millis() as u64;
        // User input is more recent than our own last action.
        if since_input_ms + BOT_INPUT_CLASSIFY_MARGIN_MS < since_bot_ms {
            Some(since_input_ms)
        } else {
            None
        }
    }

    /// Cooperative pause: block until the user has been idle for at
    /// least `cooperative_idle_ms`. No-op if disabled (0) or already
    /// idle. Respects Stop so the user can abort.
    fn wait_for_user_idle(&self) {
        let threshold_ms = self.timing.cooperative_idle_ms;
        if threshold_ms == 0 {
            return;
        }
        let chunk = Duration::from_millis(COOPERATIVE_POLL_MS);
        let mut waited_for_user = false;
        loop {
            if self.stopped() {
                return;
            }
            match self.ms_since_user_input() {
                Some(ms) if ms < threshold_ms => {
                    if !waited_for_user {
                        tracing::debug!(
                            since_user_input_ms = ms,
                            threshold_ms,
                            "user is active — pausing"
                        );
                        waited_for_user = true;
                    }
                    thread::sleep(chunk);
                }
                _ => {
                    if waited_for_user {
                        tracing::debug!("user idle — resuming");
                    }
                    return;
                }
            }
        }
    }

    /// Checked every click — no TTL cache. Paid currency is on the line
    /// and `GetForegroundWindow` is sub-microsecond; the previous half-
    /// second cache let alt-tabs between check and click steal the input.
    fn ensure_foreground(&self, capture: &dyn Capture) -> Result<()> {
        // HWND check first so a dead window surfaces as WindowHandleInvalid,
        // not the misleading "another app is blocking focus".
        if !capture.hwnd_is_valid() {
            return Err(Error::WindowHandleInvalid);
        }
        if capture.is_foreground() {
            return Ok(());
        }
        if !capture.try_bring_foreground() {
            // Distinguish "window died between the two checks" from
            // "focus locked by another app".
            if !capture.hwnd_is_valid() {
                return Err(Error::WindowHandleInvalid);
            }
            return Err(Error::WindowNotForeground);
        }
        let chunk = Duration::from_millis(SLEEP_CHUNK_MS);
        let until = Instant::now() + Duration::from_millis(FOREGROUND_SETTLE_MS);
        while Instant::now() < until {
            if self.stopped() {
                return Ok(());
            }
            let remaining = until.saturating_duration_since(Instant::now());
            thread::sleep(chunk.min(remaining));
        }
        Ok(())
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn sleep_interruptible(&self, dur: Duration) {
        let chunk = Duration::from_millis(SLEEP_CHUNK_MS);
        let deadline = Instant::now() + dur;
        while Instant::now() < deadline {
            if self.stopped() {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            thread::sleep(chunk.min(remaining));
        }
    }

    /// Click at a window-local pixel coordinate. Window position is
    /// resolved at click time, so the user can move the window between
    /// actions and clicks still land on the right spot.
    pub fn click_local(&mut self, capture: &dyn Capture, local_x: i32, local_y: i32) -> Result<()> {
        let (jx, jy) = self.jitter_target(local_x, local_y);
        self.click_at_local_no_jitter(capture, jx, jy)
    }

    /// Click a uniform random point inside `rect`. The zone supplies the
    /// spread, so we skip the Rayleigh jitter — adding it on top would
    /// occasionally push the click outside the user-drawn zone.
    pub fn click_local_in_rect(&mut self, capture: &dyn Capture, rect: [i32; 4]) -> Result<()> {
        let [x, y, w, h] = rect;
        let w = w.max(1);
        let h = h.max(1);
        let dx = self.rng.random_range(0..w);
        let dy = self.rng.random_range(0..h);
        self.click_at_local_no_jitter(capture, x + dx, y + dy)
    }

    fn click_at_local_no_jitter(
        &mut self,
        capture: &dyn Capture,
        local_x: i32,
        local_y: i32,
    ) -> Result<()> {
        // Before foreground steal — yanking focus while the user types
        // would feel just as bad as yanking the mouse.
        self.wait_for_user_idle();
        self.ensure_foreground(capture)?;
        let (tx, ty) = capture.local_to_screen(local_x, local_y)?;
        let start = self.enigo.location().unwrap_or((tx, ty));
        tracing::debug!(
            local_x,
            local_y,
            screen_x = tx,
            screen_y = ty,
            start_x = start.0,
            start_y = start.1,
            "click path computed"
        );

        self.move_human(start, (tx, ty))?;
        let landed = self.enigo.location().unwrap_or((tx, ty));
        tracing::debug!(
            expected_x = tx,
            expected_y = ty,
            landed_x = landed.0,
            landed_y = landed.1,
            delta_x = landed.0 - tx,
            delta_y = landed.1 - ty,
            "move_human finished"
        );

        let dwell = self.uniform_ms(
            self.timing.move_to_click_min_ms,
            self.timing.move_to_click_max_ms,
        );
        self.sleep_interruptible(dwell);
        if self.stopped() {
            return Ok(());
        }
        self.enigo.button(Button::Left, Direction::Click)?;
        self.mark_bot_input();
        tracing::debug!("button click sent");
        Ok(())
    }

    /// Log-normal inter-click delay clamped to `[click_delay_min_ms,
    /// click_delay_max_ms]`.
    pub fn human_pause(&mut self) {
        let t = &self.timing;
        let sample = log_normal_ms(&mut self.rng, t.click_delay_mean_ms, t.click_delay_sigma);
        let clamped = sample.clamp(t.click_delay_min_ms as f64, t.click_delay_max_ms as f64);
        self.sleep_interruptible(Duration::from_millis(clamped as u64));
    }

    /// Positive `lines` scrolls down, negative scrolls up.
    pub fn scroll_at(
        &mut self,
        capture: &dyn Capture,
        local_x: i32,
        local_y: i32,
        lines: i32,
    ) -> Result<()> {
        self.wait_for_user_idle();
        self.ensure_foreground(capture)?;
        let (sx, sy) = capture.local_to_screen(local_x, local_y)?;
        self.enigo.move_mouse(sx, sy, Coordinate::Abs)?;
        self.mark_bot_input();
        self.sleep_interruptible(Duration::from_millis(40));
        if self.stopped() {
            return Ok(());
        }
        self.enigo.scroll(lines, Axis::Vertical)?;
        self.mark_bot_input();
        Ok(())
    }

    pub fn pause_ms(&self, ms: u64) {
        self.sleep_interruptible(Duration::from_millis(ms));
    }

    pub fn inter_round_pause(&mut self) {
        let ms = self.uniform_ms(
            self.timing.inter_round_min_ms,
            self.timing.inter_round_max_ms,
        );
        self.sleep_interruptible(ms);
    }

    /// "Looking at screen" pause — can be up to long_pause_max_ms
    /// (default 25s), so Stop responsiveness matters here.
    pub fn long_human_pause(&mut self) {
        let ms = self.uniform_ms(self.timing.long_pause_min_ms, self.timing.long_pause_max_ms);
        self.sleep_interruptible(ms);
    }

    fn move_human(&mut self, start: (i32, i32), target: (i32, i32)) -> Result<()> {
        let steps = self
            .rng
            .random_range(self.timing.move_steps_min..=self.timing.move_steps_max)
            .max(1);

        let (sx, sy) = (start.0 as f32, start.1 as f32);
        let (tx, ty) = (target.0 as f32, target.1 as f32);
        let (dx, dy) = (tx - sx, ty - sy);
        let dist = (dx * dx + dy * dy).sqrt().max(1.0);

        // Unit perpendicular vector to (start → target), used for arcing.
        let (px, py) = (-dy / dist, dx / dist);
        let amp_max = self.timing.move_curve_amplitude_px;
        let arc = if amp_max > 0.0 {
            self.rng.random_range(-amp_max..=amp_max)
        } else {
            0.0
        };

        for i in 1..=steps {
            if self.stopped() {
                return Ok(());
            }
            let t = i as f32 / steps as f32;
            // Ease-out cubic + sin bump on the perpendicular: faster start,
            // gentle landing, max arc in the middle.
            let eased = 1.0 - (1.0 - t).powi(3);
            let bump = (eased * std::f32::consts::PI).sin();
            let x = sx + dx * eased + px * arc * bump;
            let y = sy + dy * eased + py * arc * bump;
            self.enigo
                .move_mouse(x.round() as i32, y.round() as i32, Coordinate::Abs)?;
            self.mark_bot_input();
            let step_ms =
                self.uniform_ms(self.timing.move_step_min_ms, self.timing.move_step_max_ms);
            self.sleep_interruptible(step_ms);
        }
        Ok(())
    }

    /// Rayleigh jitter around the target: σ = r/√2 so 95% of points fall
    /// inside r×2. Soft circular blob instead of a hard square grid.
    fn jitter_target(&mut self, x: i32, y: i32) -> (i32, i32) {
        let r = self.timing.jitter_radius_px;
        if r <= 0.0 {
            return (x, y);
        }
        let sigma = r / std::f32::consts::SQRT_2;
        let u: f32 = self.rng.random_range(1e-6..1.0);
        let radius = sigma * (-2.0 * u.ln()).sqrt();
        let angle: f32 = self.rng.random_range(0.0..std::f32::consts::TAU);
        let dx = (radius * angle.cos()).round() as i32;
        let dy = (radius * angle.sin()).round() as i32;
        (x + dx, y + dy)
    }

    fn uniform_ms(&mut self, min: u64, max: u64) -> Duration {
        let ms = if min >= max {
            min
        } else {
            self.rng.random_range(min..=max)
        };
        Duration::from_millis(ms)
    }
}

impl Input for Clicker {
    fn click_local_in_rect(&mut self, capture: &dyn Capture, rect: [i32; 4]) -> Result<()> {
        Clicker::click_local_in_rect(self, capture, rect)
    }
    fn scroll_at(
        &mut self,
        capture: &dyn Capture,
        local_x: i32,
        local_y: i32,
        lines: i32,
    ) -> Result<()> {
        Clicker::scroll_at(self, capture, local_x, local_y, lines)
    }
    fn human_pause(&mut self) {
        Clicker::human_pause(self)
    }
    fn pause_ms(&self, ms: u64) {
        Clicker::pause_ms(self, ms)
    }
    fn inter_round_pause(&mut self) {
        Clicker::inter_round_pause(self)
    }
    fn long_human_pause(&mut self) {
        Clicker::long_human_pause(self)
    }
}

/// Log-normal sample whose *real* mean matches `mean_ms`. The config's
/// `click_delay_sigma` is the dispersion of the underlying normal; we
/// solve for log-space mu so the natural-space mean lines up:
/// `E[X] = exp(mu + sigma²/2)`.
fn log_normal_ms(rng: &mut SmallRng, mean_ms: f64, log_sigma: f64) -> f64 {
    let mu = mean_ms.ln() - 0.5 * log_sigma * log_sigma;
    LogNormal::new(mu, log_sigma)
        .expect("validated: click_delay_sigma is finite and > 0")
        .sample(rng)
}
