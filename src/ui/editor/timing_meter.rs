//! The Click-timing per-action bars: a draggable meter on a shared time ruler,
//! a fixed baseline segment plus the player's random-extra slack. Self-contained
//! custom-painter widget, lifted out of the editor shell.

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

/// The meter height and the fixed time ruler the bars sit on. Constant (not
/// fitted to the values) so a bar's length is a stable reading across rows,
/// and it clears the longest baseline with room to drag real slack on top.
/// Width is the row's: bars fill to the content edge under a fixed label column.
const METER_H: f32 = 22.0;
/// The ruler's length in ms, checked against the baselines at compile time;
/// the float twin is what the painting math uses.
const RULER_MS_U64: u64 = 2_500;
const RULER_MS: f32 = RULER_MS_U64 as f32;

// Tripwires for the ruler's invariant: every tuned baseline `fine_tune_body`
// paints must fit under it. A constant grown past it (e.g. a lengthened game
// animation) leaves an empty bar with its grip pinned at zero, and — without
// the total clamp in `slack_from_target` — an `f32::clamp` panic inside an
// egui interaction, taking the whole window down. Raise `RULER_MS_U64` when
// one of these is retuned.
const _: () = assert!(plan::WAIT_SHOP_OPENED_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_REFRESHED_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_CONFIRM_REFRESH_MODAL_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_BUY_MODAL_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_BETWEEN_BUYS_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_SCROLL_SETTLE_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_PURCHASE_RESUMED_MS <= RULER_MS_U64);
const _: () = assert!(plan::WAIT_RECOVERY_MS <= RULER_MS_U64);
/// The label column: wide enough for the longest action name ("watchdog
/// re-issue"), painted in an exact-size box so a label can never grow the column.
const LABEL_W: f32 = 150.0;
/// The resolved-time column right of every bar: fixed so values align and
/// never sit over the bar or its grip.
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
        // Exact-size box + painted label pins the bar's start (see `LABEL_W`).
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
/// The resolved wait shows in the row's value column, not inside, so the grip
/// never cuts through it.
fn timing_meter(ui: &mut egui::Ui, width: f32, baseline: u64, value: &mut DelayRange) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, METER_H), egui::Sense::click_and_drag());

    // Drag or click sets the random extra: the pointer's x is the target total
    // wait; slack is whatever sits past the baseline, clamped to [0, ruler end].
    //
    // Gated on `dragged() || clicked()` and not on `interact_pointer_pos()`
    // alone: that returns `Some` from the frame the button goes *down*, before
    // the press has resolved into either gesture, so merely putting a finger on
    // a bar rewrote it. The write is destructive and one-way — this ruler ends
    // at `RULER_MS`, well under `plan::MAX_TIMING_MS`, so a hand-authored
    // `refreshed = { min_ms = 200, max_ms = 30000 }` collapsed toward `(0, 0)`
    // on contact (`set_max_ms` brings the floor down with the ceiling), lit
    // Apply, and `persist::save` wrote the collapse over the player's file.
    if (response.dragged() || response.clicked())
        && let Some(pos) = response.interact_pointer_pos()
    {
        let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        // `set_max_ms` alone also brings a config-seeded floor down with the
        // ceiling just dragged past it — `DelayRange`'s private fields make a
        // second line here (to restore that invariant) unnecessary.
        value.set_max_ms(slack_from_target(frac * RULER_MS, baseline));
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }

    let painter = ui.painter().clone();
    let radius = egui::CornerRadius::same(4);
    painter.rect_filled(rect, radius, theme::HAIRLINE);
    // A plain `+`, not a saturating one: `DelayRange` carries
    // `min_ms <= max_ms <= plan::MAX_TIMING_MS` by construction, so `max_ms`
    // is the top of the band, and a baseline under the 2500 ms ruler plus at
    // most one minute cannot overflow. The total can still be many times the
    // ruler — the ceiling is deliberately far above this widget's own scale —
    // so the bar paints past its own rect; the grip clamp below keeps it readable.
    let total = baseline + value.max_ms();
    let base_w = rect.width() * (baseline as f32 / RULER_MS);
    // Clamped to the bar, unlike the grip clamp below, which only repositions
    // the grip and never bounded this rect. `total` can be many times the
    // ruler — `MAX_TIMING_MS` is 60 000 against a 2 500 ms ruler — and
    // `ui.painter()` is not clipped to the allocated rect, so a legal
    // `max_ms = 60000` painted an accent fill ~24× the bar's width: straight
    // across the fixed resolved-time column beside it and out to the scroll
    // area's edge, leaving the row a solid band with its own value text
    // unreadable on top.
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
    // space; painted over the fills, subtle enough not to compete.
    for mark_ms in [1_000.0_f32, 2_000.0] {
        let x = rect.left() + rect.width() * (mark_ms / RULER_MS);
        painter.vline(
            x,
            rect.y_range(),
            egui::Stroke::new(1.0, egui::Color32::from_white_alpha(18)),
        );
    }
    // The grip sits at the draggable edge — the end of the slack, or the
    // baseline when there is none.
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
/// the fixed baseline, never negative and never past the ruler's end.
///
/// The upper bound is floored at zero on purpose: `f32::clamp` asserts
/// `min <= max`, so `RULER_MS - baseline` alone panics once a tuned baseline
/// grows past the ruler — inside an egui interaction, taking the whole window
/// down. The const asserts above make that unreachable today; the clamp stays
/// anyway since the two guards protect different edits.
fn slack_from_target(target_ms: f32, baseline: u64) -> u64 {
    let headroom = (RULER_MS - baseline as f32).max(0.0);
    // The `as u64` is range-verified: `target_ms` is `frac * RULER_MS`, and
    // `frac` comes from a division by a bar width floored at 80.0 (see
    // `editor/mod.rs`), so it is finite — no `NaN`, which `as` would map to 0
    // — and `clamp` bounds it to `0..=RULER_MS`, well inside `u64`. Both
    // guards are needed: clamp alone still lets `NaN` through; finiteness
    // alone would not bound the value.
    (target_ms - baseline as f32).clamp(0.0, headroom).round() as u64
}

/// The resolved wait the game will actually take: `baseline + min` to
/// `baseline + max`, in seconds. A point range (no slack, or reversed) collapses
/// to a single figure, matching the draw.
fn resolved_band(baseline: u64, value: &DelayRange) -> String {
    // Plain addition, for the same reason as the bar's `total` above: ordered
    // and bounded by construction, so neither sum can overflow.
    secs_range(baseline + value.min_ms(), baseline + value.max_ms())
}

/// `lo..hi` milliseconds as seconds; a zero-width range shows one figure. The
/// one place ms becomes the `x.xx s` / `x.xx–y.yy s` reading, shared by the
/// per-action bars and the routine total.
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
        // `DelayRange` can no longer hold `max_ms = u64::MAX`; its ceiling is
        // still ~24x what this widget's ruler can show, so the display must
        // print a range far past its own scale without clamping the number.
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
        // A retuned baseline past the ruler must yield no slack, not panic in
        // `f32::clamp` (which would take the window down mid-interaction).
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
