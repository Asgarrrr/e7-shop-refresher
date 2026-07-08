//! Window-relative coordinates for Epic Seven's Secret Shop layout.
//!
//! The E7 shop is a fixed grid — items always stack in the same column,
//! the Refresh button is always bottom-left, the confirm modals always
//! pop in the centre. Once you know the window size, every position is
//! derivable from these ratios. Replaces user-drawn zones/regions.
//!
//! All values are window-relative `[0, 1]` ratios over the **client
//! area** (`Capture::rect()` returns the client rect, not the raw OS
//! window). Tuned against the STOVE client at the default window size
//! (1495×872) and verified to hold at 1920×1080. Adjust here — not via
//! the GUI — if the game UI changes.

/// `[x, y, w, h]` ratio rect.
pub type RectRatio = [f32; 4];

/// Returns the rect used to overlay the column of "1/1 Buy" pills. Y
/// range matches `SHOP_GRID` so the strip visually pairs with the
/// items it serves; X range comes from `BUY_COLUMN_X` / `BUY_COLUMN_W`.
pub fn buy_column_overlay_rect() -> RectRatio {
    [BUY_COLUMN_X, SHOP_GRID[1], BUY_COLUMN_W, SHOP_GRID[3]]
}

/// Refresh button (bottom-left, includes the skystone icon + "Refresh"
/// glyphs). Click jitter picks a uniform random point inside.
pub const REFRESH: RectRatio = [0.05, 0.88, 0.22, 0.10];

/// "Confirm" pill in the refresh-skystone modal (right side, blue).
pub const REFRESH_CONFIRM: RectRatio = [0.529, 0.621, 0.121, 0.057];

/// "Cancel" pill in the refresh-skystone modal (left side, brown).
/// Recovery target when the confirm click misses and the modal stays up.
pub const REFRESH_CANCEL: RectRatio = [0.35, 0.621, 0.121, 0.057];

/// "Buy" pill in the item-buy modal (right side, green).
pub const BUY_CONFIRM: RectRatio = [0.488, 0.686, 0.198, 0.059];

/// "Cancel" pill in the item-buy modal (left side, brown).
pub const BUY_CANCEL: RectRatio = [0.31, 0.686, 0.13, 0.059];

/// Full-size item icon in the item-buy modal. Reclassified before the
/// confirm click so a drifted row click can never buy the wrong item.
pub const BUY_MODAL_ICON: RectRatio = [0.245, 0.42, 0.08, 0.145];

/// Buy-button column X-range. Y is per-row, supplied at click time from
/// the matched icon's Y coordinate.
pub const BUY_COLUMN_X: f32 = 0.83;
pub const BUY_COLUMN_W: f32 = 0.142;

/// Item-list column used for the modal-dim luminance checks, the
/// reroll / scroll-bottom hashes, and the Setup-tab preview search.
/// Row-cell classification uses `ICON_COLUMN_X/W` instead.
pub const SHOP_GRID: RectRatio = [0.425, 0.085, 0.09, 0.915];

/// Row geometry: window-height fraction between a row's buy-button
/// centre and its item-icon centre (icon sits slightly higher).
/// Measured 0.019–0.026 across rows on live captures.
pub const ROW_ICON_Y_OFFSET: f32 = 0.023;

/// X band of the item-icon column for row-cell classification: the
/// bundled grid column widened by ~2% of width per side. Deliberately
/// NOT the user-calibratable `regions.shop_grid` — legacy overrides
/// (drawn when that region was only a reroll-hash zone) would move the
/// search off the icons entirely. Icon centres measured at 0.45–0.46
/// of width across two window aspect ratios; this band holds both with
/// margin.
pub const ICON_COLUMN_X: f32 = 0.405;
pub const ICON_COLUMN_W: f32 = 0.13;

/// Tag for the GUI overlay: do we look here (NCC search) or click here
/// (mouse target)? Drives the colour split on the debug overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    Search,
    Click,
}

/// Every layout rect the bot uses, with a human label and a usage tag
/// for the GUI's debug overlay. Cheap to build per frame — keeps the
/// overlay always in sync with the live constants instead of needing
/// a parallel registry to maintain.
///
/// Reflects the runtime concepts: the bot searches `shop_grid` for any
/// enabled item, then clicks inside the `buy_column` X-strip at the
/// matched row's Y.
pub fn overlay_rects() -> Vec<(String, RectRatio, OverlayKind)> {
    vec![
        ("shop_grid".to_string(), SHOP_GRID, OverlayKind::Search),
        ("refresh".to_string(), REFRESH, OverlayKind::Click),
        (
            "refresh_confirm".to_string(),
            REFRESH_CONFIRM,
            OverlayKind::Click,
        ),
        ("buy_confirm".to_string(), BUY_CONFIRM, OverlayKind::Click),
        (
            "refresh_cancel".to_string(),
            REFRESH_CANCEL,
            OverlayKind::Click,
        ),
        ("buy_cancel".to_string(), BUY_CANCEL, OverlayKind::Click),
        (
            "buy_modal_icon".to_string(),
            BUY_MODAL_ICON,
            OverlayKind::Search,
        ),
        (
            "buy_column".to_string(),
            buy_column_overlay_rect(),
            OverlayKind::Click,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect_in_unit_square(r: RectRatio) {
        let [x, y, w, h] = r;
        assert!((0.0..=1.0).contains(&x), "x out of range: {x}");
        assert!((0.0..=1.0).contains(&y), "y out of range: {y}");
        assert!(w > 0.0 && x + w <= 1.0 + 0.01, "w out of range: {w}");
        assert!(h > 0.0 && y + h <= 1.0 + 0.01, "h out of range: {h}");
    }

    #[test]
    fn all_constants_are_valid_rects() {
        rect_in_unit_square(REFRESH);
        rect_in_unit_square(REFRESH_CONFIRM);
        rect_in_unit_square(REFRESH_CANCEL);
        rect_in_unit_square(BUY_CONFIRM);
        rect_in_unit_square(BUY_CANCEL);
        rect_in_unit_square(BUY_MODAL_ICON);
        rect_in_unit_square(SHOP_GRID);
        rect_in_unit_square(buy_column_overlay_rect());
    }

    #[test]
    fn buy_column_strip_aligns_with_shop_grid_y_range() {
        // The overlay's buy_column should span the same vertical band
        // as shop_grid — otherwise the two halves of the row (icon +
        // buy button) would look disconnected.
        let overlays = overlay_rects();
        let buy = overlays
            .iter()
            .find(|(n, _, _)| n == "buy_column")
            .expect("buy_column overlay exists");
        assert_eq!(buy.1[1], SHOP_GRID[1]);
        assert_eq!(buy.1[3], SHOP_GRID[3]);
    }
}
