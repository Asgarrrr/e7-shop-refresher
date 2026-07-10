//! Draft filter/limits owned by the window, and the widgets that edit them;
//! Apply pushes a draft to the session as a command.

use eframe::egui;

use crate::app::Command;
use crate::domain::control::Limits;
use crate::domain::filter::{Filter, SubstatReq};
use crate::domain::shop::ItemKind;
use crate::render::kind_label;

/// Draft criteria owned by the window until Apply pushes them to the
/// session; seeded from the controller's live criteria at startup. Edits are
/// session-only — config.toml is never rewritten.
pub struct EditorState {
    filter: Filter,
    limits: Limits,
    name_input: String,
    set_input: String,
    substat_input: String,
}

impl EditorState {
    pub fn new(filter: Filter, limits: Limits) -> Self {
        Self {
            filter,
            limits,
            name_input: String::new(),
            set_input: String::new(),
            substat_input: String::new(),
        }
    }
}

/// The collapsible filter editor; `Some` when the player applied the draft.
pub fn edit_filter(ui: &mut egui::Ui, editor: &mut EditorState) -> Option<Command> {
    let mut clicked = None;
    ui.collapsing("Filter", |ui| {
        ui.horizontal(|ui| {
            // Unknown included: a config-seeded criterion must always be
            // visible and clearable.
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
        string_list(
            ui,
            "names (exact internal ids)",
            &mut editor.filter.names,
            &mut editor.name_input,
        );
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
        optional_value(ui, "min substats", &mut editor.filter.min_substats, 1);
        // Seeded above the covenant-bookmark price so a fresh cap still
        // matches the default hunt targets.
        optional_value(
            ui,
            "max price (gold)",
            &mut editor.filter.max_price,
            300_000,
        );
        ui.checkbox(&mut editor.filter.include_sold_out, "include sold out");
        let restricted = !editor.filter.is_unrestricted();
        if !restricted {
            ui.weak("at least one criterion is required before Apply");
        }
        if ui
            .add_enabled(restricted, egui::Button::new("Apply filter"))
            .clicked()
        {
            clicked = Some(Command::SetFilter(editor.filter.clone()));
        }
    });
    clicked
}

/// The collapsible limits editor; `Some` when the player applied the draft.
pub fn edit_limits(ui: &mut egui::Ui, editor: &mut EditorState) -> Option<Command> {
    let mut clicked = None;
    ui.collapsing("Limits", |ui| {
        optional_value(ui, "max refreshes", &mut editor.limits.max_refreshes, 10);
        optional_value(ui, "max spend (crystals)", &mut editor.limits.max_spend, 30);
        optional_value(ui, "max matches", &mut editor.limits.max_matches, 5);
        duration_minutes(ui, &mut editor.limits.max_duration_ms);
        if ui.button("Apply limits").clicked() {
            clicked = Some(Command::SetLimits(editor.limits.clone()));
        }
    });
    clicked
}

/// One editable any-of list: entries with a remove cross plus an add row.
fn string_list(ui: &mut egui::Ui, label: &str, values: &mut Vec<String>, input: &mut String) {
    ui.label(label);
    let mut removed = None;
    for (index, value) in values.iter().enumerate() {
        // Stable per-row ids: without them a removal shifts the positional
        // auto-ids of every widget below (focus, edit state).
        ui.push_id(index, |ui| {
            ui.horizontal(|ui| {
                ui.monospace(value);
                if ui.small_button("✕").clicked() {
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
        if ui.button("add").clicked() && !input.trim().is_empty() {
            values.push(input.trim().to_owned());
            input.clear();
        }
    });
}

/// Required-substat rows: name, optional min threshold, remove cross.
fn substat_reqs(ui: &mut egui::Ui, reqs: &mut Vec<SubstatReq>, input: &mut String) {
    ui.label("required substats");
    let mut removed = None;
    for (index, req) in reqs.iter_mut().enumerate() {
        ui.push_id(index, |ui| {
            ui.horizontal(|ui| {
                ui.monospace(&req.name);
                let mut has_min = req.min.is_some();
                ui.checkbox(&mut has_min, "min");
                if has_min {
                    ui.add(egui::DragValue::new(req.min.get_or_insert(1.0)).speed(0.5));
                } else {
                    req.min = None;
                }
                if ui.small_button("✕").clicked() {
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
        if ui.button("add").clicked() && !input.trim().is_empty() {
            reqs.push(SubstatReq {
                name: input.trim().to_owned(),
                min: None,
            });
            input.clear();
        }
    });
}

/// Checkbox-gated numeric criterion: unchecked means "no constraint". The
/// seed must be a sensible non-zero value — a zero limit halts the session at
/// the next check-point and a zero criterion constrains nothing.
fn optional_value<T: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<T>,
    seed: T,
) {
    ui.horizontal(|ui| {
        let mut on = value.is_some();
        ui.checkbox(&mut on, label);
        if on {
            ui.add(egui::DragValue::new(value.get_or_insert(seed)));
        } else {
            *value = None;
        }
    });
}

/// The duration limit, edited in whole minutes (stored as ms).
fn duration_minutes(ui: &mut egui::Ui, value: &mut Option<u64>) {
    ui.horizontal(|ui| {
        let mut on = value.is_some();
        ui.checkbox(&mut on, "max duration (minutes)");
        if on {
            let ms = value.get_or_insert(60 * 60_000);
            // Ceil so a sub-minute config value never reads as 0; edits are
            // whole minutes (the player-facing unit) and only rewrite the
            // stored value when the player actually drags.
            let mut minutes = ms.div_ceil(60_000);
            if ui.add(egui::DragValue::new(&mut minutes)).changed() {
                *ms = minutes.saturating_mul(60_000);
            }
        } else {
            *value = None;
        }
    });
}
