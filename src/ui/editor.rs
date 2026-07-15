//! The Setup surface: draft filter/limits/timings owned by the window, the
//! widgets that edit them, and the single Apply that commits the changed
//! drafts to the session. Laid out as three groups by the player's real
//! priority — Hunt (what to buy) and Stop (when to quit) always open, Click
//! timing (expert tuning) collapsed — under one primary action.

use eframe::egui;

use super::theme;
use crate::actuator::plan::{self, DelayRange, Timings};
use crate::app::Command;
use crate::domain::control::Limits;
use crate::domain::filter::{Filter, SubstatReq};
use crate::domain::shop::ItemKind;
use crate::render::kind_label;

/// Draft criteria owned by the window until Apply pushes them to the session;
/// seeded from the controller's live criteria (and the startup timings) at
/// startup. Each draft carries the last-applied copy beside it so Apply lights
/// up only on a real change and sends nothing that has not moved. Edits are
/// session-only — config.toml is never rewritten.
pub struct EditorState {
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
}

impl EditorState {
    pub fn new(filter: Filter, limits: Limits, timings: Timings) -> Self {
        Self {
            applied_filter: filter.clone(),
            applied_limits: limits.clone(),
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
        }
    }
}

/// The whole Setup surface: three journal-style collapsible sections over one
/// Apply. Returns the commands the player committed — one per draft that
/// changed, empty until Apply fires (or while it stays disabled).
pub fn edit_setup(ui: &mut egui::Ui, editor: &mut EditorState) -> Vec<Command> {
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
    ui.add_space(theme::SP_XL);
    commit_row(ui, editor)
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

/// One-line recap of the hunt draft for the folded Hunt bar: the labels of what
/// the loop would buy (tokens named via the haul headliners, then kinds, then a
/// count of the finer criteria), so folding hides the controls, not the intent.
fn hunt_summary(filter: &Filter) -> String {
    let mut parts: Vec<String> = Vec::new();
    for name in &filter.names {
        // Reuse the haul's wire→label map so a hunted token reads "Covenant",
        // not "ticketrare_name"; an unknown id shows verbatim.
        let mut label = name.clone();
        for (wire, headliner) in crate::render::HAUL_HEADLINERS {
            if name == wire {
                label = headliner.to_owned();
                break;
            }
        }
        parts.push(label);
    }
    for kind in &filter.kinds {
        parts.push(kind_label(*kind).to_owned());
    }
    if !filter.sets.is_empty() {
        parts.push(count_label(filter.sets.len(), "set", "sets"));
    }
    if !filter.required_substats.is_empty() {
        parts.push(count_label(
            filter.required_substats.len(),
            "substat",
            "substats",
        ));
    }
    if parts.is_empty() {
        return "nothing selected".to_owned();
    }
    // Cap the trailing summary so it never crowds the title; the body has the rest.
    let cap = 3;
    if parts.len() <= cap {
        parts.join(", ")
    } else {
        format!("{} +{}", parts[..cap].join(", "), parts.len() - cap)
    }
}

/// One-line recap of the active stop limits for the folded Stop bar; "no limits"
/// when the run is uncapped.
fn stop_summary(limits: &Limits) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(n) = limits.max_refreshes {
        parts.push(count_label(n as usize, "refresh", "refreshes"));
    }
    if let Some(n) = limits.max_spend {
        parts.push(format!("{n} crystals"));
    }
    if let Some(n) = limits.max_matches {
        parts.push(count_label(n as usize, "match", "matches"));
    }
    if let Some(ms) = limits.max_duration_ms {
        parts.push(format!("{} min", ms.div_ceil(60_000)));
    }
    if parts.is_empty() {
        "no limits".to_owned()
    } else {
        parts.join(" · ")
    }
}

/// The Click timing bar is always folded on arrival, so its peek carries the
/// most weight: whether the player tuned any extra delay on top of the tuned
/// baselines, or left them at zero.
fn timing_summary(timings: &Timings) -> &'static str {
    if *timings == Timings::default() {
        "no extra delay"
    } else {
        "custom extra delay"
    }
}

/// `n singular` / `n plural`, e.g. `1 refresh` / `3 refreshes`.
fn count_label(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

/// Hunt: the item-interest criteria — what the loop buys. Open on arrival:
/// without at least one criterion the loop refuses to arm, so this is the first
/// thing the player sets.
fn hunt_body(ui: &mut egui::Ui, editor: &mut EditorState) {
    ui.horizontal(|ui| {
        // Unknown included: a config-seeded criterion must always be visible
        // and clearable.
        for kind in [
            ItemKind::Equipment,
            ItemKind::Hero,
            ItemKind::Token,
            ItemKind::Unknown,
        ] {
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

/// One-click add for the two tokens ~90% of players hunt (covenant bookmark,
/// mystic medal), spelling their internal ids so the player never types a
/// `ticketrare_name`. Reuses the haul's headliner table — one wire→label map.
fn quick_add_names(ui: &mut egui::Ui, names: &mut Vec<String>) {
    ui.horizontal(|ui| {
        ui.weak("quick add");
        for (wire, label) in crate::render::HAUL_HEADLINERS {
            let present = names.iter().any(|name| name == wire);
            if ui
                .add_enabled(!present, egui::Button::new(format!("+ {label}")))
                .clicked()
            {
                names.push(wire.to_owned());
            }
        }
    });
}

/// Stop: the run's safety rails. A uniform numeric block, so it lays in a grid
/// — the checkboxes and their values line up in two columns instead of drifting
/// with each label's width.
fn stop_body(ui: &mut egui::Ui, editor: &mut EditorState) {
    egui::Grid::new("stop-limits")
        .num_columns(2)
        .spacing([theme::SP_SM, theme::SP_XS])
        .show(ui, |ui| {
            optional_value(ui, "max refreshes", &mut editor.limits.max_refreshes, 10);
            ui.end_row();
            optional_value(ui, "max spend (crystals)", &mut editor.limits.max_spend, 30);
            ui.end_row();
            optional_value(ui, "max matches", &mut editor.limits.max_matches, 5);
            ui.end_row();
            duration_minutes(ui, &mut editor.limits.max_duration_ms);
            ui.end_row();
        });
}

/// Click timing: a random extra-wait range (min..max ms) per action, drawn
/// fresh each time and added on top of the tuned baseline shown beside it.
/// Folded by default (via `edit_setup`) — expert tuning, out of the way.
fn timing_body(ui: &mut egui::Ui, editor: &mut EditorState) {
    let t = &mut editor.timings;
    ui.weak("random extra delay (min..max ms) added on top of each tuned baseline");
    ui.add_space(theme::SP_XS);
    delay_range(
        ui,
        "after shop opens",
        &mut t.shop_opened,
        plan::WAIT_SHOP_OPENED_MS,
    );
    delay_range(
        ui,
        "after paid refresh",
        &mut t.refreshed,
        plan::WAIT_REFRESHED_MS,
    );
    delay_range(
        ui,
        "after a purchase",
        &mut t.purchase_resumed,
        plan::WAIT_PURCHASE_RESUMED_MS,
    );
    delay_range(
        ui,
        "watchdog re-issue",
        &mut t.recovery,
        plan::WAIT_RECOVERY_MS,
    );
    delay_range(
        ui,
        "refresh → confirm",
        &mut t.confirm_refresh_modal,
        plan::WAIT_CONFIRM_REFRESH_MODAL_MS,
    );
    delay_range(
        ui,
        "buy → confirm",
        &mut t.buy_modal,
        plan::WAIT_BUY_MODAL_MS,
    );
    delay_range(
        ui,
        "between buys",
        &mut t.between_buys,
        plan::WAIT_BETWEEN_BUYS_MS,
    );
    delay_range(
        ui,
        "after a scroll",
        &mut t.scroll_settle,
        plan::WAIT_SCROLL_SETTLE_MS,
    );
}

/// The single commit: one primary Apply that sends every draft that moved and
/// re-seeds its applied twin. Disabled until something changed — and, when the
/// filter is the change, until it is restricted enough to arm (an unrestricted
/// filter the loop would refuse never reaches the session). Timing/limit-only
/// edits apply even while the filter sits unrestricted, since the domain only
/// gates arming on the filter.
fn commit_row(ui: &mut egui::Ui, editor: &mut EditorState) -> Vec<Command> {
    let dirty_filter = editor.filter != editor.applied_filter;
    let dirty_limits = editor.limits != editor.applied_limits;
    let dirty_timings = editor.timings != editor.applied_timings;
    let dirty = dirty_filter || dirty_limits || dirty_timings;
    // Only a *changed* filter must clear the arming bar; an already-applied
    // restricted filter lets limit/timing edits through untouched.
    let blocked = dirty_filter && editor.filter.is_unrestricted();

    let mut commands = Vec::new();
    ui.horizontal(|ui| {
        if blocked {
            ui.weak("add at least one hunt criterion before Apply");
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let clicked = ui
                .add_enabled_ui(dirty && !blocked, |ui| theme::primary_button(ui, "Apply"))
                .inner
                .clicked();
            if clicked {
                if dirty_filter {
                    commands.push(Command::SetFilter(editor.filter.clone()));
                    editor.applied_filter = editor.filter.clone();
                }
                if dirty_limits {
                    commands.push(Command::SetLimits(editor.limits.clone()));
                    editor.applied_limits = editor.limits.clone();
                }
                if dirty_timings {
                    commands.push(Command::SetTimings(editor.timings));
                    editor.applied_timings = editor.timings;
                }
            }
        });
    });
    ui.add_space(theme::SP_XS);
    ui.weak("edits apply to this session only — config.toml is unchanged");
    commands
}

/// One extra-wait row: min and max drags (0-floored) with the tuned baseline
/// shown as a hint, so the player reads the range added on top of it. `max`
/// is floored at `min` only when a drag actually moves, so a reversed
/// config-seeded range is left untouched (never a phantom "dirty" on arrival).
fn delay_range(ui: &mut egui::Ui, label: &str, value: &mut DelayRange, baseline: u64) {
    ui.horizontal(|ui| {
        let min = ui.add(
            egui::DragValue::new(&mut value.min_ms)
                .range(0..=u64::MAX)
                .prefix("min ")
                .suffix(" ms"),
        );
        let max = ui.add(
            egui::DragValue::new(&mut value.max_ms)
                .range(0..=u64::MAX)
                .prefix("max ")
                .suffix(" ms"),
        );
        if min.changed() || max.changed() {
            value.max_ms = value.max_ms.max(value.min_ms);
        }
        ui.label(label);
        ui.weak(format!("(+{baseline} base)"));
    });
}

/// Row remove control: a `✕` on a 24px-square target. `small_button` gave an
/// ~18px hit area that was easy to miss when pruning a list.
fn remove_button(ui: &mut egui::Ui) -> egui::Response {
    ui.add(egui::Button::new("✕").min_size(egui::vec2(24.0, 24.0)))
}

/// One editable any-of list: entries with a remove cross plus an add row.
fn string_list(ui: &mut egui::Ui, label: &str, values: &mut Vec<String>, input: &mut String) {
    ui.label(label);
    let mut removed = None;
    for (index, value) in values.iter().enumerate() {
        // Content-keyed row ids (duplicates are rejected on add): focus and
        // edit state survive a removal above the row.
        ui.push_id(egui::Id::new(value), |ui| {
            ui.horizontal(|ui| {
                ui.monospace(value);
                if remove_button(ui).clicked() {
                    removed = Some(index);
                }
            });
        });
    }
    if let Some(index) = removed {
        values.remove(index);
    }
    ui.horizontal(|ui| {
        ui.text_edit_singleline(input);
        if ui.button("add").clicked() {
            let value = input.trim();
            if !value.is_empty() && !values.iter().any(|kept| kept == value) {
                values.push(value.to_owned());
                input.clear();
            }
        }
    });
}

/// Required-substat rows: name, optional min threshold, remove cross.
fn substat_reqs(ui: &mut egui::Ui, reqs: &mut Vec<SubstatReq>, input: &mut String) {
    ui.label("required substats");
    let mut removed = None;
    for (index, req) in reqs.iter_mut().enumerate() {
        let row_id = egui::Id::new(&req.name);
        ui.push_id(row_id, |ui| {
            ui.horizontal(|ui| {
                ui.monospace(&req.name);
                let mut has_min = req.min.is_some();
                ui.checkbox(&mut has_min, "min");
                if has_min {
                    ui.add(egui::DragValue::new(req.min.get_or_insert(1.0)).speed(0.5));
                } else {
                    req.min = None;
                }
                if remove_button(ui).clicked() {
                    removed = Some(index);
                }
            });
        });
    }
    if let Some(index) = removed {
        reqs.remove(index);
    }
    ui.horizontal(|ui| {
        ui.text_edit_singleline(input);
        if ui.button("add").clicked() {
            let name = input.trim();
            if !name.is_empty() && !reqs.iter().any(|req| req.name == name) {
                reqs.push(SubstatReq {
                    name: name.to_owned(),
                    min: None,
                });
                input.clear();
            }
        }
    });
}

/// Checkbox-gated numeric criterion, laid as two grid cells (label, value) so a
/// column of them lines up. Unchecked means "no constraint", expressed by the
/// unchecked box — never a 0. A freshly checked box seeds a non-zero value and
/// dragging is floored at 1, but `clamp_existing_to_range` is off: a value
/// already present (e.g. a `max_refreshes = 0` seeded from config.toml) is shown
/// as-is, not silently rewritten to 1 on the first render — which would desync
/// the draft and make Apply send a value the player never chose.
fn optional_value<T: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<T>,
    seed: T,
) {
    let mut on = value.is_some();
    ui.checkbox(&mut on, label);
    if on {
        ui.add(
            egui::DragValue::new(value.get_or_insert(seed))
                .range(T::from_f64(1.0)..=T::MAX)
                .clamp_existing_to_range(false),
        );
    } else {
        *value = None;
    }
}

/// The duration limit, edited in whole minutes (stored as ms). A grid row like
/// `optional_value`, kept apart for its minute↔ms conversion.
fn duration_minutes(ui: &mut egui::Ui, value: &mut Option<u64>) {
    let mut on = value.is_some();
    ui.checkbox(&mut on, "max duration (minutes)");
    if on {
        let ms = value.get_or_insert(60 * 60_000);
        // Ceil so a sub-minute config value never reads as 0; edits are whole
        // minutes (the player-facing unit) and only rewrite the stored value
        // when the player actually drags.
        let mut minutes = ms.div_ceil(60_000);
        if ui
            .add(egui::DragValue::new(&mut minutes).range(1..=u64::MAX / 60_000))
            .changed()
        {
            *ms = minutes.saturating_mul(60_000);
        }
    } else {
        *value = None;
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

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
        let sent = RefCell::new(Vec::new());
        let mut harness = Harness::new_ui(|ui| {
            let commands = edit_setup(ui, editor);
            if !commands.is_empty() {
                *sent.borrow_mut() = commands;
            }
        });
        harness.get_by_label("Apply").click();
        harness.run();
        drop(harness);
        sent.into_inner()
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
    fn apply_inert_while_nothing_changed() {
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        assert!(run_setup(&mut editor).is_empty());
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
    fn timing_section_folds_its_body_until_opened() {
        // Click timing arrives folded, so its rows are hidden; clicking the bar
        // reveals them (a delay row's baseline hint is a body-only label). While
        // folded, the bar's accessible name trails its summary peek.
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        let mut harness = Harness::new_ui(|ui| {
            edit_setup(ui, &mut editor);
        });
        assert!(harness.query_by_label("(+780 base)").is_none());
        harness
            .get_by_label("Click timing · no extra delay")
            .click();
        harness.run();
        harness.get_by_label("(+780 base)");
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
        let click = harness.get_by_label("Click timing · no extra delay").rect();
        assert_eq!(hunt.max.y, stop.min.y, "Hunt and Stop must tile");
        assert_eq!(stop.max.y, click.min.y, "Stop and Click timing must tile");
    }

    #[test]
    fn hunt_summary_names_the_hunted_tokens() {
        // A folded Hunt bar peeks what the loop would buy: the covenant token
        // reads by its haul label, not its wire id.
        assert_eq!(hunt_summary(&named_filter()), "Covenant");
        assert_eq!(hunt_summary(&Filter::default()), "nothing selected");
    }

    #[test]
    fn stop_summary_lists_active_limits() {
        let limits = Limits {
            max_refreshes: Some(1),
            max_matches: Some(5),
            ..Limits::default()
        };
        assert_eq!(stop_summary(&limits), "1 refresh · 5 matches");
        assert_eq!(stop_summary(&Limits::default()), "no limits");
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
    fn apply_sends_changed_timings() {
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        let timings = Timings {
            refreshed: DelayRange {
                min_ms: 200,
                max_ms: 800,
            },
            ..Timings::default()
        };
        editor.timings = timings;
        assert_eq!(run_setup(&mut editor), vec![Command::SetTimings(timings)]);
    }
}
