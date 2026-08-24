//! egui window over the session: copies the controller state once per frame,
//! renders it, and pushes player commands through the existing channel. No
//! egui type crosses into the app layer or the domain.
//!
//! Layout: a status bar on top, a collapsible journal at the bottom, and a
//! tabbed center. This file is the shell and the tab strip.

mod capture_health;
mod editor;
mod icons;
mod journal;
mod shop;
mod statusbar;
mod theme;
mod view;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;

use crate::actuator::ClickMode;
use crate::actuator::plan::Timings;
use crate::app::{Command, SessionHandles};
use crate::config;
use crate::domain::control::Status;
use crate::journal::LogLine;
use crate::watch::HaltSource;

use capture_health::{capture_view, render_capture_health};
use editor::EditorState;
use icons::SetIcons;
use view::{SlotRow, SlotRows, ViewState, slot_detail, view_state};

/// Where the session's terminal outcome lands (fatal error, crash, or clean
/// end): written once by the spawn wrapper in `main`, shown as a banner.
pub type SessionErrorSlot = Arc<Mutex<Option<String>>>;

/// The window's only width. Pinned rather than merely suggested: every panel
/// below is laid out against it, so a window that could be widened would be
/// showing a layout nobody tuned. Callers set it as the inner size *and* as both
/// bounds — a minimum alone still lets the frame grow.
pub const WINDOW_WIDTH: f32 = 440.0;

/// Shortest the open journal may be dragged, and the height it opens at. The
/// default is clamped into the range at use, because a short window shrinks the
/// range's top below it.
///
/// The opening height is read against [`CAPTURE_HEALTH_SHARE`], not on its own:
/// the readout may take over half the panel, so what the log actually gets is
/// the rest. At 600 the log gets the better part of the window, which on the
/// default 824-high frame leaves the centre pane close to the `160.0` floor
/// [`journal_sizing`] reserves for it.
const JOURNAL_MIN: f32 = 80.0;
const JOURNAL_DEFAULT: f32 = 600.0;

/// The share of the open journal the capture readout may take before it starts
/// scrolling inside itself, leaving the rest to the log.
///
/// A little over half: the readout is at most a sentence, a note and a counts
/// line, so it reaches this only on a window narrow enough to wrap all three —
/// exactly the case where the log must not be squeezed to nothing.
const CAPTURE_HEALTH_SHARE: f32 = 0.55;

/// The open journal's opening height and its ceiling, for a window with
/// `available` pixels left above it.
///
/// Both halves matter and only one of them used to be there. The ceiling is
/// floored at [`JOURNAL_MIN`] so a short window cannot invert the range — and
/// the opening height is then clamped *into* that range, because a window short
/// enough to pull the ceiling under [`JOURNAL_DEFAULT`] otherwise asks the panel
/// to open taller than it is allowed to be. That is how the journal ended up
/// shorter than its own content, clipping the capture readout and the log with
/// it.
///
/// `160.0` is the room left for everything above: status bar, tab strip, and
/// enough of the centre pane to stay a pane.
fn journal_sizing(available: f32) -> (f32, f32) {
    let ceiling = (available - 160.0).max(JOURNAL_MIN);
    (JOURNAL_DEFAULT.clamp(JOURNAL_MIN, ceiling), ceiling)
}

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
    /// The gear-set pictures the Setup tab's set chips draw, uploaded once per
    /// catalog under the same rule as the caches above. Held here and not in
    /// [`EditorState`] because a texture belongs to an `egui::Context`, which
    /// the drafts know nothing about.
    set_icons: SetIcons,
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
        click_mode: ClickMode,
        config_path: PathBuf,
    ) -> Self {
        theme::apply(&cc.egui_ctx);
        // The drafts seed from the controller; neither `Timings` nor
        // `ClickMode` is domain state, so both seed from the config value the
        // process launched with. From there the session owns them, and Apply
        // retunes it like any other draft.
        let (editor, slots) = {
            let ctrl = lock_ignoring_poison(&handles.controller);
            let mut slots = SlotRows::default();
            // Synced here so the first frame finds the cache already current.
            slots.sync(&ctrl);
            (
                EditorState::new(ctrl.filter().clone(), *ctrl.limits(), timings, click_mode),
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
            // Empty until a catalog lands, which is the every-chip-is-text case.
            set_icons: SetIcons::default(),
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
        // One short hold: the projection reads `Copy` state, and rows re-derive
        // only if the shop or checklist moved.
        //
        // The clock is read outside the lock and handed in: it is the session's
        // own (`EventLog::now_ms`), the one the session stamps domain events
        // with, so the elapsed time the band shows and the duration limit the
        // controller enforces cannot drift apart.
        let now_ms = self.handles.journal.now_ms();
        let view = {
            let ctrl = lock_ignoring_poison(&self.handles.controller);
            self.slots.sync(&ctrl);
            view_state(&ctrl, now_ms)
        };
        // Two atomic loads and one short-lived-lock snapshot, neither held
        // past this block: the same "no lock across a frame" rule the
        // controller read above follows.
        //
        // Three reads and no state of its own. Which run this describes is
        // decided by `app::session`, on the edge that opens the `WatchGate`, so
        // the frame has no edge to detect and nothing to remember between
        // repaints — see `capture_health::capture_view`.
        let capture = {
            let counters = self.handles.capture_health.snapshot();
            let pipeline = self.handles.budget.snapshot();
            capture_view(&counters, &pipeline, self.handles.run_baseline.baseline())
        };
        let generation = self.handles.journal.generation();
        if generation != self.journal_generation {
            self.journal_cache = self.handles.journal.to_entries();
            self.journal_generation = generation;
        }
        // Same generation-gated copy as the journal above, and here for the same
        // reason: the Setup pickers read it every frame and it is written once.
        //
        // The icons ride that gate rather than getting one of their own: they
        // come off the vocabulary the sync just copied, so "the catalog moved"
        // is the same answer for both, and decoding twenty-two PNGs into
        // twenty-two textures per frame would be the one per-frame cost this
        // window cannot pay.
        if self.editor.sync_vocabulary(&self.handles.vocabulary) {
            self.set_icons.load(ui.ctx(), self.editor.icons());
        }
        let outcome = lock_ignoring_poison(&self.error).clone();
        // A terminal outcome disables every control: the click would hit a dead
        // channel.
        let session_alive = outcome.is_none();

        // `theme::EDGE` as the side inset, so the table text lines up under the
        // status bar.
        let margin = egui::Margin::symmetric(theme::EDGE, 10);
        // The status band closes itself — with the run's gauge, or a plain rule
        // — and that closing line has to land on the panel's last pixel. Bottom
        // padding under it, plus a separator of egui's own below that, left a
        // band edge, a strip of nothing, and a second edge. So: no padding, no
        // separator, and `statusbar` owns the edge in both of its bands.
        let band_margin = egui::Margin {
            bottom: 0,
            ..margin
        };
        let clicked = egui::Panel::top("status_bar")
            .frame(egui::Frame::side_top_panel(ui.style()).inner_margin(band_margin))
            .show_separator_line(false)
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
            let (default, journal_max) = journal_sizing(ui.available_rect_before_wrap().height());
            panel = panel
                .default_size(default)
                .size_range(egui::Rangef::new(JOURNAL_MIN, journal_max));
        }
        panel.show(ui, |ui| {
            if journal::render_journal_header(ui, open, latest, margin) {
                self.journal_open = !open;
            }
            if open {
                // Reserves the header's bottom margin, so its hover fill does
                // not cover the first log line.
                ui.add_space(f32::from(margin.bottom));
                // Above the log, and only here: the status bar is what a player
                // watches while a run goes well, so a diagnostic readout parked
                // in it reads as a standing verdict on a healthy session. This
                // panel is the one someone opens to find out what is happening,
                // and it is collapsed by default — so its own disclosure is the
                // gate, and no second toggle is needed inside it.
                //
                // Gated on the same "a run exists" condition the status bar's
                // haul row uses: before `Start` every counter is a zero that
                // diagnoses nothing.
                if !matches!(view.status_kind, Status::Idle) {
                    // Capped, and scrollable past the cap. How tall this readout
                    // is depends on how far its sentence and note wrap, which
                    // depends on the window's width — so on a narrow window it
                    // could outgrow the panel, clip itself, and take the log
                    // down with it. The cap keeps the majority of the panel for
                    // the log whatever the text does; the scrollbar only appears
                    // in the case that used to be a rendering bug.
                    let capped = (ui.available_height() * CAPTURE_HEALTH_SHARE).max(48.0);
                    egui::ScrollArea::vertical()
                        .id_salt("capture_health")
                        .max_height(capped)
                        // Fills the width, shrinks to the text's own height, so
                        // a one-line readout reserves nothing it does not use.
                        .auto_shrink([false, true])
                        .show(ui, |ui| render_capture_health(ui, &capture));
                    // Full-bleed and undimmed, unlike the status bar's: this
                    // one divides two zones — a readout and a log — where that
                    // one divides two rows of the same block. egui's own
                    // `separator` stops at the side margin, which left the
                    // capture block looking like a floating paragraph rather
                    // than a region with an edge.
                    ui.add_space(theme::SP_SM);
                    theme::rule(ui, theme::HAIRLINE);
                    ui.add_space(theme::SP_SM);
                }
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
            detail: &detail,
        };
        let applied = egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::ZERO))
            .show(ui, |ui| {
                render_tab_content(
                    ui,
                    &pane,
                    self.tab,
                    &mut self.editor,
                    &self.set_icons,
                    session_alive,
                )
            })
            .inner;
        self.commit(clicked.into_iter().chain(applied));
    }
}

impl ShopApp {
    /// Hands this frame's clicks to the session and records what it took.
    ///
    /// Split out of [`eframe::App::ui`] because it is the one part of a frame
    /// that draws nothing: everything above it reads state and paints panels,
    /// and this writes — to the session's queue, to the editor's applied marks,
    /// and to `config.toml`. Keeping it inline put a channel send, a
    /// persistence policy and a two-sink error report in the middle of a list
    /// of panels.
    fn commit(&mut self, commands: impl Iterator<Item = Command>) {
        // Dispatch first, then record: "applied" state and persistence both key
        // off what the session actually took.
        let mut delivered = Vec::new();
        for command in commands {
            if deliver_command(&self.handles, command.clone()) {
                delivered.push(command);
            }
        }
        self.editor.mark_applied(&delivered);
        // Best-effort: a write failure only costs the on-disk copy, so it is
        // journaled and moved past.
        let sections = persisted_sections(&delivered);
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
            // `emit_at` would drop while duplicating the prose. Same level, so
            // the window colors it the same as a line that did go through
            // `emit_at`.
            self.handles.journal.push(
                tracing::Level::WARN,
                &[format!(
                    "config.toml not saved ({labels}): {}",
                    err.report()
                )],
            );
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
        // `emit_at` can't express (INFO/WARN/ERROR only). `Level::WARN` here is
        // only the window's color for the line, not a second file record.
        handles.journal.push(
            tracing::Level::WARN,
            &[">> command dropped — the session is busy, try again".to_owned()],
        );
        return false;
    }
    true
}

/// The tab strip: labels over a full-width hairline, active segment underlined.
///
/// The strip takes no side margin so its hairline and hover fills reach the
/// window edges, which leaves it to place its own labels on [`theme::EDGE`]:
/// a `SelectableLabel`'s `button_padding.x` is narrower, and left the words four
/// pixels inside the line every other text in the window stands on. (The
/// `CentralPanel` has no margin either, but its contents re-apply the inset.)
///
/// Returns the span it underlined the active tab with — a painted line leaves no
/// widget behind, so this is the only way a test can see where the marker went.
fn render_tabs(ui: &mut egui::Ui, tab: &mut Tab) -> egui::Rangef {
    // Tabs read as tabs, not buttons: pill fills stripped, the active one
    // marked by an underline instead.
    let tabs = ui.horizontal(|ui| {
        // Read before the labels are placed: both the lead-in and the underline
        // are stated relative to this rather than to a literal.
        let pad = egui::vec2(ui.spacing().button_padding.x, 0.0);
        // Lands the *word* on the edge line, the padding being already part of
        // what stands between the panel and the glyphs.
        ui.add_space(f32::from(theme::EDGE) - pad.x);
        let visuals = &mut ui.style_mut().visuals;
        visuals.selection.bg_fill = egui::Color32::TRANSPARENT;
        visuals.widgets.hovered.weak_bg_fill = egui::Color32::TRANSPARENT;
        visuals.widgets.hovered.bg_stroke = egui::Stroke::NONE;
        visuals.widgets.active.weak_bg_fill = egui::Color32::TRANSPARENT;
        visuals.widgets.active.bg_stroke = egui::Stroke::NONE;
        visuals.widgets.inactive.fg_stroke.color = theme::INK_MUTED;
        let shop = ui.selectable_value(tab, Tab::Shop, "Shop").rect;
        let setup = ui.selectable_value(tab, Tab::Setup, "Setup").rect;
        // The word's extent, not the clickable box: the box is sized for a
        // finger, and underlining it put the marker a padding wide of the word
        // on both sides — on the leading tab, out to the window frame.
        (if *tab == Tab::Shop { shop } else { setup }).shrink2(pad)
    });
    // The accent underline shares the hairline's y and paints second, so the
    // two read as one baseline.
    let baseline = tabs.response.rect.bottom();
    ui.painter().hline(
        ui.max_rect().x_range(),
        baseline,
        ui.visuals().widgets.noninteractive.bg_stroke,
    );
    let marker = tabs.inner.x_range();
    ui.painter()
        .hline(marker, baseline, egui::Stroke::new(2.0, theme::ACCENT));
    // Reserves the lower half of the 2px underline, which is centred on the
    // baseline and would otherwise be clipped.
    ui.add_space(theme::SP_XS);
    marker
}

/// The persistable sections for a batch of Apply commands. The non-`Set*` arm
/// is spelled out rather than `_`: this is the only bridge from Apply to
/// `config.toml`, so a wildcard would let a new `Set*` retune the session and
/// silently vanish on the next launch.
fn persisted_sections(commands: &[Command]) -> Vec<config::persist::Section> {
    use config::persist::Section;
    commands
        .iter()
        .flat_map(|command| match command {
            Command::SetFilter(filter) => vec![Section::Filter(filter.clone())],
            Command::SetLimits(limits) => vec![Section::Limits(*limits)],
            Command::SetTimings(timings) => vec![Section::Timings(*timings)],
            // The one command that becomes two keys — `flat_map` rather than
            // `filter_map` exists for this arm alone. Both are written even
            // when only one moved: the command carries the pair because a
            // single Apply must not land in halves, and writing back an
            // unchanged value through `set_key` reproduces its line byte for
            // byte, comment included.
            Command::SetClickMode(mode) => vec![
                Section::DryRun(mode.dry_run),
                Section::Backend(mode.backend),
            ],
            Command::Start | Command::Stop | Command::Toggle => Vec::new(),
        })
        .collect()
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
/// stays under clippy's argument limit: none of these three are read by the
/// `Tab::Setup` arm, only by `Tab::Shop`'s call into `shop::render_shop_tab`.
struct ShopPane<'a> {
    view: &'a ViewState,
    rows: &'a [SlotRow],
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
    icons: &SetIcons,
    session_alive: bool,
) -> Vec<Command> {
    match tab {
        // No inset: the shop table bleeds its hover fill to the edges itself.
        Tab::Shop => {
            egui::ScrollArea::vertical()
                .id_salt("tab-shop")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    shop::render_shop_tab(ui, pane.view, pane.rows, pane.detail)
                });
            Vec::new()
        }
        Tab::Setup => render_setup_tab(ui, editor, icons, session_alive),
    }
}

/// The Setup tab: a pinned commit bar over a scrolling body, so Apply stays
/// reachable at any scroll offset.
#[must_use]
fn render_setup_tab(
    ui: &mut egui::Ui,
    editor: &mut EditorState,
    icons: &SetIcons,
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
                ui.add_enabled_ui(session_alive, |ui| {
                    editor::edit_sections(ui, editor, icons);
                });
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
mod sizing_tests {
    use super::{JOURNAL_DEFAULT, JOURNAL_MIN, journal_sizing};

    /// The journal must never be asked to open at a height outside the range it
    /// is given, at any window size. It was, on a short window, and the panel
    /// that came out was shorter than its content — which egui then clipped,
    /// taking the capture readout and the first log lines off screen.
    #[test]
    fn the_journal_never_opens_outside_its_own_size_range() {
        // Past the plausible ends on both sides: a window smaller than the
        // chrome above it yields a negative `available`, which is what made the
        // range invert in the first place.
        for available in [
            -400.0, -1.0, 0.0, 1.0, 120.0, 240.0, 330.0, 361.0, 900.0, 4000.0,
        ] {
            let (default, ceiling) = journal_sizing(available);
            assert!(
                ceiling >= JOURNAL_MIN,
                "inverted range at {available}: ceiling {ceiling}"
            );
            assert!(
                (JOURNAL_MIN..=ceiling).contains(&default),
                "default {default} outside {JOURNAL_MIN}..={ceiling} at {available}"
            );
        }
    }

    /// A tall window still opens the journal at the size it was tuned to, so
    /// the clamp above is a floor for small windows and not a redesign.
    #[test]
    fn a_tall_window_still_opens_the_journal_at_its_tuned_height() {
        let (default, ceiling) = journal_sizing(1000.0);
        assert_eq!(default, JOURNAL_DEFAULT);
        assert!(ceiling > JOURNAL_DEFAULT);
    }
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
        let _ = render_tabs(ui, tab);
        let pane = ShopPane {
            view,
            rows,
            detail: &|_| String::new(),
        };
        // No icons: these tests drive the tab strip, and a set chip with no
        // picture is the branch every other test in this file already reads.
        render_tab_content(ui, &pane, *tab, editor, &SetIcons::default(), session_alive)
    }

    /// The one command that becomes two keys, and the reason `persisted_sections`
    /// is a `flat_map`.
    #[test]
    fn one_click_mode_command_persists_both_of_its_keys() {
        let mode = ClickMode {
            dry_run: true,
            backend: config::ActuatorBackend::Input,
        };
        assert_eq!(
            persisted_sections(&[Command::SetClickMode(mode)]),
            vec![
                config::persist::Section::DryRun(true),
                config::persist::Section::Backend(config::ActuatorBackend::Input),
            ]
        );
    }

    /// Both Clicking keys are one collapsible, so the "not saved" report must
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
        (view_state(controller, 0), slots)
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

    /// Every text in this window stands on [`theme::EDGE`] — the status band's
    /// figures, the slot table's columns, the journal. The tab strip takes no
    /// side margin, so it has to place its labels itself, and it did not: a
    /// `SelectableLabel` brings its own narrower `button_padding.x`, which left
    /// the two words four pixels inside the line everything else shares.
    ///
    /// Measured against the `Ui`'s own left, so it means "on the edge line" at
    /// whatever origin a harness hands it — and under the real theme, since the
    /// four pixels are the gap between [`theme::EDGE`] and the [`theme::SP_MD`]
    /// `apply` installs. On stock spacing this would pass while the window did
    /// not, which the `pad` assertion below guards.
    #[test]
    fn the_tab_labels_stand_on_the_windows_edge_line() {
        let placed = std::cell::Cell::new((0.0, 0.0));
        let mut tab = Tab::Shop;
        let mut harness = Harness::new_ui(|ui| {
            theme::apply(ui.ctx());
            placed.set((ui.max_rect().left(), ui.spacing().button_padding.x));
            render_tabs(ui, &mut tab);
        });
        // A second frame: `apply` writes through the context, so the first one
        // is laid out on the style it replaces.
        harness.run();
        let (origin, pad) = placed.get();
        assert_eq!(
            pad,
            theme::SP_MD,
            "the theme is not installed; this test would measure nothing"
        );
        // The label's box starts a padding short of its word; the word is what
        // has to land on the line.
        let word = harness.get_by_label("Shop").rect().left() + pad;
        assert!(
            (word - origin - f32::from(theme::EDGE)).abs() < 0.5,
            "tab label at {} from the panel edge, expected {}",
            word - origin,
            theme::EDGE
        );
    }

    /// The active tab's underline covers its word and nothing else.
    ///
    /// The expected width is asked of the font, not recomputed from the padding
    /// `render_tabs` reads: a test that redoes the code's arithmetic agrees with
    /// it by construction, including when both are wrong.
    #[test]
    fn the_active_tabs_underline_covers_its_word_only() {
        let mut tab = Tab::Shop;
        let measured = std::cell::Cell::new((0.0, 0.0));
        let mut harness = Harness::new_ui(|ui| {
            theme::apply(ui.ctx());
            let marker = render_tabs(ui, &mut tab);
            let font = egui::TextStyle::Button.resolve(ui.style());
            let word = ui.ctx().fonts_mut(|fonts| {
                fonts
                    .layout_no_wrap("Shop".to_owned(), font, theme::INK)
                    .size()
                    .x
            });
            measured.set((marker.span(), word));
        });
        harness.run();

        let (marker, word) = measured.get();
        assert!(
            (marker - word).abs() < 1.0,
            "the underline spans {marker} for a word {word} wide"
        );
    }

    #[test]
    fn shop_tab_hides_the_editors() {
        let (view, slots) = idle_view();
        let mut tab = Tab::Shop;
        let mut editor = EditorState::new(
            Filter::default(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
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
            ClickMode::default(),
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
            ClickMode::default(),
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
            ClickMode::default(),
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
            ClickMode::default(),
        );
        let harness = Harness::new_ui(|ui| {
            render_center(ui, &view, slots.rows(), &mut tab, &mut editor, true);
        });
        assert!(harness.query_by_label("QUICK START").is_none());
    }
}
