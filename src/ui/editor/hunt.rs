//! Hunt: the item-interest criteria — the section's summary and its leaf
//! widgets (id lists, substat rows, the checkbox-gated numeric cell). Works on
//! a `Filter` field or a scratch buffer handed in by the caller, needing no
//! `EditorState`. `hunt_body` stays in the shell because it reaches four draft
//! fields at once — see `_HANDOFF.md` for the draft-grouping prerequisite that
//! would let it move here too.

use eframe::egui;

use super::{arm_optional, bounded_field, count_label, optional_field};
use crate::domain::filter::{Filter, SubstatReq};
use crate::render::kind_label;

/// The gear grades the game ships, and so the only floors `[filter] min_grade`
/// accepts. Spelled here because `domain::filter`'s pair is private and
/// `src/domain/` stays clear of GUI code; that module's parse-time check is the
/// authority, and `the_grade_field_is_bounded_by_what_the_config_accepts` runs
/// this range through it so the two cannot drift apart unnoticed.
const GRADE_MIN: u8 = 2;
const GRADE_MAX: u8 = 4;

/// One-line recap of the hunt draft for the folded Hunt bar: the labels of what
/// the loop would buy (haul-headliner names, then kinds, then the finer
/// criteria).
///
/// Every field [`Filter::is_unrestricted`] counts as a criterion is listed
/// here, and that is not decoration. "nothing selected" is the only thing this
/// tab says about an empty hunt, so a criterion that restricts without
/// appearing reads as an empty filter that nonetheless arms — and the loop then
/// refreshes forever, buys nothing, and the bar gives no clue why.
/// `min_grade` was exactly that; `max_price` and `min_substats` had editors in
/// the body but no part here, so a config setting only one of them told the
/// same lie from the folded bar.
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
    // `Some(0)` is skipped rather than shown as "0+ substats": it constrains
    // nothing, `is_unrestricted` refuses to count it, and Apply stays dark
    // over it — a part here would be the mirror-image lie, a bar naming a
    // criterion for a filter the loop calls empty.
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
        //
        // Keyed on `(label, value)` and not on `value` alone: `push_id` salts
        // the *parent* `Ui`'s id, and `hunt_body` calls this twice — "names"
        // and "sets" — on the same `Ui`, so the content alone gave the two
        // lists' rows one id (measured: the two child `ui.id()` values compare
        // equal). That is egui's "Double ID" case — the overlay, and two
        // widgets sharing one registration for focus and interaction state.
        // The two fields are adjacent and both ask for "exact internal ids",
        // so the same string landing in both is the ordinary way in.
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
        // Salted like `string_list`'s rows and for the same reason: this list
        // shares a parent `Ui` with the two string lists, so a substat named
        // the same as a set or a name entry would collide with it.
        let row_id = egui::Id::new(("required substats", &req.name));
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

/// The gear-grade floor: [`optional_value`]'s cell over the closed domain the
/// game actually ships. A twin rather than one more call to it — the way
/// [`super::duration_row`] is [`super::limit_row`]'s twin — because the bound
/// *is* the point. `optional_field` is open above, so a 5 dragged in here would
/// be a criterion no item satisfies, written to config.toml by Apply and then
/// refused outright by `domain::filter`'s `grade_floor` at the next launch:
/// one drag, and the app no longer starts. [`bounded_field`] clamps what the
/// player drags while still leaving a seeded value exactly as the file spelled
/// it, so both halves hold.
///
/// Seeded at [`GRADE_MAX`]: the epic floor is what `config.example.toml`
/// documents and what a player reaching for this criterion is after — the same
/// "seed the useful value, not the range's floor" call `max_price`'s seed makes.
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
        // The folded Hunt bar reads the covenant token by its haul label.
        let named = Filter {
            names: vec!["ticketrare_name".to_owned()],
            ..Filter::default()
        };
        assert_eq!(hunt_summary(&named), "Covenant");
        assert_eq!(hunt_summary(&Filter::default()), "nothing selected");
    }

    #[test]
    fn a_restricting_filter_is_never_summarized_as_nothing_selected() {
        // The bar's contract, in the terms the loop uses: `is_unrestricted` is
        // what decides whether the hunt can arm, so anything it counts has to
        // show. A `min_grade`-only config used to fold up as "nothing
        // selected" while `matches` dropped every gradeless item — an armed
        // loop refreshing forever with an empty-looking Hunt bar.
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
        // And the converse of the test above: `min_substats = 0` restricts
        // nothing, so the bar must keep calling it an empty hunt.
        let inert = Filter {
            min_substats: Some(0),
            ..Filter::default()
        };
        assert!(inert.is_unrestricted());
        assert_eq!(hunt_summary(&inert), "nothing selected");
    }

    #[test]
    fn the_grade_field_is_bounded_by_what_the_config_accepts() {
        // The drag range is a copy of a domain the loader owns, so hold the two
        // in step here rather than trusting the comment on `GRADE_MIN`. Widen
        // the domain without widening this range and the tab silently refuses a
        // legal floor; narrow it without narrowing the range and the tab
        // authors a config.toml the next launch will not load.
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
