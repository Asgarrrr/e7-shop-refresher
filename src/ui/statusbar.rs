//! Top status bar: the status word + one contextual action on the first row,
//! and the stat-tile metrics row (balances | refreshes + per-token haul) under
//! it.

use eframe::egui;

use crate::app::Command;
use crate::domain::control::Status;
use crate::domain::shop::{Crystals, Gold};

use super::theme;
use super::view::ViewState;
use crate::render::amount_or_dash;

/// Top chrome: the error banner, then two rows — status (dot + word + clause)
/// with the one contextual button on the right, and under it a row of stat
/// tiles (balances | refreshes + the per-token haul). Returns the clicked
/// command.
#[must_use]
pub(super) fn render_status_bar(
    ui: &mut egui::Ui,
    view: &ViewState,
    outcome: Option<&str>,
    session_alive: bool,
) -> Option<Command> {
    let mut clicked = None;
    if let Some(outcome) = outcome {
        ui.colored_label(ui.visuals().error_fg_color, outcome);
        ui.separator();
    }

    ui.add_space(theme::SP_XS);
    let color = theme::status_color(view.status_kind);
    let armed = matches!(view.status_kind, Status::Watching | Status::Paused);
    // Row 1: button width first (right-aligned), then status fills the rest,
    // so the clause has room and does not truncate.
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Sends the explicit command, never Toggle: the 4 Hz poll can show
            // a label up to 250 ms stale, and a toggle raced by an auto-stop
            // would re-arm the loop (the domain no-ops a redundant Stop and
            // refuses a redundant Start).
            let (label, command) = if armed {
                ("Stop", Command::Stop)
            } else {
                ("Start", Command::Start)
            };
            ui.add_enabled_ui(session_alive, |ui| {
                if theme::primary_button(ui, label).clicked() {
                    clicked = Some(command);
                }
            });
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                // The dot carries the color; the word stays ink.
                status_dot(ui, color);
                ui.add_space(theme::SP_SM);
                ui.add(
                    egui::Label::new(theme::emphasis(view.status_word).color(theme::INK))
                        .truncate(),
                );
                if let Some(hint) = view.status_hint {
                    ui.add_space(theme::SP_SM);
                    ui.add(egui::Label::new(egui::RichText::new(hint).weak()).truncate());
                }
            });
        });
    });
    // Row 2: balance tiles, then the run's readouts once a run exists (while
    // Idle they'd be all zeros, noise).
    ui.add_space(theme::SP_SM);
    row_separator(ui);
    ui.add_space(theme::SP_SM);
    // Wrapped, not a flat row: a large Gold balance plus the haul tiles can
    // exceed the panel's minimum width; wrapping folds the overflow to a
    // second line instead of clipping tiles off-screen.
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = theme::SP_SM;
        skystone_tile(ui, view.crystal_balance);
        gold_tile(ui, view.gold_balance);
        if !matches!(view.status_kind, Status::Idle) {
            // Size the divider to the tiles already laid.
            let tile_height = ui.min_size().y;
            group_divider(ui, tile_height);
            // The haul replaces the old generic Spent/Matches counters with the
            // per-token buy tally, bucketing the rest into Other.
            stat_tile(
                ui,
                "Refreshes",
                value_over_limit(view.progress.refreshes, view.limits.max_refreshes),
            );
            for (label, count) in &view.haul {
                stat_tile(ui, label, count.to_string());
            }
            if view.haul_others > 0 {
                stat_tile(ui, "Other", format!("+{}", view.haul_others));
            }
        }
    });
    ui.add_space(theme::SP_XS);
    clicked
}

/// The crystal balance tile. "Skystones" is the game's word; the code and the
/// `RefreshMeta` that feeds it say crystals.
///
/// A function per currency, not two `stat_tile(ui, "…", …)` calls: the old code
/// called `stat_tile` for both balances directly, differing only by a string
/// literal and which `view` field was passed, so swapping the two arguments
/// compiled and mislabelled both balances. `Option<Crystals>` and `Option<Gold>`
/// are not interchangeable, so each currency now has its own typed helper.
/// The generic tiles below (Refreshes, haul tokens) stay plain counts with no
/// ledger to bind to.
fn skystone_tile(ui: &mut egui::Ui, balance: Option<Crystals>) {
    stat_tile(ui, "Skystones", amount_or_dash(balance));
}

/// The gold balance tile — see [`skystone_tile`] for why each currency has its
/// own.
fn gold_tile(ui: &mut egui::Ui, balance: Option<Gold>) {
    stat_tile(ui, "Gold", amount_or_dash(balance));
}

/// One KPI tile: a small grey uppercase label over its value in full ink.
fn stat_tile(ui: &mut egui::Ui, label: &str, value: String) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = theme::SP_XS;
        ui.label(theme::section(label));
        ui.label(egui::RichText::new(value).color(theme::INK));
    });
}

/// A full-bleed hairline splitting the status/action row from the metrics row
/// below it. Reserves a 1px strip for layout, then paints across the panel's
/// clip rect (the full window width, past the side margin) so it reaches the
/// edges like the tab and table rules. Dimmed below the plain hairline, since
/// it divides two rows of the same chrome block rather than whole zones.
fn row_separator(ui: &mut egui::Ui) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
    ui.painter().hline(
        ui.clip_rect().x_range(),
        rect.center().y,
        egui::Stroke::new(1.0, theme::HAIRLINE.gamma_multiply(0.5)),
    );
}

/// A hairline of the given height splitting the balance tiles from the counter
/// tiles — a group divider, not a full-height rule.
fn group_divider(ui: &mut egui::Ui, height: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(1.0, height), egui::Sense::hover());
    ui.painter().vline(
        rect.center().x,
        rect.y_range(),
        egui::Stroke::new(1.0, theme::HAIRLINE),
    );
}

/// A small painted status dot, not a `●` glyph, so it never depends on the
/// stock font carrying the symbol.
fn status_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(7.0, 7.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

/// `3/10` against a limit, `3/—` without one.
fn value_over_limit(value: u32, limit: Option<u32>) -> String {
    match limit {
        Some(limit) => format!("{value}/{limit}"),
        None => format!("{value}/—"),
    }
}

#[cfg(test)]
mod tests {
    use egui_kittest::{Harness, kittest::Queryable};

    use crate::domain::control::{Controller, Event, Limits};
    use crate::domain::filter::Filter;

    use super::super::view::view_state;
    use super::*;

    fn idle_view() -> ViewState {
        let controller = Controller::new(Filter::default(), Limits::default());
        view_state(&controller)
    }

    fn watching_view() -> ViewState {
        let mut controller = Controller::new(Filter::matching_default_items(), Limits::default());
        let _ = controller.handle(Event::Start { now_ms: 0 });
        view_state(&controller)
    }

    #[test]
    fn value_over_limit_renders_missing_limit_as_dash() {
        assert_eq!(value_over_limit(3, Some(10)), "3/10");
        assert_eq!(value_over_limit(3, None), "3/—");
    }

    #[test]
    fn idle_status_bar_hides_stop_and_toggle() {
        let view = idle_view();
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, None, true);
        });
        assert!(harness.query_by_label("Stop").is_none());
        assert!(harness.query_by_label("Toggle").is_none());
    }

    #[test]
    fn status_bar_shows_currencies_and_a_clean_status() {
        let view = idle_view();
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, None, true);
        });
        harness.get_by_label("SKYSTONES");
        harness.get_by_label("GOLD");
        harness.get_by_label("Idle");
        harness.get_by_label("define a filter first");
    }

    #[test]
    fn run_tiles_hidden_only_while_idle() {
        let idle = idle_view();
        let idle_bar = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &idle, None, true);
        });
        idle_bar.get_by_label("SKYSTONES");
        assert!(idle_bar.query_by_label("REFRESHES").is_none());

        let limits = Limits {
            max_refreshes: Some(10),
            ..Limits::default()
        };
        let mut controller = Controller::new(Filter::matching_default_items(), limits);
        let _ = controller.handle(Event::Start { now_ms: 0 });
        let armed = view_state(&controller);
        let armed_bar = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &armed, None, true);
        });
        for label in ["REFRESHES", "0/10", "COVENANT", "MYSTIC"] {
            armed_bar.get_by_label(label);
        }
        assert!(armed_bar.query_by_label("SPENT").is_none());
        assert!(armed_bar.query_by_label("MATCHES").is_none());

        // The final totals survive a stop (an auto-stop is exactly when the
        // player wants to read them).
        let mut controller = Controller::new(Filter::matching_default_items(), Limits::default());
        let _ = controller.handle(Event::Start { now_ms: 0 });
        let _ = controller.handle(Event::Stop);
        let stopped = view_state(&controller);
        let stopped_bar = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &stopped, None, true);
        });
        stopped_bar.get_by_label("REFRESHES");
    }

    #[test]
    fn idle_start_click_emits_start() {
        let view = idle_view();
        // `Harness::new_ui`'s bound is `impl FnMut`, so the closure captures
        // `clicked` mutably; `drop(harness)` releases the borrow before the read.
        let mut clicked = None;
        let mut harness = Harness::new_ui(|ui| {
            if let Some(command) = render_status_bar(ui, &view, None, true) {
                clicked = Some(command);
            }
        });
        harness.get_by_label("Start").click();
        harness.run();
        drop(harness);
        assert_eq!(clicked, Some(Command::Start));
    }

    #[test]
    fn armed_status_bar_hides_start_and_toggle() {
        let view = watching_view();
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, None, true);
        });
        assert!(harness.query_by_label("Start").is_none());
        assert!(harness.query_by_label("Toggle").is_none());
    }

    #[test]
    fn armed_stop_click_emits_stop() {
        let view = watching_view();
        let mut clicked = None;
        let mut harness = Harness::new_ui(|ui| {
            if let Some(command) = render_status_bar(ui, &view, None, true) {
                clicked = Some(command);
            }
        });
        harness.get_by_label("Stop").click();
        harness.run();
        drop(harness);
        assert_eq!(clicked, Some(Command::Stop));
    }
}
