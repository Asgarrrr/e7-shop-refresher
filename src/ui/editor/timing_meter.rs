//! The Click-timing per-action bars: a draggable meter on a shared time ruler,
//! a fixed baseline segment plus the player's random-extra slack.

use eframe::egui;

use super::super::theme;
use crate::actuator::plan::{self, DelayRange};

/// Names the two segments of every meter: a muted swatch for the fixed tuned
/// wait, a bright one for the random extra.
pub(super) fn timing_legend(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        legend_swatch(ui, theme::METER_BASE, "fixed tuned wait");
        ui.add_space(theme::SP_XL);
        legend_swatch(ui, theme::ACCENT, "random extra");
    });
}

fn legend_swatch(ui: &mut egui::Ui, color: egui::Color32, label: &str) {
    let (chip, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(chip, egui::CornerRadius::same(3), color);
    ui.weak(label);
}

const METER_H: f32 = 22.0;
/// The ruler's length in ms. Constant, not fitted to the values, so a bar's
/// length is a stable reading across rows; checked against the baselines at
/// compile time below. The float twin is what the painting math uses.
const RULER_MS_U64: u64 = 2_500;
const RULER_MS: f32 = RULER_MS_U64 as f32;

// Every tuned baseline must fit under the ruler: one grown past it would panic
// `f32::clamp` inside an egui interaction, taking the window down. Raise
// `RULER_MS_U64` when one of these is retuned.
const _: () = assert!(plan::WAIT_SHOP_OPENED_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_REFRESHED_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_CONFIRM_REFRESH_MODAL_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_BUY_MODAL_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_BETWEEN_BUYS_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_SCROLL_SETTLE_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_PURCHASE_RESUMED_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_RECOVERY_MS <= RULER_MS_U64);
/// Wide enough for the longest action name ("watchdog re-issue"), painted in
/// an exact-size box so a label can never grow the column.
const LABEL_W: f32 = 150.0;
/// Fixed so resolved times align and never sit over the bar or its grip.
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

/// One action row: fixed-width label column, the bar filling the middle, and a
/// fixed-width resolved-time column on the right, so bars and values both align.
fn timing_row(ui: &mut egui::Ui, label: &str, value: &mut DelayRange, baseline: u64) {
    ui.horizontal(|ui| {
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

/// One action's bar: a muted fixed segment for the tuned baseline, and a bright
/// random-extra segment grown by dragging past it.
fn timing_meter(ui: &mut egui::Ui, width: f32, baseline: u64, value: &mut DelayRange) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, METER_H), egui::Sense::click_and_drag());

    // Gated on `dragged() || clicked()`: `interact_pointer_pos()` is `Some`
    // from the frame the button goes *down*, so gating the write on it rewrote
    // the value on mere contact — and the write is destructive, collapsing a
    // hand-authored range toward `(0, 0)` and saving it over the player's file.
    if (response.dragged() || response.clicked())
        && let Some(pos) = response.interact_pointer_pos()
    {
        let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        // `set_max_ms` brings a seeded floor down with the ceiling, so no
        // second line is needed to restore `min <= max`.
        value.set_max_ms(slack_from_target(frac * RULER_MS, baseline));
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }

    let painter = ui.painter().clone();
    let radius = egui::CornerRadius::same(4);
    painter.rect_filled(rect, radius, theme::HAIRLINE);
    // Plain `+`: `DelayRange` is ordered and capped by construction, so a
    // baseline under the ruler plus at most one minute cannot overflow.
    let total = baseline + value.max_ms();
    let base_w = rect.width() * (baseline as f32 / RULER_MS);
    // Clamped to the bar: `total` can be many times the ruler and
    // `ui.painter()` is not clipped to the allocated rect, so a legal
    // `max_ms = 60000` painted a fill ~24× the bar's width, across the value
    // column and out to the scroll area's edge.
    let total_w = (rect.width() * (total as f32 / RULER_MS)).min(rect.width());
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
    // Faint second-marks so the bar's empty tail reads as a ruler, not dead
    // space.
    for mark_ms in [1_000.0_f32, 2_000.0] {
        let x = rect.left() + rect.width() * (mark_ms / RULER_MS);
        painter.vline(
            x,
            rect.y_range(),
            egui::Stroke::new(1.0, egui::Color32::from_white_alpha(18)),
        );
    }
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

/// The random extra a pointer landing at `target_ms` sets: whatever sits past
/// the baseline, never negative and never past the ruler's end.
///
/// The headroom is floored at zero because `f32::clamp` asserts `min <= max`,
/// so a baseline grown past the ruler would panic mid-interaction. The const
/// asserts above make that unreachable today, but the two guards protect
/// different edits.
fn slack_from_target(target_ms: f32, baseline: u64) -> u64 {
    let headroom = (RULER_MS - baseline as f32).max(0.0);
    // The `as u64` is range-verified: `target_ms` is finite (the bar width is
    // floored at 80.0, so no `NaN`, which `as` would map to 0) and `clamp`
    // bounds it well inside `u64`. Neither guard suffices alone.
    (target_ms - baseline as f32).clamp(0.0, headroom).round() as u64
}

/// The resolved wait the game will actually take, in seconds. A point range
/// collapses to a single figure, matching the draw.
fn resolved_band(baseline: u64, value: &DelayRange) -> String {
    // Plain addition, for the same reason as the bar's `total` above.
    secs_range(baseline + value.min_ms(), baseline + value.max_ms())
}

/// `lo..hi` milliseconds as seconds; a zero-width range shows one figure. The
/// one place ms becomes an `x.xx s` reading, shared by the per-action bars and
/// the routine total.
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
    fn resolved_band_reads_the_widest_range_the_type_allows() {
        // The type's ceiling is ~24× the ruler, so the display must print past
        // its own scale without clamping.
        let value = DelayRange::ceiling(plan::MAX_TIMING_MS);
        assert_eq!(
            resolved_band(plan::WAIT_SHOP_OPENED_MS, &value),
            secs_range(
                plan::WAIT_SHOP_OPENED_MS,
                plan::WAIT_SHOP_OPENED_MS + plan::MAX_TIMING_MS
            )
        );
    }

    #[test]
    fn slack_is_clamped_even_when_the_baseline_outgrows_the_ruler() {
        // A retuned baseline past the ruler yields no slack, and no panic.
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
