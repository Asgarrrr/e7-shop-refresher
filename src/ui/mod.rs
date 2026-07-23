//! egui window over the session: copies the controller state once per frame,
//! renders it, and pushes player commands through the existing channel. No
//! egui type crosses into the app layer or the domain.
//!
//! Layout: permanent chrome (status bar on top, a collapsible journal at the
//! bottom — closed by default so the shop table owns the height) around a
//! tabbed center — the state the player watches is always on screen, the
//! editors never push it away. The surfaces live in their own modules
//! (`statusbar`, `shop`, `journal`); this file is the shell and the tab strip.

mod editor;
mod journal;
mod shop;
mod statusbar;
mod theme;
mod view;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;

use crate::actuator::plan::Timings;
use crate::app::{Command, SessionHandles};
use crate::config;
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
    config_path: PathBuf,
    error: SessionErrorSlot,
    editor: EditorState,
    tab: Tab,
    /// Journal snapshot re-cloned only when the generation changes: the
    /// journal grows at human pace, repaints at display rate.
    journal_cache: Vec<LogLine>,
    journal_generation: u64,
    /// The journal is secondary; it starts collapsed to its title bar so the
    /// shop table owns the height, and expands on a click.
    journal_open: bool,
}

impl ShopApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        handles: SessionHandles,
        error: SessionErrorSlot,
        timings: Timings,
        config_path: PathBuf,
    ) -> Self {
        theme::apply(&cc.egui_ctx);
        // Seed the drafts from the controller itself — the single source of
        // the criteria actually running. Timings aren't domain state, so their
        // seed is the startup config value (the running value before any
        // retune).
        let editor = {
            let ctrl = lock_ignoring_poison(&handles.controller);
            EditorState::new(ctrl.filter().clone(), ctrl.limits().clone(), timings)
        };
        let journal_cache = handles.journal.entries();
        let journal_generation = handles.journal.generation();
        Self {
            handles,
            config_path,
            error,
            editor,
            tab: Tab::Shop,
            journal_cache,
            journal_generation,
            journal_open: false,
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
        let clicked = egui::Panel::top("status_bar")
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(margin))
            .show(ui, |ui| {
                statusbar::render_status_bar(ui, &view, outcome.as_deref(), session_alive)
            })
            .inner;
        // Journal: collapsed to its title bar by default so the shop table
        // owns the height; a click expands it to a resizable log. When open it
        // may grow but must never crush the center — the tab strip and a few
        // rows always survive the divider drag.
        let open = self.journal_open;
        // The peek only matters while collapsed; borrow it (no per-frame clone).
        let latest = if open {
            None
        } else {
            self.journal_cache.last().map(|line| line.text.as_str())
        };
        let mut panel = egui::Panel::bottom("journal")
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(margin))
            .resizable(open);
        if open {
            // Floor the max at the min (80) so a very short window can't invert
            // the range.
            let journal_max = (ui.available_rect_before_wrap().height() - 160.0).max(80.0);
            panel = panel
                .default_size(200.0)
                .size_range(egui::Rangef::new(80.0, journal_max));
        }
        panel.show(ui, |ui| {
            if journal::render_journal_header(ui, open, latest, margin) {
                self.journal_open = !open;
            }
            if open {
                // Reserve the header's bottom margin so its symmetric hover
                // fill lands in real space, not over the first log line.
                ui.add_space(f32::from(margin.bottom));
                journal::render_journal_body(ui, &self.journal_cache);
            }
        });
        // Tab strip in its own zero-margin top panel: the tabs and their
        // underline span the full width, flush to the window edges, instead of
        // sitting inset inside the content margin. Its own baseline hairline is
        // the divider, so egui's panel separator would double it — suppress it.
        egui::Panel::top("tabs")
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::ZERO))
            .show_separator_line(false)
            .show(ui, |ui| render_tabs(ui, &mut self.tab));
        // The central panel always comes last: it takes whatever space the
        // chrome left over. Only the top margin survives — the content spans
        // full width so the shop table's hover fill and rules reach the window
        // edges; text is inset per-tab (`theme::EDGE`) instead. No vertical
        // margin: the tab strip's own SP_XS below the underline is the only gap
        // to the content, and the journal's separator sits flush below.
        let applied = egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::ZERO))
            .show(ui, |ui| {
                render_tab_content(ui, &view, self.tab, &mut self.editor, session_alive)
            })
            .inner;
        // Persist the Setup edits before dispatch. Best-effort: the live apply
        // below is unaffected by a write failure — a read-only or unwritable
        // config.toml only costs the on-disk copy, journaled and moved past.
        let sections = persisted_sections(&applied);
        if !sections.is_empty()
            && let Err(err) = config::persist::save(&self.config_path, &sections)
        {
            self.handles
                .journal
                .push(&[format!("config.toml not saved: {err}")]);
        }
        // The status bar sends at most one command; Setup's single Apply may
        // send several (one per changed draft). A full channel only happens
        // with a dead session loop, where the banner already explains the
        // situation: dropping the click is fine.
        for command in clicked.into_iter().chain(applied) {
            let _ = self.handles.commands.try_send(command);
        }
    }
}

/// The tab strip: flush selectable labels over a full-width hairline with the
/// active segment underlined in accent. Lives in a zero-margin panel, so its
/// underline spans the whole window.
fn render_tabs(ui: &mut egui::Ui, tab: &mut Tab) {
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
    // The underline is a 2px stroke centred on the baseline: reserve its lower
    // half so the zero-margin panel doesn't clip it.
    ui.add_space(theme::SP_XS);
}

/// The persistable sections for a batch of Apply commands — only the three
/// `Set*` producers; Start/Stop and friends are skipped.
fn persisted_sections(commands: &[Command]) -> Vec<config::persist::Section> {
    commands
        .iter()
        .filter_map(|command| match command {
            Command::SetFilter(filter) => Some(config::persist::Section::Filter(filter.clone())),
            Command::SetLimits(limits) => Some(config::persist::Section::Limits(limits.clone())),
            Command::SetTimings(timings) => Some(config::persist::Section::Timings(*timings)),
            _ => None,
        })
        .collect()
}

/// The active tab's content. Returns the commands the player committed (Setup's
/// single Apply lives here, and may send several). One scroll state per tab —
/// Setup's offset must not bleed into the table.
fn render_tab_content(
    ui: &mut egui::Ui,
    view: &ViewState,
    tab: Tab,
    editor: &mut EditorState,
    session_alive: bool,
) -> Vec<Command> {
    match tab {
        // The shop table bleeds its hover fill to the edges itself, so it takes
        // no inset and commits nothing.
        Tab::Shop => {
            egui::ScrollArea::vertical()
                .id_salt("tab-shop")
                .auto_shrink([false, false])
                .show(ui, |ui| shop::render_shop_tab(ui, view));
            Vec::new()
        }
        Tab::Setup => render_setup_tab(ui, editor, session_alive),
    }
}

/// The Setup tab, split into a pinned commit bar over a scrolling body: the bar
/// is a bottom sub-panel (shown first so it reserves its strip), the sections
/// scroll in whatever height is left. This keeps Apply reachable at any scroll
/// offset instead of trailing the last section off-screen. Its `side_top_panel`
/// frame draws the hairline that separates it from the body.
fn render_setup_tab(
    ui: &mut egui::Ui,
    editor: &mut EditorState,
    session_alive: bool,
) -> Vec<Command> {
    let mut clicked = Vec::new();
    egui::Panel::bottom("setup_commit")
        .frame(
            egui::Frame::side_top_panel(ui.style())
                .inner_margin(egui::Margin::symmetric(theme::EDGE, 8)),
        )
        .show(ui, |ui| {
            clicked = editor::commit_row(ui, editor, session_alive);
        });
    egui::ScrollArea::vertical()
        .id_salt("tab-setup")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            content_inset(ui, |ui| {
                ui.add_enabled_ui(session_alive, |ui| editor::edit_sections(ui, editor));
            });
        });
    clicked
}

/// Inset tab content by the side padding so text sits off the window edges.
/// The shop table opts out of this to bleed its hover fill and rules full
/// width; everything else (editors, the quick-start screen) takes the inset.
pub(super) fn content_inset<R>(
    ui: &mut egui::Ui,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(theme::EDGE, 0))
        .show(ui, add_contents)
        .inner
}

#[cfg(test)]
mod tests {
    use egui_kittest::{Harness, kittest::Queryable};

    use crate::domain::control::{Controller, Event, Limits};
    use crate::domain::filter::Filter;
    use crate::domain::shop::{ShopItem, ShopSnapshot};

    use super::*;

    /// Test shim: the shell renders the strip and the content as two panels;
    /// the harness stacks them in one ui so a test can click a tab and then
    /// assert on the content it reveals.
    fn render_center(
        ui: &mut egui::Ui,
        view: &ViewState,
        tab: &mut Tab,
        editor: &mut EditorState,
        session_alive: bool,
    ) -> Vec<Command> {
        render_tabs(ui, tab);
        render_tab_content(ui, view, *tab, editor, session_alive)
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

    #[test]
    fn only_setup_commands_become_persisted_sections() {
        use crate::app::Command;
        let commands = vec![
            Command::Start,
            Command::SetLimits(crate::domain::control::Limits::default()),
        ];
        let sections = persisted_sections(&commands);
        // Start is not persisted; the limits edit is.
        assert_eq!(sections.len(), 1);
        assert!(matches!(
            sections[0],
            crate::config::persist::Section::Limits(_)
        ));
    }

    #[test]
    fn shop_tab_hides_the_editors() {
        let view = idle_view();
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(Filter::default(), Limits::default(), Timings::default());
        let harness = Harness::new_ui(|ui| {
            render_center(ui, &view, &mut tab, &mut editor, true);
        });
        // The Hunt section header and the single Apply live only on the Setup tab.
        assert!(harness.query_by_label("Hunt").is_none());
        assert!(harness.query_by_label("Apply").is_none());
    }

    #[test]
    fn setup_tab_reveals_the_editors() {
        let view = idle_view();
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(Filter::default(), Limits::default(), Timings::default());
        let mut harness = Harness::new_ui(|ui| {
            render_center(ui, &view, &mut tab, &mut editor, true);
        });
        harness.get_by_label("Setup").click();
        harness.run();
        // The Hunt section bar and the commit button reveal on the Setup tab.
        harness.get_by_label("Hunt");
        harness.get_by_label("Apply");
        drop(harness);
        assert_eq!(tab, Tab::Setup);
    }

    #[test]
    fn empty_shop_tab_shows_the_quick_start() {
        let view = idle_view();
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(Filter::default(), Limits::default(), Timings::default());
        let harness = Harness::new_ui(|ui| {
            render_center(ui, &view, &mut tab, &mut editor, true);
        });
        harness.get_by_label("QUICK START");
    }

    #[test]
    fn captured_shop_replaces_the_quick_start() {
        let view = captured_view();
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(Filter::default(), Limits::default(), Timings::default());
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
        let mut editor = EditorState::new(Filter::default(), Limits::default(), Timings::default());
        let harness = Harness::new_ui(|ui| {
            render_center(ui, &view, &mut tab, &mut editor, true);
        });
        assert!(harness.query_by_label("QUICK START").is_none());
    }
}
