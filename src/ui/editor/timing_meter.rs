//! The Click-timing per-action bars: a draggable meter on a shared time ruler.
//! Self-contained custom-painter widget — a fixed baseline segment plus the
//! player's random-extra slack — lifted out of the editor shell.

use eframe::egui;

use super::super::theme;
use crate::actuator::plan::{self, DelayRange};

/// Names the two segments of every meter so the bars read at a glance: a muted
/// swatch for the fixed tuned wait, a bright one for the random extra.
pub(super) fn timing_legend(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        legend_swatch(ui, theme::METER_BASE, "fixed tuned wait");
        ui.add_space(theme::SP_XL);
        legend_swatch(ui, theme::ACCENT, "random extra");
    });
}

/// One legend entry: a small rounded colour chip followed by its label.
fn legend_swatch(ui: &mut egui::Ui, color: egui::Color32, label: &str) {
    let (chip, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(chip, egui::CornerRadius::same(3), color);
    ui.weak(label);
}

/// The meter height and the fixed time ruler the bars sit on. The ruler is
/// constant (not fitted to the values) so a bar's length is a stable reading of
/// its wait and every row compares on the same scale; it clears the longest
/// baseline with room to drag real slack on top. The width is the row's — the
/// bars fill to the content edge, aligned under a fixed label column.
const METER_H: f32 = 22.0;
/// The ruler's length, kept in ms so the baselines can be checked against it at
/// compile time; the float twin is the only value the painting math uses.
const RULER_MS_U64: u64 = 2_500;
const RULER_MS: f32 = RULER_MS_U64 as f32;

// Tripwires for the ruler's invariant: every tuned baseline `fine_tune_body`
// paints fits under it. The slack math subtracts the baseline from the ruler, so
// a constant grown past it (a game patch lengthening an animation) leaves an
// empty bar with its grip pinned at zero — and, without the total clamp in
// `slack_from_target`, an `f32::clamp` panic inside an egui interaction, taking
// the whole window down. Raise `RULER_MS_U64` when one of these is retuned.
const _: () = assert!(plan::WAIT_SHOP_OPENED_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_REFRESHED_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_CONFIRM_REFRESH_MODAL_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_BUY_MODAL_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_BETWEEN_BUYS_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_SCROLL_SETTLE_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_PURCHASE_RESUMED_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_RECOVERY_MS <= RULER_MS_U64);
/// The label column: wide enough for the longest action name ("watchdog
/// re-issue") so every bar starts at the same x. Painted in an exact-size box so
/// a long label can never grow the column and shove its bar out of alignment.
const LABEL_W: f32 = 150.0;
/// The resolved-time column to the right of every bar: fixed so the values form
/// an aligned column and never sit over the bar or its grip.
const VALUE_W: f32 = 96.0;

/// One phase of the timing tab: a small-caps header over its bars.
pub(super) fn timing_group(
    ui: &mut egui::Ui,
    title: &str,
    rows: &mut [(&str, &mut DelayRange, u64)],
) {
    ui.label(theme::section(title));
    ui.add_space(theme::SP_XS);
    for (label, value, baseline) in rows.iter_mut() {
        timing_row(ui, label, value, *baseline);
    }
    ui.add_space(theme::SP_SM);
}

/// One action row: a fixed-width label column, the bar filling the middle, and a
/// fixed-width resolved-time column on the right — so every bar aligns, and the
/// values line up in their own column instead of floating over the bars.
fn timing_row(ui: &mut egui::Ui, label: &str, value: &mut DelayRange, baseline: u64) {
    ui.horizontal(|ui| {
        // Exact-size box + painted label: a plain `ui.label` grows its cell to
        // the text, so the longest name would push its bar right of the others.
        // Allocating the column and painting into it pins every bar's start.
        let (label_rect, _) =
            ui.allocate_exact_size(egui::vec2(LABEL_W, METER_H), egui::Sense::hover());
        ui.painter().with_clip_rect(label_rect).text(
            egui::pos2(label_rect.left(), label_rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::new(14.0, egui::FontFamily::Proportional),
            theme::INK_MUTED,
        );
        let bar_w = (ui.available_width() - VALUE_W - theme::SP_SM).max(80.0);
        timing_meter(ui, bar_w, baseline, value);
        ui.allocate_ui_with_layout(
            egui::vec2(VALUE_W, METER_H),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.monospace(resolved_band(baseline, value));
            },
        );
    });
}

/// One action's bar: a draggable meter on the shared ruler. The tuned baseline
/// is a muted fixed segment; dragging past it grows the bright random-extra
/// segment (the `max` of the range, drawn fresh in `[min, max]` at runtime).
/// Drag-to-set replaces the old min/max boxes — one gesture, and the bar is the
/// control. The resolved wait is shown in the row's value column, not inside, so
/// the grip never cuts through it.
fn timing_meter(ui: &mut egui::Ui, width: f32, baseline: u64, value: &mut DelayRange) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, METER_H), egui::Sense::click_and_drag());

    // Drag or click sets the random extra: the pointer's x is the target total
    // wait on the ruler, and the slack is whatever sits past the fixed baseline
    // (never negative, never past the ruler's end).
    if let Some(pos) = response.interact_pointer_pos() {
        let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        value.max_ms = slack_from_target(frac * RULER_MS, baseline);
        // Keep the invariant a config floor could otherwise break: min never
        // exceeds the max the player just set.
        value.min_ms = value.min_ms.min(value.max_ms);
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }

    let painter = ui.painter().clone();
    let radius = egui::CornerRadius::same(4);
    painter.rect_filled(rect, radius, theme::HAIRLINE);
    // Saturating: `DelayRange` itself carries no invariant, and the engine
    // deliberately supports a `max_ms` up to `u64::MAX` (`DelayRange::draw`
    // saturates too). The loader's `validate_timings` bounds what `config.toml`
    // may seed, but the type it produces does not, so the widget must still cope
    // with any `u64`. A plain `+` here would panic in debug and, worse, wrap
    // silently in release — painting a full-ruler wait as a 0.3% sliver.
    let total = baseline.saturating_add(value.max_ms.max(value.min_ms));
    let base_w = rect.width() * (baseline as f32 / RULER_MS);
    let total_w = rect.width() * (total as f32 / RULER_MS);
    painter.rect_filled(
        egui::Rect::from_min_size(rect.min, egui::vec2(base_w, rect.height())),
        radius,
        theme::METER_BASE,
    );
    if total_w > base_w {
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(rect.left() + base_w, rect.top()),
                egui::pos2(rect.left() + total_w, rect.bottom()),
            ),
            radius,
            theme::ACCENT,
        );
    }
    // Faint second-marks so the empty right of a bar reads as a time ruler you
    // drag along, not dead space. Painted over the fills, subtle enough not to
    // compete with them.
    for mark_ms in [1_000.0_f32, 2_000.0] {
        let x = rect.left() + rect.width() * (mark_ms / RULER_MS);
        painter.vline(
            x,
            rect.y_range(),
            egui::Stroke::new(1.0, egui::Color32::from_white_alpha(18)),
        );
    }
    // The grip sits at the draggable edge (the end of the slack, or the baseline
    // when there is none) — a bright cap that reads as "grab here to add slack".
    let grip_x = (rect.left() + total_w).clamp(rect.left() + 2.0, rect.right() - 2.0);
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(grip_x - 2.0, rect.top() + 2.0),
            egui::pos2(grip_x + 2.0, rect.bottom() - 2.0),
        ),
        egui::CornerRadius::same(2),
        theme::INK,
    );
}

/// The random extra a pointer landing at `target_ms` on the ruler sets: whatever
/// sits past the fixed baseline, never negative and never past the ruler's end.
///
/// The upper bound is floored at zero on purpose. `f32::clamp` asserts
/// `min <= max`, so `RULER_MS - baseline` alone panics the moment a tuned
/// baseline grows past the ruler — inside an egui interaction, i.e. the whole
/// window. The const asserts above make that unreachable today; this keeps the
/// clamp total anyway, since the two guards protect different edits.
fn slack_from_target(target_ms: f32, baseline: u64) -> u64 {
    let headroom = (RULER_MS - baseline as f32).max(0.0);
    // The `as u64` is range-verified rather than lucky: `target_ms` is
    // `frac * RULER_MS` where `frac` came from a division by a bar width floored
    // at 80.0 (`editor/mod.rs`), so it is finite — no `NaN`, which `as` would map
    // to 0 — and the `clamp` bounds the result to `0..=RULER_MS`, well inside
    // `u64`. Both guards are needed: the clamp alone would still pass a `NaN`
    // through, and finiteness alone would not bound the value.
    (target_ms - baseline as f32).clamp(0.0, headroom).round() as u64
}

/// The resolved wait the game will actually take: `baseline + min` to
/// `baseline + max`, in seconds. A point range (no slack, or a reversed one)
/// collapses to a single figure, matching the draw.
fn resolved_band(baseline: u64, value: &DelayRange) -> String {
    // Saturating for the same reason as the bar's `total`: `DelayRange` carries
    // no invariant of its own, and the engine honours a `u64::MAX` wait.
    let lo = baseline.saturating_add(value.min_ms);
    let hi = baseline.saturating_add(value.max_ms.max(value.min_ms));
    secs_range(lo, hi)
}

/// `lo..hi` milliseconds as seconds; a zero-width range shows one figure. The
/// one place the timing UI turns ms into the `x.xx s` / `x.xx–y.yy s` reading,
/// shared by the per-action bars and the routine total.
pub(super) fn secs_range(lo_ms: u64, hi_ms: u64) -> String {
    if lo_ms == hi_ms {
        format!("{:.2} s", lo_ms as f64 / 1000.0)
    } else {
        format!(
            "{:.2}–{:.2} s",
            lo_ms as f64 / 1000.0,
            hi_ms as f64 / 1000.0
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_band_saturates_on_an_unbounded_max() {
        // `DelayRange` carries no invariant — the loader bounds what config.toml
        // may seed, the type does not — and the engine honours a full-range
        // wait, so the display must saturate, not overflow.
        let value = DelayRange {
            min_ms: 0,
            max_ms: u64::MAX,
        };
        assert_eq!(
            resolved_band(plan::WAIT_SHOP_OPENED_MS, &value),
            secs_range(plan::WAIT_SHOP_OPENED_MS, u64::MAX)
        );
    }

    #[test]
    fn slack_is_clamped_even_when_the_baseline_outgrows_the_ruler() {
        // Today's baselines all fit (the const asserts prove it), but a retuned
        // constant past the ruler must yield no slack rather than panic in
        // `f32::clamp` — which would take the window down mid-interaction.
        assert_eq!(slack_from_target(RULER_MS, RULER_MS_U64 * 2), 0);
        assert_eq!(slack_from_target(0.0, RULER_MS_U64 * 2), 0);
    }

    #[test]
    fn slack_is_the_wait_past_the_baseline() {
        assert_eq!(slack_from_target(1_000.0, 400), 600);
        // Left of the baseline: no negative slack.
        assert_eq!(slack_from_target(100.0, 400), 0);
        // Past the ruler's end: capped at the remaining headroom.
        assert_eq!(slack_from_target(RULER_MS * 2.0, 400), RULER_MS_U64 - 400);
    }
}
