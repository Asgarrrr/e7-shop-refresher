//! egui window over the session: copies the controller state once per frame,
//! renders it, and pushes player commands through the existing channel. No
//! egui type crosses into the app layer or the domain.
//!
//! Layout: a status bar on top, a collapsible journal at the bottom (closed
//! by default), and a tabbed center. Surfaces live in their own modules
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
use crate::watch::HaltSource;

use editor::EditorState;
use view::{SlotRow, SlotRows, ViewState, slot_detail, view_state};

/// Where the session's terminal outcome lands (fatal error, crash, or clean
/// end): written once by the spawn wrapper in `main`, shown as a banner.
pub type SessionErrorSlot = Arc<Mutex<Option<String>>>;

/// A poisoned lock means the session panicked. Keep rendering the last
/// state (the banner reports the crash) rather than double-panic the window.
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
    /// Journal snapshot re-cloned only when the generation changes.
    journal_cache: Vec<LogLine>,
    journal_generation: u64,
    /// The slot table's rows, cached under the same rule. See [`SlotRows`].
    slots: SlotRows,
    /// Starts collapsed to its title bar so the shop table owns the height.
    journal_open: bool,
    /// Drives the error banner's Download button. Idle unless a player without
    /// Npcap presses it, and its worker outlives no session — it is a property
    /// of the window, not of a run.
    fetcher: crate::install::Fetcher,
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
        // Seed the drafts from the controller; Timings aren't domain state,
        // so they seed from the startup config value instead.
        let (editor, slots) = {
            let ctrl = lock_ignoring_poison(&handles.controller);
            let mut slots = SlotRows::default();
            // Seeded here so the first frame finds the cache already current.
            slots.sync(&ctrl);
            (
                EditorState::new(ctrl.filter().clone(), *ctrl.limits(), timings),
                slots,
            )
        };
        let journal_cache = handles.journal.to_entries();
        let journal_generation = handles.journal.generation();
        Self {
            handles,
            config_path,
            error,
            editor,
            tab: Tab::Shop,
            journal_cache,
            journal_generation,
            slots,
            journal_open: false,
            fetcher: crate::install::Fetcher::new(),
        }
    }
}

impl eframe::App for ShopApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Poll-based repaint: state arrives at human pace; 4 Hz keeps the
        // window fresh without coupling app.rs to egui.
        ui.ctx().request_repaint_after(Duration::from_millis(250));
        // One short hold for both: the projection reads `Copy` state, and
        // rows re-derive only if the shop or checklist moved. See
        // `view::SlotRows`.
        let view = {
            let ctrl = lock_ignoring_poison(&self.handles.controller);
            self.slots.sync(&ctrl);
            view_state(&ctrl)
        };
        let generation = self.handles.journal.generation();
        if generation != self.journal_generation {
            self.journal_cache = self.handles.journal.to_entries();
            self.journal_generation = generation;
        }
        let outcome = lock_ignoring_poison(&self.error).clone();
        // A terminal outcome disables every control: its click would hit a dead channel.
        let session_alive = outcome.is_none();

        // Roomier margins than egui's stock 8px, using `theme::EDGE` as the
        // side inset so the table text lines up under the status bar.
        let margin = egui::Margin::symmetric(theme::EDGE, 10);
        let clicked = egui::Panel::top("status_bar")
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(margin))
            .show(ui, |ui| {
                statusbar::render_status_bar(
                    ui,
                    &view,
                    outcome.as_deref(),
                    session_alive,
                    &self.fetcher,
                )
            })
            .inner;
        // Collapsed to its title bar by default; expands on click. The tab
        // strip and a few rows always survive the divider drag.
        let open = self.journal_open;
        // The peek only matters while collapsed; borrow it (no per-frame clone).
        let latest = if open {
            None
        } else {
            self.journal_cache.last().map(|line| &*line.text)
        };
        let mut panel = egui::Panel::bottom("journal")
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(margin))
            .resizable(open);
        if open {
            // Floor the max at the min (80) so a short window can't invert the range.
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
                // Reserve the header's bottom margin so its hover fill doesn't cover the first log line.
                ui.add_space(f32::from(margin.bottom));
                journal::render_journal_body(ui, &self.journal_cache);
            }
        });
        // Zero-margin panel so the tab strip spans full width, flush to the
        // edges. Its own hairline is the divider, so egui's separator is
        // suppressed to avoid doubling it.
        egui::Panel::top("tabs")
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::ZERO))
            .show_separator_line(false)
            .show(ui, |ui| render_tabs(ui, &mut self.tab));
        // Only the top margin survives here: content spans full width for
        // the hover fill, other tabs inset text via `theme::EDGE`. Tooltip
        // source built here, not carried in the projection — see
        // `view::slot_detail`.
        let controller = &self.handles.controller;
        let detail = |index: usize| slot_detail(&lock_ignoring_poison(controller), index);
        let rows = self.slots.rows();
        let applied = egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::ZERO))
            .show(ui, |ui| {
                render_tab_content(
                    ui,
                    &view,
                    rows,
                    self.tab,
                    &mut self.editor,
                    session_alive,
                    &detail,
                )
            })
            .inner;
        // Dispatch first, then record: "applied" state and persistence both
        // key off what the session actually took. Stop bypasses the queue so
        // saturation can't suppress it.
        let mut delivered = Vec::new();
        for command in clicked.into_iter().chain(applied) {
            if deliver_command(&self.handles, command.clone()) {
                delivered.push(command);
            }
        }
        // Only now is a draft "applied": Apply goes dark and the peek clears.
        self.editor.mark_applied(&delivered);
        // Persist what the session accepted; best-effort — a write failure
        // only costs the on-disk copy, so it's journaled and moved past.
        let sections = persisted_sections(&delivered);
        if !sections.is_empty()
            && let Err(err) = config::persist::save(&self.config_path, &sections)
        {
            // The journal is a 500-entry ring that dies with the window, so
            // this also goes to the subscriber reaching the log file. Names
            // the sections lost.
            let labels = section_labels(&sections);
            tracing::warn!(
                error = ?err,
                path = %self.config_path.display(),
                sections = %labels,
                "config.toml not saved"
            );
            // `push`, not `emit_at`: the file half is the `warn!` above with
            // its fields; `emit_at` would duplicate the prose and drop them.
            self.handles.journal.push(&[format!(
                "config.toml not saved ({labels}): {}",
                err.report()
            )]);
        }
    }
}

/// Hands one command to the session; `true` when it was taken.
///
/// `Stop` bypasses the capacity-16 queue via the gate's halt latch, so it
/// can never be dropped. Everything else rides the queue, which a dying or
/// throttled session can leave full or closed; a dropped `try_send` is
/// journaled and reported back rather than silently swallowed.
#[must_use]
fn deliver_command(handles: &SessionHandles, command: Command) -> bool {
    if command == Command::Stop {
        handles.gate.request_halt(HaltSource::PlayerStopped);
        return true;
    }
    if handles.commands.try_send(command).is_err() {
        // Journalled and logged: the ring dies with the window, so a silent
        // drop would be undiagnosable afterward.
        tracing::debug!("a player command was dropped: the session queue is full or closed");
        // `push`, same reason as above: the file half is the `debug!`
        // above, at a level `emit_at` can't express (INFO/WARN/ERROR only).
        handles
            .journal
            .push(&[">> command dropped — the session is busy, try again".to_owned()]);
        return false;
    }
    true
}

/// The tab strip: labels over a full-width hairline, active segment underlined.
fn render_tabs(ui: &mut egui::Ui, tab: &mut Tab) {
    // Tabs read as tabs, not buttons: pill fills are stripped, the active tab marked by an underline instead.
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
    // The accent underline shares the hairline's y and paints second, so both read as one baseline.
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
    // The underline is a 2px stroke centred on the baseline; reserve its lower half so it isn't clipped.
    ui.add_space(theme::SP_XS);
}

/// The persistable sections for a batch of Apply commands — only the three
/// `Set*` producers; Start/Stop and friends are skipped.
///
/// The non-`Set*` arm is spelled out rather than `_`: this is the only
/// bridge from Apply to `config.toml`, so a wildcard would let a new `Set*`
/// retune the session and silently vanish on next launch. `write_sections`
/// is exhaustive over `Section`, so the compiler catches a missed write —
/// but never a missed collection here.
fn persisted_sections(commands: &[Command]) -> Vec<config::persist::Section> {
    commands
        .iter()
        .filter_map(|command| match command {
            Command::SetFilter(filter) => Some(config::persist::Section::Filter(filter.clone())),
            Command::SetLimits(limits) => Some(config::persist::Section::Limits(*limits)),
            Command::SetTimings(timings) => Some(config::persist::Section::Timings(*timings)),
            Command::Start | Command::Stop | Command::Toggle => None,
        })
        .collect()
}

/// The Setup section titles behind a batch of sections, for the "not saved"
/// report — same words the UI uses, so the message points at the block.
fn section_labels(sections: &[config::persist::Section]) -> String {
    sections
        .iter()
        .map(|section| match section {
            config::persist::Section::Filter(_) => "Hunt",
            config::persist::Section::Limits(_) => "Stop",
            config::persist::Section::Timings(_) => "Click timing",
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The active tab's content; returns the commands the player committed. One
/// scroll state per tab, so Setup's offset can't bleed into the table.
#[must_use]
fn render_tab_content(
    ui: &mut egui::Ui,
    view: &ViewState,
    rows: &[SlotRow],
    tab: Tab,
    editor: &mut EditorState,
    session_alive: bool,
    detail: &dyn Fn(usize) -> String,
) -> Vec<Command> {
    match tab {
        // The shop table bleeds hover fill to the edges itself: no inset, and commits nothing.
        Tab::Shop => {
            egui::ScrollArea::vertical()
                .id_salt("tab-shop")
                .auto_shrink([false, false])
                .show(ui, |ui| shop::render_shop_tab(ui, view, rows, detail));
            Vec::new()
        }
        Tab::Setup => render_setup_tab(ui, editor, session_alive),
    }
}

/// The Setup tab: a pinned commit bar (bottom sub-panel, shown first) over a
/// scrolling body, keeping Apply reachable at any scroll offset instead of
/// trailing off-screen.
#[must_use]
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

/// Insets tab content by the side padding. The shop table opts out, to
/// bleed its hover fill full width; everything else takes the inset.
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

    /// Test shim: shell renders the strip and content as two panels so a
    /// test can click a tab and assert on the content it reveals.
    fn render_center(
        ui: &mut egui::Ui,
        view: &ViewState,
        rows: &[SlotRow],
        tab: &mut Tab,
        editor: &mut EditorState,
        session_alive: bool,
    ) -> Vec<Command> {
        render_tabs(ui, tab);
        // No test here hovers a row; `view::tests` covers the tooltip source.
        render_tab_content(ui, view, rows, *tab, editor, session_alive, &|_| {
            String::new()
        })
    }

    /// One frame's projection, as `ShopApp::ui` takes it: cheap state plus gated rows, one borrow.
    fn project(controller: &Controller) -> (ViewState, SlotRows) {
        let mut slots = SlotRows::default();
        slots.sync(controller);
        (view_state(controller), slots)
    }

    fn idle_view() -> (ViewState, SlotRows) {
        project(&Controller::new(Filter::default(), Limits::default()))
    }

    fn captured_view() -> (ViewState, SlotRows) {
        let mut controller = Controller::new(Filter::default(), Limits::default());
        let _ = controller.handle(Event::Snapshot {
            snapshot: ShopSnapshot {
                merchant: None,
                slots: vec![ShopItem::default()],
                refresh: None,
            },
            now_ms: 0,
        });
        project(&controller)
    }

    #[test]
    fn only_setup_commands_become_persisted_sections() {
        let commands = vec![Command::Start, Command::SetLimits(Limits::default())];
        let sections = persisted_sections(&commands);
        assert_eq!(
            sections,
            vec![config::persist::Section::Limits(Limits::default())]
        );
        // …and the failure report names it by its Setup section title.
        assert_eq!(section_labels(&sections), "Stop");
    }

    /// Live handles built as `main` builds them. `Session` must stay alive
    /// for the caller — dropping it closes the channel instead of filling it.
    fn session_handles() -> (crate::app::Session, SessionHandles) {
        let (session, handles, _shutdown) = crate::app::setup(config::Config::default());
        (session, handles)
    }

    /// Saturates the command queue, so the next `try_send` can only fail.
    fn fill_command_queue(handles: &SessionHandles) {
        while handles.commands.try_send(Command::Toggle).is_ok() {}
    }

    #[tokio::test]
    async fn full_command_queue_cannot_drop_stop() {
        let (_session, handles) = session_handles();
        fill_command_queue(&handles);
        assert!(matches!(
            handles.commands.try_send(Command::Toggle),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_))
        ));
        // Arm the gate first so the assert below reads the halt, not the startup state.
        handles.gate.set(true);

        assert!(deliver_command(&handles, Command::Stop));

        assert!(!handles.gate.is_enabled());
        assert_eq!(handles.gate.next_halt().await, HaltSource::PlayerStopped);
    }

    #[test]
    fn a_dropped_command_is_journaled_and_reported() {
        // The window is the only interface: a click the saturated queue refuses must reach the player.
        let (_session, handles) = session_handles();
        fill_command_queue(&handles);
        let before = handles.journal.to_entries().len();

        assert!(!deliver_command(&handles, Command::Start));

        let entries = handles.journal.to_entries();
        assert_eq!(entries.len(), before + 1);
        let line = &entries.last().expect("a journaled line").text;
        assert!(line.contains("command dropped"), "unexpected line: {line}");
    }

    #[test]
    fn a_dropped_apply_is_persisted_nowhere() {
        // What the session refused never reaches config.toml: the persisted
        // batch is the *delivered* commands, not the emitted ones.
        let (_session, handles) = session_handles();
        fill_command_queue(&handles);
        let command = Command::SetLimits(Limits {
            max_refreshes: Some(7),
            ..Limits::default()
        });

        // The shell's own loop, in miniature.
        let mut delivered = Vec::new();
        if deliver_command(&handles, command.clone()) {
            delivered.push(command);
        }

        assert!(delivered.is_empty());
        assert!(persisted_sections(&delivered).is_empty());
    }

    #[test]
    fn a_delivered_apply_is_persisted() {
        let (_session, handles) = session_handles();
        let command = Command::SetLimits(Limits {
            max_refreshes: Some(7),
            ..Limits::default()
        });

        assert!(deliver_command(&handles, command.clone()));

        assert_eq!(persisted_sections(&[command]).len(), 1);
    }

    #[test]
    fn shop_tab_hides_the_editors() {
        let (view, slots) = idle_view();
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(Filter::default(), Limits::default(), Timings::default());
        let harness = Harness::new_ui(|ui| {
            render_center(ui, &view, slots.rows(), &mut tab, &mut editor, true);
        });
        // The Hunt section header and the single Apply live only on the Setup tab.
        assert!(harness.query_by_label("Hunt").is_none());
        assert!(harness.query_by_label("Apply").is_none());
    }

    #[test]
    fn setup_tab_reveals_the_editors() {
        let (view, slots) = idle_view();
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(Filter::default(), Limits::default(), Timings::default());
        let mut harness = Harness::new_ui(|ui| {
            render_center(ui, &view, slots.rows(), &mut tab, &mut editor, true);
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
        let (view, slots) = idle_view();
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(Filter::default(), Limits::default(), Timings::default());
        let harness = Harness::new_ui(|ui| {
            render_center(ui, &view, slots.rows(), &mut tab, &mut editor, true);
        });
        harness.get_by_label("QUICK START");
    }

    #[test]
    fn captured_shop_replaces_the_quick_start() {
        let (view, slots) = captured_view();
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(Filter::default(), Limits::default(), Timings::default());
        let harness = Harness::new_ui(|ui| {
            render_center(ui, &view, slots.rows(), &mut tab, &mut editor, true);
        });
        assert!(harness.query_by_label("QUICK START").is_none());
    }

    #[test]
    fn slotless_snapshot_does_not_resurrect_the_quick_start() {
        // A tolerated degraded shop message counts as captured: mid-session onboarding would lie.
        let mut controller = Controller::new(Filter::default(), Limits::default());
        let _ = controller.handle(Event::Snapshot {
            snapshot: ShopSnapshot {
                merchant: None,
                slots: vec![],
                refresh: None,
            },
            now_ms: 0,
        });
        let (view, slots) = project(&controller);
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(Filter::default(), Limits::default(), Timings::default());
        let harness = Harness::new_ui(|ui| {
            render_center(ui, &view, slots.rows(), &mut tab, &mut editor, true);
        });
        assert!(harness.query_by_label("QUICK START").is_none());
    }
}
