//! The Setup surface: draft filter/limits/timings owned by the window, the
//! widgets that edit them, and the single Apply that commits the changed
//! drafts to the session. Hunt (what to buy) and Stop (when to quit) open on
//! arrival; Click timing is expert tuning and stays folded.

// A section file holds the parts that work on a `Filter` / `Limits` /
// `Timings` handed in by the caller; everything reaching into `EditorState`
// stays here.
mod clicking;
mod hunt;
mod stop;
mod timing;
mod timing_meter;

use clicking::{backend_row, clicking_summary, dry_run_row, timing_notice};
use hunt::{choice_list, grade_value, optional_value, quick_add_names, string_list, substat_reqs};
use stop::{duration_row, limit_row};
// Re-exported one level up: the idle status band describes the plan with the
// same two summaries the folded Hunt and Stop bars use, so the window has one
// phrasing per draft rather than two that drift.
pub(super) use hunt::hunt_summary;
pub(super) use stop::stop_summary;
use timing::{fine_tune_body, mode_hint, pass_estimate, timing_summary};

use eframe::egui;

use super::theme;
use crate::actuator::ClickMode;
#[cfg(test)]
use crate::actuator::plan::DelayRange;
use crate::actuator::plan::{TimingPreset, Timings};
use crate::app::Command;
use crate::domain::control::Limits;
#[cfg(test)]
use crate::domain::filter::SubstatReq;
use crate::domain::filter::{Filter, HUNTABLE_KINDS};
use crate::uplink::VocabularyCell;
use crate::uplink::protocol::{FilterVocabulary, VocabularyEntry};
// `currency_row` lends each cap its raw number for the frame the drag needs it.
use crate::domain::shop::{Crystals, Gold};
// Only the tests still name a kind directly; the checkbox row reads `HUNTABLE_KINDS`.
#[cfg(test)]
use crate::domain::shop::ItemKind;
use crate::render::kind_label;

/// Draft criteria owned by the window until Apply pushes them to the session.
/// Apply retunes the live session and writes the changed sections back to
/// config.toml, best-effort.
pub(super) struct EditorState {
    filter: Filter,
    limits: Limits,
    timings: Timings,
    /// A draft is dirty — Apply lit — exactly when it differs from its twin.
    applied_filter: Filter,
    applied_limits: Limits,
    applied_timings: Timings,
    /// The rehearsal switch and the click backend, drafted as one value because
    /// one `Command` carries both — a single Apply must not land in halves.
    click_mode: ClickMode,
    applied_click_mode: ClickMode,
    name_input: String,
    set_input: String,
    slot_input: String,
    substat_input: String,
    /// What the server offered, cached off `VocabularyCell` so the pickers do
    /// not clone forty strings per frame. Empty until a `catalog` message
    /// arrives, and empty for good against a server with no Catalog — every
    /// list then falls back to the free-text entry it had before.
    vocabulary: FilterVocabulary,
    /// The cell generation `vocabulary` was copied at. `0` is the pre-message
    /// value and one no write can produce, so the first sync always copies.
    vocabulary_generation: u64,
    /// One fold flag per section, five loose fields rather than a bitset or an
    /// array indexed by section. Each is only ever handed to [`section`] as
    /// `&mut editor.<name>_open`, named at every use and never passed
    /// positionally, so there is no site where two could be swapped unnoticed;
    /// an index would invent one. All thirty-two combinations are legal and
    /// mean what they say — a player folds whatever they are done with — so
    /// there is no impossible state left for an enum to rule out.
    hunt_open: bool,
    stop_open: bool,
    timing_open: bool,
    clicking_open: bool,
    /// Whether the Custom segment is selected, revealing the per-action bars.
    fine_tune_open: bool,
}

impl EditorState {
    pub(super) fn new(
        filter: Filter,
        limits: Limits,
        timings: Timings,
        click_mode: ClickMode,
    ) -> Self {
        Self {
            applied_filter: filter.clone(),
            applied_limits: limits,
            applied_timings: timings,
            applied_click_mode: click_mode,
            filter,
            limits,
            timings,
            click_mode,
            name_input: String::new(),
            set_input: String::new(),
            slot_input: String::new(),
            substat_input: String::new(),
            vocabulary: FilterVocabulary::default(),
            vocabulary_generation: 0,
            hunt_open: true,
            stop_open: true,
            timing_open: false,
            // Folded on arrival: a player opens Setup to change what they hunt,
            // not how the clicks are sent. Hunt and Stop keep the two open slots.
            clicking_open: false,
            fine_tune_open: false,
        }
    }

    /// Copies the server's vocabulary in, if it has moved since the last copy.
    ///
    /// Gated on the generation rather than run unconditionally: the window
    /// redraws every frame and the vocabulary is written once per session, so
    /// an ungated clone would copy forty-odd strings sixty times a second to
    /// learn nothing. Called once per frame, before the pickers read it.
    pub(super) fn sync_vocabulary(&mut self, cell: &VocabularyCell) {
        let generation = cell.generation();
        if generation != self.vocabulary_generation {
            self.vocabulary = cell.get();
            self.vocabulary_generation = generation;
        }
    }

    /// Re-seeds the applied twins from the commands the session *actually*
    /// took, so a draft counts as applied only once its command cleared the
    /// bounded queue. Read out of the command, never off the draft.
    pub(super) fn mark_applied(&mut self, delivered: &[Command]) {
        for command in delivered {
            match command {
                Command::SetFilter(filter) => self.applied_filter = filter.clone(),
                Command::SetLimits(limits) => self.applied_limits = *limits,
                Command::SetTimings(timings) => self.applied_timings = *timings,
                Command::SetClickMode(mode) => self.applied_click_mode = *mode,
                Command::Start | Command::Stop | Command::Toggle => {}
            }
        }
    }
}

/// The whole Setup surface in one `ui` so a test can drive sections and Apply
/// at once; the live window mounts them as separate panels.
#[cfg(test)]
fn edit_setup(ui: &mut egui::Ui, editor: &mut EditorState) -> Vec<Command> {
    edit_sections(ui, editor);
    ui.add_space(theme::SP_XL);
    commit_row(ui, editor, true)
}

/// The four collapsible sections — the Setup tab's scrolling body.
///
/// No `session_alive` parameter, and its brief life is worth a sentence:
/// `8d25453` added one so the restart-only settings could stay editable with
/// no session running, since the file was the only place they landed.
/// `08d67e9` made them a live retune, so all four sections now need somewhere
/// to send a command and the caller gates them together again.
pub(super) fn edit_sections(ui: &mut egui::Ui, editor: &mut EditorState) {
    // Summaries are built only while folded. No space between collapsed bars:
    // they tile on item spacing alone so hover strips meet with no dead seam.
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
    let clicking = (!editor.clicking_open).then(|| clicking_summary(editor.click_mode));
    section(
        ui,
        "Clicking",
        clicking.as_deref(),
        &mut editor.clicking_open,
    );
    if editor.clicking_open {
        clicking_body(ui, editor);
        ui.add_space(theme::SP_SM);
    }
}

/// The Clicking section's body. Last of the four, and the notice closes it, so
/// the last thing read before Apply is the sentence saying when the change
/// bites.
fn clicking_body(ui: &mut egui::Ui, editor: &mut EditorState) {
    dry_run_row(ui, &mut editor.click_mode.dry_run);
    backend_row(ui, &mut editor.click_mode.backend);
    ui.add_space(theme::SP_SM);
    timing_notice(ui, editor.click_mode != editor.applied_click_mode);
}

/// A list the server normally enumerates: checkboxes over what it offered, or
/// free text when it offered nothing.
///
/// The fallback is not a transition state. The relay talks to a server that may
/// have no Catalog to read, and the ids already in a player's `config.toml`
/// have to stay enterable then — a checkbox row over an empty list is a dead
/// control, and drawing nothing withdraws a criterion that still filters.
fn offered_list(
    ui: &mut egui::Ui,
    label: &str,
    values: &mut Vec<String>,
    input: &mut String,
    choices: &[VocabularyEntry],
) {
    if choices.is_empty() {
        string_list(ui, label, values, input);
    } else {
        choice_list(ui, label, values, choices);
    }
}

/// One collapsible section bar plus the breathing room its open body needs.
fn section(ui: &mut egui::Ui, title: &str, summary: Option<&str>, open: &mut bool) {
    if theme::collapsing_section(ui, title, summary, *open) {
        *open = !*open;
    }
    if *open {
        ui.add_space(theme::SP_SM);
    }
}

/// `n singular` / `n plural`, e.g. `1 refresh` / `3 refreshes`.
fn count_label(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

/// Hunt: what the loop buys. Without a criterion the loop refuses to arm.
fn hunt_body(ui: &mut egui::Ui, editor: &mut EditorState) {
    ui.horizontal(|ui| {
        // Driven by `HUNTABLE_KINDS`: an "unknown" box wrote
        // `kinds = ["unknown"]`, which the next launch refused to load.
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
    // Names have no vocabulary: the server publishes sets, substats and slots,
    // and an item name is not a closed list it can enumerate. `quick_add_names`
    // stays the shortcut for the two tokens nearly everyone hunts.
    // Names have no vocabulary: the server publishes sets, substats and slots,
    // and an item name is not a closed list it can enumerate. `quick_add_names`
    // stays the shortcut for the two tokens nearly everyone hunts.
    string_list(
        ui,
        "names (exact internal ids)",
        &mut editor.filter.names,
        &mut editor.name_input,
    );
    quick_add_names(ui, &mut editor.filter.names);
    offered_list(
        ui,
        "sets",
        &mut editor.filter.sets,
        &mut editor.set_input,
        &editor.vocabulary.sets,
    );
    offered_list(
        ui,
        "gear slots",
        &mut editor.filter.slots,
        &mut editor.slot_input,
        &editor.vocabulary.slots,
    );
    substat_reqs(
        ui,
        &mut editor.filter.required_substats,
        &mut editor.substat_input,
        &editor.vocabulary.substats,
    );
    ui.add_space(theme::SP_XS);
    egui::Grid::new("hunt-numerics")
        .num_columns(2)
        .spacing([theme::SP_SM, theme::SP_XS])
        .show(ui, |ui| {
            optional_value(ui, "min substats", &mut editor.filter.min_substats, 1);
            ui.end_row();
            // See [`grade_value`] for why the floor is a twin of the above.
            grade_value(ui, &mut editor.filter.min_grade);
            ui.end_row();
            // Seeded above the covenant-bookmark price, so a fresh cap still
            // matches the default hunt targets.
            currency_row(&mut editor.filter.max_price, Gold::get, Gold::new, |cap| {
                optional_value(ui, "max price (gold)", cap, 300_000)
            });
            ui.end_row();
        });
    ui.add_space(theme::SP_XS);
    ui.checkbox(&mut editor.filter.include_sold_out, "include sold out");
}

/// Stop: the run's safety rails, one ledger row each.
fn stop_body(ui: &mut egui::Ui, editor: &mut EditorState) {
    ui.weak("Stop the run at the first limit it reaches.");
    ui.add_space(theme::SP_SM);
    limit_row(ui, "refreshes", &mut editor.limits.max_refreshes, 10);
    currency_row(
        &mut editor.limits.max_spend,
        Crystals::get,
        Crystals::new,
        |cap| limit_row(ui, "crystals spent", cap, 30),
    );
    limit_row(ui, "matches", &mut editor.limits.max_matches, 5);
    duration_row(ui, &mut editor.limits.max_duration_ms);
}

/// Arming, for every optional criterion: unchecked writes `None`, a freshly
/// checked box seeds a non-zero value, an existing value is left alone.
fn arm_optional<T>(armed: bool, value: &mut Option<T>, seed: T) {
    if armed {
        value.get_or_insert(seed);
    } else {
        *value = None;
    }
}

/// Lends an optional *currency* field its raw number for one frame, so it can
/// use the same widget as every other criterion. Neither [`Gold`] nor
/// [`Crystals`] implements `egui::emath::Numeric` deliberately: `src/domain/`
/// compiles under `--no-default-features`, and the impl would pull egui into
/// the ledger types. The round-trip is exact, so [`commit_row`]'s bit-exact
/// dirty check is unaffected.
fn currency_row<T: Copy>(
    value: &mut Option<T>,
    get: impl Fn(T) -> u32,
    wrap: impl Fn(u32) -> T,
    edit: impl FnOnce(&mut Option<u32>),
) {
    let mut raw = value.map(get);
    edit(&mut raw);
    *value = raw.map(wrap);
}

/// The drag field for an armed optional criterion, floored at 1 and open
/// above — the shape every criterion but the grade floor has.
fn optional_field<T: egui::emath::Numeric>(ui: &mut egui::Ui, value: &mut T) -> egui::Response {
    bounded_field(ui, value, T::from_f64(1.0)..=T::MAX)
}

/// The same field over an explicit range, for a criterion the game defines on
/// a closed set of values ([`hunt::grade_value`] is the one).
///
/// `clamp_existing_to_range(false)` is exactly the pair a bounded criterion
/// needs: existing values are not clamped — a seeded `max_refreshes = 0`
/// survives instead of being silently rewritten to 1, see
/// `seeded_zero_limit_is_not_silently_clamped` — while dragged and typed values
/// still are.
fn bounded_field<T: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    value: &mut T,
    range: std::ops::RangeInclusive<T>,
) -> egui::Response {
    ui.add(
        egui::DragValue::new(value)
            .range(range)
            .clamp_existing_to_range(false),
    )
}

/// Click timing: a fixed tuned delay per click, plus a random extra the player
/// dials in so the loop never clicks like a metronome.
fn timing_body(ui: &mut egui::Ui, editor: &mut EditorState) {
    ui.label("How human should the clicks look?");
    ui.add_space(theme::SP_SM);
    // Carried out so the hint reuses this lookup rather than rescanning.
    let active = preset_row(ui, editor);
    ui.add_space(theme::SP_SM);
    // In the hint sentence, not a stat row, where a lone number read as a
    // misplaced KPI.
    ui.weak(format!(
        "{} About {} per pass.",
        mode_hint(active),
        pass_estimate(&editor.timings)
    ));

    if active.is_none() {
        ui.add_space(theme::SP_SM);
        fine_tune_body(ui, &mut editor.timings);
    }
}

/// The humanization mode as one segmented control. Clicking a preset
/// overwrites every action's random extra — and only that, see
/// [`TimingPreset::applied_to`] — then hides the bars; Custom reveals them
/// without touching the timings, and returns `None`.
fn preset_row(ui: &mut egui::Ui, editor: &mut EditorState) -> Option<TimingPreset> {
    let detected = TimingPreset::from_timings(&editor.timings);
    // Custom wins whenever the bars are open or the mix matches no preset, so
    // a config-seeded custom timing lands there with bars ready.
    let custom = editor.fine_tune_open || detected.is_none();
    // Snug segments in one strip, so the presets read as a control rather than
    // bare labels.
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
                        // `applied_to`, not `timings()`: the bare value carries
                        // `min_ms = 0` on all eight actions, so assigning it
                        // dropped every config-set floor on the next Apply.
                        editor.timings = preset.applied_to(&editor.timings);
                        editor.fine_tune_open = false;
                    }
                }
                if ui.selectable_label(custom, "Custom").clicked() {
                    editor.fine_tune_open = true;
                }
            });
        });
    // The pre-click state, so the hint below matches the segment on screen.
    if custom { None } else { detected }
}

/// One primary Apply that emits every draft that moved. The twins are *not*
/// touched here — [`EditorState::mark_applied`] re-seeds them from what the
/// session took, so a click lost to a saturated queue leaves Apply lit instead
/// of claiming a setting nobody received. Disabled while nothing changed, while
/// a changed filter is too unrestricted to arm, and once the session is dead.
///
/// `08d67e9` collapsed this back to one return value. While the click mode
/// was restart-only it had no `Command`, so Apply had to hand the caller a
/// second thing to persist and a second rule for when a draft counts as
/// applied. It is a live retune now, so it rides the same channel and the same
/// rule as the other three.
///
/// The shared `&EditorState` is what makes the paragraph above checkable: the
/// only way to break the twin rule from here is to widen this borrow, which is
/// a line in a diff rather than an assignment buried in the click branch.
#[must_use]
pub(super) fn commit_row(
    ui: &mut egui::Ui,
    editor: &EditorState,
    session_alive: bool,
) -> Vec<Command> {
    let dirty = Dirty::of(editor);
    // Only a *changed* filter clears the arming bar, so an already-applied one
    // lets limit/timing edits through.
    let blocked = dirty.filter && editor.filter.is_unrestricted();

    let mut commands = Vec::new();
    ui.horizontal(|ui| {
        // The blocking reason wins the peek slot: it explains the dark button.
        if blocked {
            ui.weak("add at least one hunt criterion before Apply");
        } else if let Some(summary) = dirty.summary() {
            ui.weak(summary);
        }
        // An explicit id, so the button's identity is its own and not its rank
        // in the sequence: the peek beside it is conditional, and egui numbers
        // an unsalted widget by that rank, so Apply changed id on the very
        // frame the peek appeared — which is the frame an edit brings the
        // button alive. Its rect does not move with it (right-aligned, while
        // the peek fills the space to its left), so egui saw one rect claimed
        // by two ids and said so — `changed id between passes`, 18 times in one
        // session's log — while the hover, focus and in-flight click it keys by
        // id were dropped at each flip.
        //
        // `UiBuilder::id` and not `push_id`: a salt only names a *child* of the
        // parent id, and egui still folds the parent's auto-id counter into
        // that child's own — so the salted button drifts exactly as far as the
        // unsalted one did. Only `IdSource::Explicit` leaves the counter out.
        ui.scope_builder(
            egui::UiBuilder::new()
                .id(ui.id().with("apply"))
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
            |ui| {
                let clicked = ui
                    .add_enabled_ui(session_alive && dirty.any() && !blocked, |ui| {
                        theme::primary_button(ui, "Apply")
                    })
                    .inner
                    .clicked();
                if clicked {
                    if dirty.filter {
                        commands.push(Command::SetFilter(editor.filter.clone()));
                    }
                    if dirty.limits {
                        commands.push(Command::SetLimits(editor.limits));
                    }
                    if dirty.timings {
                        commands.push(Command::SetTimings(editor.timings));
                    }
                    if dirty.click_mode {
                        commands.push(Command::SetClickMode(editor.click_mode));
                    }
                }
            },
        );
    });
    commands
}

/// Which of the four drafts differ from their applied twins, decided once per
/// frame and then read four times over.
///
/// One value rather than four `bool` locals because they were being read
/// positionally: `dirty_summary(filter, limits, timings, clicking)` type-checks
/// under every permutation of its arguments, so a transposed pair would name
/// the wrong section in the peek — and that peek is the only thing telling a
/// player which part of config.toml Apply is about to overwrite. Fields carry
/// the name to each use instead.
///
/// Four `bool`s and no further folding: they are one per draft the session can
/// be sent, they are read one at a time by name, and the flat alternative — the
/// commands themselves, built each frame and discarded unless clicked — would
/// clone the whole [`Filter`] on every repaint to answer a question about
/// whether it changed.
#[derive(Clone, Copy, Default)]
struct Dirty {
    filter: bool,
    limits: bool,
    timings: bool,
    click_mode: bool,
}

impl Dirty {
    /// Bit-exact on purpose, `min`'s `f64` included: this is change detection,
    /// not a numeric test, so an epsilon would make a real edit invisible. It
    /// cannot survive a non-finite `min` (`NaN != NaN` lights Apply forever),
    /// which is why neither the loader nor [`substat_reqs`] admits one.
    fn of(editor: &EditorState) -> Self {
        Self {
            filter: editor.filter != editor.applied_filter,
            limits: editor.limits != editor.applied_limits,
            timings: editor.timings != editor.applied_timings,
            click_mode: editor.click_mode != editor.applied_click_mode,
        }
    }

    /// Whether Apply has anything to send at all.
    const fn any(self) -> bool {
        self.filter || self.limits || self.timings || self.click_mode
    }

    /// The commit bar's peek, e.g. `Hunt, Stop edited`. Labels mirror the
    /// section titles so it points at the collapsible that changed.
    fn summary(self) -> Option<String> {
        let mut parts: Vec<&str> = Vec::new();
        if self.filter {
            parts.push("Hunt");
        }
        if self.limits {
            parts.push("Stop");
        }
        if self.timings {
            parts.push("Click timing");
        }
        if self.click_mode {
            parts.push("Clicking");
        }
        (!parts.is_empty()).then(|| format!("{} edited", parts.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use egui_kittest::{
        Harness,
        kittest::{NodeT, Queryable},
    };

    use super::*;

    fn named_filter() -> Filter {
        Filter {
            names: vec!["ticketrare_name".to_owned()],
            ..Filter::default()
        }
    }

    /// An editor already holding the server's vocabulary, as one looks once a
    /// `catalog` message has landed.
    fn stocked_editor() -> EditorState {
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        let cell = VocabularyCell::new();
        cell.set(FilterVocabulary {
            sets: vec![
                entry("set_speed", "Speed Set"),
                entry("set_cri", "Critical Set"),
            ],
            substats: vec![entry("speed", "Speed"), entry("att_rate", "Attack(%)")],
            slots: vec![entry("helm", "Helmet"), entry("boot", "Boots")],
        });
        editor.sync_vocabulary(&cell);
        editor
    }

    fn entry(id: &str, label: &str) -> VocabularyEntry {
        VocabularyEntry {
            id: id.to_owned(),
            label: label.to_owned(),
        }
    }

    /// Draw Setup once, without committing anything.
    fn draw_setup(editor: &mut EditorState) -> Harness<'_> {
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, editor);
        });
        harness.run();
        harness
    }

    /// The checkbox row only exists once the server named something. Until
    /// then every list keeps its free-text field, because the ids in a player's
    /// config must stay enterable against a server with no Catalog.
    #[test]
    fn the_lists_fall_back_to_free_text_with_no_vocabulary() {
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        let harness = draw_setup(&mut editor);
        // One "add" button per free-text list: names, sets, gear slots and
        // required substats.
        assert_eq!(
            harness.get_all_by_label("add").count(),
            4,
            "every list should offer its text field"
        );
    }

    /// With a vocabulary the three enumerable criteria become checkbox rows,
    /// and only `names` — which is not a closed list — keeps its field.
    #[test]
    fn a_vocabulary_turns_the_enumerable_lists_into_choices() {
        let mut editor = stocked_editor();
        let harness = draw_setup(&mut editor);
        assert_eq!(
            harness.get_all_by_label("add").count(),
            1,
            "only `names` has no vocabulary to offer"
        );
        // Sets and slots draw a box each; substats draw an add button each.
        for label in ["Speed Set", "Critical Set", "Helmet", "Boots"] {
            assert_eq!(
                harness.get_all_by_label(label).count(),
                1,
                "{label} should have a checkbox"
            );
        }
        for label in ["+ Speed", "+ Attack(%)"] {
            assert_eq!(
                harness.get_all_by_label(label).count(),
                1,
                "{label} should be offered as a substat requirement"
            );
        }
    }

    /// Ticking writes the ID and never the label: the label is the game's
    /// words, the id is what `matches` compares and what config.toml stores.
    #[test]
    fn ticking_a_set_stores_its_internal_id() {
        let mut editor = stocked_editor();
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.get_by_label("Speed Set").click();
        harness.run();
        drop(harness);
        assert_eq!(editor.filter.sets, vec!["set_speed".to_owned()]);
    }

    /// And unticking takes it back out, rather than leaving a box that lies.
    #[test]
    fn unticking_a_slot_drops_its_id() {
        let mut editor = stocked_editor();
        editor.filter.slots = vec!["helm".to_owned()];
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.get_by_label("Helmet").click();
        harness.run();
        drop(harness);
        assert!(editor.filter.slots.is_empty());
    }

    /// Adding a substat requirement stores the id too, and the row it creates
    /// carries the threshold control the checkbox lists have no room for.
    #[test]
    fn adding_a_substat_requirement_stores_its_internal_id() {
        let mut editor = stocked_editor();
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.get_by_label("+ Attack(%)").click();
        harness.run();
        drop(harness);
        assert_eq!(editor.filter.required_substats.len(), 1);
        assert_eq!(editor.filter.required_substats[0].name, "att_rate");
        assert_eq!(editor.filter.required_substats[0].min, None);
    }

    /// An already-required substat leaves the offer row: `matches` walks the
    /// requirements, so a second row for one name is a duplicate threshold on
    /// a value that only has one.
    #[test]
    fn an_already_required_substat_is_not_offered_again() {
        let mut editor = stocked_editor();
        editor.filter.required_substats = vec![SubstatReq {
            name: "speed".to_owned(),
            min: None,
        }];
        let harness = draw_setup(&mut editor);
        // `query_all_*`, not `get_all_*`: the getters panic on no match, and
        // "no match" is exactly the assertion here.
        assert_eq!(harness.query_all_by_label("+ Speed").count(), 0);
        assert_eq!(harness.query_all_by_label("+ Attack(%)").count(), 1);
    }

    /// An id the vocabulary cannot name has no box of its own, so it is drawn
    /// as its own removable row — otherwise a criterion written before the
    /// catalog arrived would filter while being invisible and unremovable.
    #[test]
    fn an_unoffered_id_stays_visible_and_removable() {
        let mut editor = stocked_editor();
        // No names: `string_list` draws a remove cross per entry, and the click
        // below has to be the only one on the surface.
        editor.filter.names.clear();
        editor.filter.sets = vec!["set_retired".to_owned()];
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.run();
        assert_eq!(harness.get_all_by_label("set_retired").count(), 1);
        harness.get_by_label("✕").click();
        harness.run();
        drop(harness);
        assert!(editor.filter.sets.is_empty());
    }

    /// The cache is generation-gated, so a vocabulary that never moved must not
    /// be re-read — and one that did must be.
    #[test]
    fn the_vocabulary_is_copied_only_when_it_moves() {
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        let cell = VocabularyCell::new();
        editor.sync_vocabulary(&cell);
        assert!(editor.vocabulary.slots.is_empty());

        cell.set(FilterVocabulary {
            slots: vec![entry("helm", "Helmet")],
            ..FilterVocabulary::default()
        });
        editor.sync_vocabulary(&cell);
        assert_eq!(editor.vocabulary.slots.len(), 1);

        // A sync against an unmoved cell keeps what it had.
        editor.sync_vocabulary(&cell);
        assert_eq!(editor.vocabulary.slots.len(), 1);
    }

    /// Drive `edit_setup` once, capturing whatever Apply committed. Only a
    /// non-empty commit is latched, so the final quiet frame can't wipe it.
    fn run_setup(editor: &mut EditorState) -> Vec<Command> {
        let mut sent = Vec::new();
        let mut harness = setup_harness(|ui| {
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

    /// A harness tall enough to hold the whole Setup surface.
    ///
    /// Explicit, not `Harness::new_ui`'s default: a clipped widget is still in
    /// the accessibility tree, so `get_by_label` finds it and the simulated
    /// click then lands outside the clip rect and does nothing. Adding the
    /// fourth section pushed Apply past the default height and five tests began
    /// reporting an empty command list — a failure that reads like a broken
    /// dirty-check and is really a broken viewport.
    ///
    /// It happened again at 1400 when Hunt grew its gear-slot list, which is
    /// why the height is generous rather than measured: anything added to Setup
    /// pushes the sections below it down, and a tight fit turns every such
    /// addition into three misleading failures somewhere else.
    fn setup_harness<'a>(app: impl FnMut(&mut egui::Ui) + 'a) -> Harness<'a> {
        Harness::builder()
            .with_size(egui::vec2(480.0, 2000.0))
            .build_ui(app)
    }

    /// A click mode different from the seeded one.
    fn other_mode() -> ClickMode {
        ClickMode {
            dry_run: true,
            backend: crate::actuator::ActuatorBackend::Input,
        }
    }

    /// Apply carries the pair in one command. The point is that it cannot be
    /// split: a job running the old backend in the new rehearsal state would
    /// engage the window and then only journal.
    #[test]
    fn the_click_mode_travels_as_one_command() {
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        editor.click_mode = other_mode();
        assert_eq!(
            run_setup(&mut editor),
            vec![Command::SetClickMode(other_mode())]
        );
    }

    /// Half a change is still a change: flipping only the rehearsal switch must
    /// still send the backend alongside it, or the executor could read a pair
    /// that never existed in the editor.
    #[test]
    fn flipping_one_half_still_sends_both() {
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        editor.click_mode.dry_run = true;
        let expected = ClickMode {
            dry_run: true,
            ..ClickMode::default()
        };
        assert_eq!(
            run_setup(&mut editor),
            vec![Command::SetClickMode(expected)]
        );
    }

    /// It follows the same twin rule as the other three now, and that is the
    /// whole point of `08d67e9`: a command lost to a saturated queue leaves
    /// Apply lit rather than claiming a mode the executor never heard about.
    #[test]
    fn the_click_mode_draft_clears_only_on_a_delivered_command() {
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        editor.click_mode = other_mode();
        assert_ne!(editor.click_mode, editor.applied_click_mode);
        editor.mark_applied(&[Command::SetLimits(Limits::default())]);
        assert_ne!(
            editor.click_mode, editor.applied_click_mode,
            "another command's delivery must not clear this draft"
        );
        editor.mark_applied(&[Command::SetClickMode(other_mode())]);
        assert_eq!(editor.click_mode, editor.applied_click_mode);
    }

    #[test]
    fn apply_sends_only_the_changed_draft() {
        let mut editor = EditorState::new(
            Filter::default(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        editor.filter = named_filter();
        assert_eq!(
            run_setup(&mut editor),
            vec![Command::SetFilter(named_filter())]
        );
    }

    #[test]
    fn apply_leaves_the_draft_dirty_until_the_shell_confirms() {
        let mut editor = EditorState::new(
            Filter::default(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        editor.filter = named_filter();
        let commands = run_setup(&mut editor);
        assert_eq!(commands, vec![Command::SetFilter(named_filter())]);
        assert_ne!(editor.filter, editor.applied_filter);

        editor.mark_applied(&commands);
        assert_eq!(editor.filter, editor.applied_filter);
        assert!(run_setup(&mut editor).is_empty());
    }

    #[test]
    fn mark_applied_ignores_the_status_bar_commands() {
        // These ride the same dispatch list and carry no draft, so they must
        // never re-seed one.
        let mut editor = EditorState::new(
            Filter::default(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        editor.filter = named_filter();
        editor.mark_applied(&[Command::Start, Command::Stop, Command::Toggle]);
        assert_ne!(editor.filter, editor.applied_filter);
    }

    #[test]
    fn apply_inert_while_nothing_changed() {
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        assert!(run_setup(&mut editor).is_empty());
    }

    #[test]
    fn a_non_finite_substat_threshold_cannot_survive_a_render() {
        // `DragValue` parses typed text with `f64::from_str`, which accepts
        // "nan"/"inf" — either lights Apply forever while matching nothing, so
        // rendering the row must snap it back.
        let filter = Filter {
            required_substats: vec![SubstatReq {
                name: "speed".to_owned(),
                min: Some(f64::NAN),
            }],
            ..Filter::default()
        };
        let mut editor = EditorState::new(
            filter,
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        let harness = Harness::new_ui(|ui| {
            edit_sections(ui, &mut editor);
        });
        drop(harness);
        assert_eq!(editor.filter.required_substats[0].min, Some(1.0));
        assert_eq!(editor.filter, editor.filter.clone());
    }

    #[test]
    fn apply_blocked_while_the_dirty_filter_is_unrestricted() {
        // The draft is dirty but unrestricted: the loop would refuse it.
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        editor.filter.names.clear();
        assert!(editor.filter.is_unrestricted());
        assert!(run_setup(&mut editor).is_empty());
    }

    #[test]
    fn dirty_summary_joins_the_changed_section_labels() {
        assert_eq!(Dirty::default().summary(), None);
        assert_eq!(
            Dirty {
                filter: true,
                ..Dirty::default()
            }
            .summary()
            .as_deref(),
            Some("Hunt edited")
        );
        assert_eq!(
            Dirty {
                filter: true,
                limits: true,
                timings: true,
                click_mode: true,
            }
            .summary()
            .as_deref(),
            Some("Hunt, Stop, Click timing, Clicking edited")
        );
        // The mode alone, which is the shape a rehearsal flip takes.
        assert_eq!(
            Dirty {
                click_mode: true,
                ..Dirty::default()
            }
            .summary()
            .as_deref(),
            Some("Clicking edited")
        );
    }

    #[test]
    fn commit_bar_names_the_dirty_section() {
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        editor.limits.max_refreshes = Some(5);
        let mut harness = Harness::new_ui(|ui| {
            edit_setup(ui, &mut editor);
        });
        harness.run();
        harness.get_by_label("Stop edited");
    }

    /// The peek label is conditional, and egui derives an unsalted widget's id
    /// from its rank in the sequence — so the summary appearing renumbered
    /// everything after it, Apply included, on the very frame Apply came alive.
    /// The button's rect never moves (it is right-aligned in a pinned bar), so
    /// egui saw one rect claimed by two ids and logged `changed id between
    /// passes` — 18 of them in one session's log — while the interaction state
    /// it keys by id was discarded at each flip.
    #[test]
    fn the_apply_button_keeps_its_id_when_the_peek_appears() {
        let editor = RefCell::new(EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        ));
        let mut harness = setup_harness(|ui| {
            let _ = commit_row(ui, &editor.borrow(), true);
        });
        harness.run();
        let button = harness.get_by_label("Apply");
        let (clean, rect) = (button.accesskit_node().id(), button.rect());

        // One edit: the peek label appears ahead of the button.
        editor.borrow_mut().limits.max_refreshes = Some(5);
        harness.run();

        let button = harness.get_by_label("Apply");
        assert_eq!(
            button.accesskit_node().id(),
            clean,
            "the id egui keys the button's interaction state by"
        );
        // The other half of the collision, and why egui could see it at all:
        // right-aligned, the button holds its rect while the peek fills the
        // space to its left.
        assert_eq!(button.rect(), rect);
    }

    #[test]
    fn quick_add_seeds_a_hunt_token() {
        let mut editor = EditorState::new(
            Filter::default(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        let mut harness = Harness::new_ui(|ui| {
            edit_setup(ui, &mut editor);
        });
        harness.get_by_label("+ Covenant").click();
        harness.run();
        drop(harness);
        assert_eq!(editor.filter.names, vec!["ticketrare_name".to_owned()]);
    }

    #[test]
    fn a_config_set_grade_floor_can_be_seen_and_cleared() {
        // With no widget for it, a config-set floor was the one criterion
        // restricting the hunt and was invisible *and* uncorrectable.
        let filter = Filter {
            min_grade: Some(2),
            ..named_filter()
        };
        let mut editor = EditorState::new(
            filter,
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        {
            // Scoped so the assert below can read the draft: a first render
            // alone must not move the value.
            let mut harness = Harness::new_ui(|ui| {
                edit_setup(ui, &mut editor);
            });
            harness.run();
        }
        assert_eq!(editor.filter.min_grade, Some(2));
        let mut harness = Harness::new_ui(|ui| {
            edit_setup(ui, &mut editor);
        });
        harness.get_by_label("min grade").click();
        harness.run();
        drop(harness);
        assert_eq!(editor.filter.min_grade, None);
    }

    #[test]
    fn arming_the_grade_floor_seeds_the_epic_grade() {
        // The seed must be a floor the game has: `hunt::GRADE_MAX`.
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        let mut harness = Harness::new_ui(|ui| {
            edit_setup(ui, &mut editor);
        });
        harness.get_by_label("min grade").click();
        harness.run();
        drop(harness);
        assert_eq!(editor.filter.min_grade, Some(4));
        let expected = Filter {
            min_grade: Some(4),
            ..named_filter()
        };
        assert_eq!(run_setup(&mut editor), vec![Command::SetFilter(expected)]);
    }

    #[test]
    fn kind_checkbox_updates_the_draft() {
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
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
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
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
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        editor.timing_open = true;
        let mut harness = setup_harness(|ui| {
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
        // Do not re-add an "unknown" box: it wrote `kinds = ["unknown"]`, which
        // the next launch refused to load (see `hunt_body`).
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
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
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        editor.timing_open = true;
        let mut harness = setup_harness(|ui| {
            edit_setup(ui, &mut editor);
        });
        harness.get_by_label("Cautious").click();
        harness.run();
        drop(harness);
        assert_eq!(editor.timings, TimingPreset::Cautious.timings());
    }

    #[test]
    fn clicking_a_preset_keeps_a_config_set_delay_floor() {
        // The click that used to cost a setting: over a config-set floor, one
        // preset click replaced the draft with ranges starting at 0, which
        // Apply then wrote to disk with no undo.
        let floored = Timings {
            refreshed: DelayRange::try_new(200, 800).expect("a valid fixture range"),
            ..Timings::default()
        };
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            floored,
            ClickMode::default(),
        );
        editor.timing_open = true;
        let mut harness = setup_harness(|ui| {
            edit_setup(ui, &mut editor);
        });
        harness.get_by_label("Human").click();
        harness.run();
        drop(harness);
        assert_eq!(
            editor.timings.refreshed.min_ms(),
            200,
            "the floor is config-only and the preset must not touch it"
        );
        assert_eq!(
            editor.timings.refreshed.max_ms(),
            TimingPreset::Human.timings().refreshed.max_ms(),
            "while the ceiling is the one just chosen"
        );
        // And the segment lights, rather than falling back to Custom over a
        // floor it does not edit.
        assert_eq!(
            TimingPreset::from_timings(&editor.timings),
            Some(TimingPreset::Human)
        );
    }

    #[test]
    fn collapsed_sections_tile_with_no_hover_gap() {
        // egui hit-tests wider than the fill, so a gap between the two leaves a
        // dead seam where hovering lights a bar the pointer isn't over.
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
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
        // A config-seeded 0 (halt at the first check) must survive rendering —
        // the old `DragValue` clamp rewrote it to 1.
        let limits = Limits {
            max_refreshes: Some(0),
            ..Limits::default()
        };
        let mut editor = EditorState::new(
            named_filter(),
            limits,
            Timings::default(),
            ClickMode::default(),
        );
        let mut harness = Harness::new_ui(|ui| {
            edit_setup(ui, &mut editor);
        });
        harness.run();
        drop(harness);
        assert_eq!(editor.limits.max_refreshes, Some(0));
    }

    #[test]
    fn apply_sends_a_changed_limit() {
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        editor.limits.max_refreshes = Some(7);
        let expected = Limits {
            max_refreshes: Some(7),
            ..Limits::default()
        };
        assert_eq!(run_setup(&mut editor), vec![Command::SetLimits(expected)]);
    }

    #[test]
    #[cfg(feature = "render-png")]
    #[ignore = "renders the Setup sections to a PNG for visual iteration; run with --ignored"]
    fn render_stop_section_png() {
        let limits = Limits {
            max_refreshes: Some(49),
            max_spend: Some(Crystals::new(30)),
            ..Limits::default()
        };
        let mut editor = EditorState::new(
            named_filter(),
            limits,
            Timings::default(),
            ClickMode::default(),
        );
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
        // Set `ARKYVE_RENDER_DIR` to collect the frames somewhere durable.
        let path = std::env::var_os("ARKYVE_RENDER_DIR")
            .map_or_else(std::env::temp_dir, std::path::PathBuf::from)
            .join("stop.png");
        image.save(&path).expect("save png");
        eprintln!("rendered {}", path.display());
    }

    #[test]
    fn apply_sends_changed_timings() {
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        let timings = Timings {
            refreshed: DelayRange::try_new(200, 800).expect("a valid fixture range"),
            ..Timings::default()
        };
        editor.timings = timings;
        assert_eq!(run_setup(&mut editor), vec![Command::SetTimings(timings)]);
    }
}
