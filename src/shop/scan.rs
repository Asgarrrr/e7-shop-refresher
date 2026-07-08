use image::{GrayImage, RgbaImage};

use crate::capture::Capture;
use crate::color_check::ColorVerifier;
use crate::detector::{Detector, Hit, alias};
use crate::error::Result;

/// One visible shop row, anchored on its buy button. `klass` is the
/// icon-cell classification: `Some(alias)` only when the hue histogram
/// AND an NCC confirm in the cell agree; `None` = not a target
/// (unknown item, grayed-out sold icon, ambiguous colour).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ShopRow {
    pub anchor: Hit,
    pub klass: Option<&'static str>,
}

/// Height of one icon cell as a fraction of window height. Row pitch is
/// ≈ 0.21, the icon ≈ 0.12 — 0.14 covers the icon with slack without
/// leaking into the neighbouring row.
const ICON_CELL_H_RATIO: f32 = 0.14;

/// Row inventory for the current view: find every buy-button anchor in
/// the buy column, then classify the icon cell each row carries at a
/// fixed offset. The anchor is large and locale-independent, so this
/// never searches for a small icon in a big frame — classification of a
/// known cell replaces detection.
pub(crate) fn scan_shop_rows(
    capture: &dyn Capture,
    detector: &Detector,
    colors: &ColorVerifier,
    buy_column: [f32; 4],
    icon_column: [f32; 4],
    icon_y_offset_ratio: f32,
) -> Result<Vec<ShopRow>> {
    let rgba = capture.snapshot_rgba()?;
    let gray = image::imageops::grayscale(&rgba);
    let frame_h = gray.height() as f32;

    // Only the X range of the buy column matters; rows can sit anywhere
    // vertically (scroll position varies).
    let roi = [buy_column[0], 0.0, buy_column[2], 1.0];
    let ctx = detector.prepare_search(&gray, Some(roi));
    let anchors = detector.find_all_in(&ctx, alias::BUY_BUTTON)?;

    let rows = anchors
        .into_iter()
        .map(|anchor| {
            let cell = icon_cell_for(anchor.y, icon_column, icon_y_offset_ratio, frame_h);
            let patch = crop_ratio_rect(&rgba, cell);
            let klass = colors.classify(&patch).filter(|item| {
                detector
                    .find(&gray, item, Some(cell))
                    .ok()
                    .flatten()
                    .is_some()
            });
            ShopRow { anchor, klass }
        })
        .collect();
    Ok(rows)
}

/// Icon-cell rect for the row anchored at `anchor_y_px`: the icon
/// column's X band, centred `icon_y_offset_ratio` above the buy button
/// (same constant the click path used in the other direction).
fn icon_cell_for(
    anchor_y_px: i32,
    icon_column: [f32; 4],
    icon_y_offset_ratio: f32,
    frame_h: f32,
) -> [f32; 4] {
    let center_y = anchor_y_px as f32 / frame_h.max(1.0) - icon_y_offset_ratio;
    let y0 = (center_y - ICON_CELL_H_RATIO / 2.0).clamp(0.0, 1.0);
    let h = ICON_CELL_H_RATIO.min(1.0 - y0);
    [icon_column[0], y0, icon_column[2], h]
}

pub(super) fn crop_ratio_rect(rgba: &RgbaImage, [x, y, w, h]: [f32; 4]) -> RgbaImage {
    let (iw, ih) = (rgba.width() as f32, rgba.height() as f32);
    let x0 = (x * iw).round().clamp(0.0, iw - 1.0) as u32;
    let y0 = (y * ih).round().clamp(0.0, ih - 1.0) as u32;
    let cw = ((w * iw).round() as u32).clamp(1, rgba.width() - x0);
    let ch = ((h * ih).round() as u32).clamp(1, rgba.height() - y0);
    use image::GenericImageView;
    rgba.view(x0, y0, cw, ch).to_image()
}

/// One capture + parallel NCC pass against `targets`. Single source of
/// truth for "what's on screen in the shop right now" — used both by the
/// bot loop and the Setup-tab live preview worker. The colour check is
/// kept at the call site so the bot can `warn!` on rejections while the
/// Setup preview drops them silently.
pub(crate) fn scan_shop_raw(
    capture: &dyn Capture,
    detector: &Detector,
    targets: &[&'static str],
    shop_grid: [f32; 4],
) -> Result<ShopScanRaw> {
    use rayon::prelude::*;

    let rgba = capture.snapshot_rgba()?;
    let gray = image::imageops::grayscale(&rgba);
    let ctx = detector.prepare_search(&gray, Some(shop_grid));
    let raw: Vec<(&'static str, Result<Option<crate::detector::Hit>>)> = targets
        .par_iter()
        .map(|alias_name| (*alias_name, detector.find_in(&ctx, alias_name)))
        .collect();
    // Propagate the first NCC error rather than masking it — matches the
    // bot's prior behaviour where a `find_in` failure stops the round.
    let hits: Vec<(&'static str, Option<crate::detector::Hit>)> = raw
        .into_iter()
        .map(|(alias, r)| r.map(|h| (alias, h)))
        .collect::<Result<Vec<_>>>()?;
    Ok(ShopScanRaw { rgba, hits })
}

pub(crate) struct ShopScanRaw {
    pub rgba: RgbaImage,
    /// `None` = template not found by NCC. The colour-check pass that
    /// turns a "raw" hit into an actionable one stays at the call site.
    pub hits: Vec<(&'static str, Option<crate::detector::Hit>)>,
}

/// Patch grows with `hit.scale` so the hue histogram covers the whole
/// rendered icon at non-native scales.
pub(crate) fn crop_icon_patch(rgba: &RgbaImage, hit: &crate::detector::Hit) -> RgbaImage {
    // 40×40 covers the largest bundled icon (covenant 47×45).
    const FALLBACK_SIDE: u32 = 40;
    let scale = hit.scale.max(0.1);
    let w = ((FALLBACK_SIDE as f32) * scale).round().max(8.0) as u32;
    let h = w;
    let (img_w, img_h) = rgba.dimensions();
    let x0 = (hit.x - (w / 2) as i32).clamp(0, (img_w.saturating_sub(1)) as i32) as u32;
    let y0 = (hit.y - (h / 2) as i32).clamp(0, (img_h.saturating_sub(1)) as i32) as u32;
    let cw = w.min(img_w - x0);
    let ch = h.min(img_h - y0);
    use image::GenericImageView;
    rgba.view(x0, y0, cw, ch).to_image()
}

/// FxHasher because pixel data is non-adversarial — 2-3× faster than
/// std's SipHash on these tiny inputs.
pub(super) fn strip_hash(gray: &GrayImage, [x, y, w, h]: [f32; 4]) -> u64 {
    use std::hash::Hasher;
    let (sw, sh) = (gray.width(), gray.height());
    let x0 = (x * sw as f32)
        .round()
        .clamp(0.0, sw.saturating_sub(1) as f32) as u32;
    let y0 = (y * sh as f32)
        .round()
        .clamp(0.0, sh.saturating_sub(1) as f32) as u32;
    let w_px = (w * sw as f32).round().clamp(1.0, (sw as f32).max(1.0)) as u32;
    let h_px = (h * sh as f32).round().clamp(1.0, (sh as f32).max(1.0)) as u32;
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
    use image::GrayImage;

    #[test]
    fn scan_shop_rows_classifies_rows_via_anchor_and_cell() {
        use super::super::test_support::FakeCapture;
        use crate::capture::WindowRect;
        use image::imageops::FilterType;

        let (w, h) = (1000u32, 778u32);
        let decode = |bytes: &[u8]| {
            image::load_from_memory(bytes)
                .expect("bundled asset decodes")
                .into_rgba8()
        };
        // Buy button at its native 778-height size; icons resized to
        // their realistic on-screen size (the bundled crops are small).
        let button = decode(include_bytes!("../../assets/buy_button.png"));
        let mystic = image::imageops::resize(
            &decode(include_bytes!("../../assets/mystic_medal.png")),
            90,
            90,
            FilterType::Triangle,
        );
        let covenant = image::imageops::resize(
            &decode(include_bytes!("../../assets/covenant.png")),
            90,
            90,
            FilterType::Triangle,
        );

        let offset_ratio = 0.045f32;
        let dy = (offset_ratio * h as f32).round() as i64;
        let mut scene = RgbaImage::from_pixel(w, h, image::Rgba([25, 25, 35, 255]));
        let paste = |scene: &mut RgbaImage, img: &RgbaImage, cx: i64, cy: i64| {
            image::imageops::overlay(
                scene,
                img,
                cx - i64::from(img.width()) / 2,
                cy - i64::from(img.height()) / 2,
            );
        };
        // Three rows anchored in the buy column: mystic, covenant, and
        // one with an empty icon cell.
        for (row_y, icon) in [(300i64, Some(&mystic)), (500, Some(&covenant)), (650, None)] {
            paste(&mut scene, &button, 875, row_y);
            if let Some(icon) = icon {
                paste(&mut scene, icon, 500, row_y - dy);
            }
        }

        let gray_of = image::imageops::grayscale::<RgbaImage>;
        let detector =
            crate::detector::Detector::from_test_images(std::collections::HashMap::from([
                (alias::BUY_BUTTON, gray_of(&button)),
                (alias::MYSTIC_MEDAL, gray_of(&mystic)),
                (alias::COVENANT, gray_of(&covenant)),
            ]));
        let capture = FakeCapture::with_rgba(
            vec![],
            vec![scene],
            WindowRect {
                x: 0,
                y: 0,
                width: w,
                height: h,
            },
        );

        let rows = scan_shop_rows(
            &capture,
            &detector,
            &ColorVerifier::new(),
            [0.8, 0.0, 0.15, 1.0],
            [0.4, 0.0, 0.2, 1.0],
            offset_ratio,
        )
        .unwrap();

        assert_eq!(rows.len(), 3, "one row per pasted buy button");
        assert_eq!(rows[0].klass, Some(alias::MYSTIC_MEDAL));
        assert_eq!(rows[1].klass, Some(alias::COVENANT));
        assert_eq!(rows[2].klass, None, "empty cell must classify as None");
        assert!((rows[0].anchor.y - 300).abs() <= 2);
        assert!((rows[1].anchor.y - 500).abs() <= 2);
    }

    #[test]
    fn icon_cell_sits_above_the_anchor_by_the_offset() {
        let cell = icon_cell_for(500, [0.4, 0.0, 0.2, 1.0], 0.045, 1000.0);
        // Cell centre = 0.5 - 0.045 = 0.455.
        assert!((cell[1] + cell[3] / 2.0 - 0.455).abs() < 1e-3);
        assert_eq!(cell[0], 0.4);
        assert_eq!(cell[2], 0.2);
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
    fn strip_hash_handles_full_window_roi() {
        let img = checker(100, 100, 4);
        let _ = strip_hash(&img, [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn strip_hash_clamps_out_of_bounds_ratios() {
        let img = solid_gray(100, 100, 64);
        let _ = strip_hash(&img, [0.5, 0.5, 5.0, 5.0]);
    }

    #[test]
    fn strip_hash_does_not_panic_on_zero_dimension_frame() {
        // A degenerate 0×0 capture (minimized/occluded window) must not crash
        // the bot worker — strip_hash returns a stable hash instead of panicking.
        let zero_w = GrayImage::new(0, 10);
        let zero_h = GrayImage::new(10, 0);
        let zero_both = GrayImage::new(0, 0);
        let _ = strip_hash(&zero_w, [0.0, 0.0, 1.0, 1.0]);
        let _ = strip_hash(&zero_h, [0.0, 0.0, 1.0, 1.0]);
        let _ = strip_hash(&zero_both, [0.5, 0.5, 0.25, 0.25]);
    }
}
