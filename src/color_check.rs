//! Hue-histogram verification on top of NCC matches.
//!
//! NCC on small grayscale templates (~37×37 px) gets cross-colour false
//! positives — orange medal correlating with green friendship bookmark
//! at score 0.94 because both have similar brightness gradients.
//! Grayscale throws away the cue (colour) that would disambiguate.
//!
//! After an NCC hit, we crop the matched RGBA patch, build a hue
//! histogram (saturation-weighted to ignore grays), and reject if the
//! distribution is too far from the bundled reference. Cheap (~µs) and
//! zero cross-colour error on E7's icon set.

use image::RgbaImage;

use crate::detector::alias;

/// 12 buckets × 30° each — separates the shop icons (orange / pink /
/// green) without picking up anti-aliasing wobble at icon edges.
const HUE_BUCKETS: usize = 12;

/// Below this, hue is meaningless and would dilute the distribution
/// toward a uniform 1/12 across buckets.
const MIN_SATURATION: f32 = 0.25;

/// Reference PNGs with masked corners, or live slot backgrounds.
const MIN_ALPHA: u8 = 32;

/// Same-icon variation typically 0.05–0.15, cross-colour > 0.4 — 0.30
/// leaves a comfortable margin on both sides.
const MATCH_THRESHOLD: f32 = 0.30;

const MYSTIC_MEDAL_PNG: &[u8] = include_bytes!("../assets/mystic_medal.png");
const COVENANT_PNG: &[u8] = include_bytes!("../assets/covenant.png");

/// `coloured_fraction` kept around so a near-grayscale patch (e.g.
/// sold-out greyed icon) is rejected instead of drifting toward
/// arbitrary buckets.
#[derive(Debug, Clone)]
pub struct HueSig {
    buckets: [f32; HUE_BUCKETS],
    coloured_fraction: f32,
}

impl HueSig {
    pub fn from_rgba(img: &RgbaImage) -> Self {
        let mut buckets = [0f32; HUE_BUCKETS];
        let mut coloured = 0u32;
        let mut total = 0u32;
        for px in img.pixels() {
            let [r, g, b, a] = px.0;
            if a < MIN_ALPHA {
                continue;
            }
            total += 1;
            let (h, s, _v) = rgb_to_hsv(r, g, b);
            if s < MIN_SATURATION {
                continue;
            }
            coloured += 1;
            let bucket = (((h / 360.0) * HUE_BUCKETS as f32) as usize).min(HUE_BUCKETS - 1);
            buckets[bucket] += 1.0;
        }
        if coloured > 0 {
            let inv = 1.0 / coloured as f32;
            for b in &mut buckets {
                *b *= inv;
            }
        }
        let coloured_fraction = if total == 0 {
            0.0
        } else {
            coloured as f32 / total as f32
        };
        Self {
            buckets,
            coloured_fraction,
        }
    }

    /// Bhattacharyya distance, `[0, 1]` — 0 identical, 1 no overlap.
    pub fn distance(&self, other: &Self) -> f32 {
        let bc: f32 = self
            .buckets
            .iter()
            .zip(other.buckets.iter())
            .map(|(a, b)| (a * b).sqrt())
            .sum();
        (1.0 - bc).clamp(0.0, 1.0).sqrt()
    }
}

/// Pre-computes reference sigs at startup — per-match check is a
/// single histogram + distance op (~50 µs on a 64×64 patch).
pub struct ColorVerifier {
    refs: Vec<(&'static str, HueSig)>,
}

#[derive(Debug, Clone, Copy)]
pub struct ColourReport {
    pub passed: bool,
    pub distance: f32,
    pub coloured_fraction: f32,
}

impl Default for ColorVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl ColorVerifier {
    pub fn new() -> Self {
        let mystic = decode_sig(MYSTIC_MEDAL_PNG);
        let covenant = decode_sig(COVENANT_PNG);
        Self {
            refs: vec![(alias::MYSTIC_MEDAL, mystic), (alias::COVENANT, covenant)],
        }
    }

    /// Single source of truth — `accepts` and the debug overlay both
    /// derive from this so a threshold change ripples consistently.
    /// `None` for unknown aliases (filter is a no-op there).
    pub fn evaluate(&self, alias: &str, patch: &RgbaImage) -> Option<ColourReport> {
        let reference = self.ref_for(alias)?;
        let sig = HueSig::from_rgba(patch);
        let distance = sig.distance(reference);
        // 0.05 lets a small icon-on-dark-background through (icon
        // contributes ~15-30% of the patch in practice) while still
        // rejecting an empty grayscale region.
        let passed = sig.coloured_fraction >= 0.05 && distance <= MATCH_THRESHOLD;
        Some(ColourReport {
            passed,
            distance,
            coloured_fraction: sig.coloured_fraction,
        })
    }

    /// `true` for unknown aliases — safe to call unconditionally.
    pub fn accepts(&self, alias: &str, patch: &RgbaImage) -> bool {
        self.evaluate(alias, patch).is_none_or(|r| r.passed)
    }

    fn ref_for(&self, alias: &str) -> Option<&HueSig> {
        self.refs.iter().find(|(n, _)| *n == alias).map(|(_, s)| s)
    }
}

fn decode_sig(bytes: &[u8]) -> HueSig {
    let img = image::load_from_memory(bytes)
        .expect("bundled asset must decode")
        .into_rgba8();
    HueSig::from_rgba(&img)
}

/// Returns `(h, s, v)` — hue in `[0, 360)`, saturation + value in `[0, 1]`.
fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;

    let h = if delta < 1e-5 {
        0.0
    } else if (max - r).abs() < 1e-5 {
        60.0 * (((g - b) / delta).rem_euclid(6.0))
    } else if (max - g).abs() < 1e-5 {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let h = if h < 0.0 { h + 360.0 } else { h };
    let s = if max < 1e-5 { 0.0 } else { delta / max };
    (h, s, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn solid(w: u32, h: u32, rgba: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba(rgba))
    }

    #[test]
    fn reference_matches_itself() {
        let v = ColorVerifier::new();
        let mystic = image::load_from_memory(MYSTIC_MEDAL_PNG)
            .unwrap()
            .into_rgba8();
        assert!(v.accepts(alias::MYSTIC_MEDAL, &mystic));
        let covenant = image::load_from_memory(COVENANT_PNG).unwrap().into_rgba8();
        assert!(v.accepts(alias::COVENANT, &covenant));
    }

    #[test]
    fn cross_colour_is_rejected() {
        let v = ColorVerifier::new();
        // Pure green patch — should NOT match the orange-ish mystic medal.
        let green = solid(64, 64, [0, 200, 50, 255]);
        assert!(!v.accepts(alias::MYSTIC_MEDAL, &green));
        // Pure orange patch — should NOT match the pink-ish covenant.
        let orange = solid(64, 64, [220, 130, 30, 255]);
        assert!(!v.accepts(alias::COVENANT, &orange));
    }

    #[test]
    fn grayscale_patch_is_rejected() {
        // Solid gray = no saturated pixels = coloured_fraction 0.
        let v = ColorVerifier::new();
        let gray = solid(64, 64, [128, 128, 128, 255]);
        assert!(!v.accepts(alias::MYSTIC_MEDAL, &gray));
        assert!(!v.accepts(alias::COVENANT, &gray));
    }

    #[test]
    fn unknown_alias_passes_through() {
        let v = ColorVerifier::new();
        let any = solid(16, 16, [255, 255, 255, 255]);
        assert!(v.accepts("not_a_real_alias", &any));
    }

    #[test]
    fn rgb_to_hsv_matches_canonical_values() {
        // Red.
        let (h, s, v) = rgb_to_hsv(255, 0, 0);
        assert!((h - 0.0).abs() < 0.1);
        assert!((s - 1.0).abs() < 0.01);
        assert!((v - 1.0).abs() < 0.01);

        // Green.
        let (h, _, _) = rgb_to_hsv(0, 255, 0);
        assert!((h - 120.0).abs() < 0.1);

        // Blue.
        let (h, _, _) = rgb_to_hsv(0, 0, 255);
        assert!((h - 240.0).abs() < 0.1);

        // Gray (saturation 0 — hue is undefined but we return 0).
        let (_, s, _) = rgb_to_hsv(128, 128, 128);
        assert!(s < 0.01);
    }

    #[test]
    fn huesig_distance_is_zero_for_identical_inputs() {
        let img = solid(32, 32, [200, 100, 30, 255]);
        let a = HueSig::from_rgba(&img);
        let b = HueSig::from_rgba(&img);
        assert!(a.distance(&b) < 0.01);
    }

    #[test]
    fn huesig_distance_is_large_for_opposite_hues() {
        let red = solid(32, 32, [255, 0, 0, 255]);
        let cyan = solid(32, 32, [0, 255, 255, 255]);
        let a = HueSig::from_rgba(&red);
        let b = HueSig::from_rgba(&cyan);
        // Bhattacharyya between disjoint single-bucket distributions
        // tops out at 1.0.
        assert!(a.distance(&b) > 0.9);
    }
}
