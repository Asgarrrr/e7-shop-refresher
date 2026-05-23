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
    /// Window size at which the templates were cropped. Auto-written by
    /// the GUI's Crop & Save. The detector uses this as the scaling
    /// reference: if the live window is at a different size, templates
    /// are resampled to match.
    pub base_resolution: [u32; 2],
    /// Pixels of size drift tolerated before erroring out. Absorbs
    /// taskbar visibility / DPI rounding jitter.
    pub resize_tolerance_px: u32,
    /// Force the window to `base_resolution` at startup via `SetWindowPos`.
    /// Off by default: the user calibrates at their native size, no
    /// resampling, and we don't fight Windows decorations / DPI rounding.
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
    /// row's buy button. E7's layout puts the button below the icon.
    /// 0.04 ≈ 44 px at 1080p.
    #[serde(default = "default_buy_button_y_offset_ratio")]
    pub buy_button_y_offset_ratio: f32,
    /// 0 = no time limit. Checked at every round boundary, so may
    /// overshoot by up to one round.
    #[serde(default)]
    pub stop_after_minutes: u32,
    /// 0 = no cap.
    #[serde(default)]
    pub stop_when_mystic_medals: u32,
    /// 0 = no cap.
    #[serde(default)]
    pub stop_when_covenants: u32,
    /// Suspend the PC when a stop condition fires. Never on manual Stop
    /// — pressing the button means the user is at the machine.
    #[serde(default)]
    pub sleep_when_done: bool,
}

fn default_buy_button_y_offset_ratio() -> f32 {
    0.04
}

impl Default for ShopConfig {
    fn default() -> Self {
        Self {
            max_refreshes: 30,
            buy_mystic_medals: true,
            buy_covenant: true,
            max_scrolls_per_round: 1,
            buy_button_y_offset_ratio: default_buy_button_y_offset_ratio(),
            stop_after_minutes: 0,
            stop_when_mystic_medals: 0,
            stop_when_covenants: 0,
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

    pub anchor_timeout_ms: u64,
    pub poll_interval_ms: u64,

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

    /// Pause after click before hashing the confirm zone. Must outlast
    /// the modal slide-in animation; anything beyond is dead time.
    #[serde(default = "default_modal_open_pause_ms")]
    pub modal_open_pause_ms: u64,
}

fn default_modal_open_pause_ms() -> u64 {
    220
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

            anchor_timeout_ms: 5000,
            poll_interval_ms: 150,

            jitter_radius_px: 3.0,

            inter_round_min_ms: 600,
            inter_round_max_ms: 1800,
            long_pause_every_n: 10,
            long_pause_min_ms: 5000,
            long_pause_max_ms: 12000,

            scroll_amount: 8,
            scroll_pause_ms: 250,
            modal_open_pause_ms: default_modal_open_pause_ms(),
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
}

impl Default for MatchingConfig {
    fn default() -> Self {
        Self {
            threshold: 0.90,
            margin: 0.05,
            // Native scale only. With auto_resize=false (default), the
            // global scale collapses to 1.0 and adding 0.97/1.03 just
            // burns NCC time for no benefit.
            extra_scales: vec![1.0],
        }
    }
}

/// `[x, y, w, h]` ratios in [0, 1]. `None` = search the whole window.
/// Only the two areas the bot still template-matches need a ROI;
/// buttons are handled via `[zones]`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RegionsConfig {
    pub shop_grid: Option<[f32; 4]>,
    pub anchor_shop: Option<[f32; 4]>,
}

/// Click targets for fixed-position buttons. The bot picks a uniform
/// random point inside the zone — faster and more robust than NCC for
/// things that don't move. `[x, y, w, h]` ratios in [0, 1].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ZonesConfig {
    pub refresh: Option<[f32; 4]>,
    pub refresh_confirm: Option<[f32; 4]>,
    pub buy_confirm: Option<[f32; 4]>,
    /// Only the X range is used — Y is supplied at click time from the
    /// matched item icon.
    pub buy_column: Option<[f32; 4]>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TemplatesConfig {
    pub dir: PathBuf,
    pub anchor_shop: String,
    pub mystic_medal: String,
    pub covenant: String,
}
