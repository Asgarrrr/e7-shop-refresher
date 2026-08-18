//! The Setup surface: draft filter/limits/timings owned by the window, the
//! widgets that edit them, and the single Apply that commits the changed
//! drafts to the session. Laid out as three groups by the player's real
//! priority — Hunt (what to buy) and Stop (when to quit) always open, Click
//! timing (expert tuning) collapsed — under one primary action.

// One file per section, plus the timing group's painting. The seam is shell
// state, not topic: a section file holds the parts that work on a `Filter` /
// `Limits` / `Timings` value handed in by the caller, while everything that
// reaches into `EditorState` — the three `*_body` functions, `preset_row`, the
// commit bar — stays here with the struct it reads. Grouping the drafts (the
// prerequisite `24-proj.md` names, still open in `_HANDOFF.md`) is what would
// let the bodies move down too.
mod hunt;
mod stop;
mod timing;
mod timing_meter;

use hunt::{hunt_summary, optional_value, quick_add_names, string_list, substat_reqs};
use stop::{duration_row, limit_row, stop_summary};
use timing::{fine_tune_body, mode_hint, pass_estimate, timing_summary};

use eframe::egui;

use super::theme;
#[cfg(test)]
use crate::actuator::plan::DelayRange;
use crate::actuator::plan::{TimingPreset, Timings};
use crate::app::Command;
use crate::domain::control::Limits;
#[cfg(test)]
use crate::domain::filter::SubstatReq;
use crate::domain::filter::{Filter, HUNTABLE_KINDS};
// The checkbox row reads `HUNTABLE_KINDS`; only the tests still name a kind
// directly.
#[cfg(test)]
use crate::domain::shop::ItemKind;
use crate::render::kind_label;

/// Draft criteria owned by the window until Apply pushes them to the session;
/// seeded from the controller's live criteria (and the startup timings) at
/// startup. Each draft carries the last-applied copy beside it so Apply lights
/// up only on a real change and sends nothing that has not moved. Drafts are
/// still session-seeded from the controller, but Apply now both retunes the
/// live session AND writes the changed sections back to config.toml (via
/// `config::persist`, format-preserving, best-effort).
pub(super) struct EditorState {
    filter: Filter,
    limits: Limits,
    timings: Timings,
    /// The values the session is actually running: a draft is "dirty" (and
    /// Apply enabled) exactly when it differs from its applied twin.
    applied_filter: Filter,
    applied_limits: Limits,
    applied_timings: Timings,
    name_input: String,
    set_input: String,
    substat_input: String,
    /// Per-section disclosure, journal-style. Hunt and Stop open on arrival (the
    /// first things a player sets); the expert Click timing block stays folded.
    hunt_open: bool,
    stop_open: bool,
    timing_open: bool,
    /// Within the Click timing section: whether the Custom mode segment is
    /// selected, revealing the per-action bars inline under the presets.
    fine_tune_open: bool,
}

impl EditorState {
    pub(super) fn new(filter: Filter, limits: Limits, timings: Timings) -> Self {
        Self {
            applied_filter: filter.clone(),
            applied_limits: limits,
            applied_timings: timings,
            filter,
            limits,
            timings,
            name_input: String::new(),
            set_input: String::new(),
            substat_input: String::new(),
            hunt_open: true,
            stop_open: true,
            timing_open: false,
            fine_tune_open: false,
        }
    }

    /// Re-seeds the applied twins from the commands the session *actually*
    /// took. This is the only writer of the twins: [`commit_row`] emits, the
    /// shell delivers, and a draft counts as applied only once its command
    /// cleared the bounded queue. The value is read back out of the command
    /// rather than off the draft, so the twin can only ever record what was
    /// handed over. Non-`Set*` commands (Start/Stop) are none of its business.
    pub(super) fn mark_applied(&mut self, delivered: &[Command]) {
        for command in delivered {
            match command {
                Command::SetFilter(filter) => self.applied_filter = filter.clone(),
                Command::SetLimits(limits) => self.applied_limits = *limits,
                Command::SetTimings(timings) => self.applied_timings = *timings,
                Command::Start | Command::Stop | Command::Toggle => {}
            }
        }
    }
}

/// The whole Setup surface in one pass: the three sections over the Apply
/// footer, stacked in a single `ui`. The live window mounts them in separate
/// panels ([`edit_sections`] in the scroll, [`commit_row`] pinned to the
/// bottom) so Apply never scrolls out of reach; this combined entry lets the
/// test harness drive both at once. Session is assumed alive — the pinned path
/// passes the real flag.
#[cfg(test)]
fn edit_setup(ui: &mut egui::Ui, editor: &mut EditorState) -> Vec<Command> {
    edit_sections(ui, editor);
    ui.add_space(theme::SP_XL);
    commit_row(ui, editor, true)
}

/// The three journal-style collapsible sections (Hunt / Stop / Click timing) —
/// the scrolling body of the Setup tab, without the commit bar.
pub(super) fn edit_sections(ui: &mut egui::Ui, editor: &mut EditorState) {
    // The section bar trails a peek of what it holds while folded, keeping the
    // intent visible without the controls. The summary is built only when the
    // section is folded (the bar drops it once open), so an expanded Setup tab
    // doesn't re-allocate discarded strings every frame. No space is inserted
    // *between* collapsed bars: they tile on the item spacing alone so their
    // hover strips meet with no dead seam (see `theme::collapsing_section`). An
    // open body gets its own trailing space to stand off the next bar.
    let hunt = (!editor.hunt_open).then(|| hunt_summary(&editor.filter));
    section(ui, "Hunt", hunt.as_deref(), &mut editor.hunt_open);
    if editor.hunt_open {
        hunt_body(ui, editor);
        ui.add_space(theme::SP_SM);
    }
    let stop = (!editor.stop_open).then(|| stop_summary(&editor.limits));
    section(ui, "Stop", stop.as_deref(), &mut editor.stop_open);
    if editor.stop_open {
        stop_body(ui, editor);
        ui.add_space(theme::SP_SM);
    }
    let timing = (!editor.timing_open).then(|| timing_summary(&editor.timings));
    section(ui, "Click timing", timing, &mut editor.timing_open);
    if editor.timing_open {
        timing_body(ui, editor);
        ui.add_space(theme::SP_SM);
    }
}

/// One collapsible section bar (journal key) plus the breathing room its open
/// body needs. `summary` (present only while folded) trails the title. Toggles
/// `open` on click.
fn section(ui: &mut egui::Ui, title: &str, summary: Option<&str>, open: &mut bool) {
    if theme::collapsing_section(ui, title, summary, *open) {
        *open = !*open;
    }
    if *open {
        ui.add_space(theme::SP_SM);
    }
}

/// `n singular` / `n plural`, e.g. `1 refresh` / `3 refreshes`. `usize` because
/// two of the four callers pass a `len()`; the `u32` limits reach it through a
/// saturating `try_from` rather than an `as`, so the widening carries no
/// unchecked cast even on a 16-bit target.
fn count_label(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

/// Hunt: the item-interest criteria — what the loop buys. Open on arrival:
/// without at least one criterion the loop refuses to arm, so this is the first
/// thing the player sets.
fn hunt_body(ui: &mut egui::Ui, editor: &mut EditorState) {
    ui.horizontal(|ui| {
        // There is no "unknown" box, and there can no longer be one: the list is
        // `HuntKind::ALL`, and `HuntKind` has no catch-all variant. The box that
        // used to be here had exactly one net effect — ticking it wrote a
        // `kinds = ["unknown"]` that the next launch refused to load, fatally
        // (error window, no main window), with hand-editing the very file the
        // player is not expected to hand-edit the only way out. Removing the box
        // fixed the symptom; the type is what makes it unspellable.
        for kind in HUNTABLE_KINDS {
            let mut on = editor.filter.kinds.contains(&kind);
            if ui.checkbox(&mut on, kind_label(kind)).changed() {
                if on {
                    editor.filter.kinds.push(kind);
                } else {
                    editor.filter.kinds.retain(|kept| *kept != kind);
                }
            }
        }
    });
    ui.add_space(theme::SP_SM);
    string_list(
        ui,
        "names (exact internal ids)",
        &mut editor.filter.names,
        &mut editor.name_input,
    );
    quick_add_names(ui, &mut editor.filter.names);
    string_list(
        ui,
        "sets (exact internal ids)",
        &mut editor.filter.sets,
        &mut editor.set_input,
    );
    substat_reqs(
        ui,
        &mut editor.filter.required_substats,
        &mut editor.substat_input,
    );
    ui.add_space(theme::SP_XS);
    egui::Grid::new("hunt-numerics")
        .num_columns(2)
        .spacing([theme::SP_SM, theme::SP_XS])
        .show(ui, |ui| {
            optional_value(ui, "min substats", &mut editor.filter.min_substats, 1);
            ui.end_row();
            // Seeded above the covenant-bookmark price so a fresh cap still
            // matches the default hunt targets.
            optional_value(
                ui,
                "max price (gold)",
                &mut editor.filter.max_price,
                300_000,
            );
            ui.end_row();
        });
    ui.add_space(theme::SP_XS);
    ui.checkbox(&mut editor.filter.include_sold_out, "include sold out");
}

/// Stop: the run's safety rails, laid as a quiet ledger — a small squared
/// checkbox, the unit read down the left column, and the cap aligned in a right
/// column, the way a system-settings pane lists its toggles. An armed rail's
/// checkbox fills the app's blue (`theme::accent_checkbox`), the only active
/// color here; its number is a borderless drag field so the value column stays a
/// clean column of figures, not a row of heavy pills. An unset rail reads a faint
/// "none". Rows keep the panel's 8px rhythm so the block breathes.
fn stop_body(ui: &mut egui::Ui, editor: &mut EditorState) {
    ui.weak("Stop the run at the first limit it reaches.");
    ui.add_space(theme::SP_SM);
    limit_row(ui, "refreshes", &mut editor.limits.max_refreshes, 10);
    limit_row(ui, "crystals spent", &mut editor.limits.max_spend, 30);
    limit_row(ui, "matches", &mut editor.limits.max_matches, 5);
    duration_row(ui, &mut editor.limits.max_duration_ms);
}

/// The arming semantics of every optional criterion, in one place: unchecked
/// means "no constraint" and writes `None`; a freshly checked box seeds a
/// non-zero value; an already-present value is left exactly as it is. All three
/// editors (Hunt's grid cells, the Stop rails, the duration rail) resolve their
/// toggle through this — the chrome was already factored out, these semantics
/// were not.
fn arm_optional<T>(armed: bool, value: &mut Option<T>, seed: T) {
    if armed {
        value.get_or_insert(seed);
    } else {
        *value = None;
    }
}

/// The drag field for an armed optional criterion, floored at 1.
///
/// `clamp_existing_to_range` is **off** and that is not a style choice: without
/// it a value already present — a `max_refreshes = 0` seeded from config.toml —
/// is silently rewritten to 1 on the first render, which desyncs the draft and
/// makes Apply send a value the player never chose
/// (`seeded_zero_limit_is_not_silently_clamped`). Shared so that fix lives once
/// instead of once per widget. [`stop::duration_row`] is the one criterion that
/// cannot use it: it drags whole minutes derived from the stored ms, so its field
/// has its own range and write-back, and shares only [`arm_optional`].
fn optional_field<T: egui::emath::Numeric>(ui: &mut egui::Ui, value: &mut T) -> egui::Response {
    ui.add(
        egui::DragValue::new(value)
            .range(T::from_f64(1.0)..=T::MAX)
            .clamp_existing_to_range(false),
    )
}

/// Click timing: each click waits a fixed tuned delay, plus a random extra the
/// player dials in on top so the loop never clicks like a metronome. One
/// draggable bar per action on a shared time ruler — a solid segment for the
/// fixed wait, a bright segment for the random extra — grouped by phase. Folded
/// by default (via `edit_setup`).
fn timing_body(ui: &mut egui::Ui, editor: &mut EditorState) {
    ui.label("How human should the clicks look?");
    ui.add_space(theme::SP_SM);
    // `active` is the lit segment: `Some(preset)` for a preset, `None` for Custom
    // (bars shown). It carries the detected preset out of `preset_row` so the
    // hint reuses that one lookup instead of scanning the timings again.
    let active = preset_row(ui, editor);
    ui.add_space(theme::SP_SM);
    // The per-pass estimate is folded into the hint sentence, not a separate
    // right-aligned stat row — a lone number floating in the empty space below a
    // preset read as a misplaced KPI. In Custom the range tracks the bars live.
    ui.weak(format!(
        "{} About {} per pass.",
        mode_hint(active),
        pass_estimate(&editor.timings)
    ));

    // The eight bars live inline under the mode control, revealed by the Custom
    // segment — no second collapse nested in the Click timing section.
    if active.is_none() {
        ui.add_space(theme::SP_SM);
        fine_tune_body(ui, &mut editor.timings);
    }
}

/// The humanization mode as one segmented control: the three presets plus a
/// Custom segment that reveals the per-action bars. The active segment is the
/// preset the timings match, or Custom when the player is fine-tuning (bars
/// open) or the timings match no preset. Clicking a preset overwrites every
/// action's random extra and hides the bars; clicking Custom reveals them
/// without touching the timings. Returns the lit segment: `Some(preset)` for a
/// preset, `None` for Custom (bars shown).
fn preset_row(ui: &mut egui::Ui, editor: &mut EditorState) -> Option<TimingPreset> {
    let detected = TimingPreset::from_timings(&editor.timings);
    // Custom wins whenever the bars are open or the mix matches no preset — so a
    // config-seeded custom timing lands on Custom with its bars ready.
    let custom = editor.fine_tune_open || detected.is_none();
    // A unified segmented track: a raised rounded strip split into snug
    // segments, so the three presets read as clickable parts of one control, not
    // bare labels beside a button. The active segment fills with the bright
    // `ACCENT` (egui's selection fill) so the chosen mode reads unmistakably;
    // unselected labels mute until hovered.
    egui::Frame::new()
        .fill(theme::STRIPE)
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(3)
        .show(ui, |ui| {
            let visuals = &mut ui.style_mut().visuals;
            visuals.widgets.inactive.fg_stroke.color = theme::INK_MUTED;
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.horizontal(|ui| {
                for preset in TimingPreset::ALL {
                    let selected = !custom && detected == Some(preset);
                    if ui.selectable_label(selected, preset.label()).clicked() {
                        editor.timings = preset.timings();
                        editor.fine_tune_open = false;
                    }
                }
                if ui.selectable_label(custom, "Custom").clicked() {
                    editor.fine_tune_open = true;
                }
            });
        });
    // Reflect the pre-click state (as `custom` above): `None` when Custom is lit.
    if custom { None } else { detected }
}

/// The single commit: one primary Apply that emits every draft that moved. The
/// applied twins are *not* touched here — the shell re-seeds them through
/// [`EditorState::mark_applied`] for the commands the session actually took, so
/// a click lost to a saturated queue leaves Apply lit and the peek standing
/// instead of claiming a setting nobody received. Disabled until something
/// changed — and, when the
/// filter is the change, until it is restricted enough to arm (an unrestricted
/// filter the loop would refuse never reaches the session). Timing/limit-only
/// edits apply even while the filter sits unrestricted, since the domain only
/// gates arming on the filter. Left of the button, a peek names the sections
/// with unsaved edits (or, when the block is on, why Apply is dark) so the
/// pinned bar reads as a pending-changes summary, not a lone button. Disabled
/// wholesale once the session is dead — the click would vanish into a closed
/// channel.
#[must_use]
pub(super) fn commit_row(
    ui: &mut egui::Ui,
    editor: &mut EditorState,
    session_alive: bool,
) -> Vec<Command> {
    // Bit-exact on purpose, `required_substats[].min`'s `f64` included: these are
    // change detection since the last write, not numeric tests. "Did this draft
    // move?" is answered by the same equality the twin was seeded with, so an
    // epsilon would make a real edit invisible. What the exactness *cannot*
    // survive is a non-finite `min` (`NaN != NaN` lights Apply forever): the
    // loader rejects one and `substat_reqs` cannot produce one, so no `Filter`
    // reaching here carries it.
    let dirty_filter = editor.filter != editor.applied_filter;
    let dirty_limits = editor.limits != editor.applied_limits;
    let dirty_timings = editor.timings != editor.applied_timings;
    let dirty = dirty_filter || dirty_limits || dirty_timings;
    // Only a *changed* filter must clear the arming bar; an already-applied
    // restricted filter lets limit/timing edits through untouched.
    let blocked = dirty_filter && editor.filter.is_unrestricted();

    let mut commands = Vec::new();
    ui.horizontal(|ui| {
        // The blocking reason wins the peek slot: it explains the dark button.
        // Otherwise, name the dirty sections so Apply's target is legible.
        if blocked {
            ui.weak("add at least one hunt criterion before Apply");
        } else if let Some(summary) = dirty_summary(dirty_filter, dirty_limits, dirty_timings) {
            ui.weak(summary);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let clicked = ui
                .add_enabled_ui(session_alive && dirty && !blocked, |ui| {
                    theme::primary_button(ui, "Apply")
                })
                .inner
                .clicked();
            if clicked {
                // Only *emit* here. The applied twins are re-seeded by
                // [`EditorState::mark_applied`], once the shell knows the
                // session actually took the command — marking them here lit
                // "applied" for a click the bounded queue may have dropped.
                if dirty_filter {
                    commands.push(Command::SetFilter(editor.filter.clone()));
                }
                if dirty_limits {
                    commands.push(Command::SetLimits(editor.limits));
                }
                if dirty_timings {
                    commands.push(Command::SetTimings(editor.timings));
                }
            }
        });
    });
    commands
}

/// The pending-edit peek for the commit bar: the section labels with unsaved
/// drafts, e.g. `Hunt, Stop edited`. `None` when nothing moved (Apply is dark
/// and the bar stays bare). Labels mirror the section titles so the peek points
/// straight at the collapsible that changed.
fn dirty_summary(filter: bool, limits: bool, timings: bool) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    if filter {
        parts.push("Hunt");
    }
    if limits {
        parts.push("Stop");
    }
    if timings {
        parts.push("Click timing");
    }
    (!parts.is_empty()).then(|| format!("{} edited", parts.join(", ")))
}

#[cfg(test)]
mod tests {
    use egui_kittest::{Harness, kittest::Queryable};

    use super::*;

    fn named_filter() -> Filter {
        Filter {
            names: vec!["ticketrare_name".to_owned()],
            ..Filter::default()
        }
    }

    /// Drive `edit_setup` once, capturing whatever Apply committed. `run` settles
    /// over several frames and the final one is a quiet no-click, so only a
    /// non-empty commit is latched — the last frame must not wipe it.
    fn run_setup(editor: &mut EditorState) -> Vec<Command> {
        // `Harness::new_ui` takes `impl FnMut`, so the closure captures `sent`
        // mutably (it already captures `editor` that way) — no interior
        // mutability needed. `drop(harness)` is what releases the borrow.
        let mut sent = Vec::new();
        let mut harness = Harness::new_ui(|ui| {
            let commands = edit_setup(ui, editor);
            if !commands.is_empty() {
                sent = commands;
            }
        });
        harness.get_by_label("Apply").click();
        harness.run();
        drop(harness);
        sent
    }

    #[test]
    fn apply_sends_only_the_changed_draft() {
        // Applied twin is the default filter; the dirty draft is the named one,
        // so Apply commits exactly SetFilter — limits and timings never moved.
        let mut editor = EditorState::new(Filter::default(), Limits::default(), Timings::default());
        editor.filter = named_filter();
        assert_eq!(
            run_setup(&mut editor),
            vec![Command::SetFilter(named_filter())]
        );
    }

    #[test]
    fn apply_leaves_the_draft_dirty_until_the_shell_confirms() {
        // `commit_row` only emits. The applied twins move on `mark_applied`,
        // which the shell calls with the commands the session actually took —
        // so a click the bounded queue dropped keeps Apply lit and the peek
        // standing instead of claiming a setting nobody received.
        let mut editor = EditorState::new(Filter::default(), Limits::default(), Timings::default());
        editor.filter = named_filter();
        let commands = run_setup(&mut editor);
        assert_eq!(commands, vec![Command::SetFilter(named_filter())]);
        assert_ne!(editor.filter, editor.applied_filter);

        editor.mark_applied(&commands);
        assert_eq!(editor.filter, editor.applied_filter);
        // Now that it landed, Apply goes dark and sends nothing more.
        assert!(run_setup(&mut editor).is_empty());
    }

    #[test]
    fn mark_applied_ignores_the_status_bar_commands() {
        // Start/Stop/Toggle ride the same dispatch list as the Setup commits;
        // they carry no draft and must never re-seed one.
        let mut editor = EditorState::new(Filter::default(), Limits::default(), Timings::default());
        editor.filter = named_filter();
        editor.mark_applied(&[Command::Start, Command::Stop, Command::Toggle]);
        assert_ne!(editor.filter, editor.applied_filter);
    }

    #[test]
    fn apply_inert_while_nothing_changed() {
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        assert!(run_setup(&mut editor).is_empty());
    }

    #[test]
    fn a_non_finite_substat_threshold_cannot_survive_a_render() {
        // `Config::validate` refuses `min = nan` from config.toml, but egui's
        // DragValue parses typed text with `f64::from_str`, which accepts "nan"
        // and "inf". Either would light Apply forever — `NaN != NaN` in the
        // derived `PartialEq` the dirty-check uses — while `value >= min` matched
        // nothing. Seeded directly here (the loader's own path is covered in
        // `config`): rendering the row must snap it back, so no draft the window
        // hands over can carry one.
        let filter = Filter {
            required_substats: vec![SubstatReq {
                name: "speed".to_owned(),
                min: Some(f64::NAN),
            }],
            ..Filter::default()
        };
        let mut editor = EditorState::new(filter, Limits::default(), Timings::default());
        let harness = Harness::new_ui(|ui| {
            edit_sections(ui, &mut editor);
        });
        drop(harness);
        assert_eq!(editor.filter.required_substats[0].min, Some(1.0));
        assert_eq!(editor.filter, editor.filter.clone());
    }

    #[test]
    fn apply_blocked_while_the_dirty_filter_is_unrestricted() {
        // Clearing the only criterion leaves the draft dirty but unrestricted:
        // the loop would refuse it, so Apply must not send it.
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        editor.filter.names.clear();
        assert!(editor.filter.is_unrestricted());
        assert!(run_setup(&mut editor).is_empty());
    }

    #[test]
    fn dirty_summary_joins_the_changed_section_labels() {
        assert_eq!(dirty_summary(false, false, false), None);
        assert_eq!(
            dirty_summary(true, false, false).as_deref(),
            Some("Hunt edited")
        );
        assert_eq!(
            dirty_summary(true, true, true).as_deref(),
            Some("Hunt, Stop, Click timing edited")
        );
    }

    #[test]
    fn commit_bar_names_the_dirty_section() {
        // A changed limit lights the Stop peek beside Apply, so the pinned bar
        // reads as a pending-changes summary rather than a lone button.
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        editor.limits.max_refreshes = Some(5);
        let mut harness = Harness::new_ui(|ui| {
            edit_setup(ui, &mut editor);
        });
        harness.run();
        harness.get_by_label("Stop edited");
    }

    #[test]
    fn quick_add_seeds_a_hunt_token() {
        let mut editor = EditorState::new(Filter::default(), Limits::default(), Timings::default());
        let mut harness = Harness::new_ui(|ui| {
            edit_setup(ui, &mut editor);
        });
        harness.get_by_label("+ Covenant").click();
        harness.run();
        drop(harness);
        assert_eq!(editor.filter.names, vec!["ticketrare_name".to_owned()]);
    }

    #[test]
    fn kind_checkbox_updates_the_draft() {
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        let mut harness = Harness::new_ui(|ui| {
            edit_setup(ui, &mut editor);
        });
        harness.get_by_label("Token").click();
        harness.run();
        drop(harness);
        assert!(editor.filter.kinds.contains(&ItemKind::Token));
    }

    #[test]
    fn open_timing_shows_the_mode_control_not_the_bars() {
        // Opening Click timing on a preset shows the segmented mode control; the
        // eight bars stay hidden until the Custom segment is chosen.
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        editor.timing_open = true;
        let mut harness = Harness::new_ui(|ui| {
            edit_setup(ui, &mut editor);
        });
        harness.run();
        harness.get_by_label("Instant");
        harness.get_by_label("Custom");
        assert!(harness.query_by_label("OPEN & REFRESH").is_none());
    }

    #[test]
    fn the_custom_segment_reveals_and_hides_the_bars() {
        // Clicking Custom reveals the bars inline; clicking a preset overwrites
        // the timings and folds them away again — no nested disclosure.
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        editor.timing_open = true;
        let mut harness = Harness::new_ui(|ui| {
            edit_setup(ui, &mut editor);
        });
        harness.get_by_label("Custom").click();
        harness.run();
        harness.get_by_label("OPEN & REFRESH");
        harness.get_by_label("Human").click();
        harness.run();
        assert!(harness.query_by_label("OPEN & REFRESH").is_none());
    }

    #[test]
    fn hunt_kinds_exclude_the_unknown_bucket() {
        // Ticking "?" wrote `kinds = ["unknown"]`, which the next launch refused
        // — a fatal, GUI-less load failure only a hand-edit of config.toml could
        // undo. The box is gone, and `Filter::kinds` holds `HuntKind` now, so the
        // row is driven by `HuntKind::ALL` and a fourth box cannot appear without
        // a fourth kind existing. The three real kinds stay.
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        let harness = Harness::new_ui(|ui| {
            edit_setup(ui, &mut editor);
        });
        assert_eq!(HUNTABLE_KINDS.len(), 3);
        for kind in HUNTABLE_KINDS {
            harness.get_by_label(kind_label(kind));
        }
        assert!(
            harness
                .query_by_label(kind_label(ItemKind::Unknown))
                .is_none()
        );
    }

    #[test]
    fn clicking_a_preset_writes_its_timings() {
        // The preset control overwrites the timing draft; Apply then commits it.
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        editor.timing_open = true;
        let mut harness = Harness::new_ui(|ui| {
            edit_setup(ui, &mut editor);
        });
        harness.get_by_label("Cautious").click();
        harness.run();
        drop(harness);
        assert_eq!(editor.timings, TimingPreset::Cautious.timings());
    }

    #[test]
    fn collapsed_sections_tile_with_no_hover_gap() {
        // Folded section bars must meet edge-to-edge: a gap between their
        // hit/fill rects leaves a dead seam where hovering lights a bar the
        // pointer is not over (the fill covers only the inner bar while egui
        // hit-tests wider). Their rects abutting is what keeps the hover strip
        // continuous and always under the cursor.
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        editor.hunt_open = false;
        editor.stop_open = false;
        editor.timing_open = false;
        let mut harness = Harness::new_ui(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
            edit_setup(ui, &mut editor);
        });
        harness.run();
        let hunt = harness.get_by_label("Hunt · Covenant").rect();
        let stop = harness.get_by_label("Stop · no limits").rect();
        let click = harness.get_by_label("Click timing · Instant").rect();
        assert_eq!(hunt.max.y, stop.min.y, "Hunt and Stop must tile");
        assert_eq!(stop.max.y, click.min.y, "Stop and Click timing must tile");
    }

    #[test]
    fn seeded_zero_limit_is_not_silently_clamped() {
        // A config-seeded 0 (max_refreshes = 0 halts at the first check) must
        // survive rendering unchanged; the old DragValue clamp rewrote it to
        // 1, so Apply sent a limit the player never set.
        let limits = Limits {
            max_refreshes: Some(0),
            ..Limits::default()
        };
        let mut editor = EditorState::new(named_filter(), limits, Timings::default());
        let mut harness = Harness::new_ui(|ui| {
            edit_setup(ui, &mut editor);
        });
        harness.run();
        drop(harness);
        assert_eq!(editor.limits.max_refreshes, Some(0));
    }

    #[test]
    fn apply_sends_a_changed_limit() {
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        editor.limits.max_refreshes = Some(7);
        let expected = Limits {
            max_refreshes: Some(7),
            ..Limits::default()
        };
        // The filter is unchanged from its applied twin, so only the limit ships.
        assert_eq!(run_setup(&mut editor), vec![Command::SetLimits(expected)]);
    }

    #[test]
    #[ignore = "renders the Setup sections to a PNG for visual iteration; run with --ignored"]
    fn render_stop_section_png() {
        let limits = Limits {
            max_refreshes: Some(49),
            max_spend: Some(30),
            ..Limits::default()
        };
        let mut editor = EditorState::new(named_filter(), limits, Timings::default());
        editor.hunt_open = false;
        editor.stop_open = true;
        editor.timing_open = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(430.0, 240.0))
            .with_pixels_per_point(2.0)
            .wgpu()
            .build_ui(move |ui| {
                theme::apply(ui.ctx());
                let bg = ui.visuals().panel_fill;
                ui.painter().rect_filled(ui.ctx().content_rect(), 0.0, bg);
                edit_sections(ui, &mut editor);
            });
        harness.run();
        let image = harness.render().expect("wgpu render");
        // Set ARKYVE_RENDER_DIR to collect the frames somewhere durable; the
        // temp directory keeps this runnable for any developer. (`ARKYVE_`, like
        // every other name the product owns — the old `SHOP_WATCHER_` prefix was
        // a leftover of the repository name.)
        let path = std::env::var_os("ARKYVE_RENDER_DIR")
            .map_or_else(std::env::temp_dir, std::path::PathBuf::from)
            .join("stop.png");
        image.save(&path).expect("save png");
        eprintln!("rendered {}", path.display());
    }

    #[test]
    fn apply_sends_changed_timings() {
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        let timings = Timings {
            refreshed: DelayRange::try_new(200, 800).expect("a valid fixture range"),
            ..Timings::default()
        };
        editor.timings = timings;
        assert_eq!(run_setup(&mut editor), vec![Command::SetTimings(timings)]);
    }
}
