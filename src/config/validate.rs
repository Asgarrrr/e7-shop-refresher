//! Cross-field validation. `validate_all` is the entry point used by
//! `Config::load`. Template file existence is checked separately so the
//! GUI can load a partial config to help the user prepare templates.

use super::sections::{
    MatchingConfig, RegionsConfig, ShopConfig, TimingConfig, WindowConfig, ZonesConfig,
};
use super::{CONFIG_VERSION, Config, MissingTemplate, MissingZone};
use crate::error::{Error, Result};

pub(super) fn validate_all(cfg: &Config) -> Result<()> {
    validate_version(cfg.version)?;
    validate_window(&cfg.window)?;
    validate_shop(&cfg.shop)?;
    validate_matching(&cfg.matching)?;
    validate_timing(&cfg.timing)?;
    validate_regions(&cfg.regions)?;
    validate_zones(&cfg.zones)?;
    Ok(())
}

pub(super) fn list_missing_templates(cfg: &Config) -> Vec<MissingTemplate> {
    let dir = cfg.template_dir();
    let t = &cfg.templates;
    let required = [
        ("anchor_shop", &t.anchor_shop),
        ("mystic_medal", &t.mystic_medal),
        ("covenant", &t.covenant),
    ];
    let mut missing = Vec::new();
    for (name, file) in required {
        let path = dir.join(file);
        if !path.exists() {
            missing.push(MissingTemplate {
                name: name.to_string(),
                path,
            });
        }
    }
    missing
}

/// `buy_column` / `buy_confirm` are only required if any buy flag is on
/// — otherwise they'd block startup for users who only want to refresh.
pub(super) fn list_missing_zones(cfg: &Config) -> Vec<MissingZone> {
    let z = &cfg.zones;
    let mut missing = Vec::new();
    let mut add_if_unset = |name, value: &Option<[f32; 4]>| {
        if value.is_none() {
            missing.push(MissingZone { name });
        }
    };
    add_if_unset("refresh", &z.refresh);
    add_if_unset("refresh_confirm", &z.refresh_confirm);
    let any_buy = cfg.shop.buy_mystic_medals || cfg.shop.buy_covenant;
    if any_buy {
        add_if_unset("buy_column", &z.buy_column);
        add_if_unset("buy_confirm", &z.buy_confirm);
    }
    missing
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
    // Per-alias stop counts with the matching buy flag off would loop
    // forever without ever reaching the target — catch that at load.
    if s.stop_when_mystic_medals > 0 && !s.buy_mystic_medals {
        return Err(Error::ConfigInvalid(
            "shop.stop_when_mystic_medals > 0 but shop.buy_mystic_medals \
             is false — the count can never be reached"
                .into(),
        ));
    }
    if s.stop_when_covenants > 0 && !s.buy_covenant {
        return Err(Error::ConfigInvalid(
            "shop.stop_when_covenants > 0 but shop.buy_covenant is false \
             — the count can never be reached"
                .into(),
        ));
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
    if t.poll_interval_ms == 0 || t.poll_interval_ms >= t.anchor_timeout_ms {
        return Err(Error::ConfigInvalid(
            "timing.poll_interval_ms must be in 1..anchor_timeout_ms".into(),
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
    for (name, region) in [
        ("regions.shop_grid", r.shop_grid),
        ("regions.anchor_shop", r.anchor_shop),
    ] {
        if let Some(rect) = region {
            validate_rect(name, rect)?;
        }
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
