use image::{GrayImage, RgbaImage};

use crate::capture::Capture;
use crate::detector::Detector;
use crate::error::Result;

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
