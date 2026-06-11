//! Cross-field validation. `validate_all` is the entry point used by
//! `Config::load`. Regions / zones are user overrides for the bundled
//! `crate::layout` defaults — when present they're validated as proper
//! ratio rects, when absent the runtime falls back to the bundled
//! constants so nothing else needs to check.

use super::sections::{
    MatchingConfig, NotificationsConfig, RegionsConfig, ShopConfig, TimingConfig, WindowConfig,
    ZonesConfig,
};
use super::{CONFIG_VERSION, Config};
use crate::error::{Error, Result};

pub(super) fn validate_all(cfg: &Config) -> Result<()> {
    validate_version(cfg.version)?;
    validate_window(&cfg.window)?;
    validate_shop(&cfg.shop)?;
    validate_matching(&cfg.matching)?;
    validate_timing(&cfg.timing)?;
    validate_regions(&cfg.regions)?;
    validate_zones(&cfg.zones)?;
    validate_notifications(&cfg.notifications)?;
    Ok(())
}

fn validate_version(version: u32) -> Result<()> {
    if version != CONFIG_VERSION {
        return Err(Error::ConfigInvalid(format!(
            "config version mismatch: expected {CONFIG_VERSION}, got {version}"
        )));
    }
    Ok(())
}

fn validate_window(w: &WindowConfig) -> Result<()> {
    if w.title_contains.trim().is_empty() {
        return Err(Error::ConfigInvalid(
            "window.title_contains must not be empty".into(),
        ));
    }
    let [bw, bh] = w.base_resolution;
    if bw == 0 || bh == 0 {
        return Err(Error::ConfigInvalid(
            "window.base_resolution must be positive".into(),
        ));
    }
    Ok(())
}

fn validate_shop(s: &ShopConfig) -> Result<()> {
    if !(0.0..=1.0).contains(&s.buy_calibration_line_y_ratio) {
        return Err(Error::ConfigInvalid(
            "shop.buy_calibration_line_y_ratio must be in [0, 1]".into(),
        ));
    }
    // Per-alias stop counts paired with the matching buy flag off can
    // never trip — warn but don't fail, otherwise the GUI (which can
    // toggle the Buy checkbox without resetting the stop count) bricks
    // its own next launch.
    if s.stop_when_mystic_medals > 0 && !s.buy_mystic_medals {
        tracing::warn!(
            "shop.stop_when_mystic_medals > 0 but shop.buy_mystic_medals is false \
             — the count will never trip"
        );
    }
    if s.stop_when_covenants > 0 && !s.buy_covenant {
        tracing::warn!(
            "shop.stop_when_covenants > 0 but shop.buy_covenant is false \
             — the count will never trip"
        );
    }
    if s.stop_when_gold_spent > 0 && !s.buy_mystic_medals && !s.buy_covenant {
        tracing::warn!(
            "shop.stop_when_gold_spent > 0 but no buy flag is on \
             — no gold will ever be spent"
        );
    }
    Ok(())
}

fn validate_matching(m: &MatchingConfig) -> Result<()> {
    if !(0.0 < m.threshold && m.threshold <= 1.0) {
        return Err(Error::ConfigInvalid(
            "matching.threshold must be in (0, 1]".into(),
        ));
    }
    if !(0.0 <= m.margin && m.margin < 1.0) {
        return Err(Error::ConfigInvalid(
            "matching.margin must be in [0, 1)".into(),
        ));
    }
    if m.extra_scales.is_empty() {
        return Err(Error::ConfigInvalid(
            "matching.extra_scales must contain at least one value".into(),
        ));
    }
    if m.extra_scales.iter().any(|s| !s.is_finite() || *s <= 0.0) {
        return Err(Error::ConfigInvalid(
            "matching.extra_scales must all be finite and positive".into(),
        ));
    }
    if !(100..=5000).contains(&m.preview_refresh_ms) {
        return Err(Error::ConfigInvalid(
            "matching.preview_refresh_ms must be in [100, 5000]".into(),
        ));
    }
    Ok(())
}

fn validate_timing(t: &TimingConfig) -> Result<()> {
    validate_range("click_delay", t.click_delay_min_ms, t.click_delay_max_ms)?;
    validate_range("move_step", t.move_step_min_ms, t.move_step_max_ms)?;
    validate_range(
        "move_to_click",
        t.move_to_click_min_ms,
        t.move_to_click_max_ms,
    )?;
    validate_range("inter_round", t.inter_round_min_ms, t.inter_round_max_ms)?;
    validate_range("long_pause", t.long_pause_min_ms, t.long_pause_max_ms)?;

    if t.scroll_pause_ms == 0 {
        return Err(Error::ConfigInvalid(
            "timing.scroll_pause_ms must be > 0".into(),
        ));
    }

    if t.move_steps_min == 0 || t.move_steps_min > t.move_steps_max {
        return Err(Error::ConfigInvalid(
            "timing.move_steps_min must be in 1..=move_steps_max".into(),
        ));
    }
    if t.click_delay_sigma <= 0.0 || !t.click_delay_sigma.is_finite() {
        return Err(Error::ConfigInvalid(
            "timing.click_delay_sigma must be > 0".into(),
        ));
    }
    if t.click_delay_mean_ms <= 0.0 || !t.click_delay_mean_ms.is_finite() {
        return Err(Error::ConfigInvalid(
            "timing.click_delay_mean_ms must be > 0".into(),
        ));
    }
    if t.jitter_radius_px < 0.0 || !t.jitter_radius_px.is_finite() {
        return Err(Error::ConfigInvalid(
            "timing.jitter_radius_px must be >= 0".into(),
        ));
    }
    if t.move_curve_amplitude_px < 0.0 || !t.move_curve_amplitude_px.is_finite() {
        return Err(Error::ConfigInvalid(
            "timing.move_curve_amplitude_px must be >= 0".into(),
        ));
    }
    Ok(())
}

fn validate_regions(r: &RegionsConfig) -> Result<()> {
    if let Some(rect) = r.shop_grid {
        validate_rect("regions.shop_grid", rect)?;
    }
    Ok(())
}

fn validate_zones(z: &ZonesConfig) -> Result<()> {
    for (name, zone) in [
        ("zones.refresh", z.refresh),
        ("zones.refresh_confirm", z.refresh_confirm),
        ("zones.buy_confirm", z.buy_confirm),
        ("zones.buy_column", z.buy_column),
    ] {
        if let Some(rect) = zone {
            validate_rect(name, rect)?;
        }
    }
    Ok(())
}

fn validate_notifications(n: &NotificationsConfig) -> Result<()> {
    let url = n.webhook_url();
    if url.is_empty() {
        return Ok(());
    }
    if !url.starts_with("https://") {
        return Err(Error::ConfigInvalid(
            "notifications.discord_webhook_url must start with https://".into(),
        ));
    }
    // Real Discord webhooks live under discord.com or discordapp.com.
    // Catches the obvious typo (http, paste-of-something-else); a hostile
    // URL still goes through, but that's a "user owns their config" thing.
    let host_ok = url.starts_with("https://discord.com/")
        || url.starts_with("https://discordapp.com/")
        || url.starts_with("https://canary.discord.com/")
        || url.starts_with("https://ptb.discord.com/");
    if !host_ok {
        return Err(Error::ConfigInvalid(
            "notifications.discord_webhook_url must point at discord.com (got something else — \
             check you copied the full webhook URL)"
                .into(),
        ));
    }
    // Webhook URLs always contain `/api/webhooks/`. Rejecting invite links,
    // channel URLs, etc. at load time means the user sees the mistake
    // immediately instead of a silent 404 after the next run.
    if !url.contains("/api/webhooks/") {
        return Err(Error::ConfigInvalid(
            "notifications.discord_webhook_url is missing the /api/webhooks/ path — looks like \
             a channel or invite link, not a webhook. In Discord: Server Settings → \
             Integrations → Webhooks → Copy Webhook URL."
                .into(),
        ));
    }
    Ok(())
}

fn validate_range(name: &str, min: u64, max: u64) -> Result<()> {
    if min > max {
        return Err(Error::ConfigInvalid(format!(
            "timing.{name}_min_ms ({min}) must be ≤ timing.{name}_max_ms ({max})"
        )));
    }
    Ok(())
}

fn validate_rect(path: &str, [x, y, w, h]: [f32; 4]) -> Result<()> {
    let in_unit = |v: f32| v.is_finite() && (0.0..=1.0).contains(&v);
    if !(in_unit(x) && in_unit(y) && in_unit(w) && in_unit(h)) {
        return Err(Error::ConfigInvalid(format!(
            "{path}: components must be finite and in [0, 1]"
        )));
    }
    if w == 0.0 || h == 0.0 {
        return Err(Error::ConfigInvalid(format!(
            "{path}: width and height must be > 0"
        )));
    }
    // Tolerate ~1% overflow — DragValue rounding can produce sums
    // like 1.002. Anything wildly larger is a real config error.
    const OVERFLOW_TOLERANCE: f32 = 0.01;
    if x + w > 1.0 + OVERFLOW_TOLERANCE || y + h > 1.0 + OVERFLOW_TOLERANCE {
        return Err(Error::ConfigInvalid(format!(
            "{path}: x+w and y+h must be ≤ 1.0 (got x+w={:.3}, y+h={:.3})",
            x + w,
            y + h
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_in_unit_square_accepts() {
        validate_rect("test", [0.0, 0.0, 1.0, 1.0]).unwrap();
        validate_rect("test", [0.1, 0.2, 0.3, 0.4]).unwrap();
    }

    #[test]
    fn rect_rejects_zero_size() {
        assert!(validate_rect("test", [0.1, 0.1, 0.0, 0.5]).is_err());
        assert!(validate_rect("test", [0.1, 0.1, 0.5, 0.0]).is_err());
    }

    #[test]
    fn rect_rejects_nan_or_inf() {
        assert!(validate_rect("test", [f32::NAN, 0.0, 0.5, 0.5]).is_err());
        assert!(validate_rect("test", [0.0, 0.0, f32::INFINITY, 0.5]).is_err());
    }

    #[test]
    fn rect_rejects_out_of_unit_range() {
        assert!(validate_rect("test", [-0.1, 0.0, 0.5, 0.5]).is_err());
        assert!(validate_rect("test", [0.0, 0.0, 1.5, 0.5]).is_err());
    }

    #[test]
    fn rect_tolerates_small_overflow_from_drag_value_rounding() {
        validate_rect("test", [0.5, 0.0, 0.505, 0.5]).unwrap();
    }

    #[test]
    fn rect_rejects_clearly_invalid_overflow() {
        assert!(validate_rect("test", [0.5, 0.0, 1.0, 0.5]).is_err());
    }
}
