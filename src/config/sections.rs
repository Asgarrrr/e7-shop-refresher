//! TOML schema types — one struct per `[section]`. Defaults only;
//! cross-field validation lives in `validate.rs`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WindowConfig {
    pub title_contains: String,
    /// Executable name filter (e.g. "stove") to disambiguate when
    /// multiple windows share a title fragment.
    #[serde(default)]
    pub process_name: Option<String>,
    /// Window size at which the user's templates were cropped (auto-
    /// written by the GUI's Crop & Save). Detector resamples when the
    /// live window has a different size.
    pub base_resolution: [u32; 2],
    /// Drift tolerated before erroring out — absorbs taskbar visibility
    /// / DPI rounding jitter.
    pub resize_tolerance_px: u32,
    /// Off by default — user calibrates at native size, no resampling,
    /// no fighting Windows decorations / DPI rounding.
    pub auto_resize: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title_contains: "Epic Seven".into(),
            process_name: None,
            base_resolution: [1920, 1080],
            resize_tolerance_px: 4,
            auto_resize: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ShopConfig {
    /// 0 = unlimited (rely on other stop conditions or manual Stop).
    pub max_refreshes: u32,
    pub buy_mystic_medals: bool,
    pub buy_covenant: bool,
    /// 6 items per refresh, 4 visible at once — 1 scroll reveals the
    /// last 2. Bump to 2 if your setup undershoots.
    pub max_scrolls_per_round: u32,
    /// Fraction of window height between an item icon's centre and its
    /// row's buy button. 0.04 ≈ 44 px at 1080p.
    #[serde(default = "default_buy_button_y_offset_ratio")]
    pub buy_button_y_offset_ratio: f32,
    /// Vertical thickness of the buy-button click band as a fraction of
    /// window height. Sets the click target's height; X is taken from
    /// `zones.buy_column`.
    #[serde(default = "default_buy_button_band_h_ratio")]
    pub buy_button_band_h_ratio: f32,
    /// User-placed reference Y for Buy-click calibration, as a fraction of
    /// window height. Pure UI affordance — the runtime uses the detected
    /// icon Y, never this — but it persists the user's "this is what a
    /// row looks like" so the box (line + offset) renders on the same
    /// spot every session. Default ≈ shop-grid centre.
    #[serde(default = "default_buy_calibration_line_y_ratio")]
    pub buy_calibration_line_y_ratio: f32,
    /// 0 = no limit. Checked at round boundary — may overshoot by one round.
    #[serde(default)]
    pub stop_after_minutes: u32,
    /// 0 = no cap.
    #[serde(default)]
    pub stop_when_mystic_medals: u32,
    /// 0 = no cap.
    #[serde(default)]
    pub stop_when_covenants: u32,
    /// 0 = no cap.
    #[serde(default)]
    pub stop_when_gold_spent: u32,
    /// Never fires on manual Stop — pressing the button means the user
    /// is at the machine.
    #[serde(default)]
    pub sleep_when_done: bool,
}

fn default_buy_button_y_offset_ratio() -> f32 {
    crate::layout::BUY_BUTTON_Y_OFFSET
}

fn default_buy_button_band_h_ratio() -> f32 {
    crate::shop::BUY_COLUMN_ROW_BAND_RATIO
}

fn default_buy_calibration_line_y_ratio() -> f32 {
    // Kept in sync with the bundled config.toml — picked empirically
    // so a fresh-from-source run and a config-file run land in the
    // same place.
    0.65
}

impl Default for ShopConfig {
    fn default() -> Self {
        Self {
            max_refreshes: 30,
            buy_mystic_medals: true,
            buy_covenant: true,
            max_scrolls_per_round: 1,
            buy_button_y_offset_ratio: default_buy_button_y_offset_ratio(),
            buy_button_band_h_ratio: default_buy_button_band_h_ratio(),
            buy_calibration_line_y_ratio: default_buy_calibration_line_y_ratio(),
            stop_after_minutes: 0,
            stop_when_mystic_medals: 0,
            stop_when_covenants: 0,
            stop_when_gold_spent: 0,
            sleep_when_done: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TimingConfig {
    /// Log-normal mean (ms) of inter-click delay.
    pub click_delay_mean_ms: f64,
    /// Log-space sigma (dispersion).
    pub click_delay_sigma: f64,
    pub click_delay_min_ms: u64,
    pub click_delay_max_ms: u64,

    pub move_steps_min: u32,
    pub move_steps_max: u32,
    pub move_step_min_ms: u64,
    pub move_step_max_ms: u64,
    /// Pause after the cursor reaches the target, before pressing.
    pub move_to_click_min_ms: u64,
    pub move_to_click_max_ms: u64,
    /// Perpendicular arc strength of the mouse path (px).
    pub move_curve_amplitude_px: f32,

    /// Rayleigh-distributed click jitter.
    pub jitter_radius_px: f32,

    pub inter_round_min_ms: u64,
    pub inter_round_max_ms: u64,
    /// Every N rounds, take a longer break. 0 disables.
    pub long_pause_every_n: u32,
    pub long_pause_min_ms: u64,
    pub long_pause_max_ms: u64,

    /// Mouse-wheel lines per scroll. Positive scrolls down.
    pub scroll_amount: i32,
    pub scroll_pause_ms: u64,

    /// Must outlast the modal slide-in animation; anything beyond is
    /// dead time.
    #[serde(default = "default_modal_open_pause_ms")]
    pub modal_open_pause_ms: u64,

    /// Cooperative mode: pause the bot when the user touches mouse /
    /// keyboard, resume after this many ms of idle. 0 disables — bot
    /// fights the user for the cursor. 1500 ms ≈ enough to send a
    /// Discord message without the bot stealing input mid-sentence.
    #[serde(default = "default_cooperative_idle_ms")]
    pub cooperative_idle_ms: u64,
}

fn default_modal_open_pause_ms() -> u64 {
    220
}

fn default_cooperative_idle_ms() -> u64 {
    1500
}

impl Default for TimingConfig {
    fn default() -> Self {
        Self {
            click_delay_mean_ms: 250.0,
            click_delay_sigma: 0.30,
            click_delay_min_ms: 120,
            click_delay_max_ms: 700,

            move_steps_min: 5,
            move_steps_max: 10,
            move_step_min_ms: 3,
            move_step_max_ms: 8,
            move_to_click_min_ms: 20,
            move_to_click_max_ms: 55,
            move_curve_amplitude_px: 3.0,

            jitter_radius_px: 3.0,

            inter_round_min_ms: 600,
            inter_round_max_ms: 1800,
            long_pause_every_n: 10,
            long_pause_min_ms: 5000,
            long_pause_max_ms: 12000,

            scroll_amount: 8,
            scroll_pause_ms: 250,
            modal_open_pause_ms: default_modal_open_pause_ms(),
            cooperative_idle_ms: default_cooperative_idle_ms(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MatchingConfig {
    /// NCC threshold in (0, 1].
    pub threshold: f32,
    /// Required gap between best and second-best peak for an unambiguous match.
    pub margin: f32,
    /// Multipliers applied on top of the global window-size scale.
    pub extra_scales: Vec<f32>,
    /// Setup-tab live preview cadence in milliseconds. Drives capture +
    /// NCC frequency only — the bot loop is not gated by this field.
    #[serde(default = "default_preview_refresh_ms")]
    pub preview_refresh_ms: u32,
}

fn default_preview_refresh_ms() -> u32 {
    500
}

impl Default for MatchingConfig {
    fn default() -> Self {
        Self {
            threshold: 0.90,
            margin: 0.05,
            // Native scale only — with auto_resize=false, global_scale
            // collapses to 1.0 and 0.97/1.03 just burn NCC time.
            extra_scales: vec![1.0],
            preview_refresh_ms: default_preview_refresh_ms(),
        }
    }
}

/// `None` = use the bundled value from `crate::layout`. Setting a
/// value shadows the bundled default — useful when the bundled layout
/// misses on an unusual resolution / UI mod.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RegionsConfig {
    pub shop_grid: Option<[f32; 4]>,
}

/// `None` = bundled default. Runner picks a uniform random point
/// inside the resolved zone — built-in jitter without the Rayleigh
/// radius the NCC-matched clicks use.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ZonesConfig {
    pub refresh: Option<[f32; 4]>,
    pub refresh_confirm: Option<[f32; 4]>,
    pub buy_confirm: Option<[f32; 4]>,
    /// Only the X range is honoured at click time — Y comes from the
    /// matched item icon's Y + `shop.buy_button_y_offset_ratio`.
    pub buy_column: Option<[f32; 4]>,
}

/// All-`#[serde(default)]` so stale on-disk configs (e.g. listing
/// legacy `anchor_shop` / `back_arrow` / `refresh_pill` / `buy_pill`
/// from before the shop-detection removal) load cleanly — serde
/// ignores the extras, fills in defaults for whatever's missing.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TemplatesConfig {
    #[serde(default = "default_templates_dir")]
    pub dir: PathBuf,
    #[serde(default = "default_mystic_medal")]
    pub mystic_medal: String,
    #[serde(default = "default_covenant")]
    pub covenant: String,
}

impl Default for TemplatesConfig {
    fn default() -> Self {
        Self {
            dir: default_templates_dir(),
            mystic_medal: default_mystic_medal(),
            covenant: default_covenant(),
        }
    }
}

fn default_templates_dir() -> PathBuf {
    "templates".into()
}
fn default_mystic_medal() -> String {
    "mystic_medal.png".into()
}
fn default_covenant() -> String {
    "covenant.png".into()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct NotificationsConfig {
    /// Raw on-disk value — use [`Self::webhook_url`] when consuming it
    /// so whitespace trim happens in one place.
    #[serde(default)]
    pub discord_webhook_url: String,
}

impl NotificationsConfig {
    /// Trimmed view — empty = disabled. Single source of truth for the
    /// three callers (validation, test button, post-run dispatch).
    pub fn webhook_url(&self) -> &str {
        self.discord_webhook_url.trim()
    }
}
