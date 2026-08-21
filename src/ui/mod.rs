//! egui window over the session: copies the controller state once per frame,
//! renders it, and pushes player commands through the existing channel. No
//! egui type crosses into the app layer or the domain.
//!
//! Layout: a status bar on top, a collapsible journal at the bottom, and a
//! tabbed center. This file is the shell and the tab strip.

mod capture_health;
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
use crate::domain::control::Status;
use crate::journal::LogLine;
use crate::watch::HaltSource;

use capture_health::{CaptureHealthView, render_capture_health};
use editor::EditorState;
pub use editor::StartupSettings;
use view::{SlotRow, SlotRows, ViewState, merchant_heading, slot_detail, view_state};

/// Where the session's terminal outcome lands (fatal error, crash, or clean
/// end): written once by the spawn wrapper in `main`, shown as a banner.
pub type SessionErrorSlot = Arc<Mutex<Option<String>>>;

/// A poisoned lock means the session panicked. Keep rendering the last state —
/// the banner is what reports that crash, and it cannot be drawn from a thread
/// that just double-panicked getting at it. Nothing the window reads is written
/// in more than one step. The policy is in [`crate::sync`].
use crate::sync::lock_ignoring_poison;

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
            // Scrolled because toml errors span many lines and the tail carries
            // the diagnosis.
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
    /// Drives the error banner's Download button. A property of the window, not
    /// of a run: its worker outlives any session.
    fetcher: crate::install::Fetcher,
}

impl ShopApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        handles: SessionHandles,
        error: SessionErrorSlot,
        timings: Timings,
        startup: StartupSettings,
        config_path: PathBuf,
    ) -> Self {
        theme::apply(&cc.egui_ctx);
        // The drafts seed from the controller; `Timings` aren't domain state,
        // so they seed from the startup config value instead — and neither are
        // the three in `startup`, for a stronger reason: the session never
        // holds them at all, so the config this process was launched with is
        // the only place they exist.
        let (editor, slots) = {
            let ctrl = lock_ignoring_poison(&handles.controller);
            let mut slots = SlotRows::default();
            // Synced here so the first frame finds the cache already current.
            slots.sync(&ctrl);
            (
                EditorState::new(ctrl.filter().clone(), *ctrl.limits(), timings, startup),
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
        // Poll-based repaint: 4 Hz keeps the window fresh without coupling
        // app.rs to egui.
        ui.ctx().request_repaint_after(Duration::from_millis(250));
        // One short hold for both: the projection reads `Copy` state, and rows
        // re-derive only if the shop or checklist moved.
        let (view, merchant) = {
            let ctrl = lock_ignoring_poison(&self.handles.controller);
            self.slots.sync(&ctrl);
            (view_state(&ctrl), merchant_heading(&ctrl))
        };
        // Two atomic loads and one short-lived-lock snapshot, neither held
        // past this block: the same "no lock across a frame" rule the
        // controller read above follows.
        let capture = {
            let counters = self.handles.capture_health.snapshot();
            let pipeline = self.handles.budget.snapshot();
            CaptureHealthView {
                delivered: counters.delivered,
                unparsed: counters.unparsed,
                admitted: counters.admitted,
                dropped_segments: pipeline.dropped_segments,
                resyncs: pipeline.resyncs,
            }
        };
        let generation = self.handles.journal.generation();
        if generation != self.journal_generation {
            self.journal_cache = self.handles.journal.to_entries();
            self.journal_generation = generation;
        }
        let outcome = lock_ignoring_poison(&self.error).clone();
        // A terminal outcome disables every control: the click would hit a dead
        // channel.
        let session_alive = outcome.is_none();

        // `theme::EDGE` as the side inset, so the table text lines up under the
        // status bar.
        let margin = egui::Margin::symmetric(theme::EDGE, 10);
        let clicked = egui::Panel::top("status_bar")
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(margin))
            .show(ui, |ui| {
                let clicked = statusbar::render_status_bar(
                    ui,
                    &view,
                    outcome.as_deref(),
                    session_alive,
                    &self.fetcher,
                );
                // New content appended after the status bar's own, not a
                // redesign of it: gated on the same "a run exists" condition
                // its stat tiles already use, since every counter reads as a
                // meaningless zero before `Start`.
                if !matches!(view.status_kind, Status::Idle) {
                    ui.add_space(theme::SP_SM);
                    render_capture_health(ui, &capture);
                }
                clicked
            })
            .inner;
        let open = self.journal_open;
        // Borrowed, not cloned: the peek only matters while collapsed.
        let latest = if open {
            None
        } else {
            self.journal_cache.last().map(|line| &*line.text)
        };
        let mut panel = egui::Panel::bottom("journal")
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(margin))
            .resizable(open);
        if open {
            // Floored at the min so a short window can't invert the range.
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
                // Reserves the header's bottom margin, so its hover fill does
                // not cover the first log line.
                ui.add_space(f32::from(margin.bottom));
                journal::render_journal_body(ui, &self.journal_cache);
            }
        });
        // Zero margin so the strip spans full width; its own hairline is the
        // divider, so egui's separator is suppressed to avoid doubling it.
        egui::Panel::top("tabs")
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::ZERO))
            .show_separator_line(false)
            .show(ui, |ui| render_tabs(ui, &mut self.tab));
        // Only the top margin survives here: content spans full width for the
        // hover fill, other tabs inset text via `theme::EDGE`.
        let controller = &self.handles.controller;
        let detail = |index: usize| slot_detail(&lock_ignoring_poison(controller), index);
        let pane = ShopPane {
            view: &view,
            rows: self.slots.rows(),
            merchant: &merchant,
            detail: &detail,
        };
        let applied = egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::ZERO))
            .show(ui, |ui| {
                render_tab_content(ui, &pane, self.tab, &mut self.editor, session_alive)
            })
            .inner;
        // Dispatch first, then record: "applied" state and persistence both key
        // off what the session actually took.
        let mut delivered = Vec::new();
        for command in clicked.into_iter().chain(applied.commands) {
            if deliver_command(&self.handles, command.clone()) {
                delivered.push(command);
            }
        }
        self.editor.mark_applied(&delivered);
        // Best-effort for the retunes above — a write failure only costs the
        // on-disk copy, because the session already took them. **Not** for the
        // restart-only keys appended here: for those the file is the only place
        // the change can land, so `mark_startup_saved` waits on the result
        // below rather than running beside `mark_applied`.
        let mut sections = persisted_sections(&delivered);
        sections.extend(startup_sections(applied.startup));
        if !sections.is_empty()
            && let Err(err) = config::persist::save(&self.config_path, &sections)
        {
            // The journal dies with the window, so this also goes to the
            // subscriber reaching the log file.
            let labels = section_labels(&sections);
            tracing::warn!(
                error = ?err,
                path = %self.config_path.display(),
                sections = %labels,
                "config.toml not saved"
            );
            // `push`: the file half is the `warn!` above with its fields, which
            // `emit_at` would drop while duplicating the prose.
            self.handles.journal.push(&[format!(
                "config.toml not saved ({labels}): {}",
                err.report()
            )]);
        } else {
            // Reached when there was nothing to write, too, which is correct:
            // an empty `startup` re-seeds the twin with what it already holds.
            self.editor.mark_startup_saved();
        }
    }
}

/// Hands one command to the session; `true` when it was taken. `Stop` bypasses
/// the bounded queue via the gate's halt latch, so it can never be dropped;
/// everything else rides a queue a dying session can leave full or closed.
#[must_use]
fn deliver_command(handles: &SessionHandles, command: Command) -> bool {
    if command == Command::Stop {
        handles.gate.request_halt(HaltSource::PlayerStopped);
        return true;
    }
    if handles.commands.try_send(command).is_err() {
        tracing::debug!("a player command was dropped: the session queue is full or closed");
        // `push`, not `emit_at`: the file half is the `debug!` above, a level
        // `emit_at` can't express (INFO/WARN/ERROR only).
        handles
            .journal
            .push(&[">> command dropped — the session is busy, try again".to_owned()]);
        return false;
    }
    true
}

/// The tab strip: labels over a full-width hairline, active segment underlined.
fn render_tabs(ui: &mut egui::Ui, tab: &mut Tab) {
    // Tabs read as tabs, not buttons: pill fills stripped, the active one
    // marked by an underline instead.
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
    // two read as one baseline.
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
    // Reserves the lower half of the 2px underline, which is centred on the
    // baseline and would otherwise be clipped.
    ui.add_space(theme::SP_XS);
}

/// The persistable sections for a batch of Apply commands. The non-`Set*` arm
/// is spelled out rather than `_`: this is the only bridge from Apply to
/// `config.toml`, so a wildcard would let a new `Set*` retune the session and
/// silently vanish on the next launch.
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

/// The restart-only half of the bridge above. Separate from
/// [`persisted_sections`] because these three reach `config.toml` without ever
/// reaching the session, so there is no `Command` to read them off — see
/// `editor::EditorState::mark_startup_saved` for why that difference matters.
///
/// One `Section` per field that moved, so an Apply that changed only the port
/// leaves the backend line exactly as the player wrote it.
fn startup_sections(edits: editor::StartupEdits) -> Vec<config::persist::Section> {
    let mut sections = Vec::new();
    if let Some(dry_run) = edits.dry_run {
        sections.push(config::persist::Section::DryRun(dry_run));
    }
    if let Some(backend) = edits.backend {
        sections.push(config::persist::Section::Backend(backend));
    }
    sections
}

/// The Setup section titles for the "not saved" report — the same words the UI
/// uses, so the message points at the block.
fn section_labels(sections: &[config::persist::Section]) -> String {
    let mut labels: Vec<&str> = sections
        .iter()
        .map(|section| match section {
            config::persist::Section::Filter(_) => "Hunt",
            config::persist::Section::Limits(_) => "Stop",
            config::persist::Section::Timings(_) => "Click timing",
            // Both live in one collapsible, so one label points at it. Naming
            // the key instead would send a player looking for a block called
            // "dry_run".
            config::persist::Section::DryRun(_) | config::persist::Section::Backend(_) => "Startup",
        })
        .collect();
    // The three Startup keys are three sections with one label, and they are
    // appended together, so a consecutive dedup is enough to keep the report
    // from reading "Startup, Startup, Startup".
    labels.dedup();
    labels.join(", ")
}

/// Everything the Shop tab's content needs, bundled so `render_tab_content`
/// stays under clippy's argument limit: none of these four are read by the
/// `Tab::Setup` arm, only by `Tab::Shop`'s call into `shop::render_shop_tab`.
struct ShopPane<'a> {
    view: &'a ViewState,
    rows: &'a [SlotRow],
    /// The heading `shop::render_shop_tab` paints — built once per frame by
    /// `view::merchant_heading`, alongside `detail` rather than inside `view`
    /// itself. See that function's doc comment for why.
    merchant: &'a str,
    detail: &'a dyn Fn(usize) -> String,
}

/// The active tab's content. One scroll state per tab, so Setup's offset can't
/// bleed into the table.
#[must_use]
fn render_tab_content(
    ui: &mut egui::Ui,
    pane: &ShopPane<'_>,
    tab: Tab,
    editor: &mut EditorState,
    session_alive: bool,
) -> editor::Committed {
    match tab {
        // No inset: the shop table bleeds its hover fill to the edges itself.
        Tab::Shop => {
            egui::ScrollArea::vertical()
                .id_salt("tab-shop")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    shop::render_shop_tab(ui, pane.view, pane.rows, pane.merchant, pane.detail)
                });
            editor::Committed::default()
        }
        Tab::Setup => render_setup_tab(ui, editor, session_alive),
    }
}

/// The Setup tab: a pinned commit bar over a scrolling body, so Apply stays
/// reachable at any scroll offset.
#[must_use]
fn render_setup_tab(
    ui: &mut egui::Ui,
    editor: &mut EditorState,
    session_alive: bool,
) -> editor::Committed {
    let mut clicked = editor::Committed::default();
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
                // `session_alive` goes *into* `edit_sections` rather than
                // wrapping it: the Startup section must stay editable on a dead
                // session, because the backend switch is the fallback for the
                // actuator fault that killed it. See `editor::commit_row`.
                editor::edit_sections(ui, editor, session_alive);
            });
        });
    clicked
}

/// Insets tab content by the side padding; the shop table opts out to bleed its
/// hover fill full width.
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

    /// Test shim for the shell's two panels, so a test can click a tab and
    /// assert on the content it reveals.
    fn render_center(
        ui: &mut egui::Ui,
        view: &ViewState,
        rows: &[SlotRow],
        tab: &mut Tab,
        editor: &mut EditorState,
        session_alive: bool,
    ) -> Vec<Command> {
        render_tabs(ui, tab);
        // No test here reaches the merchant heading — see `src/ui/shop.rs`'s
        // own tests for that — so this shim only needs a stand-in string.
        let pane = ShopPane {
            view,
            rows,
            merchant: "Secret Shop",
            detail: &|_| String::new(),
        };
        render_tab_content(ui, &pane, *tab, editor, session_alive).commands
    }

    /// The second bridge, per field: nothing that did not move gets written.
    #[test]
    fn only_the_startup_fields_that_moved_become_sections() {
        assert!(startup_sections(editor::StartupEdits::default()).is_empty());
        assert_eq!(
            startup_sections(editor::StartupEdits {
                dry_run: Some(true),
                ..Default::default()
            }),
            vec![config::persist::Section::DryRun(true)]
        );
        assert_eq!(
            startup_sections(editor::StartupEdits {
                dry_run: Some(true),
                backend: Some(config::ActuatorBackend::Input),
            }),
            vec![
                config::persist::Section::DryRun(true),
                config::persist::Section::Backend(config::ActuatorBackend::Input),
            ]
        );
    }

    /// Both Startup keys are one collapsible, so the "not saved" report must
    /// name it once — not twice.
    #[test]
    fn the_not_saved_report_names_the_startup_block_once() {
        let sections = vec![
            config::persist::Section::Limits(Limits::default()),
            config::persist::Section::DryRun(true),
            config::persist::Section::Backend(config::ActuatorBackend::Input),
        ];
        assert_eq!(section_labels(&sections), "Stop, Startup");
    }

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
        assert_eq!(section_labels(&sections), "Stop");
    }

    /// `Session` must stay alive for the caller — dropping it closes the
    /// channel instead of filling it.
    fn session_handles() -> (crate::app::Session, SessionHandles) {
        let (session, handles, _shutdown) = crate::app::setup(config::Config::default());
        (session, handles)
    }

    /// Saturates the queue, so the next `try_send` can only fail.
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
        // Armed first, so the assert below reads the halt and not the startup
        // state.
        handles.gate.set(true);

        assert!(deliver_command(&handles, Command::Stop));

        assert!(!handles.gate.is_enabled());
        assert_eq!(handles.gate.next_halt().await, HaltSource::PlayerStopped);
    }

    #[test]
    fn a_dropped_command_is_journaled_and_reported() {
        // The window is the only interface, so a refused click has to reach the
        // player.
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
        // The persisted batch is the *delivered* commands, not the emitted ones.
        let (_session, handles) = session_handles();
        fill_command_queue(&handles);
        let command = Command::SetLimits(Limits {
            max_refreshes: Some(7),
            ..Limits::default()
        });

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
        let mut editor = EditorState::new(
            Filter::default(),
            Limits::default(),
            Timings::default(),
            StartupSettings::default(),
        );
        let harness = Harness::new_ui(|ui| {
            render_center(ui, &view, slots.rows(), &mut tab, &mut editor, true);
        });
        assert!(harness.query_by_label("Hunt").is_none());
        assert!(harness.query_by_label("Apply").is_none());
    }

    #[test]
    fn setup_tab_reveals_the_editors() {
        let (view, slots) = idle_view();
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(
            Filter::default(),
            Limits::default(),
            Timings::default(),
            StartupSettings::default(),
        );
        let mut harness = Harness::new_ui(|ui| {
            render_center(ui, &view, slots.rows(), &mut tab, &mut editor, true);
        });
        harness.get_by_label("Setup").click();
        harness.run();
        harness.get_by_label("Hunt");
        harness.get_by_label("Apply");
        drop(harness);
        assert_eq!(tab, Tab::Setup);
    }

    #[test]
    fn empty_shop_tab_shows_the_quick_start() {
        let (view, slots) = idle_view();
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(
            Filter::default(),
            Limits::default(),
            Timings::default(),
            StartupSettings::default(),
        );
        let harness = Harness::new_ui(|ui| {
            render_center(ui, &view, slots.rows(), &mut tab, &mut editor, true);
        });
        harness.get_by_label("QUICK START");
    }

    #[test]
    fn captured_shop_replaces_the_quick_start() {
        let (view, slots) = captured_view();
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(
            Filter::default(),
            Limits::default(),
            Timings::default(),
            StartupSettings::default(),
        );
        let harness = Harness::new_ui(|ui| {
            render_center(ui, &view, slots.rows(), &mut tab, &mut editor, true);
        });
        assert!(harness.query_by_label("QUICK START").is_none());
    }

    #[test]
    fn slotless_snapshot_does_not_resurrect_the_quick_start() {
        // A degraded shop message still counts as captured.
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
        let mut editor = EditorState::new(
            Filter::default(),
            Limits::default(),
            Timings::default(),
            StartupSettings::default(),
        );
        let harness = Harness::new_ui(|ui| {
            render_center(ui, &view, slots.rows(), &mut tab, &mut editor, true);
        });
        assert!(harness.query_by_label("QUICK START").is_none());
    }
}
