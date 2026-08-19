//! Crystal-blue dark theme: the palette, the visual style, and the status →
//! color mapping. Widgets take colors from here, never hand-picked hex.
//! Typography is egui's stock font: hierarchy comes from size and color
//! only, with a single saturated element (the primary button) per screen.

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle};

use crate::domain::control::{Status, StopReason};

/// Page background, behind the panels.
const PAGE: Color32 = Color32::from_rgb(0x0d, 0x0d, 0x0d);
/// Panel background.
const PANEL: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x19);
/// Row hover, one step above the panel. (Also egui's `faint_bg_color`.)
pub(super) const STRIPE: Color32 = Color32::from_rgb(0x22, 0x22, 0x21);
/// Accent: selection, links, the active tab's underline, the primary button.
pub(super) const ACCENT: Color32 = Color32::from_rgb(0x39, 0x87, 0xe5);
/// Accent under the pointer.
const ACCENT_HOVER: Color32 = Color32::from_rgb(0x4f, 0x95, 0xea);
/// Accent while pressed.
const ACCENT_PRESSED: Color32 = Color32::from_rgb(0x2f, 0x74, 0xc9);
/// Primary ink.
pub(super) const INK: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
/// Secondary ink (static labels, counters, inactive tabs).
pub(super) const INK_MUTED: Color32 = Color32::from_rgb(0xc3, 0xc2, 0xb7);
/// Tertiary ink: hints, the journal caret.
pub(super) const INK_FAINT: Color32 = Color32::from_rgb(0x89, 0x87, 0x81);
/// Hairline strokes and separators (panel dividers, the table's header rule).
pub(super) const HAIRLINE: Color32 = Color32::from_rgb(0x2c, 0x2c, 0x2a);
/// Watching: the loop is doing its job.
const GREEN: Color32 = Color32::from_rgb(0x0c, 0xa3, 0x0c);
/// Paused, and stops the player planned (limits). Also the rail the Setup
/// tab's Stop section paints beside an armed limit (see `status_color`).
pub(super) const AMBER: Color32 = Color32::from_rgb(0xfa, 0xb2, 0x19);
/// Stops the player did not plan (machine faults).
const RED: Color32 = Color32::from_rgb(0xe5, 0x48, 0x4d);
/// Matched rows in the shop table: brighter than the status green, to stay
/// legible as body text on the panel.
pub(super) const WANTED: Color32 = Color32::from_rgb(0x90, 0xee, 0x90);
/// The tuned-baseline fill of a timing meter: a muted steel blue, so the
/// bright `ACCENT` slack painted past it stands out as the player-controlled
/// part.
pub(super) const METER_BASE: Color32 = Color32::from_rgb(0x2c, 0x42, 0x60);

/// Spacing scale (4px grid). Every gap the layout inserts comes from here,
/// named by size, applied by role at the call site.
pub(super) const SP_XS: f32 = 4.0;
pub(super) const SP_SM: f32 = 8.0;
/// The one step between `SP_SM` and `SP_XL`: horizontal button padding, which is
/// wider than the vertical rhythm on purpose (the Linear-style pill).
pub(super) const SP_MD: f32 = 12.0;
pub(super) const SP_XL: f32 = 24.0;

/// Horizontal inset for tab content: full-width rules and row-hover fills
/// bleed to the window edges, text sits this far in. Matches the chrome's
/// 16px side margin, so the table text lines up under the status bar.
pub(super) const EDGE: i8 = 16;

/// Installs the theme on the context: visuals and text styles. Called once
/// per window, before the first frame.
pub(super) fn apply(ctx: &egui::Context) {
    // The palette is dark by design: pin the theme so an OS in light mode
    // does not swap in egui's light visuals under it.
    ctx.set_theme(egui::Theme::Dark);
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = PANEL;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = PAGE;
    visuals.faint_bg_color = STRIPE;
    visuals.selection.bg_fill = ACCENT;
    visuals.selection.stroke = Stroke::new(1.0, INK);
    visuals.hyperlink_color = ACCENT;
    visuals.error_fg_color = RED;
    visuals.warn_fg_color = AMBER;
    // `ui.weak` and hand-colored hints share this grey.
    visuals.weak_text_color = Some(INK_FAINT);
    visuals.window_corner_radius = CornerRadius::same(6);
    visuals.menu_corner_radius = CornerRadius::same(6);
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = CornerRadius::same(6);
    }
    visuals.widgets.noninteractive.bg_stroke.color = HAIRLINE;
    visuals.widgets.noninteractive.fg_stroke.color = INK_MUTED;
    visuals.widgets.inactive.fg_stroke.color = INK;
    visuals.widgets.hovered.fg_stroke.color = INK;
    visuals.widgets.active.fg_stroke.color = INK;
    visuals.widgets.open.fg_stroke.color = INK;
    ctx.set_visuals(visuals);

    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(14.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Heading,
                FontId::new(16.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(11.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(12.5, FontFamily::Monospace),
            ),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(SP_SM, SP_SM);
        style.spacing.button_padding = egui::vec2(SP_MD, SP_SM);
    });
}

/// Section header: small grey capitals, the quiet divider of the layout.
/// `.small()`/`.weak()` so a retune of the Small style or the weak grey in
/// `apply` carries over.
pub(super) fn section(text: &str) -> egui::RichText {
    egui::RichText::new(text.to_uppercase()).small().weak()
}

/// The one emphasis size, shared by the status label and the primary button.
pub(super) fn emphasis(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into()).size(15.0)
}

/// The one saturated element on screen: accent fill. Text color comes from
/// the themed `fg_stroke` (INK when enabled), so a disabled button still
/// mutes properly. `active.bg_stroke` is kept: keyboard focus renders with
/// the Active state, and that stroke is the focus ring.
pub(super) fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.scope(|ui| {
        let widgets = &mut ui.style_mut().visuals.widgets;
        widgets.inactive.weak_bg_fill = ACCENT;
        widgets.hovered.weak_bg_fill = ACCENT_HOVER;
        widgets.hovered.bg_stroke = Stroke::NONE;
        widgets.active.weak_bg_fill = ACCENT_PRESSED;
        ui.add(egui::Button::new(emphasis(text)).min_size(egui::vec2(88.0, 32.0)))
    })
    .inner
}

/// A bare (label-less) checkbox whose checked box fills the accent, matching
/// the primary button, selection, and tab underline. Squared to a 2px radius:
/// the global 6px radius rounds the ~16px checkbox icon into a circle that
/// reads as a radio button. The caller owns the row's label (the Stop limits
/// set the unit in their own column).
pub(super) fn accent_checkbox(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    ui.scope(|ui| {
        let visuals = &mut ui.style_mut().visuals;
        for state in [
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
        ] {
            state.corner_radius = CornerRadius::same(2);
        }
        if *on {
            visuals.widgets.inactive.bg_fill = ACCENT;
            visuals.widgets.hovered.bg_fill = ACCENT_HOVER;
            visuals.widgets.active.bg_fill = ACCENT_PRESSED;
        }
        ui.checkbox(on, "")
    })
    .inner
}

/// A full-width collapsible section header: a painted disclosure caret
/// (right = closed, down = open) beside the small-caps title, the whole bar
/// lighting up on hover and bleeding to the window edges. Text stays inset at
/// the bar's left. Painted, not nested widgets, so nothing steals hover over
/// the bar. Returns true on click.
///
/// While collapsed, `summary` trails right-aligned in muted ink, and rides in
/// the accessible name too (`title · summary`), so a folded section still
/// shows what it holds.
pub(super) fn collapsing_section(
    ui: &mut egui::Ui,
    title: &str,
    summary: Option<&str>,
    open: bool,
) -> bool {
    let (bar, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), egui::Sense::hover());
    // Bleed the fill + hit target to the clip rect (the full column past the
    // content inset). Grow the y-range by half the item spacing so consecutive
    // bars tile at the seam: without it the fill is only 28px while egui
    // hit-tests a wider area, so hovering the gap lights the wrong bar. The
    // growth is symmetric, so the label stays centred.
    let pad = ui.spacing().item_spacing.y * 0.5;
    let full = egui::Rect::from_x_y_ranges(
        ui.clip_rect().x_range(),
        egui::Rangef::new(bar.top() - pad, bar.bottom() + pad),
    );
    let response = ui.interact(full, ui.id().with(("section", title)), egui::Sense::click());
    let enabled = ui.is_enabled();
    let peek = (!open).then_some(()).and(summary);
    // Built inside the closure: `widget_info` only calls it when AccessKit is
    // live, a test harness is reading, or the widget was just clicked/focused.
    // Building the string outside would pay the `format!` every frame for
    // nothing. Only `Copy` inputs are captured.
    response.widget_info(|| {
        let name = match peek {
            Some(summary) => format!("{title} · {summary}"),
            None => title.to_owned(),
        };
        egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, &name)
    });

    // Setup section headers: a heading-sized uppercase label in bright ink, so
    // they read as titles. The caret trails one step down so the word leads;
    // hover lifts both to full ink over the fill.
    let hovered = response.hovered();
    let (title_color, caret_color) = if hovered {
        (INK, INK)
    } else {
        (INK_MUTED, INK_FAINT)
    };
    if hovered {
        ui.painter().rect_filled(full, CornerRadius::ZERO, STRIPE);
    }
    let painter = ui.painter().with_clip_rect(full);
    caret(&painter, bar, open, caret_color);
    let title_rect = painter.text(
        egui::pos2(bar.left() + 22.0, bar.center().y),
        egui::Align2::LEFT_CENTER,
        title.to_uppercase(),
        FontId::new(13.0, FontFamily::Proportional),
        title_color,
    );
    // The collapsed peek: right-aligned at the content edge, clipped left of the
    // title so a long summary never crosses it.
    if let Some(summary) = peek {
        let region = egui::Rect::from_min_max(
            egui::pos2(title_rect.right() + SP_SM, bar.top()),
            egui::pos2(bar.right() - SP_XS, bar.bottom()),
        );
        ui.painter().with_clip_rect(region).text(
            egui::pos2(region.right(), bar.center().y),
            egui::Align2::RIGHT_CENTER,
            summary,
            FontId::new(12.0, FontFamily::Proportional),
            INK_FAINT,
        );
    }
    // A full-bleed rule at the tiled seam (not the inner bar), so the hover
    // fill stops exactly at the divider instead of spilling into the next row.
    // Drawn with the unclipped painter so the seam-edge line is not cut.
    ui.painter()
        .hline(full.x_range(), full.bottom(), Stroke::new(1.0, HAIRLINE));
    response.clicked()
}

/// A small painted disclosure caret at the left of `row` (right = closed,
/// down = open), not a `▸` glyph, so it never depends on the stock font
/// carrying the symbol. Shared by the journal bar and the Setup sections.
pub(super) fn caret(painter: &egui::Painter, row: egui::Rect, open: bool, color: Color32) {
    let c = egui::pos2(row.left() + 6.0, row.center().y);
    let r = 3.5;
    let points = if open {
        vec![
            egui::pos2(c.x - r, c.y - r * 0.5),
            egui::pos2(c.x + r, c.y - r * 0.5),
            egui::pos2(c.x, c.y + r),
        ]
    } else {
        vec![
            egui::pos2(c.x - r * 0.5, c.y - r),
            egui::pos2(c.x - r * 0.5, c.y + r),
            egui::pos2(c.x + r, c.y),
        ]
    };
    painter.add(egui::Shape::convex_polygon(points, color, Stroke::NONE));
}

/// The status dot's color. One reading: green = working, amber = waiting on
/// the player or a limit they set, red = the machine gave up, faint = nothing
/// running.
pub(super) fn status_color(status: Status) -> Color32 {
    match status {
        Status::Idle => INK_FAINT,
        Status::Watching => GREEN,
        Status::Paused => AMBER,
        Status::Stopped(reason) => match reason {
            StopReason::PlayerStopped => INK_FAINT,
            StopReason::OutOfFunds
            | StopReason::MaxRefreshes
            | StopReason::MaxSpend
            | StopReason::MaxMatches
            | StopReason::Timeout => AMBER,
            StopReason::SessionEnded | StopReason::ActuatorFailed | StopReason::Unresponsive => RED,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_color_idle_and_player_stop_read_as_calm() {
        assert_eq!(status_color(Status::Idle), INK_FAINT);
        assert_eq!(
            status_color(Status::Stopped(StopReason::PlayerStopped)),
            INK_FAINT
        );
    }

    #[test]
    fn status_color_watching_is_green() {
        assert_eq!(status_color(Status::Watching), GREEN);
    }

    #[test]
    fn status_color_paused_is_amber() {
        assert_eq!(status_color(Status::Paused), AMBER);
    }

    #[test]
    fn status_color_limit_stops_are_amber() {
        for reason in [
            StopReason::OutOfFunds,
            StopReason::MaxRefreshes,
            StopReason::MaxSpend,
            StopReason::MaxMatches,
            StopReason::Timeout,
        ] {
            assert_eq!(status_color(Status::Stopped(reason)), AMBER);
        }
    }

    #[test]
    fn status_color_failure_stops_are_red() {
        for reason in [
            StopReason::SessionEnded,
            StopReason::ActuatorFailed,
            StopReason::Unresponsive,
        ] {
            assert_eq!(status_color(Status::Stopped(reason)), RED);
        }
    }

    #[test]
    fn section_headers_render_as_capitals() {
        assert_eq!(section("Quick start").text(), "QUICK START");
    }
}
