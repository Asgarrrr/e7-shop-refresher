//! Hunt: the item-interest criteria — the section's summary and its leaf
//! widgets. Works on a `Filter` field or a scratch buffer handed in by the
//! caller; `hunt_body` stays in the shell because it reaches four drafts.

use eframe::egui;

use super::{arm_optional, bounded_field, count_label, optional_field};
use crate::domain::filter::{Filter, SubstatReq};
use crate::render::kind_label;

/// The gear grades the game ships, and so the only floors `[filter] min_grade`
/// accepts. A copy of a domain `domain::filter` owns privately;
/// `the_grade_field_is_bounded_by_what_the_config_accepts` holds the two in
/// step.
const GRADE_MIN: u8 = 2;
const GRADE_MAX: u8 = 4;

/// One-line recap of the hunt draft for the folded Hunt bar. If a filter
/// restricts, the summary must say so: every field [`Filter::is_unrestricted`]
/// counts has to appear, or the bar reads "nothing selected" over a hunt that
/// arms, refreshes forever and buys nothing.
/// Visible to the whole window, not just Setup: the idle status band reuses it
/// to say what a run would hunt (`view::plan_summary`).
pub(in crate::ui) fn hunt_summary(filter: &Filter) -> String {
    let mut parts: Vec<String> = Vec::new();
    for name in &filter.names {
        // The haul's wire→label map, so a token reads "Covenant" and not
        // "ticketrare_name"; an unknown id shows verbatim.
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
    // `Some(0)` constrains nothing and `is_unrestricted` refuses to count it,
    // so naming it would be the mirror-image lie.
    if let Some(min) = filter.min_substats.filter(|min| *min > 0) {
        parts.push(format!("{min}+ substats"));
    }
    if let Some(max) = filter.max_price {
        // `Gold` groups itself, so this reads like the shop table's price
        // column rather than a bare seven-digit number.
        parts.push(format!("≤{max} gold"));
    }
    if let Some(min) = filter.min_grade {
        parts.push(format!("grade {min}+"));
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

/// One-click add for the two tokens ~90% of players hunt, spelling their
/// internal ids so the player never types a `ticketrare_name`.
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
        // Content-keyed so focus survives a removal above the row, and salted
        // with `label` because `push_id` salts the *parent* `Ui`: `hunt_body`
        // calls this twice on one `Ui`, so content alone gave both lists' rows
        // a single id — egui's "Double ID" case.
        ui.push_id(egui::Id::new((label, value)), |ui| {
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
        // Salted like `string_list`'s rows: this list shares a parent `Ui` with
        // them, so a substat named like a set entry would collide.
        let row_id = egui::Id::new(("required substats", &req.name));
        ui.push_id(row_id, |ui| {
            ui.horizontal(|ui| {
                ui.monospace(&req.name);
                let mut has_min = req.min.is_some();
                ui.checkbox(&mut has_min, "min");
                if has_min {
                    let min = req.min.get_or_insert(1.0);
                    ui.add(egui::DragValue::new(min).speed(0.5));
                    // egui parses typed text with `f64::from_str`, which takes
                    // "nan"/"inf": either matches nothing, lights Apply forever
                    // (`NaN != NaN`), and is refused on the next launch.
                    // Snapped back, not range-clamped, which would rewrite a
                    // config-seeded value (see `clamp_existing_to_range`).
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
/// Unchecked means "no constraint" — never a 0.
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

/// The gear-grade floor: [`optional_value`]'s cell over the closed domain the
/// game ships. A twin rather than another call to it because the bound *is* the
/// point — `optional_field` is open above, and a 5 dragged in here would be
/// written to config.toml by Apply and refused by `grade_floor` at the next
/// launch, so the app would stop starting. Seeded at [`GRADE_MAX`].
pub(super) fn grade_value(ui: &mut egui::Ui, value: &mut Option<u8>) {
    let mut on = value.is_some();
    ui.checkbox(&mut on, "min grade");
    arm_optional(on, value, GRADE_MAX);
    if let Some(current) = value.as_mut() {
        bounded_field(ui, current, GRADE_MIN..=GRADE_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shop::Gold;

    #[test]
    fn hunt_summary_names_the_hunted_tokens() {
        let named = Filter {
            names: vec!["ticketrare_name".to_owned()],
            ..Filter::default()
        };
        assert_eq!(hunt_summary(&named), "Covenant");
        assert_eq!(hunt_summary(&Filter::default()), "nothing selected");
    }

    #[test]
    fn a_restricting_filter_is_never_summarized_as_nothing_selected() {
        // A `min_grade`-only config used to fold up as "nothing selected" while
        // `matches` dropped every gradeless item.
        for filter in [
            Filter {
                min_grade: Some(4),
                ..Filter::default()
            },
            Filter {
                max_price: Some(Gold::new(300_000)),
                ..Filter::default()
            },
            Filter {
                min_substats: Some(3),
                ..Filter::default()
            },
        ] {
            assert!(!filter.is_unrestricted());
            assert_ne!(
                hunt_summary(&filter),
                "nothing selected",
                "a filter the loop calls restricted must not fold up as empty: {filter:?}"
            );
        }
    }

    #[test]
    fn the_numeric_criteria_read_in_the_shop_table_s_terms() {
        let filter = Filter {
            min_grade: Some(4),
            max_price: Some(Gold::new(300_000)),
            min_substats: Some(3),
            ..Filter::default()
        };
        assert_eq!(
            hunt_summary(&filter),
            "3+ substats, ≤300,000 gold, grade 4+"
        );
        // The converse: `min_substats = 0` restricts nothing, so the bar must
        // keep calling it an empty hunt.
        let inert = Filter {
            min_substats: Some(0),
            ..Filter::default()
        };
        assert!(inert.is_unrestricted());
        assert_eq!(hunt_summary(&inert), "nothing selected");
    }

    #[test]
    fn the_grade_field_is_bounded_by_what_the_config_accepts() {
        // Widen the loader's domain without widening this range and the tab
        // refuses a legal floor; narrow it without narrowing the range and the
        // tab authors a config.toml the next launch will not load.
        for grade in GRADE_MIN..=GRADE_MAX {
            let filter: Filter = toml::from_str(&format!("min_grade = {grade}"))
                .expect("the field must not offer a grade the loader refuses");
            assert_eq!(filter.min_grade, Some(grade));
        }
        for outside in [GRADE_MIN - 1, GRADE_MAX + 1] {
            assert!(
                toml::from_str::<Filter>(&format!("min_grade = {outside}")).is_err(),
                "the field must not refuse a grade the loader accepts: {outside}"
            );
        }
    }
}
