use std::path::Path;

use crate::config::Config;

/// Snapshot of every auto-saved field, compared frame-to-frame to detect
/// edits without per-panel `.changed()` plumbing. `PartialEq` on `f32`
/// is fine because validated configs never carry NaN.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct AutoSavedFields {
    pub base_resolution: [u32; 2],
    pub max_refreshes: u32,
    pub stop_after_minutes: u32,
    pub stop_when_mystic_medals: u32,
    pub stop_when_covenants: u32,
    pub stop_when_gold_spent: u32,
    pub buy_mystic_medals: bool,
    pub buy_covenant: bool,
    pub buy_button_y_offset_ratio: f32,
    pub buy_button_band_h_ratio: f32,
    pub buy_calibration_line_y_ratio: f32,
    pub threshold: f32,
    pub preview_refresh_ms: u32,
    pub sleep_when_done: bool,
    pub shop_grid: Option<[f32; 4]>,
    pub z_refresh: Option<[f32; 4]>,
    pub z_refresh_confirm: Option<[f32; 4]>,
    pub z_buy_confirm: Option<[f32; 4]>,
    pub z_buy_column: Option<[f32; 4]>,
    pub click_delay_mean_ms: f64,
    pub click_delay_sigma: f64,
    pub click_delay_min_ms: u64,
    pub click_delay_max_ms: u64,
    pub move_steps_min: u32,
    pub move_steps_max: u32,
    pub move_step_min_ms: u64,
    pub move_step_max_ms: u64,
    pub move_to_click_min_ms: u64,
    pub move_to_click_max_ms: u64,
    pub move_curve_amplitude_px: f32,
    pub jitter_radius_px: f32,
    pub inter_round_min_ms: u64,
    pub inter_round_max_ms: u64,
    pub long_pause_every_n: u32,
    pub long_pause_min_ms: u64,
    pub long_pause_max_ms: u64,
    pub scroll_amount: i32,
    pub cooperative_idle_ms: u64,
    pub discord_webhook_url: String,
}

impl AutoSavedFields {
    pub(super) fn from_config(cfg: &Config) -> Self {
        Self {
            base_resolution: cfg.window.base_resolution,
            max_refreshes: cfg.shop.max_refreshes,
            stop_after_minutes: cfg.shop.stop_after_minutes,
            stop_when_mystic_medals: cfg.shop.stop_when_mystic_medals,
            stop_when_covenants: cfg.shop.stop_when_covenants,
            stop_when_gold_spent: cfg.shop.stop_when_gold_spent,
            buy_mystic_medals: cfg.shop.buy_mystic_medals,
            buy_covenant: cfg.shop.buy_covenant,
            buy_button_y_offset_ratio: cfg.shop.buy_button_y_offset_ratio,
            buy_button_band_h_ratio: cfg.shop.buy_button_band_h_ratio,
            buy_calibration_line_y_ratio: cfg.shop.buy_calibration_line_y_ratio,
            threshold: cfg.matching.threshold,
            preview_refresh_ms: cfg.matching.preview_refresh_ms,
            sleep_when_done: cfg.shop.sleep_when_done,
            shop_grid: cfg.regions.shop_grid,
            z_refresh: cfg.zones.refresh,
            z_refresh_confirm: cfg.zones.refresh_confirm,
            z_buy_confirm: cfg.zones.buy_confirm,
            z_buy_column: cfg.zones.buy_column,
            click_delay_mean_ms: cfg.timing.click_delay_mean_ms,
            click_delay_sigma: cfg.timing.click_delay_sigma,
            click_delay_min_ms: cfg.timing.click_delay_min_ms,
            click_delay_max_ms: cfg.timing.click_delay_max_ms,
            move_steps_min: cfg.timing.move_steps_min,
            move_steps_max: cfg.timing.move_steps_max,
            move_step_min_ms: cfg.timing.move_step_min_ms,
            move_step_max_ms: cfg.timing.move_step_max_ms,
            move_to_click_min_ms: cfg.timing.move_to_click_min_ms,
            move_to_click_max_ms: cfg.timing.move_to_click_max_ms,
            move_curve_amplitude_px: cfg.timing.move_curve_amplitude_px,
            jitter_radius_px: cfg.timing.jitter_radius_px,
            inter_round_min_ms: cfg.timing.inter_round_min_ms,
            inter_round_max_ms: cfg.timing.inter_round_max_ms,
            long_pause_every_n: cfg.timing.long_pause_every_n,
            long_pause_min_ms: cfg.timing.long_pause_min_ms,
            long_pause_max_ms: cfg.timing.long_pause_max_ms,
            scroll_amount: cfg.timing.scroll_amount,
            cooperative_idle_ms: cfg.timing.cooperative_idle_ms,
            discord_webhook_url: cfg.notifications.discord_webhook_url.clone(),
        }
    }
}

/// Re-reads on-disk TOML, applies every auto-saved field in a single
/// document mutation via toml_edit (preserves inline comments and
/// whitespace), then writes it back.
pub(super) fn write_all_back(path: &Path, config: &Config) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut doc: toml_edit::DocumentMut = raw.parse()?;

    // [window]
    let [bw, bh] = config.window.base_resolution;
    set_scalar(&mut doc, "window", "base_resolution", pair_array(bw, bh));

    // [shop]
    set_scalar(
        &mut doc,
        "shop",
        "max_refreshes",
        i64::from(config.shop.max_refreshes),
    );
    set_scalar(
        &mut doc,
        "shop",
        "stop_after_minutes",
        i64::from(config.shop.stop_after_minutes),
    );
    set_scalar(
        &mut doc,
        "shop",
        "stop_when_mystic_medals",
        i64::from(config.shop.stop_when_mystic_medals),
    );
    set_scalar(
        &mut doc,
        "shop",
        "stop_when_covenants",
        i64::from(config.shop.stop_when_covenants),
    );
    set_scalar(
        &mut doc,
        "shop",
        "stop_when_gold_spent",
        i64::from(config.shop.stop_when_gold_spent),
    );
    set_scalar(
        &mut doc,
        "shop",
        "buy_mystic_medals",
        config.shop.buy_mystic_medals,
    );
    set_scalar(&mut doc, "shop", "buy_covenant", config.shop.buy_covenant);
    set_scalar(
        &mut doc,
        "shop",
        "buy_button_y_offset_ratio",
        rounded3(f64::from(config.shop.buy_button_y_offset_ratio)),
    );
    set_scalar(
        &mut doc,
        "shop",
        "buy_button_band_h_ratio",
        rounded3(f64::from(config.shop.buy_button_band_h_ratio)),
    );
    set_scalar(
        &mut doc,
        "shop",
        "buy_calibration_line_y_ratio",
        rounded3(f64::from(config.shop.buy_calibration_line_y_ratio)),
    );
    set_scalar(
        &mut doc,
        "shop",
        "sleep_when_done",
        config.shop.sleep_when_done,
    );

    // [matching]
    set_scalar(
        &mut doc,
        "matching",
        "threshold",
        rounded3(f64::from(config.matching.threshold)),
    );
    set_scalar(
        &mut doc,
        "matching",
        "preview_refresh_ms",
        i64::from(config.matching.preview_refresh_ms),
    );

    // [timing]
    set_scalar(
        &mut doc,
        "timing",
        "click_delay_mean_ms",
        config.timing.click_delay_mean_ms,
    );
    set_scalar(
        &mut doc,
        "timing",
        "click_delay_sigma",
        rounded3(config.timing.click_delay_sigma),
    );
    set_scalar(
        &mut doc,
        "timing",
        "click_delay_min_ms",
        i64::try_from(config.timing.click_delay_min_ms).unwrap_or(i64::MAX),
    );
    set_scalar(
        &mut doc,
        "timing",
        "click_delay_max_ms",
        i64::try_from(config.timing.click_delay_max_ms).unwrap_or(i64::MAX),
    );
    set_scalar(
        &mut doc,
        "timing",
        "move_steps_min",
        i64::from(config.timing.move_steps_min),
    );
    set_scalar(
        &mut doc,
        "timing",
        "move_steps_max",
        i64::from(config.timing.move_steps_max),
    );
    set_scalar(
        &mut doc,
        "timing",
        "move_step_min_ms",
        i64::try_from(config.timing.move_step_min_ms).unwrap_or(i64::MAX),
    );
    set_scalar(
        &mut doc,
        "timing",
        "move_step_max_ms",
        i64::try_from(config.timing.move_step_max_ms).unwrap_or(i64::MAX),
    );
    set_scalar(
        &mut doc,
        "timing",
        "move_to_click_min_ms",
        i64::try_from(config.timing.move_to_click_min_ms).unwrap_or(i64::MAX),
    );
    set_scalar(
        &mut doc,
        "timing",
        "move_to_click_max_ms",
        i64::try_from(config.timing.move_to_click_max_ms).unwrap_or(i64::MAX),
    );
    set_scalar(
        &mut doc,
        "timing",
        "move_curve_amplitude_px",
        rounded3(f64::from(config.timing.move_curve_amplitude_px)),
    );
    set_scalar(
        &mut doc,
        "timing",
        "jitter_radius_px",
        rounded3(f64::from(config.timing.jitter_radius_px)),
    );
    set_scalar(
        &mut doc,
        "timing",
        "inter_round_min_ms",
        i64::try_from(config.timing.inter_round_min_ms).unwrap_or(i64::MAX),
    );
    set_scalar(
        &mut doc,
        "timing",
        "inter_round_max_ms",
        i64::try_from(config.timing.inter_round_max_ms).unwrap_or(i64::MAX),
    );
    set_scalar(
        &mut doc,
        "timing",
        "long_pause_every_n",
        i64::from(config.timing.long_pause_every_n),
    );
    set_scalar(
        &mut doc,
        "timing",
        "long_pause_min_ms",
        i64::try_from(config.timing.long_pause_min_ms).unwrap_or(i64::MAX),
    );
    set_scalar(
        &mut doc,
        "timing",
        "long_pause_max_ms",
        i64::try_from(config.timing.long_pause_max_ms).unwrap_or(i64::MAX),
    );
    set_scalar(
        &mut doc,
        "timing",
        "scroll_amount",
        i64::from(config.timing.scroll_amount),
    );
    set_scalar(
        &mut doc,
        "timing",
        "cooperative_idle_ms",
        i64::try_from(config.timing.cooperative_idle_ms).unwrap_or(i64::MAX),
    );

    // [regions]
    set_rect_in(&mut doc, "regions", "shop_grid", config.regions.shop_grid);

    // [zones]
    set_rect_in(&mut doc, "zones", "refresh", config.zones.refresh);
    set_rect_in(
        &mut doc,
        "zones",
        "refresh_confirm",
        config.zones.refresh_confirm,
    );
    set_rect_in(&mut doc, "zones", "buy_confirm", config.zones.buy_confirm);
    set_rect_in(&mut doc, "zones", "buy_column", config.zones.buy_column);

    // [notifications]
    set_scalar(
        &mut doc,
        "notifications",
        "discord_webhook_url",
        config.notifications.discord_webhook_url.clone(),
    );

    // Write-then-rename so an interrupted write (power loss, AV holding
    // a handle, std::fs::write killed mid-flight) can't truncate the
    // live config.toml. std::fs::rename on Windows uses MoveFileEx with
    // REPLACE_EXISTING|WRITE_THROUGH so the swap is atomic.
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, doc.to_string())?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

fn set_region(table: &mut toml_edit::Table, key: &str, value: Option<[f32; 4]>) {
    match value {
        Some([x, y, w, h]) => {
            // Clamp before writing so a reload doesn't trip the validator
            // on rectangles like x+w > 1.0. Width takes priority over X
            // so a user-set width near the right edge keeps its size and
            // X slides left instead of being silently truncated.
            let w = w.clamp(0.001, 1.0);
            let h = h.clamp(0.001, 1.0);
            let x = x.clamp(0.0, 1.0 - w);
            let y = y.clamp(0.0, 1.0 - h);
            let rounded = [x, y, w, h].map(|v| (f64::from(v) * 1000.0).round() / 1000.0);

            // Mutate the existing array in place — a fresh `value(...)`
            // call would drop surrounding decor (blank lines, comments).
            if let Some(existing) = table.get_mut(key)
                && let Some(arr) = existing.as_array_mut()
            {
                arr.clear();
                for v in rounded {
                    arr.push(v);
                }
                arr.fmt();
            } else {
                let mut arr = toml_edit::Array::new();
                for v in rounded {
                    arr.push(v);
                }
                arr.fmt();
                table.insert(key, toml_edit::value(arr));
            }
        }
        None => {
            table.remove(key);
        }
    }
}

fn set_rect_in(
    doc: &mut toml_edit::DocumentMut,
    section: &str,
    key: &str,
    value: Option<[f32; 4]>,
) {
    let table = doc
        .entry(section)
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let Some(table) = table.as_table_mut() else {
        return;
    };
    set_region(table, key, value);
}

/// Matches `DragValue::max_decimals(3)` — without rounding, dragging
/// persists a noisy `0.0399999…` instead of `0.04`.
fn rounded3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn pair_array(w: u32, h: u32) -> toml_edit::Array {
    let mut arr = toml_edit::Array::new();
    arr.push(i64::from(w));
    arr.push(i64::from(h));
    arr.fmt();
    arr
}

fn set_scalar<V>(doc: &mut toml_edit::DocumentMut, section: &str, key: &str, value: V)
where
    V: Into<toml_edit::Value>,
{
    let table = doc
        .entry(section)
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let Some(table) = table.as_table_mut() else {
        return;
    };
    let v = value.into();
    if let Some(existing) = table.get_mut(key)
        && let Some(slot) = existing.as_value_mut()
    {
        *slot = v;
    } else {
        table.insert(key, toml_edit::Item::Value(v));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// Unique per test so parallel runs don't collide. Caller cleans up.
    fn temp_config(name: &str, contents: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "e7-persist-test-{}-{name}.toml",
            std::process::id()
        ));
        std::fs::write(&path, contents).expect("write temp config");
        path
    }

    fn non_default_config() -> Config {
        let mut cfg: Config = toml::from_str(crate::config::DEFAULT_TOML).unwrap();
        cfg.window.base_resolution = [1600, 900];
        cfg.shop.max_refreshes = 7;
        cfg.shop.stop_after_minutes = 90;
        cfg.shop.stop_when_mystic_medals = 5;
        cfg.shop.stop_when_covenants = 6;
        cfg.shop.stop_when_gold_spent = 500_000;
        cfg.shop.buy_mystic_medals = false;
        cfg.shop.buy_covenant = false;
        cfg.shop.buy_button_y_offset_ratio = 0.11;
        cfg.shop.buy_button_band_h_ratio = 0.05;
        cfg.shop.buy_calibration_line_y_ratio = 0.4;
        cfg.shop.sleep_when_done = true;
        cfg.matching.threshold = 0.88;
        cfg.matching.preview_refresh_ms = 750;
        cfg.regions.shop_grid = Some([0.05, 0.1, 0.9, 0.8]);
        cfg.zones.refresh = Some([0.1, 0.2, 0.15, 0.1]);
        cfg.zones.refresh_confirm = Some([0.3, 0.4, 0.2, 0.1]);
        cfg.zones.buy_confirm = Some([0.5, 0.6, 0.2, 0.1]);
        cfg.zones.buy_column = Some([0.7, 0.1, 0.2, 0.5]);
        cfg.timing.click_delay_mean_ms = 300.0;
        cfg.timing.click_delay_sigma = 0.45;
        cfg.timing.click_delay_min_ms = 100;
        cfg.timing.click_delay_max_ms = 800;
        cfg.timing.move_steps_min = 4;
        cfg.timing.move_steps_max = 12;
        cfg.timing.move_step_min_ms = 2;
        cfg.timing.move_step_max_ms = 9;
        cfg.timing.move_to_click_min_ms = 25;
        cfg.timing.move_to_click_max_ms = 60;
        cfg.timing.move_curve_amplitude_px = 4.5;
        cfg.timing.jitter_radius_px = 2.5;
        cfg.timing.inter_round_min_ms = 700;
        cfg.timing.inter_round_max_ms = 1900;
        cfg.timing.long_pause_every_n = 8;
        cfg.timing.long_pause_min_ms = 4000;
        cfg.timing.long_pause_max_ms = 11_000;
        cfg.timing.scroll_amount = 6;
        cfg.timing.cooperative_idle_ms = 2000;
        cfg.notifications.discord_webhook_url =
            "https://discord.com/api/webhooks/1/round-trip-test".into();
        cfg
    }

    /// The repo's documented #1 failure mode: a field present in
    /// AutoSavedFields but missing its write site in write_all_back means
    /// GUI edits are silently discarded. Every field here carries a
    /// non-default, 3-decimal-clean value; if write_all_back misses one,
    /// the reloaded side differs and the assert names the field.
    #[test]
    fn every_auto_saved_field_survives_write_and_reload() {
        let cfg = non_default_config();
        let path = temp_config("round-trip", crate::config::DEFAULT_TOML);
        write_all_back(&path, &cfg).expect("write_all_back");
        let reloaded = Config::load(&path).expect("reload after write_all_back");
        assert_eq!(
            AutoSavedFields::from_config(&cfg),
            AutoSavedFields::from_config(&reloaded)
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Exhaustive on purpose — no `..` rest pattern. Adding a field to
    /// AutoSavedFields breaks this build, which is the point: give the new
    /// field a non-default value in non_default_config() above so the
    /// round-trip test covers its write site too.
    #[test]
    fn auto_saved_fields_is_exhaustively_covered() {
        let AutoSavedFields {
            base_resolution: _,
            max_refreshes: _,
            stop_after_minutes: _,
            stop_when_mystic_medals: _,
            stop_when_covenants: _,
            stop_when_gold_spent: _,
            buy_mystic_medals: _,
            buy_covenant: _,
            buy_button_y_offset_ratio: _,
            buy_button_band_h_ratio: _,
            buy_calibration_line_y_ratio: _,
            threshold: _,
            preview_refresh_ms: _,
            sleep_when_done: _,
            shop_grid: _,
            z_refresh: _,
            z_refresh_confirm: _,
            z_buy_confirm: _,
            z_buy_column: _,
            click_delay_mean_ms: _,
            click_delay_sigma: _,
            click_delay_min_ms: _,
            click_delay_max_ms: _,
            move_steps_min: _,
            move_steps_max: _,
            move_step_min_ms: _,
            move_step_max_ms: _,
            move_to_click_min_ms: _,
            move_to_click_max_ms: _,
            move_curve_amplitude_px: _,
            jitter_radius_px: _,
            inter_round_min_ms: _,
            inter_round_max_ms: _,
            long_pause_every_n: _,
            long_pause_min_ms: _,
            long_pause_max_ms: _,
            scroll_amount: _,
            cooperative_idle_ms: _,
            discord_webhook_url: _,
        } = AutoSavedFields::from_config(&non_default_config());
    }

    #[test]
    fn write_all_back_preserves_comments_and_unknown_keys() {
        let path = temp_config("decor", crate::config::DEFAULT_TOML);
        write_all_back(&path, &non_default_config()).expect("write_all_back");
        let raw = std::fs::read_to_string(&path).expect("read back");
        assert!(
            raw.contains("# Inter-click delay: log-normal sample clamped to [min, max]."),
            "inline comment was lost"
        );
        assert!(
            raw.contains("max_scrolls_per_round"),
            "unknown key was dropped"
        );
        assert!(
            raw.contains("resize_tolerance_px"),
            "unknown key was dropped"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_all_back_removes_zone_keys_when_none() {
        let base = format!(
            "{}\n[zones]\nrefresh = [0.1, 0.2, 0.15, 0.1]\n",
            crate::config::DEFAULT_TOML
        );
        let path = temp_config("zone-removal", &base);
        let mut cfg: Config = toml::from_str(crate::config::DEFAULT_TOML).unwrap();
        cfg.zones.refresh = None;
        write_all_back(&path, &cfg).expect("write_all_back");
        let reloaded = Config::load(&path).expect("reload");
        assert!(reloaded.zones.refresh.is_none(), "zone was not removed");
        let raw = std::fs::read_to_string(&path).expect("read back");
        assert!(
            !raw.contains("refresh = ["),
            "zone key still present in raw text"
        );
        let _ = std::fs::remove_file(&path);
    }
}
