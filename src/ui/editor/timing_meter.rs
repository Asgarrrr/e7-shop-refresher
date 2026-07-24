//! The Click-timing per-action bars: a draggable meter on a shared time ruler.
//! Self-contained custom-painter widget — a fixed baseline segment plus the
//! player's random-extra slack — lifted out of the editor shell.

use eframe::egui;

use super::super::theme;
use crate::actuator::plan::DelayRange;

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
const RULER_MS: f32 = 2_500.0;
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
        let target_ms = frac * RULER_MS;
        let slack = (target_ms - baseline as f32).clamp(0.0, RULER_MS - baseline as f32);
        value.max_ms = slack.round() as u64;
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
    let total = baseline + value.max_ms.max(value.min_ms);
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

/// The resolved wait the game will actually take: `baseline + min` to
/// `baseline + max`, in seconds. A point range (no slack, or a reversed one)
/// collapses to a single figure, matching the draw.
fn resolved_band(baseline: u64, value: &DelayRange) -> String {
    let lo = baseline + value.min_ms;
    let hi = baseline + value.max_ms.max(value.min_ms);
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
