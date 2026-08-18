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
use crate::watch::HaltSource;

use editor::EditorState;
use view::{ViewState, slot_detail, view_state};

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
        // The side inset is `theme::EDGE` itself, not a second copy of 16 — the
        // table text is inset by that same constant so the columns line up
        // under the status bar.
        let margin = egui::Margin::symmetric(theme::EDGE, 10);
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
        // The slot table's tooltip is built through this, not carried in the
        // projection: egui calls it for the hovered row only, so the item line
        // costs one short lock per hovered frame instead of one `format_item`
        // per row inside the frame's own lock hold (see `view::slot_detail`).
        let controller = &self.handles.controller;
        let detail = |index: usize| slot_detail(&lock_ignoring_poison(controller), index);
        let applied = egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::ZERO))
            .show(ui, |ui| {
                render_tab_content(
                    ui,
                    &view,
                    self.tab,
                    &mut self.editor,
                    session_alive,
                    &detail,
                )
            })
            .inner;
        // Dispatch first, then record. Everything downstream — the editor's
        // "applied" state and the on-disk copy — is keyed on what the session
        // actually took, so a click the bounded queue dropped leaves no trace
        // claiming otherwise. Stop bypasses that queue so saturation can never
        // suppress the immediate gate cutoff or its durable notification.
        let mut delivered = Vec::new();
        for command in clicked.into_iter().chain(applied) {
            if deliver_command(&self.handles, command.clone()) {
                delivered.push(command);
            }
        }
        // Only now is a draft "applied": Apply goes dark and the peek clears.
        self.editor.mark_applied(&delivered);
        // Persist what the session accepted. Best-effort: a write failure only
        // costs the on-disk copy — the live retune already went through — so it
        // is journaled and moved past.
        let sections = persisted_sections(&delivered);
        if !sections.is_empty()
            && let Err(err) = config::persist::save(&self.config_path, &sections)
        {
            // The journal is a 500-entry in-memory ring that dies with the
            // window, and this is exactly the failure a player comes back to ask
            // about ("my Setup changes keep reverting") — so it also goes to the
            // subscriber, which is what reaches the log file they are asked to
            // send. The journal line names the sections so the banner says
            // *what* was lost, not only that something was.
            let labels = section_labels(&sections);
            tracing::warn!(
                error = ?err,
                path = %self.config_path.display(),
                sections = %labels,
                "config.toml not saved"
            );
            self.handles
                .journal
                .push(&[format!("config.toml not saved ({labels}): {err}")]);
        }
    }
}

/// Hands one command to the session; `true` when it was taken.
///
/// `Stop` is a safety cutoff and never rides the bounded queue — the gate's
/// durable halt latch takes it, so it cannot be dropped and always counts as
/// delivered. Everything else goes through the capacity-16 channel, which a
/// dying or throttled session can leave full or closed. In the shipped build
/// stdin is inert and this window is the only interface, so a swallowed
/// `try_send` would make a click a perfectly silent no-op. The same rule the
/// session already applies to actuator jobs holds here: a lost click must not be
/// silent, so the drop is journaled *and* reported back to the caller.
#[must_use]
fn deliver_command(handles: &SessionHandles, command: Command) -> bool {
    if command == Command::Stop {
        handles.gate.request_halt(HaltSource::PlayerStopped);
        return true;
    }
    if handles.commands.try_send(command).is_err() {
        // Journalled for the player and logged for us: the ring is gone once the
        // window closes, and "the button did nothing" is only diagnosable after
        // the fact from the file.
        tracing::debug!("a player command was dropped: the session queue is full or closed");
        handles
            .journal
            .push(&[">> command dropped — the session is busy, try again".to_owned()]);
        return false;
    }
    true
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
///
/// The non-`Set*` arm is spelled out rather than a `_`, like the sibling
/// [`EditorState::mark_applied`]: this function is the *only* bridge from a
/// delivered Apply to `config.toml`, so a fourth `Set*` falling into a wildcard
/// would retune the live session and then silently vanish on the next launch.
/// Nothing else would catch it — `persist::write_sections` is exhaustive over
/// `Section`, so the compiler would demand the new section be *written* while
/// never demanding it be collected here.
fn persisted_sections(commands: &[Command]) -> Vec<config::persist::Section> {
    commands
        .iter()
        .filter_map(|command| match command {
            Command::SetFilter(filter) => Some(config::persist::Section::Filter(filter.clone())),
            Command::SetLimits(limits) => Some(config::persist::Section::Limits(limits.clone())),
            Command::SetTimings(timings) => Some(config::persist::Section::Timings(*timings)),
            Command::Start | Command::Stop | Command::Toggle => None,
        })
        .collect()
}

/// The Setup section titles behind a batch of sections, for the "not saved"
/// report. Same words the collapsible bars and the commit-bar peek use, so the
/// message points straight at the block whose edit did not reach disk.
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

/// The active tab's content. Returns the commands the player committed (Setup's
/// single Apply lives here, and may send several). One scroll state per tab —
/// Setup's offset must not bleed into the table.
#[must_use]
fn render_tab_content(
    ui: &mut egui::Ui,
    view: &ViewState,
    tab: Tab,
    editor: &mut EditorState,
    session_alive: bool,
    detail: &dyn Fn(usize) -> String,
) -> Vec<Command> {
    match tab {
        // The shop table bleeds its hover fill to the edges itself, so it takes
        // no inset and commits nothing.
        Tab::Shop => {
            egui::ScrollArea::vertical()
                .id_salt("tab-shop")
                .auto_shrink([false, false])
                .show(ui, |ui| shop::render_shop_tab(ui, view, detail));
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
        // No test here hovers a row, so the tooltip source is never called;
        // `view::tests` covers what the live shell passes in its place.
        render_tab_content(ui, view, *tab, editor, session_alive, &|_| String::new())
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
        let commands = vec![
            Command::Start,
            Command::SetLimits(crate::domain::control::Limits::default()),
        ];
        let sections = persisted_sections(&commands);
        // Start is not persisted; the limits edit is.
        assert_eq!(
            sections,
            vec![crate::config::persist::Section::Limits(Limits::default())]
        );
        // …and the failure report names it by its Setup section title.
        assert_eq!(section_labels(&sections), "Stop");
    }

    /// Live handles built the way `main` builds them, so a field added to
    /// `SessionHandles` never breaks this file. The `Session` half must stay
    /// alive for the caller: it owns the command receiver, and dropping it
    /// would close the channel instead of filling it.
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
        // Arm the gate first, so the assert below reads the halt, not the
        // startup state.
        handles.gate.set(true);

        assert!(deliver_command(&handles, Command::Stop));

        assert!(!handles.gate.is_enabled());
        assert_eq!(
            handles.gate.halt_requested().await,
            HaltSource::PlayerStopped
        );
    }

    #[test]
    fn a_dropped_command_is_journaled_and_reported() {
        // The window is the only interface in the shipped build: a click the
        // saturated queue refuses must reach the player, not vanish.
        let (_session, handles) = session_handles();
        fill_command_queue(&handles);
        let before = handles.journal.entries().len();

        assert!(!deliver_command(&handles, Command::Start));

        let entries = handles.journal.entries();
        assert_eq!(entries.len(), before + 1);
        let line = &entries.last().expect("a journaled line").text;
        assert!(line.contains("command dropped"), "unexpected line: {line}");
    }

    #[test]
    fn a_dropped_apply_is_persisted_nowhere() {
        // The other half of the honesty rule (the editor's own twins are
        // covered in `editor::tests`): what the session refused never reaches
        // config.toml either, because the persisted batch is the *delivered*
        // commands, not the emitted ones.
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
