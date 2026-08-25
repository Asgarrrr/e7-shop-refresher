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
use hunt::{choice_list, optional_value, rarity_ladder, string_list, substat_chips, token_cards};
use stop::{duration_row, limit_row};
// Re-exported one level up: the idle status band describes the plan with the
// same two summaries the folded Hunt and Stop bars use, so the window has one
// phrasing per draft rather than two that drift.
pub(super) use hunt::hunt_summary;
pub(super) use stop::stop_summary;
use timing::{fine_tune_body, mode_hint, pass_estimate, timing_summary};

use eframe::egui;

use super::icons::SetIcons;
use super::theme;
use crate::actuator::ClickMode;
#[cfg(test)]
use crate::actuator::plan::DelayRange;
use crate::actuator::plan::{TimingPreset, Timings};
use crate::app::Command;
use crate::domain::control::Limits;
#[cfg(test)]
use crate::domain::filter::SubstatReq;
use crate::domain::filter::{Filter, GearRule};
use crate::uplink::VocabularyCell;
use crate::uplink::protocol::{FilterVocabulary, VocabularyEntry};
// `currency_row` lends each cap its raw number for the frame the drag needs it.
use crate::domain::shop::{Crystals, Gold};
// `kinds` has no control in the window any more, so only a test still names a
// kind — the one pinning that a config-set one survives a render.
#[cfg(test)]
use crate::domain::shop::ItemKind;

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
    set_input: String,
    slot_input: String,
    /// Which gear rule the Hunt body is editing.
    ///
    /// It may point one PAST the last rule, and that is the whole trick behind
    /// the `+` cell: a rule with nothing set does not restrict, so materializing
    /// one the moment a tab is added would put an inert `[[filter.gear]]` in the
    /// player's file and — worse — light Apply over an edit nobody made. The
    /// index alone says the tab is there; [`with_rule`] writes the rule into the
    /// filter on the first criterion that restricts, and takes it back out when
    /// the last one is cleared.
    gear_index: usize,
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
            set_input: String::new(),
            slot_input: String::new(),
            gear_index: 0,
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

    /// Copies the server's vocabulary in, if it has moved since the last copy,
    /// and says whether it did.
    ///
    /// Gated on the generation rather than run unconditionally: the window
    /// redraws every frame and the vocabulary is written once per session, so
    /// an ungated clone would copy forty-odd strings sixty times a second to
    /// learn nothing. Called once per frame, before the pickers read it.
    ///
    /// The answer is what the window's [`SetIcons`] hangs off: decoding
    /// twenty-two PNGs and uploading twenty-two textures is a per-catalog cost,
    /// and this is the one call that knows a catalog landed.
    pub(super) fn sync_vocabulary(&mut self, cell: &VocabularyCell) -> bool {
        let generation = cell.generation();
        if generation == self.vocabulary_generation {
            return false;
        }
        self.vocabulary = cell.get();
        self.vocabulary_generation = generation;
        true
    }

    /// The wire icon table the last catalog carried, base64 as it arrived.
    /// Decoding is [`SetIcons::load`]'s job, and the window owns the result.
    pub(super) fn icons(&self) -> &std::collections::HashMap<String, String> {
        &self.vocabulary.icons
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
///
/// It also decodes the set icons per frame, where the window loads them once per
/// catalog: a test drives an `EditorState` and no `ShopApp`, so this is the only
/// place the two can be held together. The fixtures carry one picture, so the
/// cost is the test's and never the app's.
#[cfg(test)]
fn edit_setup(ui: &mut egui::Ui, editor: &mut EditorState) -> Vec<Command> {
    let mut icons = SetIcons::default();
    icons.load(ui.ctx(), editor.icons());
    edit_sections(ui, editor, &icons);
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
pub(super) fn edit_sections(ui: &mut egui::Ui, editor: &mut EditorState, icons: &SetIcons) {
    // Summaries are built only while folded. No space between collapsed bars:
    // they tile on item spacing alone so hover strips meet with no dead seam.
    let hunt = (!editor.hunt_open).then(|| hunt_summary(&editor.filter));
    section(ui, "Hunt", hunt.as_deref(), &mut editor.hunt_open);
    if editor.hunt_open {
        hunt_body(ui, editor, icons);
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
    icons: &SetIcons,
) {
    if choices.is_empty() {
        string_list(ui, label, values, input);
    } else {
        choice_list(ui, label, values, choices, icons);
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
///
/// Two blocks, because [`Filter::matches`] has two branches: what an item IS,
/// and what a piece of gear looks like. An item satisfying either is a hit, and
/// the rule between them is what says so on screen — stacked as one list, the
/// same controls read as a conjunction the engine stopped applying.
///
/// `kinds` is not here. It still gates both branches and still loads from
/// `config.toml`; what it has no place doing is duplicating a statement the
/// blocks below already make — a name criterion names tokens, a gear criterion
/// names gear, and a third control saying the same is a way to contradict
/// yourself into a hunt that matches nothing.
fn hunt_body(ui: &mut egui::Ui, editor: &mut EditorState, icons: &SetIcons) {
    // `names` is an open field the filter matches literally, but the tokens the
    // shop sells are a closed list of three — and they are what a name criterion
    // is nearly always for. It is the one criterion with NO free-text fallback,
    // because the cards are built from `hunt::HUNT_TOKENS` and so cannot fail to
    // be built; a name they do not offer keeps its own removable row, exactly as
    // an unoffered set id does.
    token_cards(ui, &mut editor.filter.names);

    branch_separator(ui, "— or gear —", "or gear");

    piece_strip(ui, &mut editor.filter.gear, &mut editor.gear_index);
    with_rule(&mut editor.filter.gear, editor.gear_index, |rule| {
        offered_list(
            ui,
            "sets",
            &mut rule.sets,
            &mut editor.set_input,
            &editor.vocabulary.sets,
            icons,
        );
        gear_rule(ui);
        // The catalog's icon table is keyed by set id and nothing else: there
        // are no gear-slot pictures on the wire and none planned. An empty
        // source says that in the one place it matters — inside `choice_list`,
        // where every value then takes the text branch.
        offered_list(
            ui,
            "gear slots",
            &mut rule.slots,
            &mut editor.slot_input,
            &editor.vocabulary.slots,
            &SetIcons::default(),
        );
        gear_rule(ui);
        substat_chips(
            ui,
            &mut rule.required_substats,
            &mut rule.substat_match,
            &editor.vocabulary.substats,
        );
        gear_rule(ui);
        // No space between the header and its control: the item spacing already
        // puts one there, and the two blocks above take exactly that — a header
        // that sits further from its own control than from the block before it
        // is the reading this section had.
        //
        // Unconditional, where it used to wait on the catalog's rarity family:
        // the cells are `theme::RARITIES` now, so there is no session in which
        // the ladder cannot be drawn. See [`rarity_ladder`].
        ui.label(theme::section("rarity"));
        rarity_ladder(ui, &mut rule.min_grade);
        ui.add_space(theme::SP_XS);
        egui::Grid::new("hunt-numerics")
            .num_columns(2)
            .spacing([theme::SP_SM, theme::SP_XS])
            .show(ui, |ui| {
                // Seeded above the covenant-bookmark price, so a fresh cap still
                // matches the default hunt targets.
                currency_row(&mut rule.max_price, Gold::get, Gold::new, |cap| {
                    optional_value(ui, "max price (gold)", cap, 300_000)
                });
                ui.end_row();
            });
    });
    // Outside both blocks, and last because of it: `matches` applies this
    // before either branch, so it widens the name hunt exactly as it widens the
    // gear one.
    ui.add_space(theme::SP_SM);
    ui.checkbox(&mut editor.filter.include_sold_out, "include sold out");
}

/// The pieces being hunted, as one segmented strip: a cell per rule, a `+` to
/// add one, and a `✕` to drop the one on screen.
///
/// A strip and not a stack of cards, because the window is pinned at
/// [`crate::ui::WINDOW_WIDTH`] and one rule's criteria already fill a panel:
/// three rules unfolded at once would be three screens of scrolling to compare
/// two chips. The strip is the same grammar the rarity ladder and the timing
/// presets wear, so an exclusive choice looks like every other one here.
///
/// The `+` cell only moves the index past the last rule — see
/// [`EditorState::gear_index`] — and the `✕` is a [`theme::bare_verb`] rather
/// than a cell, because it acts on the selection instead of being one.
fn piece_strip(ui: &mut egui::Ui, rules: &mut Vec<GearRule>, index: &mut usize) {
    // A rule a config file holds and the window cannot reach would filter while
    // being invisible and unremovable — the defect `unoffered_rows` exists to
    // close for a value the vocabulary cannot name. Clamped rather than
    // asserted: the count changes under the index whenever a rule is removed.
    *index = (*index).min(rules.len());
    ui.horizontal(|ui| {
        ui.label(theme::section("pieces"));
        theme::segmented_strip(ui, |ui| {
            for cell in 0..=rules.len() {
                // The cell past the last rule is the one being drafted, and it
                // is drawn as a `+` until a criterion makes it real.
                let label = if cell == rules.len() {
                    "+".to_owned()
                } else {
                    (cell + 1).to_string()
                };
                if ui
                    .selectable_label(*index == cell, egui::RichText::new(label).small())
                    .clicked()
                {
                    *index = cell;
                }
            }
        });
        // Only over a rule that exists: the draft cell has nothing to remove,
        // and a disabled verb beside it would be a control that never acts.
        if *index < rules.len() && theme::bare_verb(ui, "remove").clicked() {
            rules.remove(*index);
            *index = (*index).min(rules.len());
        }
    });
    ui.add_space(theme::SP_SM);
}

/// Lends the Hunt body the rule at `index`, and puts it back only if it says
/// something.
///
/// **A rule with nothing set never reaches the filter**, which is what keeps the
/// `+` cell honest. `GearRule::restricts` is false for a fresh one, so storing
/// it would write an inert `[[filter.gear]]` into the player's file and light
/// Apply over an edit nobody made — and `mark_applied` could never put it out,
/// since the applied twin would keep coming back without it.
///
/// The same test in reverse takes a rule out: clearing the last criterion of an
/// existing rule removes it rather than leaving a card that filters nothing.
///
/// `mem::take` and not a clone: this runs every frame, and the rule holds three
/// `Vec`s of `String`.
fn with_rule<R>(
    rules: &mut Vec<GearRule>,
    index: usize,
    edit: impl FnOnce(&mut GearRule) -> R,
) -> R {
    let held = index < rules.len();
    let mut rule = if held {
        std::mem::take(&mut rules[index])
    } else {
        GearRule::default()
    };
    let out = edit(&mut rule);
    match (rule.restricts(), held) {
        (true, true) => rules[index] = rule,
        (true, false) => rules.push(rule),
        (false, true) => {
            rules.remove(index);
        }
        (false, false) => {}
    }
    out
}

/// The boundary between the two branches of [`Filter::matches`] — what an item
/// IS, and what a piece of gear LOOKS LIKE.
///
/// A full-width rule with the word sitting on it, breathing on both sides. It
/// was a weak label fenced by two typed em-dashes, which is a caption
/// impersonating a separation: the engine really does branch here, and the
/// blocks either side are not rows of one list. The dashes are gone from the
/// picture because the rule is the separation now — drawing both says it twice.
///
/// [`theme::BLOCK_RULE`] and not [`theme::HAIRLINE`], which is the one
/// [`gear_rule`] takes: that doc already reserved the undimmed rule for exactly
/// this boundary.
///
/// **The word is painted and its name restated**, the way
/// [`theme::collapsing_section`] paints its title. Painted text publishes no
/// accessibility node at all, and this one has to stay findable: it is the only
/// thing on the surface saying the criteria above and below are an OR rather than
/// an AND. `name` keeps the dashes the label always had, so the node a reader —
/// and `the_hunt_body_separates_names_from_gear` — asks for is unchanged while
/// the drawing is not.
///
/// The ground punched behind the word comes from `visuals.panel_fill`, the value
/// [`theme::apply`] writes, so the hole cannot drift from the panel it is cut in.
fn branch_separator(ui: &mut egui::Ui, name: &str, word: &str) {
    ui.add_space(theme::SP_XL);
    let line = theme::rule(ui, theme::BLOCK_RULE);
    let galley = ui.painter().layout_no_wrap(
        word.to_owned(),
        egui::TextStyle::Small.resolve(ui.style()),
        theme::INK_FAINT,
    );
    // Centred on the line as PAINTED — full-bleed to the clip edges — rather
    // than on the content box the cursor allocates, or the word would sit off
    // the middle of the rule it interrupts by the side margin's width.
    let center = egui::pos2(ui.clip_rect().x_range().center(), line.center().y);
    let text = egui::Align2::CENTER_CENTER.anchor_size(center, galley.size());
    let ground = text.expand2(egui::vec2(theme::SP_SM, 0.0));
    ui.painter()
        .rect_filled(ground, egui::CornerRadius::ZERO, ui.visuals().panel_fill);
    ui.painter().galley(text.min, galley, theme::INK_FAINT);
    let response = ui.interact(
        ground,
        ui.id().with(("branch separator", name)),
        egui::Sense::hover(),
    );
    let enabled = ui.is_enabled();
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, enabled, name));
    ui.add_space(theme::SP_XL);
}

/// The divider between two gear criteria.
///
/// [`theme::HAIRLINE`] and not the undimmed rule, per [`theme::rule`]'s own
/// split: these are rows of ONE block — everything below `— or gear —` is the
/// gear branch of [`Filter::matches`] — and a full-strength rule between them
/// would read as the same boundary that separates the branches.
///
/// **Asymmetric, and that is the fix rather than the style.** With the same gap
/// either side, a block's small-caps header sat as close to the rule above it as
/// its own chips sat below — so the header read as a caption on the block it
/// followed instead of a title for the one it opens. The air under the rule is
/// what attaches it downwards.
fn gear_rule(ui: &mut egui::Ui) {
    ui.add_space(theme::SP_SM);
    theme::rule(ui, theme::HAIRLINE);
    ui.add_space(theme::SP_LG);
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
/// above — the shape every criterion still entered as a number has.
fn optional_field<T: egui::emath::Numeric>(ui: &mut egui::Ui, value: &mut T) -> egui::Response {
    bounded_field(ui, value, T::from_f64(1.0)..=T::MAX)
}

/// The same field over an explicit range. No caller passes a narrower one since
/// the grade floor left the window — a criterion the game defines on a closed
/// set is a [`hunt::segmented`] row now, which cannot express a value off it at
/// all — so this exists for the pair below, which every field wants.
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
    // bare labels. Hunt's substat floor wears the same strip.
    theme::segmented_strip(ui, |ui| {
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
    /// not a numeric test, so an epsilon would make a real edit invisible. What
    /// it cannot survive is a non-finite `min`: `NaN != NaN` lights Apply
    /// forever, and [`EditorState::mark_applied`] cannot put it out, because the
    /// twin it re-seeds is a clone of the value that fails to equal itself.
    ///
    /// The loader is no help there — TOML 1.0 has special floats and
    /// `SubstatReq.min` is a plain `f64`, so `min = nan` in `config.toml` loads
    /// and arrives here. The guarantee is the render's alone: [`substat_chips`]
    /// snaps every non-finite threshold back before this reads them, over the
    /// whole list rather than per chip, so a requirement no chip is drawn for is
    /// covered too.
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
        stocked_editor_with(named_filter())
    }

    /// The same, over a filter the config file is taken to have carried — so
    /// `applied_filter` is seeded from it too, which is the shape a startup
    /// draft has and the only one where "Apply is lit for no edit" means
    /// anything.
    fn stocked_editor_with(filter: Filter) -> EditorState {
        // No icons: chips with no picture behind them are what every test in
        // this file already reads.
        stocked_editor_from(filter, std::collections::HashMap::new())
    }

    /// The stocked editor a `catalog` message carrying pictures leaves behind:
    /// one set has an icon, the other has none, so a single fixture holds both
    /// branches of the set chip.
    fn stocked_editor_with_icons() -> EditorState {
        stocked_editor_from(
            named_filter(),
            std::collections::HashMap::from([(
                "set_speed".to_owned(),
                crate::ui::icons::red_dot_base64(),
            )]),
        )
    }

    fn stocked_editor_from(
        filter: Filter,
        icons: std::collections::HashMap<String, String>,
    ) -> EditorState {
        let mut editor = EditorState::new(
            filter,
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
            // One of each family, because the threshold stepper reads them
            // differently: `att_rate` is sent as a fraction, `speed` whole.
            substats: vec![
                entry("speed", "Speed"),
                percent_entry("att_rate", "Attack(%)"),
            ],
            slots: vec![entry("helm", "Helmet"), entry("boot", "Boots")],
            icons,
        });
        editor.sync_vocabulary(&cell);
        editor
    }

    /// The rule the Hunt body edits, created on demand.
    ///
    /// A test writing a gear criterion is a test about the ONE rule the window
    /// draws, and `with_rule` would otherwise take it back out before the
    /// assertion — a fresh rule is empty until something restricts.
    fn rule_of(editor: &mut EditorState) -> &mut GearRule {
        if editor.filter.gear.is_empty() {
            editor.filter.gear.push(GearRule::default());
        }
        &mut editor.filter.gear[0]
    }

    fn entry(id: &str, label: &str) -> VocabularyEntry {
        VocabularyEntry {
            id: id.to_owned(),
            label: label.to_owned(),
            percent: false,
        }
    }

    /// A substat the server flagged percent-bearing — the wire sends
    /// `att_rate: 0.03` for 3%.
    fn percent_entry(id: &str, label: &str) -> VocabularyEntry {
        VocabularyEntry {
            percent: true,
            ..entry(id, label)
        }
    }

    /// Tick `≥` on the one required substat the editor holds, and answer with
    /// the threshold that armed. One `≥` on the surface by construction: the
    /// stepper unfolds beside the chip it belongs to and nowhere else, which
    /// `only_the_required_chip_unfolds_a_threshold` pins.
    fn arm_threshold(editor: &mut EditorState, name: &str) -> f64 {
        rule_of(editor).required_substats = vec![SubstatReq {
            name: name.to_owned(),
            min: None,
        }];
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, editor);
        });
        harness.get_by_label("≥").click();
        harness.run();
        drop(harness);
        editor.filter.only_rule().required_substats[0]
            .min
            .expect("arming the threshold seeds it")
    }

    /// Draw Setup once, without committing anything.
    fn draw_setup(editor: &mut EditorState) -> Harness<'_> {
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, editor);
        });
        harness.run();
        harness
    }

    /// The degradation contract, in one render: with no `catalog` message the
    /// three wire-fed criteria fall back and the two constant-fed ones draw
    /// anyway.
    ///
    /// The checkbox rows only exist once the server named something. Until then
    /// the sets and gear-slot lists keep their field, because the ids in a
    /// player's config must stay enterable against a server with no Catalog.
    ///
    /// Required substats are the exception, and it is a deliberate one: a chip
    /// row over an empty vocabulary draws nothing, so that criterion is
    /// unreachable from the window until a `catalog` message lands. What backs
    /// it is that a config-set requirement still filters and is still counted in
    /// the folded Hunt bar — as `1 substat`, see `hunt_summary`: a tally that
    /// says a requirement is there and never which. That is enough to keep the
    /// bar from reading "nothing selected" over a hunt that restricts, and it is
    /// all it is: naming the id is the offered/unoffered rows' job, and they
    /// need the vocabulary this case does not have.
    ///
    /// The tokens and the rarities used to be the same story and are not any
    /// more: their words are `hunt::HUNT_TOKENS` and `theme::RARITIES`, so both
    /// controls are built from this end and neither has a state it cannot be
    /// drawn in. That is the simplification, and this is where it is stated.
    #[test]
    fn the_lists_fall_back_to_free_text_with_no_vocabulary() {
        let mut editor = EditorState::new(
            named_filter(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        let harness = draw_setup(&mut editor);
        // One "add" button per free-text list: sets and gear slots.
        assert_eq!(
            harness.get_all_by_label("add").count(),
            2,
            "every free-text list should offer its field"
        );
        assert_eq!(harness.query_all_by_label("Speed").count(), 0);
        // While the two the relay spells itself are on screen regardless.
        for rarity in ["Heroic", "Epic"] {
            assert_eq!(harness.get_all_by_label(rarity).count(), 1);
        }
        assert_eq!(harness.get_all_by_label("Covenant Bookmarks").count(), 1);
    }

    /// With a full vocabulary every criterion becomes a tick, `names` included:
    /// it is an open field, but the tokens the shop sells are a closed list of
    /// three the relay spells itself.
    #[test]
    fn a_vocabulary_turns_the_enumerable_lists_into_choices() {
        let mut editor = stocked_editor();
        let harness = draw_setup(&mut editor);
        assert_eq!(
            harness.query_all_by_label("add").count(),
            0,
            "no criterion is left asking for a typed id"
        );
        // One box per offered value, substats and tokens included: a
        // requirement is a tick now, not an add button.
        for label in [
            "Speed Set",
            "Critical Set",
            "Helmet",
            "Boots",
            "Speed",
            "Attack(%)",
            "Covenant Bookmarks",
        ] {
            assert_eq!(
                harness.get_all_by_label(label).count(),
                1,
                "{label} should have a checkbox"
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
        assert_eq!(editor.filter.only_rule().sets, vec!["set_speed".to_owned()]);
    }

    /// With a texture the set chip is its icon alone — 22 labelled chips wrap
    /// to eight rows at the window's fixed 440px, icons alone to three. The
    /// name stays the accessible name, so the control is still findable and a
    /// screen reader still announces it.
    #[test]
    fn a_set_with_an_icon_is_still_named() {
        let mut editor = stocked_editor_with_icons();
        let harness = draw_setup(&mut editor);
        assert_eq!(harness.query_all_by_label("Speed Set").count(), 1);
    }

    /// Clicking it stores the id, exactly as the text chip does — the icon is
    /// presentation, never what the filter matches on.
    #[test]
    fn ticking_an_icon_chip_stores_the_internal_id() {
        let mut editor = stocked_editor_with_icons();
        rule_of(&mut editor).sets.clear();
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.get_by_label("Speed Set").click();
        harness.run();
        drop(harness);
        assert_eq!(editor.filter.only_rule().sets, vec!["set_speed".to_owned()]);
    }

    /// A set whose icon did not arrive keeps its text chip rather than becoming
    /// an unclickable gap. The icons are decoration; the vocabulary is not.
    #[test]
    fn a_set_without_an_icon_keeps_its_text_chip() {
        let mut editor = stocked_editor();
        let harness = draw_setup(&mut editor);
        assert_eq!(harness.query_all_by_label("Speed Set").count(), 1);
    }

    /// The three above hold before and after the icon branch exists — a text
    /// chip is named, clickable and drawn too, which is the point of them. This
    /// is the one that separates the two: the whole reason the branch exists is
    /// width, and the window cannot be widened to absorb a chip that grew.
    #[test]
    fn an_icon_chip_is_narrower_than_the_name_it_replaces() {
        let mut with = stocked_editor_with_icons();
        let mut without = stocked_editor();
        let icon = chip_width(&draw_setup(&mut with), "Speed Set");
        let text = chip_width(&draw_setup(&mut without), "Speed Set");
        assert!(
            icon < text,
            "the icon chip should be narrower than its label: {icon} vs {text}"
        );
        // Squarish, not merely smaller: a chip laid out around a 32px picture
        // is what makes 22 of them wrap to three rows at `WINDOW_WIDTH`.
        assert!(icon < 56.0, "the icon chip should be chip-sized: {icon}");
    }

    /// The gear sets a live `catalog` message carries, at their real lengths.
    const LIVE_SETS: [&str; 24] = [
        "Attack Set",
        "Health Set",
        "Defense Set",
        "Critical Set",
        "Hit Set",
        "Resist Set",
        "Speed Set",
        "Destruction Set",
        "Lifesteal Set",
        "Counter Set",
        "Unity Set",
        "Immunity Set",
        "Rage Set",
        "Revenge Set",
        "Injury Set",
        "Penetration Set",
        "Protection Set",
        "Torrent Set",
        "Reversal Set",
        "Riposte Set",
        "Warfare Set",
        "Pursuit Set",
        "Scar Set",
        "Slaughter Set",
    ];

    /// The live set list as a vocabulary, given the field to put it in.
    ///
    /// No icons, which is the shape the window has before the server has
    /// published a picture for any set — so every chip takes `choice_list`'s
    /// text-checkbox branch, the one the reported screenshot was drawn from.
    fn chip_row_editor(field: impl Fn(&mut FilterVocabulary, Vec<VocabularyEntry>)) -> EditorState {
        let mut editor = EditorState::new(
            Filter::default(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        let mut vocabulary = FilterVocabulary::default();
        field(
            &mut vocabulary,
            LIVE_SETS
                .iter()
                .map(|label| entry(&label.to_lowercase().replace(' ', "_"), label))
                .collect(),
        );
        let cell = VocabularyCell::new();
        cell.set(vocabulary);
        editor.sync_vocabulary(&cell);
        editor
    }

    /// The labels a chip row laid out past the window's right edge.
    fn overflowing_chips(harness: &Harness<'_>) -> Vec<String> {
        LIVE_SETS
            .iter()
            .map(|label| (label, chip_right_edge(harness, label)))
            .filter(|(_, right)| *right > f64::from(crate::ui::WINDOW_WIDTH))
            .map(|(label, right)| format!("{label} ends at {right:.0}"))
            .collect()
    }

    /// The whole set list has to fit the window's fixed width, and the only way
    /// twenty-four chips do that is by wrapping.
    ///
    /// The assertion is the RIGHT EDGE of every chip and deliberately not the
    /// row's height, which the bug it pins would satisfy: laid out on one line,
    /// the fifth chip got the few pixels left before the edge and broke "Hit
    /// Set" one letter per line, so the broken row measured 225px tall — MORE
    /// than the six wrapped rows it should have been, and a "several lines high"
    /// test would have passed on it. Measured before the fix, at
    /// `WINDOW_WIDTH` = 440: chips ran unbroken to x=1070.
    #[test]
    fn every_set_chip_stays_inside_the_window() {
        let mut editor = chip_row_editor(|vocabulary, sets| vocabulary.sets = sets);
        let overflowing = overflowing_chips(&draw_setup(&mut editor));
        assert!(
            overflowing.is_empty(),
            "the sets row laid out past the window instead of wrapping: {overflowing:?}"
        );
    }

    /// `substat_chips` is the second reader of that layout and carried the same
    /// defect, which is why the fix is not a one-liner in `choice_list`.
    ///
    /// The list is the SETS one because it is the only chip census taken off a
    /// live catalog: what this asks is whether a wrapped chip row wraps, and
    /// the answer must not depend on which criterion is drawing it. On the real
    /// eleven substats the row was less spectacular and still wrong — measured
    /// at x=608 against the window's 440.
    #[test]
    fn every_substat_chip_stays_inside_the_window() {
        let mut editor = chip_row_editor(|vocabulary, sets| vocabulary.substats = sets);
        let overflowing = overflowing_chips(&draw_setup(&mut editor));
        assert!(
            overflowing.is_empty(),
            "the substats row laid out past the window instead of wrapping: {overflowing:?}"
        );
    }

    /// Two criteria offering the very same value keep two controls, which is
    /// what the id salt on each chip row is for — one shared id would collapse
    /// them into a single node, leaving a criterion invisible and untickable.
    ///
    /// It is stated as behaviour rather than as ids because the guard moved:
    /// the salt used to sit on each chip, where it cost the wrap above.
    #[test]
    fn two_rows_offering_the_same_value_keep_their_own_chips() {
        let mut editor = EditorState::new(
            Filter::default(),
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        let cell = VocabularyCell::new();
        // Same id AND same label on both rows: the worst case for a chip keyed
        // by what it offers.
        cell.set(FilterVocabulary {
            sets: vec![entry("speed", "Speed")],
            slots: vec![entry("speed", "Speed")],
            ..FilterVocabulary::default()
        });
        editor.sync_vocabulary(&cell);
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.run();
        assert_eq!(
            harness.get_all_by_label("Speed").count(),
            2,
            "each row should own its chip"
        );
        // And they are two controls and not one drawn twice: ticking the first
        // writes the set criterion and leaves the slot one alone.
        harness
            .get_all_by_label("Speed")
            .next()
            .expect("the sets chip")
            .click();
        harness.run();
        drop(harness);
        assert_eq!(editor.filter.only_rule().sets, vec!["speed".to_owned()]);
        assert!(editor.filter.only_rule().slots.is_empty());
    }

    /// A gear block's header belongs to the block it OPENS, not to the one it
    /// closes: the rule above it plus that rule's air has to outweigh the gap
    /// down to its own first control.
    ///
    /// Measured in the theme's own rungs rather than in pixels, so it states the
    /// rule instead of a screenshot — and it goes red on the layout this
    /// replaced, where the same `SP_SM` sat either side of the rule and the
    /// header read as a caption on the chips above it.
    #[test]
    fn a_gear_block_header_belongs_to_the_block_below_it() {
        let mut editor = stocked_editor();
        let harness = draw_setup(&mut editor);
        // The gear-slot row above, the substats header, its own first chip.
        let above = node_bounds(&harness, "Boots");
        let header = node_bounds(&harness, "REQUIRED SUBSTATS");
        let below = node_bounds(&harness, "Speed");
        let over = header.y0 - above.y1;
        let under = below.y0 - header.y1;
        assert!(
            over >= f64::from(theme::SP_SM + theme::SP_LG),
            "the boundary above the header is thinner than its own rungs: {over}"
        );
        assert!(
            over > under,
            "the header sits closer to the block above than to its own: {over} vs {under}"
        );
    }

    /// The accessibility box of the node a label names.
    fn node_bounds(harness: &Harness<'_>, label: &str) -> egui::accesskit::Rect {
        harness
            .get_by_label(label)
            .accesskit_node()
            .bounding_box()
            .expect("egui gives every node its bounds")
    }

    /// The right-hand edge of the control a label names.
    fn chip_right_edge(harness: &Harness<'_>, label: &str) -> f64 {
        harness
            .get_by_label(label)
            .accesskit_node()
            .bounding_box()
            .expect("egui gives every node its bounds")
            .x1
    }

    /// The on-screen width of the control a label names.
    fn chip_width(harness: &Harness<'_>, label: &str) -> f64 {
        harness
            .get_by_label(label)
            .accesskit_node()
            .bounding_box()
            .expect("egui gives every node its bounds")
            .width()
    }

    /// A second piece is a second rule, and each keeps its own criteria.
    ///
    /// The whole point of the list: flat, ticking a set while boots were
    /// selected added the set to the SAME conjunction, and the hunt became
    /// "a boot that is also of that set" rather than two pieces.
    #[test]
    fn a_second_piece_is_a_rule_of_its_own() {
        let mut editor = stocked_editor();
        rule_of(&mut editor).slots = vec!["boot".to_owned()];
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.get_by_label("+").click();
        harness.run();
        harness.get_by_label("Speed Set").click();
        harness.run();
        drop(harness);
        assert_eq!(editor.filter.gear.len(), 2);
        assert_eq!(editor.filter.gear[0].slots, vec!["boot".to_owned()]);
        assert!(
            editor.filter.gear[0].sets.is_empty(),
            "the first is untouched"
        );
        assert_eq!(editor.filter.gear[1].sets, vec!["set_speed".to_owned()]);
    }

    /// Opening a piece and setting nothing writes no rule — and, because it
    /// writes none, leaves Apply dark.
    ///
    /// An empty rule in the draft would differ from the applied twin forever:
    /// `mark_applied` re-seeds that twin from what the session took, and what
    /// it took would never carry the empty rule back.
    #[test]
    fn opening_a_piece_and_setting_nothing_writes_no_rule() {
        let mut editor = stocked_editor();
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.get_by_label("+").click();
        harness.run();
        drop(harness);
        assert!(editor.filter.gear.is_empty());
        assert!(run_setup(&mut editor).is_empty(), "and Apply stays dark");
    }

    /// A piece can be taken back out, which is what keeps a rule from a config
    /// file reachable: it is drawn, and it can be dropped.
    #[test]
    fn removing_a_piece_drops_its_rule() {
        let mut editor = stocked_editor();
        rule_of(&mut editor).slots = vec!["boot".to_owned()];
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.get_by_label("remove").click();
        harness.run();
        drop(harness);
        assert!(editor.filter.gear.is_empty());
    }

    /// And unticking takes it back out, rather than leaving a box that lies.
    #[test]
    fn unticking_a_slot_drops_its_id() {
        let mut editor = stocked_editor();
        rule_of(&mut editor).slots = vec!["helm".to_owned()];
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.get_by_label("Helmet").click();
        harness.run();
        drop(harness);
        // The rule went with its last criterion: a card that constrains
        // nothing is not a piece being hunted.
        assert!(editor.filter.gear.is_empty());
    }

    /// A token card writes the wire NAME, which is what `Filter::names`
    /// compares against — never the label the player reads.
    #[test]
    fn a_token_card_writes_the_wire_name() {
        let mut editor = stocked_editor();
        // The seeded filter already hunts that token, so its card arrives
        // ticked; clearing is what makes the click below an add.
        editor.filter.names.clear();
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.get_by_label("Covenant Bookmarks").click();
        harness.run();
        drop(harness);
        assert_eq!(editor.filter.names, vec!["ticketrare_name".to_owned()]);
    }

    /// And unticking takes it back out, rather than leaving a card that lies.
    #[test]
    fn unticking_a_token_card_drops_its_name() {
        let mut editor = stocked_editor();
        editor.filter.names = vec!["ticketrare_name".to_owned()];
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.get_by_label("Covenant Bookmarks").click();
        harness.run();
        drop(harness);
        assert!(editor.filter.names.is_empty());
    }

    /// The price rides on the card, because it is what decides whether a hit is
    /// worth stopping for — a bare chip has nowhere to put one.
    #[test]
    fn a_token_card_states_its_price() {
        let mut editor = stocked_editor();
        let harness = draw_setup(&mut editor);
        harness.get_by_label("184,000 gold");
    }

    /// A name no card offers keeps a row of its own, exactly as an unoffered set
    /// id does. Nothing constrains `names` to the shop's three tokens — a
    /// player's `config.toml` can name anything, and the cards are the whole of
    /// what that criterion offers, so a name with none is a criterion that
    /// filters while being invisible and unremovable.
    #[test]
    fn an_unoffered_name_stays_visible_and_removable() {
        let mut editor = stocked_editor();
        // Not one of the shop's three: those all have a card now.
        editor.filter.names = vec!["ecq4h_name".to_owned()];
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.run();
        assert_eq!(harness.get_all_by_label("ecq4h_name").count(), 1);
        // The only cross on the surface: every gear list is a choice row here,
        // and each offers what it holds.
        harness.get_by_label("✕").click();
        harness.run();
        drop(harness);
        assert!(editor.filter.names.is_empty());
    }

    /// The screen states the OR where it acts: the name criteria and the gear
    /// criteria are two blocks, not one list.
    ///
    /// The word is painted on a rule now rather than laid out as a label, so
    /// this also stands for the whole of what a screen reader gets from that
    /// boundary — painted text publishes nothing, and the name is restated by
    /// hand. It has to keep its old spelling, dashes included, or the node moves
    /// under everyone who looks for it.
    #[test]
    fn the_hunt_body_separates_names_from_gear() {
        let mut editor = stocked_editor();
        let harness = draw_setup(&mut editor);
        assert_eq!(harness.query_all_by_label("— or gear —").count(), 1);
        // And it sits on the rule it interrupts: inside the window, straddling
        // its middle. Centring on the content box instead of the painted line
        // is off by the side margin, which is invisible in a green test and
        // obvious on screen.
        let word = harness
            .get_by_label("— or gear —")
            .accesskit_node()
            .bounding_box()
            .expect("egui gives every node its bounds");
        let middle = f64::from(crate::ui::WINDOW_WIDTH) / 2.0;
        assert!(
            word.x0 > 0.0 && word.x1 < f64::from(crate::ui::WINDOW_WIDTH),
            "the word ran outside the window: {word:?}"
        );
        assert!(
            word.x0 < middle && word.x1 > middle,
            "the word should straddle the middle of the rule: {word:?}"
        );
    }

    /// `kinds` leaves the window: picking a name says tokens, picking gear says
    /// gear, and a third control saying the same is a way to contradict
    /// yourself. It stays loadable from config.toml.
    #[test]
    fn the_kind_checkboxes_are_gone_from_the_window() {
        let mut editor = stocked_editor();
        let harness = draw_setup(&mut editor);
        for label in ["Equipment", "Hero", "Token"] {
            assert_eq!(harness.query_all_by_label(label).count(), 0);
        }
    }

    /// The threshold belongs to the chip that carries it: one required substat
    /// unfolds one stepper, not a column of them down every offered chip.
    #[test]
    fn only_the_required_chip_unfolds_a_threshold() {
        let mut editor = stocked_editor();
        rule_of(&mut editor).required_substats = vec![SubstatReq {
            name: "speed".to_owned(),
            min: None,
        }];
        let harness = draw_setup(&mut editor);
        // `query_all_*`, not `get_all_*`: the getters panic on no match, and a
        // count is what separates "beside its own chip" from "on all of them".
        assert_eq!(harness.query_all_by_label("≥").count(), 1);
    }

    /// A percent-bearing substat is read in whole percent and STORED as the
    /// fraction the wire and `Filter::matches` both speak.
    ///
    /// The seed used to be `1.0` for every family, which on this one is one
    /// hundred percent — eight times the largest roll the game produces — so
    /// ticking `≥` on Attack(%) armed a floor nothing could satisfy and the hunt
    /// went quiet.
    #[test]
    fn arming_a_percent_threshold_stores_a_fraction() {
        let mut editor = stocked_editor();
        let min = arm_threshold(&mut editor, "att_rate");
        assert!(
            min < 1.0,
            "a percent threshold is stored as the fraction, not as the number on screen: {min}"
        );
    }

    /// The other family is its own unit and passes through untouched — the flag
    /// decides, so the whole path must not pick up a hundredth.
    #[test]
    fn arming_a_whole_threshold_stores_the_whole_number() {
        let mut editor = stocked_editor();
        let min = arm_threshold(&mut editor, "speed");
        assert!(
            (min - 1.0).abs() < f64::EPSILON,
            "a whole substat keeps the number it shows: {min}"
        );
    }

    /// The seed a player is left with has to be one the game can actually
    /// produce, or the first thing the control does is ask to be dragged away
    /// from.
    ///
    /// Both are the lowest roll of their own substat, measured on the wire —
    /// `hunt::SUBSTAT_RANGE` carries the sample and the numbers. Pinned here as
    /// well because this is the whole path: the click, the chip, the lookup and
    /// the write into the filter, where `hunt`'s own test walks the table alone.
    #[test]
    fn both_seeds_are_inside_the_range_the_game_rolls() {
        let mut editor = stocked_editor();
        let percent = arm_threshold(&mut editor, "att_rate");
        assert!(
            (percent - 0.03).abs() < f64::EPSILON,
            "the lowest attack% the shop sells is 3%: {percent}"
        );
        let whole = arm_threshold(&mut editor, "speed");
        assert!(
            (whole - 1.0).abs() < f64::EPSILON,
            "the lowest speed roll is 1: {whole}"
        );
    }

    /// The two halves of one threshold, side by side: the field reads `3` and
    /// the filter stores `0.03`.
    ///
    /// It is also what says the unit out loud. Nothing else can — a `DragValue`
    /// publishes its value and no label at all — so without the suffix `3` and
    /// `3%` are the same three pixels over numbers a hundredfold apart.
    #[test]
    fn a_percent_threshold_reads_in_percent_and_stores_the_fraction() {
        let mut editor = stocked_editor();
        let min = arm_threshold(&mut editor, "att_rate");
        let harness = draw_setup(&mut editor);
        // By role: a `DragValue` has no accessible label to find it by, which
        // is the same sentence as the one above.
        let field = harness
            .get_by_role(egui::accesskit::Role::SpinButton)
            .accesskit_node();
        assert_eq!(field.numeric_value(), Some(3.0), "what the player reads");
        assert!(
            field.value().is_some_and(|shown| shown.ends_with('%')),
            "and the unit it is read in: {:?}",
            field.value()
        );
        assert!((min - 0.03).abs() < f64::EPSILON, "what is stored: {min}");
    }

    /// A substat chip states its name; ticked it unfolds a threshold in place,
    /// so the numeric control exists only where it applies.
    #[test]
    fn ticking_a_substat_chip_adds_a_requirement_with_no_threshold() {
        let mut editor = stocked_editor();
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.get_by_label("Speed").click();
        harness.run();
        drop(harness);
        assert_eq!(editor.filter.only_rule().required_substats.len(), 1);
        assert_eq!(editor.filter.only_rule().required_substats[0].name, "speed");
        assert_eq!(editor.filter.only_rule().required_substats[0].min, None);
    }

    /// The Hunt body wires the mode strip to the filter's own field, and a
    /// requirement already held keeps its threshold across the switch.
    ///
    /// `substat_chips` is where the strip is drawn and `hunt::tests` is where it
    /// is exercised; what this adds is the wiring — the control reaches
    /// `filter.substat_match` and not a draft of its own, which is the failure a
    /// unit test of the widget cannot see.
    #[test]
    fn the_substat_mode_reaches_the_filter_from_the_hunt_body() {
        let mut editor = stocked_editor();
        rule_of(&mut editor).required_substats = vec![SubstatReq {
            name: "speed".to_owned(),
            min: Some(8.0),
        }];
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.get_by_label("any").click();
        harness.run();
        drop(harness);
        assert_eq!(
            editor.filter.only_rule().substat_match,
            crate::domain::filter::SubstatMatch::Any
        );
        assert_eq!(
            editor.filter.only_rule().required_substats[0].min,
            Some(8.0),
            "switching how requirements combine must not touch one"
        );
    }

    /// And unticking removes it, rather than leaving a chip that lies.
    #[test]
    fn unticking_a_substat_chip_removes_the_requirement() {
        let mut editor = stocked_editor();
        rule_of(&mut editor).required_substats = vec![SubstatReq {
            name: "speed".to_owned(),
            min: None,
        }];
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.get_by_label("Speed").click();
        harness.run();
        drop(harness);
        assert!(editor.filter.gear.is_empty(), "and the rule with it");
    }

    /// The quality control is a rarity ladder writing `min_grade`, and it
    /// writes the ORDINAL the server sent beside the word — never the word.
    #[test]
    fn the_rarity_ladder_writes_the_grade_behind_its_label() {
        let mut editor = stocked_editor();
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.get_by_label("Heroic").click();
        harness.run();
        drop(harness);
        assert_eq!(editor.filter.only_rule().min_grade, Some(4));
        // And the criterion it replaced is left exactly as the file had it.
        assert_eq!(editor.filter.only_rule().min_substats, None);
    }

    /// Epic is the whole reason the axis moved: it carries four substats like
    /// Heroic, so the ladder this replaced could not ask for it at all.
    #[test]
    fn the_ladder_can_ask_for_epic_alone() {
        let mut editor = stocked_editor();
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.get_by_label("Epic").click();
        harness.run();
        drop(harness);
        assert_eq!(editor.filter.only_rule().min_grade, Some(5));
    }

    /// Clicking the active segment clears the floor — a criterion you can set
    /// and cannot unset is a trap.
    #[test]
    fn clicking_the_active_rarity_clears_the_floor() {
        let mut editor = stocked_editor();
        rule_of(&mut editor).min_grade = Some(4);
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.get_by_label("Heroic").click();
        harness.run();
        drop(harness);
        assert!(editor.filter.gear.is_empty(), "and the rule with it");
    }

    /// Every rung is a NAMED node, which is what a `ComboBox` here would not be
    /// — and the two have to be findable one by one, since each writes a
    /// different floor.
    #[test]
    fn every_rarity_is_named_and_stays_inside_the_window() {
        let mut editor = stocked_editor();
        let harness = draw_setup(&mut editor);
        for label in ["Heroic", "Epic"] {
            assert_eq!(
                harness.get_all_by_label(label).count(),
                1,
                "{label} should be one named cell"
            );
            // The window is pinned at `WINDOW_WIDTH` and cannot be widened, so
            // a ladder that overflows is a ladder to make narrower.
            let right = chip_right_edge(&harness, label);
            assert!(
                right <= f64::from(crate::ui::WINDOW_WIDTH),
                "{label} ends at {right:.0}, past the window"
            );
        }
    }

    /// The two rungs nobody hunts have no cell at all.
    ///
    /// Stated on the RENDER rather than on the table, because the table still
    /// names them — `hunt::grade_label` answers for a `config.toml` floor of 2
    /// or 3, and `a_config_set_grade_floor_still_names_itself_in_the_folded_bar`
    /// is the other half of that. What must be gone is the control: a paid
    /// re-roll spent on a blue piece is a re-roll wasted, so offering that floor
    /// invites a hunt the player did not mean.
    #[test]
    fn the_ladder_offers_no_rung_below_heroic() {
        let mut editor = stocked_editor();
        let harness = draw_setup(&mut editor);
        for gone in ["Good", "Rare"] {
            assert_eq!(
                harness.query_all_by_label(gone).count(),
                0,
                "{gone} should have no cell on the ladder"
            );
        }
    }

    /// A `min_substats` a config file already carries survives a render: the
    /// window has no control for it any more, so nothing in the window may drop
    /// it either — the same rule `kinds` follows.
    #[test]
    fn a_config_set_substat_floor_survives_a_window_with_no_control_for_it() {
        let mut editor = stocked_editor_with(Filter {
            gear: vec![GearRule {
                min_substats: Some(3),
                ..GearRule::default()
            }],
            ..named_filter()
        });
        let harness = draw_setup(&mut editor);
        // Nothing offers the old ladder's segments any more.
        for gone in ["2+", "3+", "4"] {
            assert_eq!(harness.query_all_by_label(gone).count(), 0);
        }
        drop(harness);
        assert_eq!(editor.filter.only_rule().min_substats, Some(3));
        // And Apply stays dark: a render that changed nothing is not an edit.
        assert!(run_setup(&mut editor).is_empty());
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
        rule_of(&mut editor).sets = vec!["set_retired".to_owned()];
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.run();
        assert_eq!(harness.get_all_by_label("set_retired").count(), 1);
        harness.get_by_label("✕").click();
        harness.run();
        drop(harness);
        assert!(editor.filter.gear.is_empty(), "and the rule with it");
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
    ///
    /// The WIDTH is the opposite case and is pinned tight, at the window's own
    /// [`crate::ui::WINDOW_WIDTH`] rather than a round 480: `main.rs` sets that
    /// as both the minimum and the maximum inner size, so the player's frame
    /// cannot be wider. A control laid out with 40px to spare would pass here
    /// and wrap on their screen. Read off the constant so the two cannot drift.
    fn setup_harness<'a>(app: impl FnMut(&mut egui::Ui) + 'a) -> Harness<'a> {
        Harness::builder()
            .with_size(egui::vec2(crate::ui::WINDOW_WIDTH, 2000.0))
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
        // rendering must snap it back.
        //
        // Over a stocked editor, because this is the offered half: the substat
        // is one the server named, so it is drawn as a chip with a stepper.
        let mut editor = stocked_editor();
        rule_of(&mut editor).required_substats = vec![SubstatReq {
            name: "speed".to_owned(),
            min: Some(f64::NAN),
        }];
        let harness = draw_setup(&mut editor);
        drop(harness);
        assert_eq!(
            editor.filter.only_rule().required_substats[0].min,
            Some(1.0)
        );
        assert_eq!(editor.filter, editor.filter.clone());
    }

    /// A required substat the vocabulary cannot name keeps a row of its own,
    /// exactly as an unoffered set id or token name does. `substat_chips` walks
    /// what the server OFFERED, so before this a requirement written in
    /// `config.toml` against an id the catalog has since dropped filtered
    /// fail-closed — silencing the whole gear branch — while drawing nothing in
    /// the open Hunt body and offering no way out of the window.
    #[test]
    fn an_unoffered_requirement_stays_visible_and_removable() {
        let mut editor = stocked_editor();
        // No names: the click below has to be the only cross on the surface,
        // and every other list here offers what it holds.
        editor.filter.names.clear();
        rule_of(&mut editor).required_substats = vec![SubstatReq {
            name: "speed_retired".to_owned(),
            min: None,
        }];
        let mut harness = setup_harness(|ui| {
            let _ = edit_setup(ui, &mut editor);
        });
        harness.run();
        assert_eq!(harness.get_all_by_label("speed_retired").count(), 1);
        harness.get_by_label("✕").click();
        harness.run();
        drop(harness);
        assert!(editor.filter.gear.is_empty(), "and the rule with it");
    }

    /// The consequence that outlives the invisibility, and the reason the
    /// snap-back had to leave the chip.
    ///
    /// `min = nan` loads: TOML 1.0 has special floats and `SubstatReq.min` is a
    /// plain `f64`. On an id no chip is drawn for, the chip's snap-back never
    /// ran, so [`Dirty::of`]'s bit-exact compare answered `NaN != NaN` — Apply
    /// lit with no edit, and [`EditorState::mark_applied`] could not clear it,
    /// because the twin it re-seeds is a clone of the value that fails to equal
    /// itself. One full commit cycle is what states that: Apply, deliver
    /// whatever it sent, and the button must be dark.
    #[test]
    fn a_non_finite_threshold_on_an_unoffered_requirement_cannot_jam_apply() {
        let mut editor = stocked_editor_with(Filter {
            gear: vec![GearRule {
                required_substats: vec![SubstatReq {
                    name: "unoffered_stat".to_owned(),
                    min: Some(f64::NAN),
                }],
                ..GearRule::default()
            }],
            ..named_filter()
        });
        let sent = run_setup(&mut editor);
        editor.mark_applied(&sent);
        assert!(
            run_setup(&mut editor).is_empty(),
            "a threshold nothing on screen can reach must not leave Apply lit for good"
        );
        assert_eq!(
            editor.filter.only_rule().required_substats[0].min,
            Some(1.0)
        );
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

    /// The folded Hunt bar names both criteria in the words their controls
    /// spell, and it does so with no catalog behind it — the state a config-set
    /// floor arrives in.
    ///
    /// A rarity floor restricts — `matches` drops every gradeless item — so a
    /// summary reading "nothing selected" over one would be the lie
    /// `a_restricting_filter_is_never_summarized_as_nothing_selected` exists to
    /// catch. It must read `Good+` and never `grade 2+`: the ordinal is the one
    /// number a player sees nowhere else in the game.
    #[test]
    fn a_config_set_grade_floor_still_names_itself_in_the_folded_bar() {
        let filter = Filter {
            gear: vec![GearRule {
                min_grade: Some(2),
                ..GearRule::default()
            }],
            ..named_filter()
        };
        let mut editor = EditorState::new(
            filter,
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        editor.hunt_open = false;
        let harness = draw_setup(&mut editor);
        harness.get_by_label("Hunt · Covenant Bookmarks, Good+");
        drop(harness);
        assert_eq!(editor.filter.only_rule().min_grade, Some(2));
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

    /// A `kinds` criterion the file already carries survives a render: nothing
    /// in the window edits it now, so nothing in the window may drop it either.
    /// The refusal of `kinds = ["unknown"]` moved back to where it always
    /// belonged — `hunt_kinds`, pinned by
    /// `a_kind_the_wire_would_tolerate_is_refused_in_a_config_file` in
    /// `domain::filter`, which runs with no GUI feature at all.
    #[test]
    fn a_config_set_kind_survives_a_window_with_no_control_for_it() {
        let filter = Filter {
            kinds: vec![ItemKind::Token],
            ..named_filter()
        };
        let mut editor = EditorState::new(
            filter,
            Limits::default(),
            Timings::default(),
            ClickMode::default(),
        );
        let harness = draw_setup(&mut editor);
        drop(harness);
        assert_eq!(editor.filter.kinds, vec![ItemKind::Token]);
        // And Apply stays dark, because a render that changed nothing is not an
        // edit — the draft would otherwise arrive at config.toml shorn of it.
        assert!(run_setup(&mut editor).is_empty());
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
        let hunt = harness.get_by_label("Hunt · Covenant Bookmarks").rect();
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
                edit_sections(ui, &mut editor, &SetIcons::default());
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

    /// The substat block, in the three states one row can hold at once: a
    /// requirement with a threshold, one without, and the values nobody asked
    /// for. The `≥` and its number ride INSIDE the requirement's own box, and a
    /// picture is the only thing that can say whether they read as one object.
    #[test]
    #[cfg(feature = "render-png")]
    #[ignore = "renders the substat block to a PNG for visual iteration; run with --ignored"]
    fn render_substat_block_png() {
        let mut reqs = vec![
            SubstatReq {
                name: "att_rate".to_owned(),
                min: Some(0.05),
            },
            SubstatReq {
                name: "speed".to_owned(),
                min: None,
            },
        ];
        let mut mode = crate::domain::filter::SubstatMatch::Any;
        let choices: Vec<VocabularyEntry> = [
            ("att", "Attack", false),
            ("att_rate", "Attack(%)", true),
            ("max_hp", "Health", false),
            ("speed", "Speed", false),
            ("cri", "Critical Hit Chance", true),
            ("cri_dmg", "Critical Hit Damage", true),
        ]
        .into_iter()
        .map(|(id, label, percent)| VocabularyEntry {
            id: id.to_owned(),
            label: label.to_owned(),
            percent,
        })
        .collect();
        let mut harness = Harness::builder()
            .with_size(egui::vec2(430.0, 160.0))
            .with_pixels_per_point(2.0)
            .wgpu()
            .build_ui(move |ui| {
                theme::apply(ui.ctx());
                let bg = ui.visuals().panel_fill;
                ui.painter().rect_filled(ui.ctx().content_rect(), 0.0, bg);
                ui.add_space(theme::SP_SM);
                substat_chips(ui, &mut reqs, &mut mode, &choices);
            });
        harness.run();
        let image = harness.render().expect("wgpu render");
        let path = std::env::var_os("ARKYVE_RENDER_DIR")
            .map_or_else(std::env::temp_dir, std::path::PathBuf::from)
            .join("substats.png");
        image.save(&path).expect("save png");
        eprintln!("rendered {}", path.display());
    }

    /// The piece strip over the top of one rule's criteria: two pieces hunted,
    /// the second on screen, the `+` waiting past it.
    #[test]
    #[cfg(feature = "render-png")]
    #[ignore = "renders the piece strip to a PNG for visual iteration; run with --ignored"]
    fn render_piece_strip_png() {
        let mut editor = stocked_editor_with(Filter {
            names: Vec::new(),
            gear: vec![
                GearRule {
                    slots: vec!["boot".to_owned()],
                    ..GearRule::default()
                },
                GearRule {
                    slots: vec!["helm".to_owned()],
                    min_grade: Some(5),
                    ..GearRule::default()
                },
            ],
            ..Filter::default()
        });
        editor.gear_index = 1;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(430.0, 200.0))
            .with_pixels_per_point(2.0)
            .wgpu()
            .build_ui(move |ui| {
                theme::apply(ui.ctx());
                let bg = ui.visuals().panel_fill;
                ui.painter().rect_filled(ui.ctx().content_rect(), 0.0, bg);
                ui.add_space(theme::SP_SM);
                piece_strip(ui, &mut editor.filter.gear, &mut editor.gear_index);
                with_rule(&mut editor.filter.gear, editor.gear_index, |rule| {
                    offered_list(
                        ui,
                        "gear slots",
                        &mut rule.slots,
                        &mut editor.slot_input,
                        &editor.vocabulary.slots,
                        &SetIcons::default(),
                    );
                    gear_rule(ui);
                    ui.label(theme::section("rarity"));
                    rarity_ladder(ui, &mut rule.min_grade);
                });
            });
        harness.run();
        let image = harness.render().expect("wgpu render");
        let path = std::env::var_os("ARKYVE_RENDER_DIR")
            .map_or_else(std::env::temp_dir, std::path::PathBuf::from)
            .join("pieces.png");
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
