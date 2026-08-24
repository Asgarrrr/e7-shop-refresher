//! Hunt: the item-interest criteria — the section's summary and its leaf
//! widgets. Works on a `Filter` field or a scratch buffer handed in by the
//! caller; `hunt_body` stays in the shell because it reaches four drafts.

use eframe::egui;

use super::{arm_optional, count_label, optional_field};
use crate::domain::filter::{Filter, SubstatReq};
use crate::render::kind_label;
use crate::ui::icons::SetIcons;
use crate::uplink::protocol::{TokenEntry, VocabularyEntry};

/// The square a gear-set icon draws at.
///
/// A width decision before a visual one: `main.rs` pins the window to
/// [`crate::ui::WINDOW_WIDTH`] as both its minimum and its maximum inner size,
/// so the twenty-two set chips have ~408px to wrap into and no player can widen
/// them out of it. Labelled, they take eight rows; at this size, three. The wire
/// icons are 44px, so this is a downscale and never an upscale.
const SET_ICON: f32 = 32.0;

/// The floors the substat control offers, low to high.
///
/// Three values and not an open field, because the shop sells only `+0` gear:
/// its substat count is the gear grade, which the game defines on 2, 3 and 4.
/// The top segment reads `4` rather than `4+` — 4 is the ceiling, so a `+`
/// there would promise a fifth substat that does not exist.
pub(super) const SUBSTAT_FLOORS: [(u8, &str); 3] = [(2, "2+"), (3, "3+"), (4, "4")];

/// The threshold a freshly armed `≥` starts at, one per substat family.
///
/// Measured on 59 real gear pieces: a percent-bearing substat arrives as a
/// fraction and never leaves `0.02..=0.12`, while `att`, `def`, `max_hp` and
/// `speed` are whole and run `1..=472`. Both seeds are the FLOOR of their own
/// domain, which is one policy and not two — a freshly armed threshold excludes
/// nothing the game can produce, and every drag from there restricts.
///
/// Both families used to seed at `1.0`. On a percent substat that is one
/// hundred percent, eight times the largest roll in the game, so ticking `≥`
/// emptied the hunt silently and the first drag step crossed the whole real
/// domain.
const PERCENT_SEED: f64 = 0.02;
const WHOLE_SEED: f64 = 1.0;

/// Drag steps, in the unit the field SHOWS. The percent domain is ten points
/// wide, so the whole field's `0.5` would cross all of it in twenty pixels.
const PERCENT_STEP: f64 = 0.1;
const WHOLE_STEP: f64 = 0.5;

/// The seed for a substat, given whether the vocabulary flagged it percent.
/// One spelling, called from the chip (which holds the entry) and from the
/// normalising pass (which has to look the entry up).
const fn seed_for(percent: bool) -> f64 {
    if percent { PERCENT_SEED } else { WHOLE_SEED }
}

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
    if !filter.slots.is_empty() {
        parts.push(count_label(filter.slots.len(), "slot", "slots"));
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

/// A criterion the server can enumerate: one checkbox per offered value.
///
/// Checkboxes rather than a dropdown, and that is a correctness point before a
/// taste one: `egui::ComboBox` contributes NO accessibility node in this
/// version — verified by dumping the tree — so a picker built on one is both
/// invisible to a screen reader and unreachable from `kittest`. Checkboxes and
/// buttons do appear, which is also why `quick_add_names` above works.
///
/// It matches the `kinds` row `hunt_body` already draws, so a criterion the
/// server can enumerate looks like every other closed choice in the section.
/// Wrapped, because the sets list runs to twenty-two entries.
///
/// `icons` is what turns a value into [`icon_chip`] instead: a picture where the
/// server sent one, the checkbox everywhere else. It is a source and not a flag
/// because the answer is per VALUE — a catalog can name a set and carry no
/// picture for it — and the criteria with no pictures at all hand in an empty
/// one rather than calling a second function whose only difference is the
/// branch it never takes.
pub(super) fn choice_list(
    ui: &mut egui::Ui,
    label: &str,
    values: &mut Vec<String>,
    choices: &[VocabularyEntry],
    icons: &SetIcons,
) {
    ui.label(label);
    ui.horizontal_wrapped(|ui| {
        for entry in choices {
            let mut on = values.contains(&entry.id);
            // Salted with `label`: two criteria can offer the same words, and
            // `push_id` salts the parent `Ui`, which every list here shares.
            let changed = ui
                .push_id(egui::Id::new((label, &entry.id)), |ui| {
                    match icons.get(&entry.id) {
                        Some(texture) => icon_chip(ui, texture, &entry.label, &mut on),
                        None => ui.checkbox(&mut on, &entry.label).changed(),
                    }
                })
                .inner;
            if changed {
                if on {
                    values.push(entry.id.clone());
                } else {
                    values.retain(|kept| *kept != entry.id);
                }
            }
        }
    });
    unoffered_rows(ui, label, values, String::as_str, |id| {
        choices.iter().any(|entry| entry.id == id)
    });
}

/// One offered value drawn as its picture: a toggling image button.
///
/// **The name is written back explicitly, and that is not decoration.** An
/// image-only [`egui::Button`] states a `WidgetInfo` carrying its selected flag
/// and NO label, so the node reaches the accessibility tree unnamed — announced
/// as nothing by a screen reader and unreachable by `get_by_label`, the very
/// failure that kept [`egui::ComboBox`] out of this section. The
/// [`egui::Response::widget_info`] call is what puts the set's name back on it,
/// as the `Checkbox` it behaves like rather than the `Button` it is built from.
/// Verified by dumping the tree, not assumed: without that call the chip is
/// absent from the dump entirely, node and name both.
///
/// It re-states the info rather than replacing it, so a click emits the button's
/// unnamed event beside this named one. That duplicate is the price of the only
/// naming hook egui offers a composed widget.
///
/// The hover text is the sighted half of the same problem: 44px of armour art
/// does not say "Speed Set" to someone who has not memorised the game's sets.
fn icon_chip(ui: &mut egui::Ui, texture: &egui::TextureHandle, label: &str, on: &mut bool) -> bool {
    let picture = egui::Image::new(egui::load::SizedTexture::from_handle(texture))
        .fit_to_exact_size(egui::vec2(SET_ICON, SET_ICON));
    let response = ui
        .add(egui::Button::image(picture).selected(*on))
        .on_hover_text(label);
    if response.clicked() {
        *on = !*on;
    }
    // After the toggle above, so the node states the value the click produced
    // rather than the one it replaced.
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, label)
    });
    response.clicked()
}

/// The values a picker cannot draw a control for, each as its own removable row.
///
/// Without them such a value would be invisible AND unremovable while still
/// filtering — a criterion written before the catalog arrived, or one the game
/// has since dropped. Every picker owes that, whatever it offers, so the three
/// share this rather than the pattern being re-derived per criterion.
///
/// Generic over the element because one of those lists is not a `Vec<String>`:
/// a required substat carries a threshold beside its id. `id` projects an entry
/// down to the one thing a row needs, and the removal below reads the value
/// back through that same projection — so no caller can select on one spelling
/// of an entry and delete by another.
fn unoffered_rows<T>(
    ui: &mut egui::Ui,
    label: &str,
    values: &mut Vec<T>,
    id: impl Fn(&T) -> &str,
    offered: impl Fn(&str) -> bool,
) {
    let unknown: Vec<String> = values
        .iter()
        .map(&id)
        .filter(|value| !offered(value))
        .map(str::to_owned)
        .collect();
    let mut dropped = None;
    for value in &unknown {
        ui.push_id(egui::Id::new((label, "unknown", value)), |ui| {
            ui.horizontal(|ui| {
                ui.monospace(value);
                ui.weak("not offered");
                if remove_button(ui).clicked() {
                    dropped = Some(value.clone());
                }
            });
        });
    }
    if let Some(value) = dropped {
        values.retain(|kept| id(kept) != value.as_str());
    }
}

/// The tokens the shop sells, one card each, ticked to hunt one.
///
/// A card and not a chip because of the price: it is the number that decides
/// whether a hit is worth stopping for, and a chip has nowhere to put one. The
/// card is still a checkbox for the reason [`choice_list`] gives — a
/// `ComboBox` contributes no accessibility node — and stacked one per row
/// rather than wrapped, since the shop sells three.
///
/// It writes `token.id`, the wire name [`Filter::matches`] compares against.
/// The label is the game's words and never reaches the filter.
pub(super) fn token_cards(ui: &mut egui::Ui, names: &mut Vec<String>, tokens: &[TokenEntry]) {
    ui.label("tokens");
    for token in tokens {
        let mut on = names.contains(&token.id);
        // Salted per token for the reason `choice_list` is salted per value:
        // `hunt_body` draws several checkbox groups on one `Ui`, and `push_id`
        // salts that shared parent.
        let changed = ui
            .push_id(egui::Id::new(("token", &token.id)), |ui| {
                ui.horizontal(|ui| {
                    let changed = ui.checkbox(&mut on, &token.label).changed();
                    if let Some(price) = token.price {
                        // `Gold` groups itself, so it reads like the shop
                        // table's price column.
                        ui.weak(format!("{price} gold"));
                    }
                    changed
                })
                .inner
            })
            .inner;
        if changed {
            if on {
                names.push(token.id.clone());
            } else {
                names.retain(|kept| *kept != token.id);
            }
        }
    }
    unoffered_rows(ui, "tokens", names, String::as_str, |id| {
        tokens.iter().any(|token| token.id == id)
    });
}

/// The substat floor, as one exclusive row of buttons.
///
/// Buttons rather than a `ComboBox` for the reason `choice_list` gives: a combo
/// contributes no accessibility node. Clicking the active segment clears the
/// floor, so every state the control can reach is reachable back out of.
///
/// It writes `min_substats` and never `min_grade`. On shop gear the two are one
/// axis — measured at 59 of 59 on real captures — and the substat count is what
/// a player reads off the piece.
pub(super) fn segmented(ui: &mut egui::Ui, value: &mut Option<u8>, choices: &[(u8, &str)]) {
    ui.horizontal(|ui| {
        for (n, label) in choices {
            let on = *value == Some(*n);
            if ui.selectable_label(on, *label).clicked() {
                *value = if on { None } else { Some(*n) };
            }
        }
    });
}

/// One editable any-of list entered as free text: entries with a remove cross
/// plus an add row.
///
/// For criteria the server cannot enumerate — `names`, whose values are item
/// names and not a closed vocabulary — and the fallback for the ones it usually
/// can, against a server with no Catalog to read. A picker over an empty list
/// would be a dead control, and drawing nothing would take away a criterion
/// that still works.
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
        // draws several lists on one `Ui`, so content alone gave two lists'
        // rows a single id — egui's "Double ID" case.
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

/// Required substats: one chip per offered substat, ticked to require it, with
/// its threshold unfolded in place.
///
/// The threshold is the number the WIRE sends, which is not the number a player
/// thinks in: a percent-bearing substat arrives as a fraction (`att_rate` is
/// `0.03` for 3%). The vocabulary states which are which — `VocabularyEntry`'s
/// `percent` — so such a chip is shown and stepped in whole percent and stores
/// the fraction [`Filter::matches`] compares against; see [`threshold_field`].
///
/// A requirement the vocabulary cannot name gets a removable row instead of a
/// chip, for the reason [`unoffered_rows`] gives — and, unlike the other two
/// lists, it also needs its threshold normalised, which is why that pass leads
/// the function rather than living in the chip.
pub(super) fn substat_chips(
    ui: &mut egui::Ui,
    reqs: &mut Vec<SubstatReq>,
    choices: &[VocabularyEntry],
) {
    // Over the WHOLE list and ahead of every widget, because both sources of a
    // non-finite threshold reach entries that get no stepper. `DragValue` parses
    // typed text with `f64::from_str`, which takes "nan"/"inf"; `config.toml`
    // carries them too, since TOML 1.0 has special floats and `min` is a plain
    // `f64`. Either matches nothing and lights Apply forever — `Dirty::of` is
    // bit-exact and `NaN != NaN`, so re-seeding the twin from the command Apply
    // sent does not put it out. Held inside the chip, the snap-back never ran
    // for a requirement drawn as an unoffered row: invisible filtering plus an
    // Apply nothing on screen could clear.
    for req in reqs.iter_mut() {
        if req.min.is_some_and(|min| !min.is_finite()) {
            // The same seed a freshly armed chip gets, so a NaN on a
            // percent-bearing substat lands on 2% rather than on `1.0` — which
            // is 100%, the very threshold nothing in the game satisfies. A
            // requirement the vocabulary cannot name states no family and takes
            // the whole-number seed, which is what every requirement had here
            // before the flag was read at all.
            let percent = choices
                .iter()
                .any(|entry| entry.id == req.name && entry.percent);
            req.min = Some(seed_for(percent));
        }
    }
    ui.label("required substats");
    let mut toggled = None;
    ui.horizontal_wrapped(|ui| {
        for entry in choices {
            let held = reqs.iter().position(|req| req.name == entry.id);
            let mut on = held.is_some();
            let changed = ui
                .push_id(egui::Id::new(("substat", &entry.id)), |ui| {
                    ui.checkbox(&mut on, &entry.label).changed()
                })
                .inner;
            if changed {
                toggled = Some(entry.id.clone());
            }
            // The threshold belongs to the chip, so it appears beside the one
            // it applies to and nowhere else.
            if let Some(index) = held {
                let req = &mut reqs[index];
                let mut has_min = req.min.is_some();
                ui.push_id(egui::Id::new(("min", &entry.id)), |ui| {
                    ui.checkbox(&mut has_min, "≥");
                    if has_min {
                        let min = req.min.get_or_insert(seed_for(entry.percent));
                        threshold_field(ui, min, entry.percent);
                    } else {
                        req.min = None;
                    }
                });
            }
        }
    });
    if let Some(id) = toggled {
        if let Some(index) = reqs.iter().position(|req| req.name == id) {
            reqs.remove(index);
        } else {
            reqs.push(SubstatReq {
                name: id,
                min: None,
            });
        }
    }
    unoffered_rows(
        ui,
        "required substats",
        reqs,
        |req: &SubstatReq| req.name.as_str(),
        |id| choices.iter().any(|entry| entry.id == id),
    );
}

/// The `≥` stepper for one required substat, in the unit a player reads.
///
/// A percent-bearing substat is shown and stepped in whole percent and STORED as
/// the fraction the filter compares against: `3%` on screen is `0.03` in
/// `config.toml` and `0.03` on the wire. A whole one is its own unit and passes
/// through.
///
/// The conversion sits on a local shown value rather than in `custom_formatter`
/// / `custom_parser`, and that is the point of the split: those two convert the
/// TEXT while leaving `speed` — and any future `range` — in wire units, so the
/// unit would be spelled in two places and only one of them would move in a
/// diff. Here every number the widget touches is in percent and the two lines
/// that cross the boundary sit next to each other. `currency_row` lends an
/// optional currency field its raw number the same way, for the same reason.
///
/// The write-back is gated on `is_finite` so this branch can never be the SOURCE
/// of a non-finite threshold: `DragValue` parses typed text with
/// `f64::from_str`, which takes "nan"/"inf", and the `/ 100.0` would carry
/// either straight through to the stored value. The normalising pass leading
/// [`substat_chips`] still owns the ones that arrive from `config.toml`, and
/// still owns the whole list — including the requirements no chip is drawn for.
fn threshold_field(ui: &mut egui::Ui, stored: &mut f64, percent: bool) {
    let mut shown = if percent { *stored * 100.0 } else { *stored };
    let field = egui::DragValue::new(&mut shown);
    let response = ui.add(if percent {
        field.speed(PERCENT_STEP).suffix("%")
    } else {
        field.speed(WHOLE_STEP)
    });
    // Only on a change, so an untouched threshold is never round-tripped
    // through the multiply and back.
    if response.changed() && shown.is_finite() {
        *stored = if percent { shown / 100.0 } else { shown };
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shop::{Gold, ItemKind};

    #[test]
    fn hunt_summary_names_the_hunted_tokens() {
        let named = Filter {
            names: vec!["ticketrare_name".to_owned()],
            ..Filter::default()
        };
        assert_eq!(hunt_summary(&named), "Covenant");
        assert_eq!(hunt_summary(&Filter::default()), "nothing selected");
    }

    /// One filter per criterion, each carrying that criterion and nothing else.
    ///
    /// Exhaustive over the fields [`Filter::is_unrestricted`] reads, which is
    /// the whole point — `slots` shipped missing from the summary precisely
    /// because this list was three entries and looked deliberate.
    /// `every_criterion_has_a_case_here` is the tripwire that makes the next
    /// omission fail rather than pass.
    fn one_per_criterion() -> Vec<Filter> {
        vec![
            Filter {
                kinds: vec![ItemKind::Equipment],
                ..Filter::default()
            },
            Filter {
                names: vec!["ticketrare_name".to_owned()],
                ..Filter::default()
            },
            Filter {
                sets: vec!["set_speed".to_owned()],
                ..Filter::default()
            },
            Filter {
                slots: vec!["helm".to_owned()],
                ..Filter::default()
            },
            Filter {
                required_substats: vec![SubstatReq {
                    name: "speed".to_owned(),
                    min: None,
                }],
                ..Filter::default()
            },
            Filter {
                min_substats: Some(3),
                ..Filter::default()
            },
            Filter {
                max_price: Some(Gold::new(300_000)),
                ..Filter::default()
            },
            Filter {
                min_grade: Some(4),
                ..Filter::default()
            },
        ]
    }

    #[test]
    fn a_restricting_filter_is_never_summarized_as_nothing_selected() {
        // A `min_grade`-only config used to fold up as "nothing selected" while
        // `matches` dropped every gradeless item; a `slots`-only one did the
        // same when the criterion was added.
        for filter in one_per_criterion() {
            assert!(!filter.is_unrestricted());
            assert_ne!(
                hunt_summary(&filter),
                "nothing selected",
                "a filter the loop calls restricted must not fold up as empty: {filter:?}"
            );
        }
    }

    /// The tripwire behind the list above, and the reason it can be trusted.
    ///
    /// Rust cannot enumerate a struct's fields, so the cases are written by
    /// hand and a new criterion is one nobody has to remember. Serializing a
    /// filter carrying every criterion at once yields one TOML key per field,
    /// which IS that enumeration at runtime — so a ninth field breaks this
    /// count and lands the author in the list above before the summary can
    /// silently omit it.
    ///
    /// `include_sold_out` is the one field with no case: it widens rather than
    /// restricts, `is_unrestricted` ignores it, and so must the summary. It is
    /// skipped when false, so a filter that leaves it off writes no key.
    #[test]
    fn every_criterion_has_a_case_here() {
        let all = Filter {
            kinds: vec![ItemKind::Equipment],
            names: vec!["ticketrare_name".to_owned()],
            sets: vec!["set_speed".to_owned()],
            slots: vec!["helm".to_owned()],
            required_substats: vec![SubstatReq {
                name: "speed".to_owned(),
                min: None,
            }],
            min_substats: Some(3),
            max_price: Some(Gold::new(300_000)),
            min_grade: Some(4),
            include_sold_out: false,
        };
        // Counted off the TABLE, not off the text: `required_substats` writes a
        // `[[required_substats]]` array-of-tables whose own `name =` line would
        // be counted as a ninth key by anything reading the serialized string.
        let written = toml::Value::try_from(&all).expect("a filter should serialize");
        let keys = written.as_table().expect("a filter is a table").len();
        assert_eq!(
            keys,
            one_per_criterion().len(),
            "`Filter` grew or lost a criterion — add or drop its case in \
             `one_per_criterion`, and name it in `hunt_summary`:\n{written:?}"
        );
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

    /// The floors offered are the ones a `+0` piece can actually have, which is
    /// the gear-grade domain — the axis [`segmented`]'s doc names. Widen the
    /// loader's grades without widening this list and the control stops
    /// offering a floor the game has.
    #[test]
    fn the_offered_floors_are_the_grades_the_loader_accepts() {
        for (floor, _) in SUBSTAT_FLOORS {
            let filter: Filter = toml::from_str(&format!("min_grade = {floor}"))
                .expect("a floor the control offers must be a grade the game has");
            assert_eq!(filter.min_grade, Some(floor));
        }
        let lowest = SUBSTAT_FLOORS[0].0;
        let highest = SUBSTAT_FLOORS[SUBSTAT_FLOORS.len() - 1].0;
        for outside in [lowest - 1, highest + 1] {
            assert!(
                toml::from_str::<Filter>(&format!("min_grade = {outside}")).is_err(),
                "the list must not stop one short of the domain: {outside}"
            );
        }
    }
}
