//! The Setup surface: draft filter/limits/timings owned by the window, the
//! widgets that edit them, and the single Apply that commits the changed
//! drafts to the session. Hunt (what to buy) and Stop (when to quit) open on
//! arrival; Click timing is expert tuning and stays folded.

// A section file holds the parts that work on a `Filter` / `Limits` /
// `Timings` handed in by the caller; everything reaching into `EditorState`
// stays here.
mod hunt;
mod stop;
mod timing;
mod timing_meter;

use hunt::{grade_value, hunt_summary, optional_value, quick_add_names, string_list, substat_reqs};
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
    name_input: String,
    set_input: String,
    substat_input: String,
    hunt_open: bool,
    stop_open: bool,
    timing_open: bool,
    /// Whether the Custom segment is selected, revealing the per-action bars.
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
    /// took, so a draft counts as applied only once its command cleared the
    /// bounded queue. Read out of the command, never off the draft.
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

/// The whole Setup surface in one `ui` so a test can drive sections and Apply
/// at once; the live window mounts them as separate panels.
#[cfg(test)]
fn edit_setup(ui: &mut egui::Ui, editor: &mut EditorState) -> Vec<Command> {
    edit_sections(ui, editor);
    ui.add_space(theme::SP_XL);
    commit_row(ui, editor, true)
}

/// The three collapsible sections — the Setup tab's scrolling body.
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
#[must_use]
pub(super) fn commit_row(
    ui: &mut egui::Ui,
    editor: &mut EditorState,
    session_alive: bool,
) -> Vec<Command> {
    // Bit-exact on purpose, `min`'s `f64` included: this is change detection,
    // not a numeric test, so an epsilon would make a real edit invisible. It
    // cannot survive a non-finite `min` (`NaN != NaN` lights Apply forever),
    // which is why neither the loader nor `substat_reqs` admits one.
    let dirty_filter = editor.filter != editor.applied_filter;
    let dirty_limits = editor.limits != editor.applied_limits;
    let dirty_timings = editor.timings != editor.applied_timings;
    let dirty = dirty_filter || dirty_limits || dirty_timings;
    // Only a *changed* filter clears the arming bar, so an already-applied one
    // lets limit/timing edits through.
    let blocked = dirty_filter && editor.filter.is_unrestricted();

    let mut commands = Vec::new();
    ui.horizontal(|ui| {
        // The blocking reason wins the peek slot: it explains the dark button.
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

/// The commit bar's peek, e.g. `Hunt, Stop edited`. Labels mirror the section
/// titles so it points at the collapsible that changed.
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

    /// Drive `edit_setup` once, capturing whatever Apply committed. Only a
    /// non-empty commit is latched, so the final quiet frame can't wipe it.
    fn run_setup(editor: &mut EditorState) -> Vec<Command> {
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
        let mut editor = EditorState::new(Filter::default(), Limits::default(), Timings::default());
        editor.filter = named_filter();
        assert_eq!(
            run_setup(&mut editor),
            vec![Command::SetFilter(named_filter())]
        );
    }

    #[test]
    fn apply_leaves_the_draft_dirty_until_the_shell_confirms() {
        let mut editor = EditorState::new(Filter::default(), Limits::default(), Timings::default());
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
        // The draft is dirty but unrestricted: the loop would refuse it.
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
    fn a_config_set_grade_floor_can_be_seen_and_cleared() {
        // With no widget for it, a config-set floor was the one criterion
        // restricting the hunt and was invisible *and* uncorrectable.
        let filter = Filter {
            min_grade: Some(2),
            ..named_filter()
        };
        let mut editor = EditorState::new(filter, Limits::default(), Timings::default());
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
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
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
        // Do not re-add an "unknown" box: it wrote `kinds = ["unknown"]`, which
        // the next launch refused to load (see `hunt_body`).
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
    fn clicking_a_preset_keeps_a_config_set_delay_floor() {
        // The click that used to cost a setting: over a config-set floor, one
        // preset click replaced the draft with ranges starting at 0, which
        // Apply then wrote to disk with no undo.
        let floored = Timings {
            refreshed: DelayRange::try_new(200, 800).expect("a valid fixture range"),
            ..Timings::default()
        };
        let mut editor = EditorState::new(named_filter(), Limits::default(), floored);
        editor.timing_open = true;
        let mut harness = Harness::new_ui(|ui| {
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
        // A config-seeded 0 (halt at the first check) must survive rendering —
        // the old `DragValue` clamp rewrote it to 1.
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
        assert_eq!(run_setup(&mut editor), vec![Command::SetLimits(expected)]);
    }

    #[test]
    #[ignore = "renders the Setup sections to a PNG for visual iteration; run with --ignored"]
    fn render_stop_section_png() {
        let limits = Limits {
            max_refreshes: Some(49),
            max_spend: Some(Crystals::new(30)),
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
        // Set `ARKYVE_RENDER_DIR` to collect the frames somewhere durable.
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
