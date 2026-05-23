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
    pub buy_mystic_medals: bool,
    pub buy_covenant: bool,
    pub buy_button_y_offset_ratio: f32,
    pub threshold: f32,
    pub sleep_when_done: bool,
    pub shop_grid: Option<[f32; 4]>,
    pub anchor_shop: Option<[f32; 4]>,
    pub z_refresh: Option<[f32; 4]>,
    pub z_refresh_confirm: Option<[f32; 4]>,
    pub z_buy_confirm: Option<[f32; 4]>,
    pub z_buy_column: Option<[f32; 4]>,
}

impl AutoSavedFields {
    pub(super) fn from_config(cfg: &Config) -> Self {
        Self {
            base_resolution: cfg.window.base_resolution,
            max_refreshes: cfg.shop.max_refreshes,
            stop_after_minutes: cfg.shop.stop_after_minutes,
            stop_when_mystic_medals: cfg.shop.stop_when_mystic_medals,
            stop_when_covenants: cfg.shop.stop_when_covenants,
            buy_mystic_medals: cfg.shop.buy_mystic_medals,
            buy_covenant: cfg.shop.buy_covenant,
            buy_button_y_offset_ratio: cfg.shop.buy_button_y_offset_ratio,
            threshold: cfg.matching.threshold,
            sleep_when_done: cfg.shop.sleep_when_done,
            shop_grid: cfg.regions.shop_grid,
            anchor_shop: cfg.regions.anchor_shop,
            z_refresh: cfg.zones.refresh,
            z_refresh_confirm: cfg.zones.refresh_confirm,
            z_buy_confirm: cfg.zones.buy_confirm,
            z_buy_column: cfg.zones.buy_column,
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

    // [regions]
    set_rect_in(&mut doc, "regions", "shop_grid", config.regions.shop_grid);
    set_rect_in(
        &mut doc,
        "regions",
        "anchor_shop",
        config.regions.anchor_shop,
    );

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

    std::fs::write(path, doc.to_string())?;
    Ok(())
}

fn set_region(table: &mut toml_edit::Table, key: &str, value: Option<[f32; 4]>) {
    match value {
        Some([x, y, w, h]) => {
            // Clamp before writing so a reload doesn't trip the validator
            // on rectangles like x+w > 1.0.
            let x = x.clamp(0.0, 1.0);
            let y = y.clamp(0.0, 1.0);
            let w = w.clamp(0.001, 1.0 - x);
            let h = h.clamp(0.001, 1.0 - y);
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
