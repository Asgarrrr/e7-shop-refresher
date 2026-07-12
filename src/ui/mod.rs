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
        &format!("{} — error", crate::APP_NAME),
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
            view_state(&ctrl)
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

        // Roomier margins than egui's stock 8px: the chrome needs to breathe.
        let margin = egui::Margin::symmetric(16, 10);
        let mut clicked = egui::Panel::top("status_bar")
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(margin))
            .show(ui, |ui| {
                render_status_bar(ui, &view, outcome.as_deref(), session_alive)
            })
            .inner;
        // The journal may grow but must never crush the center: whatever the
        // window height or divider drag, the tab strip and a few rows stay.
        let journal_max = (ui.available_rect_before_wrap().height() - 160.0).max(60.0);
        egui::Panel::bottom("journal")
            .resizable(true)
            .default_size(200.0)
            .size_range(egui::Rangef::new(60.0, journal_max))
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(margin))
            .show(ui, |ui| render_journal(ui, &self.journal_cache));
        // The central panel always comes last: it takes whatever space the
        // chrome left over.
        let applied = egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(margin))
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

/// Top chrome: the error banner, then one row — colored state word and clause
/// on the left, the skystone/gold readout and the Start/Stop button on the
/// right — with the run counters on a second line. Returns the clicked command.
fn render_status_bar(
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

    ui.add_space(4.0);
    let color = theme::status_color(view.status_kind);
    let armed = matches!(view.status_kind, Status::Watching | Status::Paused);
    // Right-to-left, bounded by `horizontal`: the button takes its width
    // first, then the currencies, so at narrow widths the status truncates
    // into the space left instead of running under them.
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // One contextual button sending the explicit command, never
            // Toggle: the 4 Hz poll can show a label up to 250 ms stale, and
            // a toggle raced by an auto-stop would re-arm the loop. The
            // domain no-ops a redundant Stop and refuses a redundant Start.
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
            // gold then skystones; the game says "skystones", the code (whose
            // meta feeds them) says "crystals".
            currency(ui, "skystones", view.crystal_balance);
            ui.add_space(10.0);
            currency(ui, "gold", view.gold_balance);
            ui.add_space(16.0);
            // The status is the one element that fills the leftover width, so
            // it truncates rather than pushing the currencies off-screen.
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(theme::emphasis(view.status_word).color(color)).truncate());
                if let Some(hint) = view.status_hint {
                    ui.add(egui::Label::new(egui::RichText::new(hint).weak()).truncate());
                }
            });
        });
    });
    // Shown from Watching onward, Stopped included, so the totals outlive an
    // auto-stop; hidden while Idle, where they all read `0/—`.
    if !matches!(view.status_kind, Status::Idle) {
        ui.add_space(2.0);
        ui.weak(format!(
            "refreshes {} · spent {} · matches {}",
            against(view.progress.refreshes, view.limits.max_refreshes),
            against(view.progress.spent, view.limits.max_spend),
            against(view.progress.matches_found, view.limits.max_matches),
        ));
    }
    ui.add_space(4.0);
    clicked
}

/// A balance readout: muted unit then bright value ("1,234,567 gold"), or "—"
/// until known. Unit is added first so the pair reads left-to-right inside the
/// bar's right-to-left row.
fn currency(ui: &mut egui::Ui, unit: &str, value: Option<u32>) {
    ui.add(egui::Label::new(egui::RichText::new(unit).weak()));
    let text = value.map_or_else(|| "—".to_owned(), grouped);
    ui.add(egui::Label::new(
        egui::RichText::new(text).color(theme::INK),
    ));
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
        visuals.widgets.hovered.weak_bg_fill = egui::Color32::TRANSPARENT;
        visuals.widgets.hovered.bg_stroke = egui::Stroke::NONE;
        visuals.widgets.active.weak_bg_fill = egui::Color32::TRANSPARENT;
        visuals.widgets.active.bg_stroke = egui::Stroke::NONE;
        visuals.widgets.inactive.fg_stroke.color = theme::INK_MUTED;
        let shop = ui.selectable_value(tab, Tab::Shop, "Shop").rect;
        let setup = ui.selectable_value(tab, Tab::Setup, "Setup").rect;
        if *tab == Tab::Shop { shop } else { setup }
    });
    // The accent underline shares the hairline's y and paints second, so the
    // two read as one baseline with the active segment on top.
    let baseline = tabs.response.rect.bottom();
    ui.painter().hline(
        ui.max_rect().x_range(),
        baseline,
        ui.visuals().widgets.noninteractive.bg_stroke,
    );
    ui.painter().hline(
        tabs.inner.x_range(),
        baseline,
        egui::Stroke::new(2.0, theme::ACCENT),
    );
    ui.add_space(6.0);
    // Tab scroll: expanded editors must never overflow a clipped panel. One
    // scroll state per tab — Setup's offset must not bleed into the table.
    egui::ScrollArea::vertical()
        .id_salt(match tab {
            Tab::Shop => "tab-shop",
            Tab::Setup => "tab-setup",
        })
        .auto_shrink([false, false])
        .show(ui, |ui| match tab {
            Tab::Shop => render_shop_tab(ui, view),
            Tab::Setup => {
                clicked = ui
                    .add_enabled_ui(session_alive, |ui| {
                        // Both editors render every frame: or_else would skip
                        // Limits on the frame Apply-filter is clicked.
                        let filter = editor::edit_filter(ui, editor);
                        let limits = editor::edit_limits(ui, editor);
                        filter.or(limits)
                    })
                    .inner;
                ui.weak("edits apply to this session only — config.toml is unchanged");
            }
        });
    clicked
}

/// The Shop tab: merchant header + the slot table, or the welcome screen
/// while nothing is captured yet.
fn render_shop_tab(ui: &mut egui::Ui, view: &ViewState) {
    // Keyed on "no capture yet", not empty rows: a tolerated slotless
    // snapshot mid-session must not resurrect first-run onboarding.
    if !view.has_snapshot {
        render_quick_start(ui);
        return;
    }
    ui.heading(view.merchant.as_str());
    if view.rows.is_empty() {
        ui.weak("the last shop message carried no slots — re-open the shop in game");
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
                        text = text.strong().color(theme::WANTED);
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

/// Welcome screen until the first snapshot lands: what the tool is, and the
/// three steps that make it go.
fn render_quick_start(ui: &mut egui::Ui) {
    ui.add_space(28.0);
    ui.vertical_centered(|ui| {
        ui.label(
            egui::RichText::new(crate::APP_NAME)
                .size(22.0)
                .color(theme::INK),
        );
        ui.weak("secret-shop relay — refresh + buy");
    });
    ui.add_space(20.0);
    ui.label(theme::section("Quick start"));
    for (number, step) in [
        (
            "1.",
            "Open the Secret Shop in game — the relay captures it live.",
        ),
        ("2.", "Setup tab — define what to hunt and when to stop."),
        ("3.", "Start — the loop refreshes and buys on its own."),
    ] {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(number).color(theme::ACCENT));
            ui.label(step);
        });
    }
}

/// Bottom chrome: the journal, filling whatever height the panel was
/// resized to.
fn render_journal(ui: &mut egui::Ui, journal: &[LogLine]) {
    ui.label(theme::section("Journal"));
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

/// Thousands-grouped decimal (`1234567` -> `1,234,567`); the stdlib has no
/// locale-free grouping to lean on.
fn grouped(n: u32) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + (len - 1) / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (len - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
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
    use crate::domain::shop::{ShopItem, ShopSnapshot};

    use super::*;

    #[test]
    fn against_renders_missing_limit_as_dash() {
        assert_eq!(against(3, Some(10)), "3/10");
        assert_eq!(against(3, None), "3/—");
    }

    #[test]
    fn grouped_inserts_thousands_separators() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(88), "88");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(1_234_567), "1,234,567");
    }

    #[test]
    fn timestamp_rolls_into_hours() {
        assert_eq!(timestamp(59_000), "+0:59");
        assert_eq!(timestamp(61_000), "+1:01");
        assert_eq!(timestamp(3_661_000), "+1:01:01");
    }

    fn idle_view() -> ViewState {
        let controller = Controller::new(Filter::default(), Limits::default());
        view_state(&controller)
    }

    fn captured_view() -> ViewState {
        let mut controller = Controller::new(Filter::default(), Limits::default());
        controller.handle(Event::Snapshot {
            snapshot: ShopSnapshot {
                merchant: None,
                slots: vec![ShopItem::default()],
                refresh: None,
            },
            now_ms: 0,
        });
        view_state(&controller)
    }

    fn watching_view() -> ViewState {
        let mut controller = Controller::new(Filter::matching_default_items(), Limits::default());
        controller.handle(Event::Start { now_ms: 0 });
        view_state(&controller)
    }

    #[test]
    fn idle_status_bar_hides_stop_and_toggle() {
        let view = idle_view();
        let harness = Harness::new_ui(|ui| {
            render_status_bar(ui, &view, None, true);
        });
        assert!(harness.query_by_label("Stop").is_none());
        assert!(harness.query_by_label("Toggle").is_none());
    }

    #[test]
    fn status_bar_shows_currencies_and_a_clean_status() {
        let view = idle_view();
        let harness = Harness::new_ui(|ui| {
            render_status_bar(ui, &view, None, true);
        });
        // Both balances are labelled by their unit, the state word is
        // title-cased, and its clause is a plain hint — no "(Start re-arms)".
        harness.get_by_label("skystones");
        harness.get_by_label("gold");
        harness.get_by_label("Idle");
        harness.get_by_label("define a filter first");
    }

    #[test]
    fn counter_line_hidden_only_while_idle() {
        let counters = "refreshes 0/— · spent 0/— · matches 0/—";

        // Idle: every counter reads 0/—, so the line is suppressed.
        let idle = idle_view();
        let idle_bar = Harness::new_ui(|ui| {
            render_status_bar(ui, &idle, None, true);
        });
        assert!(idle_bar.query_by_label(counters).is_none());

        // Watching: a live run shows its progress.
        let armed = watching_view();
        let armed_bar = Harness::new_ui(|ui| {
            render_status_bar(ui, &armed, None, true);
        });
        armed_bar.get_by_label(counters);

        // Stopped: the final totals survive the stop (an auto-stop is exactly
        // when the player wants to read them).
        let mut controller = Controller::new(Filter::matching_default_items(), Limits::default());
        controller.handle(Event::Start { now_ms: 0 });
        controller.handle(Event::Stop);
        let stopped = view_state(&controller);
        let stopped_bar = Harness::new_ui(|ui| {
            render_status_bar(ui, &stopped, None, true);
        });
        stopped_bar.get_by_label(counters);
    }

    #[test]
    fn idle_start_click_emits_start() {
        let view = idle_view();
        let clicked = RefCell::new(None);
        let mut harness = Harness::new_ui(|ui| {
            if let Some(command) = render_status_bar(ui, &view, None, true) {
                *clicked.borrow_mut() = Some(command);
            }
        });
        harness.get_by_label("Start").click();
        harness.run();
        drop(harness);
        assert_eq!(clicked.into_inner(), Some(Command::Start));
    }

    #[test]
    fn armed_status_bar_hides_start_and_toggle() {
        let view = watching_view();
        let harness = Harness::new_ui(|ui| {
            render_status_bar(ui, &view, None, true);
        });
        assert!(harness.query_by_label("Start").is_none());
        assert!(harness.query_by_label("Toggle").is_none());
    }

    #[test]
    fn armed_stop_click_emits_stop() {
        let view = watching_view();
        let clicked = RefCell::new(None);
        let mut harness = Harness::new_ui(|ui| {
            if let Some(command) = render_status_bar(ui, &view, None, true) {
                *clicked.borrow_mut() = Some(command);
            }
        });
        harness.get_by_label("Stop").click();
        harness.run();
        drop(harness);
        assert_eq!(clicked.into_inner(), Some(Command::Stop));
    }

    #[test]
    fn shop_tab_hides_the_editors() {
        let view = idle_view();
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(Filter::default(), Limits::default());
        let harness = Harness::new_ui(|ui| {
            render_center(ui, &view, &mut tab, &mut editor, true);
        });
        assert!(harness.query_by_label("Filter").is_none());
        assert!(harness.query_by_label("Apply filter").is_none());
    }

    #[test]
    fn setup_tab_reveals_the_editors() {
        let view = idle_view();
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(Filter::default(), Limits::default());
        let mut harness = Harness::new_ui(|ui| {
            render_center(ui, &view, &mut tab, &mut editor, true);
        });
        harness.get_by_label("Setup").click();
        harness.run();
        harness.get_by_label("Filter").click();
        harness.run();
        harness.get_by_label("Apply filter");
        drop(harness);
        assert_eq!(tab, Tab::Setup);
    }

    #[test]
    fn empty_shop_tab_shows_the_quick_start() {
        let view = idle_view();
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(Filter::default(), Limits::default());
        let harness = Harness::new_ui(|ui| {
            render_center(ui, &view, &mut tab, &mut editor, true);
        });
        harness.get_by_label("QUICK START");
    }

    #[test]
    fn captured_shop_replaces_the_quick_start() {
        let view = captured_view();
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(Filter::default(), Limits::default());
        let harness = Harness::new_ui(|ui| {
            render_center(ui, &view, &mut tab, &mut editor, true);
        });
        assert!(harness.query_by_label("QUICK START").is_none());
    }

    #[test]
    fn slotless_snapshot_does_not_resurrect_the_quick_start() {
        // A tolerated degraded shop message (slots dropped by lenient
        // decoding) counts as captured: mid-session onboarding would lie.
        let mut controller = Controller::new(Filter::default(), Limits::default());
        controller.handle(Event::Snapshot {
            snapshot: ShopSnapshot {
                merchant: None,
                slots: vec![],
                refresh: None,
            },
            now_ms: 0,
        });
        let view = view_state(&controller);
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(Filter::default(), Limits::default());
        let harness = Harness::new_ui(|ui| {
            render_center(ui, &view, &mut tab, &mut editor, true);
        });
        assert!(harness.query_by_label("QUICK START").is_none());
    }

    #[test]
    fn journal_lines_render_with_timestamps() {
        let lines = vec![LogLine {
            at_ms: 61_000,
            text: "refresh advised".to_owned(),
        }];
        let harness = Harness::new_ui(|ui| render_journal(ui, &lines));
        harness.get_by_label("[+1:01] refresh advised");
    }
}
