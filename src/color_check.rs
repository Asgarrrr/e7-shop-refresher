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

/// Absolute ceiling on the distance to the claimed reference. Kept
/// generous because a global screen tint (Windows Night Light, an ICC
/// profile, HDR) inflates *every* distance uniformly — same-icon
/// variation is 0.05–0.15 untinted but climbs past 0.5 under a strong
/// warm cast. The nearest-reference margin below does the real
/// cross-colour discrimination; this only drops near-grayscale or
/// wildly-off patches.
pub(crate) const DEFAULT_MATCH_THRESHOLD: f32 = 0.60;

/// The claimed reference must beat every other reference by at least
/// this much. A uniform tint moves all distances together, so the
/// *ranking* survives even when absolute values don't — this is what
/// keeps a green bookmark from passing as an orange medal.
pub(crate) const DEFAULT_MATCH_MARGIN: f32 = 0.05;

/// 0.05 lets a small icon-on-dark-background through (icon contributes
/// ~15-30% of the patch in practice) while still rejecting an empty
/// grayscale region.
const MIN_COLOURED_FRACTION: f32 = 0.05;

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
    threshold: f32,
    margin: f32,
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
        Self::with_thresholds(DEFAULT_MATCH_THRESHOLD, DEFAULT_MATCH_MARGIN)
    }

    pub fn with_thresholds(threshold: f32, margin: f32) -> Self {
        let mystic = decode_sig(MYSTIC_MEDAL_PNG);
        let covenant = decode_sig(COVENANT_PNG);
        Self {
            refs: vec![(alias::MYSTIC_MEDAL, mystic), (alias::COVENANT, covenant)],
            threshold,
            margin,
        }
    }

    /// Single source of truth — `accepts` and the debug overlay both
    /// derive from this so a threshold change ripples consistently.
    /// `None` for unknown aliases (filter is a no-op there).
    pub fn evaluate(&self, alias: &str, patch: &RgbaImage) -> Option<ColourReport> {
        let reference = self.ref_for(alias)?;
        let sig = HueSig::from_rgba(patch);
        let distance = sig.distance(reference);
        // Nearest competing reference. INFINITY when the claimed alias is
        // the only one — the margin test then reduces to the ceiling.
        let nearest_other = self
            .refs
            .iter()
            .filter(|(n, _)| *n != alias)
            .map(|(_, s)| sig.distance(s))
            .fold(f32::INFINITY, f32::min);
        let wins_by_margin = distance + self.margin <= nearest_other;
        let passed = sig.coloured_fraction >= MIN_COLOURED_FRACTION
            && distance <= self.threshold
            && wins_by_margin;
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

    /// Nearest-reference classification of an icon patch. `Some(alias)`
    /// only when the closest reference is inside the ceiling AND beats
    /// the runner-up by the margin; `None` for grayed-out, ambiguous or
    /// unknown icons — the caller must treat that as "don't buy".
    pub fn classify(&self, patch: &RgbaImage) -> Option<&'static str> {
        let sig = HueSig::from_rgba(patch);
        if sig.coloured_fraction < MIN_COLOURED_FRACTION {
            return None;
        }
        let mut dists: Vec<(&'static str, f32)> = self
            .refs
            .iter()
            .map(|(name, reference)| (*name, sig.distance(reference)))
            .collect();
        dists.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let (best, d0) = dists[0];
        let d1 = dists.get(1).map_or(f32::INFINITY, |d| d.1);
        (d0 <= self.threshold && d0 + self.margin <= d1).then_some(best)
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

    /// Approximates a warm screen cast (Night Light / warm ICC profile):
    /// boost red, crush blue, uniformly across the patch.
    fn warm_tint(img: &RgbaImage) -> RgbaImage {
        let mut out = img.clone();
        for px in out.pixels_mut() {
            let [r, g, b, a] = px.0;
            let r = (r as f32 * 1.2).min(255.0) as u8;
            let b = (b as f32 * 0.4) as u8;
            px.0 = [r, g, b, a];
        }
        out
    }

    #[test]
    fn warm_tint_still_accepts_real_medal() {
        let v = ColorVerifier::new();
        let mystic = image::load_from_memory(MYSTIC_MEDAL_PNG)
            .unwrap()
            .into_rgba8();
        let tinted = warm_tint(&mystic);
        let report = v.evaluate(alias::MYSTIC_MEDAL, &tinted).unwrap();
        // The whole point: the tint pushes it past the old 0.30 absolute
        // cut, yet the relative check still recognises it as the medal.
        assert!(
            report.distance > 0.30,
            "tint should move distance past the old absolute threshold (got {})",
            report.distance
        );
        assert!(
            report.passed,
            "relative check must still accept a tinted real medal (distance {}, fraction {})",
            report.distance, report.coloured_fraction
        );
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
    fn classify_recognises_each_bundled_reference() {
        let v = ColorVerifier::new();
        let mystic = image::load_from_memory(MYSTIC_MEDAL_PNG)
            .unwrap()
            .into_rgba8();
        assert_eq!(v.classify(&mystic), Some(alias::MYSTIC_MEDAL));
        let covenant = image::load_from_memory(COVENANT_PNG).unwrap().into_rgba8();
        assert_eq!(v.classify(&covenant), Some(alias::COVENANT));
    }

    #[test]
    fn classify_rejects_grayscale_and_off_palette_patches() {
        let v = ColorVerifier::new();
        // Grayed-out (sold) icon: no saturated pixels.
        assert_eq!(v.classify(&solid(64, 64, [128, 128, 128, 255])), None);
        // Unknown item colour, far from both references.
        assert_eq!(v.classify(&solid(64, 64, [40, 90, 220, 255])), None);
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
