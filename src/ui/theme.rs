//! Crystal-blue dark theme: THE palette, the embedded Inter fonts, and the
//! status → color mapping. Widgets never hand-pick hex colors — they take
//! them from here.

use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle,
};

use crate::domain::control::{Status, StopReason};

/// Page background, behind the panels.
const PAGE: Color32 = Color32::from_rgb(0x0d, 0x0d, 0x0d);
/// Panel background.
const PANEL: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x19);
/// Table stripes: one step above the panel, visible but quiet.
const STRIPE: Color32 = Color32::from_rgb(0x22, 0x22, 0x21);
/// Accent: selection, links, the active tab's underline.
pub(super) const ACCENT: Color32 = Color32::from_rgb(0x39, 0x87, 0xe5);
/// Primary ink.
pub(super) const INK: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
/// Secondary ink (static labels, counters, inactive tabs).
pub(super) const INK_MUTED: Color32 = Color32::from_rgb(0xc3, 0xc2, 0xb7);
/// Tertiary ink: hints, and the color of a state nobody needs to act on.
pub(super) const INK_FAINT: Color32 = Color32::from_rgb(0x89, 0x87, 0x81);
/// Hairline strokes and separators.
const HAIRLINE: Color32 = Color32::from_rgb(0x2c, 0x2c, 0x2a);
/// Watching: the loop is doing its job.
const GREEN: Color32 = Color32::from_rgb(0x0c, 0xa3, 0x0c);
/// Paused, and stops the player planned (limits).
const AMBER: Color32 = Color32::from_rgb(0xfa, 0xb2, 0x19);
/// Stops the player did not plan (machine faults).
const RED: Color32 = Color32::from_rgb(0xe5, 0x48, 0x4d);

const INTER_REGULAR: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
const INTER_SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/Inter-SemiBold.ttf");

/// The named family carrying Inter SemiBold: headings and emphasis (status
/// label, primary button) — `RichText::strong` would fake-bold Regular.
pub(super) fn semibold() -> FontFamily {
    FontFamily::Name("inter-semibold".into())
}

/// Installs the theme on the context: fonts, visuals, text styles. Called
/// once per window, before the first frame.
pub(super) fn apply(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "inter".to_owned(),
        Arc::new(FontData::from_static(INTER_REGULAR)),
    );
    fonts.font_data.insert(
        "inter-semibold".to_owned(),
        Arc::new(FontData::from_static(INTER_SEMIBOLD)),
    );
    let proportional = fonts
        .families
        .get_mut(&FontFamily::Proportional)
        .expect("default proportional family");
    proportional.insert(0, "inter".to_owned());
    // Same fallback stack (emoji, symbols), SemiBold in front.
    let mut semibold_stack = proportional.clone();
    semibold_stack[0] = "inter-semibold".to_owned();
    fonts.families.insert(semibold(), semibold_stack);
    ctx.set_fonts(fonts);

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
            (TextStyle::Heading, FontId::new(17.0, semibold())),
            (
                TextStyle::Small,
                FontId::new(11.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(12.5, FontFamily::Monospace),
            ),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(14.0, 7.0);
    });
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
    fn status_color_watching_is_green_paused_is_amber() {
        assert_eq!(status_color(Status::Watching), GREEN);
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
}
