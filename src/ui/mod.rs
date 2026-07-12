//! egui window over the session: copies the controller state once per frame,
//! renders it, and pushes player commands through the existing channel. No
//! egui type crosses into the app layer or the domain.
//!
//! Layout: permanent chrome (status bar on top, journal at the bottom) around
//! a tabbed center — the state the player watches is always on screen, the
//! editors never push it away.

mod editor;
mod theme;
mod view;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;

use crate::app::{Command, SessionHandles};
use crate::domain::control::Status;
use crate::journal::LogLine;

use editor::EditorState;
use view::{ViewState, view_state};

/// Where the session's terminal outcome lands (fatal error, crash, or clean
/// end): written once by the spawn wrapper in `main`, shown as a banner.
pub type SessionErrorSlot = Arc<Mutex<Option<String>>>;

/// A poisoned lock means the session panicked. The view keeps rendering the
/// last state (the banner reports the crash) instead of tearing the window
/// down with a second panic.
fn lock_ignoring_poison<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Fallback window for errors raised before any session exists (bad
/// config.toml): a double-clicked exe must not flash a console and vanish.
pub fn show_fatal(message: String) -> eframe::Result {
    eframe::run_native(
        "Arkyve Refresh Shop — error",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([560.0, 260.0]),
            ..Default::default()
        },
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(FatalApp(message)))
        }),
    )
}

struct FatalApp(String);

impl eframe::App for FatalApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            // toml errors span many lines; the tail carries the diagnosis.
            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.colored_label(ui.visuals().error_fg_color, &self.0);
                });
        });
    }
}

/// Center tabs: what the player watches (Shop) vs what they tune (Setup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Shop,
    Setup,
}

/// The eframe application: a thin shell around the session handles.
pub struct ShopApp {
    handles: SessionHandles,
    error: SessionErrorSlot,
    editor: EditorState,
    tab: Tab,
    /// Journal snapshot re-cloned only when the generation changes: the
    /// journal grows at human pace, repaints at display rate.
    journal_cache: Vec<LogLine>,
    journal_generation: u64,
}

impl ShopApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        handles: SessionHandles,
        error: SessionErrorSlot,
    ) -> Self {
        theme::apply(&cc.egui_ctx);
        // Seed the drafts from the controller itself — the single source of
        // the criteria actually running.
        let editor = {
            let ctrl = lock_ignoring_poison(&handles.controller);
            EditorState::new(ctrl.filter().clone(), ctrl.limits().clone())
        };
        let journal_cache = handles.journal.entries();
        let journal_generation = handles.journal.generation();
        Self {
            handles,
            error,
            editor,
            tab: Tab::Shop,
            journal_cache,
            journal_generation,
        }
    }
}

impl eframe::App for ShopApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Poll-based repaint: state changes arrive from the session loop at
        // human pace; 4 Hz keeps the window fresh without coupling app.rs to
        // egui.
        ui.ctx().request_repaint_after(Duration::from_millis(250));
        let view = {
            let ctrl = lock_ignoring_poison(&self.handles.controller);
            view_state(&ctrl, self.handles.gate.is_enabled())
        };
        let generation = self.handles.journal.generation();
        if generation != self.journal_generation {
            self.journal_cache = self.handles.journal.entries();
            self.journal_generation = generation;
        }
        let outcome = lock_ignoring_poison(&self.error).clone();
        // A terminal outcome means the command channel is dead: disable every
        // control whose click would otherwise vanish into a closed channel.
        let session_alive = outcome.is_none();

        let mut clicked = egui::Panel::top("status_bar")
            .show(ui, |ui| render_status_bar(ui, &view, outcome.as_deref()))
            .inner;
        egui::Panel::bottom("journal")
            .resizable(true)
            .default_size(200.0)
            .show(ui, |ui| render_journal(ui, &self.journal_cache));
        // The central panel always comes last: it takes whatever space the
        // chrome left over.
        let applied = egui::CentralPanel::default()
            .show(ui, |ui| {
                render_center(ui, &view, &mut self.tab, &mut self.editor, session_alive)
            })
            .inner;
        clicked = clicked.or(applied);
        if let Some(command) = clicked {
            // A full channel only happens with a dead session loop, where the
            // banner already explains the situation: dropping the click is fine.
            let _ = self.handles.commands.try_send(command);
        }
    }
}

/// Top chrome: error banner, colored status dot + label, the one contextual
/// button, and the counters line. Returns the command the player clicked.
fn render_status_bar(
    ui: &mut egui::Ui,
    view: &ViewState,
    outcome: Option<&str>,
) -> Option<Command> {
    let mut clicked = None;
    if let Some(outcome) = outcome {
        ui.colored_label(ui.visuals().error_fg_color, outcome);
        ui.separator();
    }
    let session_alive = outcome.is_none();

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let color = theme::status_color(view.status_kind);
        let (dot, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
        ui.painter().circle_filled(dot.center(), 5.0, color);
        ui.label(
            egui::RichText::new(view.status)
                .family(theme::semibold())
                .size(15.0),
        );
        if let Some(reason) = view.stop_reason {
            ui.label(egui::RichText::new(format!("({reason})")).color(theme::INK_FAINT));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // One contextual button sending the explicit command, never
            // Toggle: the 4 Hz poll can show a label up to 250 ms stale, and
            // a toggle raced by an auto-stop would re-arm the loop. The
            // domain no-ops a redundant Stop and refuses a redundant Start.
            let armed = matches!(view.status_kind, Status::Watching | Status::Paused);
            let (label, command) = if armed {
                ("Stop", Command::Stop)
            } else {
                ("Start", Command::Start)
            };
            ui.add_enabled_ui(session_alive, |ui| {
                let text = egui::RichText::new(label).family(theme::semibold());
                if ui.button(text).clicked() {
                    clicked = Some(command);
                }
            });
        });
    });
    ui.horizontal(|ui| {
        ui.label(format!(
            "refreshes {}",
            against(view.progress.refreshes, view.limits.max_refreshes)
        ));
        ui.label(format!(
            "spent {}",
            against(view.progress.spent, view.limits.max_spend)
        ));
        ui.label(format!(
            "matches {}",
            against(view.progress.matches_found, view.limits.max_matches)
        ));
        let balance = match view.crystal_balance {
            Some(balance) => balance.to_string(),
            None => "—".to_owned(),
        };
        ui.label(format!(
            "crystals {balance} (refresh costs {})",
            view.refresh_cost
        ));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let capture = if view.capture_on {
                "capture on"
            } else {
                "capture off"
            };
            ui.label(egui::RichText::new(capture).color(theme::INK_FAINT));
        });
    });
    ui.add_space(4.0);
    clicked
}

/// Tabbed center: the strip, then the active tab's content in its own
/// scroll area. Returns the command the player clicked (Apply lives in Setup).
fn render_center(
    ui: &mut egui::Ui,
    view: &ViewState,
    tab: &mut Tab,
    editor: &mut EditorState,
    session_alive: bool,
) -> Option<Command> {
    let mut clicked = None;
    ui.add_space(2.0);
    // Tabs read as tabs, not buttons: the pill fills are stripped within the
    // scope, the active tab is marked by an accent underline instead.
    let tabs = ui.horizontal(|ui| {
        let visuals = &mut ui.style_mut().visuals;
        visuals.selection.bg_fill = egui::Color32::TRANSPARENT;
        visuals.selection.stroke.color = theme::INK;
        visuals.widgets.hovered.weak_bg_fill = egui::Color32::TRANSPARENT;
        visuals.widgets.hovered.bg_stroke = egui::Stroke::NONE;
        visuals.widgets.active.weak_bg_fill = egui::Color32::TRANSPARENT;
        visuals.widgets.active.bg_stroke = egui::Stroke::NONE;
        visuals.widgets.inactive.fg_stroke.color = theme::INK_MUTED;
        let label = |text: &str| egui::RichText::new(text).family(theme::semibold());
        [
            ui.selectable_value(tab, Tab::Shop, label("Shop")).rect,
            ui.selectable_value(tab, Tab::Setup, label("Setup")).rect,
        ]
    });
    // The strip's baseline: one hairline across, the accent segment under the
    // active tab painted over it.
    let baseline = tabs.response.rect.bottom();
    ui.painter().hline(
        ui.max_rect().x_range(),
        baseline,
        ui.visuals().widgets.noninteractive.bg_stroke,
    );
    let active = match tab {
        Tab::Shop => tabs.inner[0],
        Tab::Setup => tabs.inner[1],
    };
    ui.painter().hline(
        active.x_range(),
        baseline,
        egui::Stroke::new(2.0, theme::ACCENT),
    );
    ui.add_space(6.0);
    // Tab scroll: expanded editors must never overflow a clipped panel.
    egui::ScrollArea::vertical()
        .id_salt("center")
        .auto_shrink([false, false])
        .show(ui, |ui| match tab {
            Tab::Shop => render_shop_tab(ui, view),
            Tab::Setup => {
                clicked = ui
                    .add_enabled_ui(session_alive, |ui| {
                        editor::edit_filter(ui, editor).or_else(|| editor::edit_limits(ui, editor))
                    })
                    .inner;
                ui.weak("edits apply to this session only — config.toml is unchanged");
            }
        });
    clicked
}

/// The Shop tab: merchant header + the slot table.
fn render_shop_tab(ui: &mut egui::Ui, view: &ViewState) {
    ui.heading(view.merchant.as_str());
    if view.rows.is_empty() {
        ui.weak("no shop captured yet — open the Secret Shop in game");
        return;
    }
    egui::Grid::new("shop")
        .num_columns(4)
        .striped(true)
        .show(ui, |ui| {
            ui.strong("Slot");
            ui.strong("Kind");
            ui.strong("Name");
            ui.strong("Price");
            ui.end_row();
            for row in &view.rows {
                let cell = |text: String| {
                    let mut text = egui::RichText::new(text);
                    if row.wanted {
                        text = text.strong().color(egui::Color32::LIGHT_GREEN);
                    }
                    if row.sold_out {
                        text = text.weak().strikethrough();
                    }
                    text
                };
                ui.label(cell(row.slot.to_string()));
                ui.label(cell(row.kind.to_owned()));
                ui.label(cell(row.name.clone().unwrap_or_else(|| "—".to_owned())))
                    .on_hover_text(&row.detail);
                ui.label(cell(match row.price {
                    Some(price) => format!("{price} gold"),
                    None => "—".to_owned(),
                }));
                ui.end_row();
            }
        });
}

/// Bottom chrome: the journal, filling whatever height the panel was
/// resized to.
fn render_journal(ui: &mut egui::Ui, journal: &[LogLine]) {
    ui.label(egui::RichText::new("Journal").family(theme::semibold()));
    // show_rows lays out only the visible slice; its uniform-row math
    // requires exactly one visual row per entry, so long lines extend into
    // a horizontal scroll instead of wrapping.
    let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
    egui::ScrollArea::both()
        .id_salt("journal")
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show_rows(ui, row_height, journal.len(), |ui, rows| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            for line in &journal[rows] {
                ui.monospace(format!("[{}] {}", timestamp(line.at_ms), line.text));
            }
        });
}

/// `3/10` against a limit, `3/—` without one.
fn against(value: u32, limit: Option<u32>) -> String {
    match limit {
        Some(limit) => format!("{value}/{limit}"),
        None => format!("{value}/—"),
    }
}

/// Session-relative `+m:ss` (hours appear once the session runs that long).
fn timestamp(at_ms: u64) -> String {
    let secs = at_ms / 1000;
    let (hours, minutes, seconds) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if hours > 0 {
        format!("+{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("+{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use egui_kittest::{Harness, kittest::Queryable};

    use crate::domain::control::{Controller, Event, Limits};
    use crate::domain::filter::Filter;

    use super::*;

    #[test]
    fn against_renders_missing_limit_as_dash() {
        assert_eq!(against(3, Some(10)), "3/10");
        assert_eq!(against(3, None), "3/—");
    }

    #[test]
    fn timestamp_rolls_into_hours() {
        assert_eq!(timestamp(59_000), "+0:59");
        assert_eq!(timestamp(61_000), "+1:01");
        assert_eq!(timestamp(3_661_000), "+1:01:01");
    }

    /// The kittest context starts un-themed and the status bar references the
    /// semibold font family, which only `theme::apply` binds. Mirror the real
    /// window (`ShopApp::new` themes before the first render): apply on the
    /// construction frame, render from the next pass — where fonts land.
    fn themed_harness<'a>(mut app: impl FnMut(&mut egui::Ui) + 'a) -> Harness<'a> {
        let mut themed = false;
        let mut harness = Harness::new_ui(move |ui| {
            if themed {
                app(ui);
            } else {
                theme::apply(ui.ctx());
                themed = true;
            }
        });
        harness.step();
        harness
    }

    fn idle_view() -> ViewState {
        let controller = Controller::new(Filter::default(), Limits::default());
        view_state(&controller, false)
    }

    fn watching_view() -> ViewState {
        let mut controller = Controller::new(Filter::matching_default_items(), Limits::default());
        controller.handle(Event::Start { now_ms: 0 });
        view_state(&controller, false)
    }

    #[test]
    fn idle_status_bar_offers_start_only() {
        let view = idle_view();
        let clicked = RefCell::new(None);
        let mut harness = themed_harness(|ui| {
            if let Some(command) = render_status_bar(ui, &view, None) {
                *clicked.borrow_mut() = Some(command);
            }
        });
        assert!(harness.query_by_label("Stop").is_none());
        assert!(harness.query_by_label("Toggle").is_none());
        harness.get_by_label("Start").click();
        harness.run();
        drop(harness);
        assert_eq!(clicked.into_inner(), Some(Command::Start));
    }

    #[test]
    fn armed_status_bar_offers_stop_only() {
        let view = watching_view();
        let clicked = RefCell::new(None);
        let mut harness = themed_harness(|ui| {
            if let Some(command) = render_status_bar(ui, &view, None) {
                *clicked.borrow_mut() = Some(command);
            }
        });
        assert!(harness.query_by_label("Start").is_none());
        assert!(harness.query_by_label("Toggle").is_none());
        harness.get_by_label("Stop").click();
        harness.run();
        drop(harness);
        assert_eq!(clicked.into_inner(), Some(Command::Stop));
    }

    #[test]
    fn setup_tab_reveals_the_editors_shop_hides_them() {
        let view = idle_view();
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(Filter::default(), Limits::default());
        let mut harness = themed_harness(|ui| {
            render_center(ui, &view, &mut tab, &mut editor, true);
        });
        // Shop is the default tab: no editor controls in sight.
        assert!(harness.query_by_label("Filter").is_none());
        assert!(harness.query_by_label("Apply filter").is_none());
        harness.get_by_label("Setup").click();
        harness.run();
        harness.get_by_label("Filter").click();
        harness.run();
        harness.get_by_label("Apply filter");
        drop(harness);
        assert_eq!(tab, Tab::Setup);
    }
}
