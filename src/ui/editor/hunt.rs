//! Hunt: the item-interest criteria — the section's summary and its leaf
//! widgets (id lists, substat rows, the checkbox-gated numeric cell). Works on
//! a `Filter` field or a scratch buffer handed in by the caller, needing no
//! `EditorState`. `hunt_body` stays in the shell because it reaches four draft
//! fields at once — see `_HANDOFF.md` for the draft-grouping prerequisite that
//! would let it move here too.

use eframe::egui;

use super::{arm_optional, count_label, optional_field};
use crate::domain::filter::{Filter, SubstatReq};
use crate::render::kind_label;

/// One-line recap of the hunt draft for the folded Hunt bar: the labels of what
/// the loop would buy (haul-headliner names, then kinds, then a count of the
/// finer criteria).
pub(super) fn hunt_summary(filter: &Filter) -> String {
    let mut parts: Vec<String> = Vec::new();
    for name in &filter.names {
        // Reuse the haul's wire→label map so a hunted token reads "Covenant"
        // rather than "ticketrare_name"; an unknown id shows verbatim.
        let label = crate::render::HAUL_HEADLINERS
            .iter()
            .find(|(wire, _)| name == wire)
            .map_or(name.as_str(), |(_, headliner)| *headliner);
        parts.push(label.to_owned());
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

/// One-click add for the two tokens ~90% of players hunt (covenant bookmark,
/// mystic medal), spelling their internal ids so the player never types a
/// `ticketrare_name`. Reuses the haul's headliner table.
pub(super) fn quick_add_names(ui: &mut egui::Ui, names: &mut Vec<String>) {
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

/// Row remove control: a `✕` on a 24px-square target — `small_button`'s
/// ~18px hit area was easy to miss when pruning a list.
fn remove_button(ui: &mut egui::Ui) -> egui::Response {
    ui.add(egui::Button::new("✕").min_size(egui::vec2(24.0, 24.0)))
}

/// One editable any-of list: entries with a remove cross plus an add row.
pub(super) fn string_list(
    ui: &mut egui::Ui,
    label: &str,
    values: &mut Vec<String>,
    input: &mut String,
) {
    ui.label(label);
    let mut removed = None;
    for (index, value) in values.iter().enumerate() {
        // Content-keyed row ids (duplicates rejected on add), so focus/edit
        // state survive a removal above the row.
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
pub(super) fn substat_reqs(ui: &mut egui::Ui, reqs: &mut Vec<SubstatReq>, input: &mut String) {
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
                    let min = req.min.get_or_insert(1.0);
                    ui.add(egui::DragValue::new(min).speed(0.5));
                    // egui parses typed text with `f64::from_str`, which accepts
                    // "nan" and "inf". Either would satisfy no `value >= min`
                    // threshold, light Apply forever (`NaN != NaN` in `Filter`'s
                    // derived `PartialEq`), and be refused by `Config::validate`
                    // on the next launch — so it is snapped back here. No range
                    // is set instead of clamped, to avoid silently rewriting a
                    // config-seeded value (see `clamp_existing_to_range(false)`).
                    if !min.is_finite() {
                        *min = 1.0;
                    }
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

/// Checkbox-gated numeric criterion, laid as two grid cells (label, value).
/// Unchecked means "no constraint" — never a 0. Arming is [`arm_optional`]'s;
/// the field is [`optional_field`]'s, shared with the Stop rails.
pub(super) fn optional_value<T: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<T>,
    seed: T,
) {
    let mut on = value.is_some();
    ui.checkbox(&mut on, label);
    arm_optional(on, value, seed);
    if let Some(current) = value.as_mut() {
        optional_field(ui, current);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunt_summary_names_the_hunted_tokens() {
        // The folded Hunt bar reads the covenant token by its haul label.
        let named = Filter {
            names: vec!["ticketrare_name".to_owned()],
            ..Filter::default()
        };
        assert_eq!(hunt_summary(&named), "Covenant");
        assert_eq!(hunt_summary(&Filter::default()), "nothing selected");
    }
}
