use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use image::imageops::FilterType;
use image::{GrayImage, Luma};
use imageproc::definitions::Image;
use imageproc::template_matching::{MatchTemplateMethod, match_template_parallel};
use tracing::{debug, trace, warn};

use crate::capture::Capture;
use crate::config::Config;
use crate::error::{Error, Result};

/// Compile-time alias constants for `Detector::find`. Fixed-position
/// buttons are clicked via `zones`, not matched, so they have no aliases.
pub mod alias {
    pub const ANCHOR_SHOP: &str = "anchor_shop";
    pub const MYSTIC_MEDAL: &str = "mystic_medal";
    pub const COVENANT: &str = "covenant";
}

#[derive(Debug, Clone, Copy)]
pub struct Hit {
    /// Center of the match in window-local pixels.
    pub x: i32,
    pub y: i32,
    pub score: f32,
    /// `window_scale × extra_scale` of the matching template.
    pub scale: f32,
    /// Best minus second-best NCC peak; low margin = ambiguous match.
    pub margin: f32,
}

/// 4× downsample for the coarse pyramid level: NCC at 1/16 the pixel
/// count, refined later at full res over a small window.
const PYRAMID_FACTOR: u32 = 4;

/// Coarse-pixel slack on each side of the coarse peak when cropping the
/// refine window. 2 × PYRAMID_FACTOR = 8 full-res pixels of wiggle room
/// per side, enough to absorb downsample-induced peak drift.
const REFINE_SLACK_COARSE_PX: u32 = 2;

/// `coarse_threshold = threshold × ratio`. Downsampling smears the
/// correlation peak so the coarse score is always lower than full-res.
const COARSE_THRESHOLD_RATIO: f32 = 0.70;

struct ScaledTemplate {
    image: GrayImage,
    coarse: GrayImage,
    scale: f32,
}

impl std::fmt::Debug for ScaledTemplate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScaledTemplate")
            .field("scale", &self.scale)
            .field("width", &self.image.width())
            .field("height", &self.image.height())
            .field("coarse_w", &self.coarse.width())
            .field("coarse_h", &self.coarse.height())
            .finish()
    }
}

pub struct Detector {
    threshold: f32,
    margin: f32,
    templates: HashMap<String, Vec<ScaledTemplate>>,
}

impl Detector {
    pub fn new(config: &Config, current_size: (u32, u32)) -> Result<Self> {
        // base_resolution = size the templates were last cropped against
        // (auto-written by the GUI's Crop & Save). The live window may be
        // at a different size, in which case templates are resampled to
        // match — without this, resize-then-reopen silently breaks NCC.
        let base = config.window.base_resolution;
        let global_scale = current_size.1 as f32 / base[1].max(1) as f32;
        debug!(
            base_w = base[0],
            base_h = base[1],
            cur_w = current_size.0,
            cur_h = current_size.1,
            global_scale,
            "computing template scales"
        );

        let t = &config.templates;
        let entries = [
            (alias::ANCHOR_SHOP, &t.anchor_shop),
            (alias::MYSTIC_MEDAL, &t.mystic_medal),
            (alias::COVENANT, &t.covenant),
        ];

        let mut templates = HashMap::with_capacity(entries.len());
        for (alias_name, file) in entries {
            let path = t.dir.join(file);
            let scaled = load_scaled(&path, global_scale, &config.matching.extra_scales)?;
            templates.insert(alias_name.to_string(), scaled);
        }

        Ok(Self {
            threshold: config.matching.threshold,
            margin: config.matching.margin,
            templates,
        })
    }

    pub fn template_count(&self) -> usize {
        self.templates.len()
    }

    /// Test-only: build a Detector from in-memory templates, skipping
    /// the disk I/O `new()` does.
    #[cfg(test)]
    pub fn from_test_images(images: HashMap<&'static str, GrayImage>) -> Self {
        let mut templates = HashMap::with_capacity(images.len());
        for (name, image) in images {
            let coarse = build_coarse(&image);
            let scaled = vec![ScaledTemplate {
                image,
                coarse,
                scale: 1.0,
            }];
            templates.insert(name.to_string(), scaled);
        }
        Self {
            threshold: 0.9,
            margin: 0.05,
            templates,
        }
    }

    /// Native-scale `(w, h)`. Used by the GUI overlay to draw an accurate
    /// bbox around each match.
    pub fn template_dimensions(&self, alias: &str) -> Option<(u32, u32)> {
        let templates = self.templates.get(alias)?;
        let first = templates.first()?;
        Some((first.image.width(), first.image.height()))
    }

    /// Best hit for `alias` inside `roi` (ratios in [0, 1]).
    ///
    /// 2-level coarse-to-fine pyramid:
    /// 1. NCC at 1/4 resolution to locate a candidate peak (16× cheaper).
    /// 2. NCC at full resolution over a small window to refine to pixel
    ///    precision.
    ///
    /// Returns `None` below threshold or on an ambiguous match (low margin).
    pub fn find(
        &self,
        frame: &GrayImage,
        alias: &str,
        roi: Option<[f32; 4]>,
    ) -> Result<Option<Hit>> {
        let templates = self
            .templates
            .get(alias)
            .ok_or_else(|| Error::UnknownTemplate(alias.into()))?;

        let (ox, oy, search) = crop_for_search(frame, roi);
        let search_ref: &GrayImage = &search;

        // Built once per find(), reused across all template scales.
        let coarse_search = build_coarse(search_ref);
        let coarse_threshold = self.threshold * COARSE_THRESHOLD_RATIO;

        let mut best: Option<Hit> = None;
        for tpl in templates {
            if tpl.image.width() >= search_ref.width()
                || tpl.image.height() >= search_ref.height()
                || tpl.coarse.width() >= coarse_search.width()
                || tpl.coarse.height() >= coarse_search.height()
            {
                trace!(
                    alias,
                    scale = tpl.scale,
                    "template too large for search area"
                );
                continue;
            }

            let coarse_scores = match_template_parallel(
                &coarse_search,
                &tpl.coarse,
                MatchTemplateMethod::CrossCorrelationNormalized,
            );
            let (coarse_max, coarse_loc, _) =
                top_with_separation(&coarse_scores, tpl.coarse.width().min(tpl.coarse.height()));
            if coarse_max < coarse_threshold {
                trace!(alias, coarse = coarse_max, "coarse pass below threshold");
                continue;
            }

            let Some((refine_ox, refine_oy, refine_area)) =
                refine_crop(search_ref, &tpl.image, coarse_loc)
            else {
                continue;
            };
            let fine_scores = match_template_parallel(
                &refine_area,
                &tpl.image,
                MatchTemplateMethod::CrossCorrelationNormalized,
            );
            let (max_val, max_loc, second_val) =
                top_with_separation(&fine_scores, tpl.image.width().min(tpl.image.height()));

            if max_val < self.threshold {
                continue;
            }

            let margin = max_val - second_val;
            if margin < self.margin && second_val.is_finite() {
                trace!(
                    alias,
                    score = max_val,
                    second = second_val,
                    margin,
                    "ambiguous match (margin too small)"
                );
                continue;
            }

            // Edge check is against the FULL search area, not the refine
            // window — only the full frame has meaningful boundaries.
            let abs_loc = (refine_ox + max_loc.0, refine_oy + max_loc.1);
            if hit_touches_edge(abs_loc, tpl.image.dimensions(), search_ref.dimensions()) {
                trace!(alias, "hit at search-area edge, likely partial — rejecting");
                continue;
            }

            if best.is_some_and(|b| b.score > max_val) {
                continue;
            }

            let center_x = ox + abs_loc.0 as i32 + (tpl.image.width() / 2) as i32;
            let center_y = oy + abs_loc.1 as i32 + (tpl.image.height() / 2) as i32;
            trace!(
                alias,
                x = center_x,
                y = center_y,
                score = max_val,
                coarse_score = coarse_max,
                scale = tpl.scale,
                margin,
                "hit"
            );
            best = Some(Hit {
                x: center_x,
                y: center_y,
                score: max_val,
                scale: tpl.scale,
                margin,
            });

            // Comfortable hit → skip the remaining scales.
            if max_val >= (self.threshold + 0.05).min(1.0) {
                break;
            }
        }

        Ok(best)
    }

    /// Block until `alias` is found inside `roi`, polling every `poll`.
    /// `stop` is polled at every snapshot and every ~60 ms inside the
    /// poll sleep, so Stop is responsive even mid-wait.
    pub fn wait_for(
        &self,
        capture: &dyn Capture,
        alias: &str,
        roi: Option<[f32; 4]>,
        timeout: Duration,
        poll: Duration,
        stop: &AtomicBool,
    ) -> Result<Hit> {
        let deadline = Instant::now() + timeout;
        loop {
            if stop.load(Ordering::Relaxed) {
                return Err(Error::AnchorTimeout(alias.into()));
            }
            let gray = capture.snapshot_gray()?;
            if let Some(hit) = self.find(&gray, alias, roi)? {
                return Ok(hit);
            }
            if Instant::now() >= deadline {
                return Err(Error::AnchorTimeout(alias.into()));
            }
            let chunk = Duration::from_millis(60);
            let until = Instant::now() + poll;
            while Instant::now() < until {
                if stop.load(Ordering::Relaxed) {
                    return Err(Error::AnchorTimeout(alias.into()));
                }
                let remaining = until.saturating_duration_since(Instant::now());
                thread::sleep(chunk.min(remaining));
            }
        }
    }
}

fn load_scaled(path: &Path, global_scale: f32, extra: &[f32]) -> Result<Vec<ScaledTemplate>> {
    let raw = image::open(path)?;
    let gray = raw.into_luma8();
    // Try scale=1.0 first so the threshold-comfortable early-out fires
    // on the most likely match. Cuts ~1/3 of the work on the fast path.
    let mut sorted_extra: Vec<f32> = extra.to_vec();
    sorted_extra.sort_by(|a, b| {
        (a - 1.0)
            .abs()
            .partial_cmp(&(b - 1.0).abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let effective: Vec<f32> = sorted_extra.iter().map(|s| s * global_scale).collect();

    let mut scaled = Vec::with_capacity(effective.len());
    for s in effective {
        let w = ((gray.width() as f32) * s).round().max(1.0) as u32;
        let h = ((gray.height() as f32) * s).round().max(1.0) as u32;
        let image = if w == gray.width() && h == gray.height() {
            gray.clone()
        } else {
            image::imageops::resize(&gray, w, h, FilterType::Triangle)
        };
        let coarse = build_coarse(&image);
        scaled.push(ScaledTemplate {
            image,
            coarse,
            scale: s,
        });
    }
    if scaled.is_empty() {
        warn!(?path, "no scales produced for template");
    }
    Ok(scaled)
}

/// Lanczos3 to limit aliasing: NCC at low resolution already smears the
/// peak, aliasing would smear it further.
fn build_coarse(full: &GrayImage) -> GrayImage {
    let cw = (full.width() / PYRAMID_FACTOR).max(2);
    let ch = (full.height() / PYRAMID_FACTOR).max(2);
    image::imageops::resize(full, cw, ch, FilterType::Lanczos3)
}

fn refine_crop(
    search: &GrayImage,
    template: &GrayImage,
    coarse_loc: (u32, u32),
) -> Option<(u32, u32, GrayImage)> {
    let (tw, th) = template.dimensions();
    let (sw, sh) = search.dimensions();
    let slack = REFINE_SLACK_COARSE_PX * PYRAMID_FACTOR;
    let cx_full = coarse_loc.0 * PYRAMID_FACTOR;
    let cy_full = coarse_loc.1 * PYRAMID_FACTOR;
    let refine_x = cx_full.saturating_sub(slack);
    let refine_y = cy_full.saturating_sub(slack);
    let refine_w = (tw + 2 * slack).min(sw.saturating_sub(refine_x));
    let refine_h = (th + 2 * slack).min(sh.saturating_sub(refine_y));
    // match_template requires search > template strictly — anything
    // smaller would panic.
    if refine_w <= tw || refine_h <= th {
        return None;
    }
    let cropped =
        image::imageops::crop_imm(search, refine_x, refine_y, refine_w, refine_h).to_image();
    Some((refine_x, refine_y, cropped))
}

fn crop_for_search(frame: &GrayImage, roi: Option<[f32; 4]>) -> (i32, i32, Cow<'_, GrayImage>) {
    let (sw, sh) = (frame.width(), frame.height());
    let Some([rx, ry, rw, rh]) = roi else {
        return (0, 0, Cow::Borrowed(frame));
    };
    let x = (rx * sw as f32).round().clamp(0.0, sw as f32 - 1.0) as u32;
    let y = (ry * sh as f32).round().clamp(0.0, sh as f32 - 1.0) as u32;
    let w = (rw * sw as f32).round() as u32;
    let h = (rh * sh as f32).round() as u32;
    let w = w.min(sw.saturating_sub(x)).max(1);
    let h = h.min(sh.saturating_sub(y)).max(1);
    let cropped = image::imageops::crop_imm(frame, x, y, w, h).to_image();
    (x as i32, y as i32, Cow::Owned(cropped))
}

/// Single-pass max + runner-up with a `sep × sep` exclusion zone around
/// the top peak (so adjacent pixels of the same peak aren't picked).
fn top_with_separation(scores: &Image<Luma<f32>>, sep: u32) -> (f32, (u32, u32), f32) {
    let mut top_val = f32::NEG_INFINITY;
    let mut top_loc = (0u32, 0u32);
    for (x, y, p) in scores.enumerate_pixels() {
        if p[0] > top_val {
            top_val = p[0];
            top_loc = (x, y);
        }
    }

    let mut second = f32::NEG_INFINITY;
    let (mx, my) = (top_loc.0 as i32, top_loc.1 as i32);
    let sep_i = sep as i32;
    for (x, y, p) in scores.enumerate_pixels() {
        let dx = (x as i32 - mx).abs();
        let dy = (y as i32 - my).abs();
        if dx < sep_i && dy < sep_i {
            continue;
        }
        if p[0] > second {
            second = p[0];
        }
    }
    (top_val, top_loc, second)
}

fn hit_touches_edge(max_loc: (u32, u32), tpl: (u32, u32), search: (u32, u32)) -> bool {
    let margin: u32 = 1;
    let (mx, my) = max_loc;
    let (tw, th) = tpl;
    let (sw, sh) = search;
    mx <= margin || my <= margin || mx + tw + margin >= sw || my + th + margin >= sh
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Luma};

    fn make_scores(w: u32, h: u32, peaks: &[(u32, u32, f32)]) -> Image<Luma<f32>> {
        let mut img: Image<Luma<f32>> = ImageBuffer::from_pixel(w, h, Luma([0.0]));
        for &(x, y, v) in peaks {
            img.put_pixel(x, y, Luma([v]));
        }
        img
    }

    #[test]
    fn top_with_separation_finds_global_max() {
        let scores = make_scores(10, 10, &[(3, 4, 0.9), (7, 7, 0.5)]);
        let (top, loc, _) = top_with_separation(&scores, 2);
        assert!((top - 0.9).abs() < 1e-6);
        assert_eq!(loc, (3, 4));
    }

    #[test]
    fn top_with_separation_skips_peaks_inside_exclusion_box() {
        let scores = make_scores(20, 20, &[(10, 10, 0.9), (11, 10, 0.8), (16, 16, 0.4)]);
        let (_, _, second) = top_with_separation(&scores, 4);
        assert!((second - 0.4).abs() < 1e-6);
    }

    #[test]
    fn top_with_separation_returns_neg_infinity_if_no_runner_up() {
        let scores = make_scores(5, 5, &[(2, 2, 1.0)]);
        let (_, _, second) = top_with_separation(&scores, 10);
        assert!(second.is_infinite() && second.is_sign_negative());
    }

    #[test]
    fn build_coarse_keeps_min_2px_dimensions() {
        // 7×7 / PYRAMID_FACTOR=4 = 1 without the clamp → match_template rejects.
        let full = GrayImage::from_pixel(7, 7, Luma([200]));
        let coarse = build_coarse(&full);
        assert!(coarse.width() >= 2);
        assert!(coarse.height() >= 2);
    }

    #[test]
    fn crop_for_search_passes_through_when_no_roi() {
        let frame = GrayImage::from_pixel(50, 50, Luma([10]));
        let (ox, oy, owned) = crop_for_search(&frame, None);
        assert_eq!((ox, oy), (0, 0));
        assert_eq!(owned.dimensions(), (50, 50));
    }

    #[test]
    fn crop_for_search_offsets_match_roi_origin() {
        let frame = GrayImage::from_pixel(100, 100, Luma([0]));
        let (ox, oy, owned) = crop_for_search(&frame, Some([0.2, 0.3, 0.5, 0.4]));
        assert_eq!(ox, 20);
        assert_eq!(oy, 30);
        assert_eq!(owned.dimensions(), (50, 40));
    }

    #[test]
    fn hit_touches_edge_detects_close_to_left() {
        assert!(hit_touches_edge((0, 5), (10, 10), (50, 50)));
        assert!(!hit_touches_edge((20, 20), (10, 10), (50, 50)));
    }

    #[test]
    fn hit_touches_edge_detects_close_to_bottom_right() {
        assert!(hit_touches_edge((40, 5), (10, 10), (50, 50)));
        assert!(hit_touches_edge((5, 40), (10, 10), (50, 50)));
    }
}
