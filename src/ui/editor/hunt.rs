//! Hunt: the item-interest criteria — the section's summary and its leaf
//! widgets. Works on a `Filter` field or a scratch buffer handed in by the
//! caller; `hunt_body` stays in the shell because it reaches four drafts.

use eframe::egui;

use super::super::theme;
use super::{arm_optional, count_label, optional_field};
use crate::domain::filter::{Filter, SubstatReq};
use crate::domain::shop::Gold;
use crate::render::kind_label;
use crate::ui::icons::SetIcons;
use crate::uplink::protocol::VocabularyEntry;

/// The tokens the Secret Shop sells: the wire name [`Filter::names`] compares
/// against, the game's own word for it, and its list price in gold.
///
/// **Spelled here rather than fetched.** Three names and three prices are what a
/// developer writes without erring, where the sets are twenty-four the game keeps
/// adding to and the substats hide `acc` behind "Effectiveness" — so those two
/// families stay on the wire and this one does not.
///
/// The words are the game's own (`localization` keys of the same name). The
/// prices were confirmed by the player, and two of the three are corroborated by
/// real captures: `friendpoint_name` at 18,000 seen five times and
/// `ticketrare_name` at 184,000 seen twice, while `ticketspecial_name` never came
/// up in the seventeen rolls sampled.
///
/// **It is not [`crate::render::HAUL_HEADLINERS`], and folding the two together
/// is the mistake to avoid.** That table is a display POLICY for the haul
/// readout — *which* two tokens earn a counter of their own, under a label short
/// enough for a tight row ("Covenant", not "Covenant Bookmarks") — and its length
/// is load-bearing, since `view::ViewState` sizes its tile array off it. This one
/// is the shop's catalogue: every token it sells, in full words, with the price a
/// card shows. Two different questions about the same three wire ids.
const HUNT_TOKENS: [(&str, &str, u32); 3] = [
    ("ticketrare_name", "Covenant Bookmarks", 184_000),
    ("ticketspecial_name", "Mystic Medals", 280_000),
    ("friendpoint_name", "Friendship Points", 18_000),
];

/// The square a gear-set icon draws at.
///
/// A width decision before a visual one: `main.rs` pins the window to
/// [`crate::ui::WINDOW_WIDTH`] as both its minimum and its maximum inner size,
/// so the twenty-four set chips have ~408px to wrap into and no player can
/// widen them out of it. Measured on the live list at that width, now that they
/// wrap at all: labelled they take six rows, at this size three. The wire icons
/// are 44px, so this is a downscale and never an upscale.
const SET_ICON: f32 = 32.0;

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
///
/// It takes the filter and nothing else. The two criteria that have words —
/// the hunted tokens and the rarity floor — read them off [`HUNT_TOKENS`] and
/// [`theme::RARITIES`], so both callers get one wording with nothing to thread:
/// the Setup bar and the idle band cannot disagree, and neither can go quiet
/// because a `catalog` message has not landed. The server's two vocabularies
/// used to ride in as an argument for exactly that job — the third token
/// otherwise reached the status line as `friendpoint_name` — and the constants
/// answer for all three without one.
pub(in crate::ui) fn hunt_summary(filter: &Filter) -> String {
    let mut parts: Vec<String> = Vec::new();
    for name in &filter.names {
        parts.push(token_label(name).to_owned());
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
        parts.push(grade_label(min));
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

/// The words for one hunted name: [`HUNT_TOKENS`]', else the id itself.
///
/// The table names every token the shop sells, so it answers wherever there is
/// an answer — the third token used to fall through the two-entry
/// [`crate::render::HAUL_HEADLINERS`] to its raw `friendpoint_name`, and that
/// table is not consulted here at all now (see [`HUNT_TOKENS`] on why the two
/// stay apart).
///
/// A name it cannot place shows verbatim: `names` is an open field a player's
/// `config.toml` can put anything in, and inventing words for such a criterion
/// would hide which one it is.
fn token_label(name: &str) -> &str {
    HUNT_TOKENS
        .iter()
        .find(|(id, _, _)| *id == name)
        .map_or(name, |(_, label, _)| *label)
}

/// The words for one rarity floor: [`theme::RARITIES`]', else the ordinal
/// itself.
///
/// The table spells the whole of the loader's `2..=5` domain, so the fallback is
/// unreachable from a config file and exists only to keep this total. A floor it
/// cannot name reads as its own ordinal — the criterion is named and the floor
/// is exact, with no word invented for a rarity the game has not published.
///
/// **It answers for the floors the ladder does not offer, and that is the point
/// of naming being wider than offering.** `min_grade = 2` still loads (see
/// [`HUNTED_FLOOR`]) and still drops every gradeless item, so the folded bar has
/// to say `Good+` — the game's own word — rather than `grade 2+`, which is a
/// number a player sees nowhere in the game, or worse "nothing selected" over a
/// hunt that restricts. Narrowing [`theme::RARITIES`] to the two rungs on
/// screen would have bought exactly that regression.
fn grade_label(min: u8) -> String {
    theme::rarity_label(min).map_or_else(|| format!("grade {min}+"), |label| format!("{label}+"))
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
/// Wrapped, because the sets list runs to twenty-four entries.
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
    ui.label(theme::section(label));
    // The salt sits on the ROW and never on a chip, and that placement is the
    // whole of this function's layout. `push_id` builds a child `Ui` off
    // `available_rect_before_wrap` and closes it with `advance_cursor_after_rect`;
    // neither reaches `Layout::next_frame`, which is where `main_wrap` lives, so
    // a chip drawn inside one is invisible to the wrap and simply takes whatever
    // the cursor has left. Measured on the live 24-set catalog at the window's
    // fixed 440px, with the salt still per chip: all twenty-four laid out on ONE
    // row out to x=1070, the fifth squeezed into the pixels before the edge with
    // its label broken one letter per line, and nineteen off-screen.
    //
    // Chips are therefore added straight to the wrapping `Ui`, whose own auto-id
    // counter makes them distinct by construction. What the salt still buys is a
    // row named rather than counted: `offered_list` draws a chip row or a
    // free-text list depending on what the server published, so the draw order
    // the default `Ui` id folds in is not a constant, and two criteria can offer
    // the same words.
    ui.push_id(label, |ui| {
        ui.horizontal_wrapped(|ui| {
            for entry in choices {
                let mut on = values.contains(&entry.id);
                let changed = match icons.get(&entry.id) {
                    Some(texture) => icon_chip(ui, texture, &entry.label, &mut on),
                    None => text_chip(ui, &entry.label, &mut on),
                };
                if changed {
                    if on {
                        values.push(entry.id.clone());
                    } else {
                        values.retain(|kept| *kept != entry.id);
                    }
                }
            }
        });
    });
    unoffered_rows(ui, label, values, String::as_str, |id| {
        choices.iter().any(|entry| entry.id == id)
    });
}

/// One offered value drawn as its name: a toggling pill.
///
/// It sits beside [`icon_chip`] because the two are one decision — a value the
/// server sent a picture for draws the picture, every other draws this — and a
/// reader comparing them has to see both the skin they share ([`theme::chip`])
/// and the accessibility contract they each restate.
///
/// **The name is written back explicitly**, for the reason [`icon_chip`] gives
/// at length: [`egui::Button`] states a `WidgetType::Button`, and this control
/// toggles a value in a list, which is a checkbox. The stock `ui.checkbox` it
/// replaces published exactly that, so restating it is what keeps the node it
/// contributed identical — same role, same name, same selected flag — while
/// only the paint changes.
fn text_chip(ui: &mut egui::Ui, label: &str, on: &mut bool) -> bool {
    let response = theme::chip(ui, egui::Button::new(label), *on);
    if response.clicked() {
        *on = !*on;
    }
    // After the toggle, so the node states the value the click produced rather
    // than the one it replaced.
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, label)
    });
    response.clicked()
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
    let response = theme::chip(ui, egui::Button::image(picture), *on).on_hover_text(label);
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
/// It writes the wire id [`Filter::matches`] compares against. The label is the
/// game's words and never reaches the filter.
///
/// **It draws in every session**, where it used to give way to a free-text list
/// against a server with no Catalog: [`HUNT_TOKENS`] is this end's own, so there
/// is no state in which the cards cannot be built. That is the whole of what the
/// fallback bought, and it is why the name criterion no longer has one.
pub(super) fn token_cards(ui: &mut egui::Ui, names: &mut Vec<String>) {
    ui.label(theme::section("tokens"));
    for (id, label, price) in HUNT_TOKENS {
        let mut on = names.iter().any(|name| name == id);
        // Salted per token for the reason `choice_list` is salted per value:
        // `hunt_body` draws several groups on one `Ui`, and `push_id` salts that
        // shared parent. Safe here where it was not on a chip row: the cards
        // stack vertically, so no wrap can be blinded by the child `Ui`.
        let changed = ui
            .push_id(egui::Id::new(("token", id)), |ui| {
                token_card(ui, label, Gold::new(price), &mut on)
            })
            .inner;
        if changed {
            if on {
                names.push(id.to_owned());
            } else {
                names.retain(|kept| kept != id);
            }
        }
    }
    unoffered_rows(ui, "tokens", names, String::as_str, |name| {
        HUNT_TOKENS.iter().any(|(id, _, _)| *id == name)
    });
}

/// The height of a token card: a body line over a small one, plus the padding
/// around them — `SP_SM` above, a 14px name, an 11px price, `SP_XS` below.
///
/// Spelled rather than derived from the two galleys, because the card is
/// allocated before either is laid out, and a height that followed its text
/// would step between tokens whose names measure differently.
const CARD_HEIGHT: f32 = 46.0;

/// One token as a bordered box: its name over its price, the whole box
/// clickable, accent-filled once chosen.
///
/// **The name is PAINTED and the price is a real label, and that split is
/// forced.** The card contributes one named node — its own, restated as the
/// checkbox it behaves like — so drawing the name as a widget too would put a
/// second node under the same name, and `get_by_label` answers a duplicate by
/// panicking rather than by picking. The price is the mirror case: it has to
/// stay findable and announced in its own right, and painted text publishes
/// nothing at all, so it is the one thing here that stays a widget.
///
/// Nothing else may become one. A second label inside this rect is a second
/// node, and if it ever carries the token's words it breaks the count.
///
/// The fill is read off [`theme::toggle_skin`] rather than picked here, so a
/// card and a chip cannot disagree about what "chosen" looks like.
///
/// The price is no longer optional: it came off a wire field that could be
/// absent, and it now comes off [`HUNT_TOKENS`], where every row has one.
fn token_card(ui: &mut egui::Ui, label: &str, price: Gold, on: &mut bool) -> bool {
    let width = ui.available_width();
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, CARD_HEIGHT), egui::Sense::click());
    if response.clicked() {
        *on = !*on;
    }
    // After the toggle, so the node states the value the click produced.
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, label)
    });

    // `contains_pointer` and not `hovered`: the price label below is allocated
    // after this rect and so hit-tests above it, which would drop the card's
    // own hover for the width of the price. The question here is geometric —
    // is the pointer on this card — and that is what this answers.
    let (fill, edge) = theme::toggle_skin(*on, response.contains_pointer());
    ui.painter().rect(
        rect,
        egui::CornerRadius::same(8),
        fill,
        edge,
        egui::StrokeKind::Inside,
    );

    // Full ink on a chosen card, one level down otherwise — the ladder every
    // other control in this section uses. The price sits a further step back on
    // both, since the name is what is being chosen.
    let (name_ink, price_ink) = if *on {
        (theme::INK, theme::INK_MUTED)
    } else {
        (theme::INK_MUTED, theme::INK_FAINT)
    };
    // Resolved off `TextStyle::Body` rather than spelled 14.0, so a retune in
    // `theme::apply` carries the name with it — the price below reads the same
    // scale through `.small()`.
    let name_rect = ui.painter().text(
        egui::pos2(rect.left() + theme::SP_MD, rect.top() + theme::SP_SM),
        egui::Align2::LEFT_TOP,
        label,
        egui::TextStyle::Body.resolve(ui.style()),
        name_ink,
    );
    // Off the name's own painted rect, not a measured offset: the second line
    // follows the first wherever the body size puts it.
    let line = egui::Rect::from_min_max(
        egui::pos2(rect.left() + theme::SP_MD, name_rect.bottom()),
        egui::pos2(rect.right() - theme::SP_MD, rect.bottom() - theme::SP_XS),
    );
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(line)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
        |ui| {
            // `Gold` groups itself, so it reads like the shop table's price
            // column.
            ui.label(
                egui::RichText::new(format!("{price} gold"))
                    .small()
                    .color(price_ink),
            );
        },
    );
    response.clicked()
}

/// The lowest rarity the ladder offers.
///
/// **The two rungs are Heroic and Epic, and `min_grade` is a FLOOR.** Heroic
/// writes 4 and Epic writes 5, so Heroic already admits every Epic piece — the
/// ladder is "this good or better" and never "exactly this". The rungs are not
/// a pair of buckets and nothing here is missing a third one for "either".
///
/// Good (2) and Rare (3) are dropped because nobody hunts them out of the refresh
/// shop: a paid re-roll spent on a blue piece is a re-roll wasted, so a control
/// offering that floor invites a hunt the player did not mean. They are still
/// FLOORS the loader takes and [`theme::RARITIES`] still names them — a
/// `config.toml` carrying one keeps filtering and keeps being named in the folded
/// bar, see [`grade_label`]. What went is the control, not the criterion.
const HUNTED_FLOOR: u8 = 4;

/// The rarity floor, as one segmented control: the rungs of [`theme::RARITIES`]
/// worth hunting, low to high, in the game's own words.
///
/// Buttons rather than a `ComboBox` for the reason `choice_list` gives: a combo
/// contributes no accessibility node. Clicking the active segment clears the
/// floor, so every state the control can reach is reachable back out of.
///
/// [`theme::segmented_strip`] and not a bare row: the choice is exclusive, and
/// the Click-timing mode row one section down already says so with a shared
/// ground. One grammar for one kind of control.
///
/// **Plain text, no per-rarity ink.** The words used to be painted in the
/// game's own blue, purple and red (see [`theme::RARITIES`], which records the
/// measured values and where they came from). Two rungs do not need three hues
/// to be told apart, and every other control on this screen is one ink — a
/// coloured word here read as an alarm rather than as a rarity. The chosen cell
/// takes the strip's own selection fill, which is the only colour the control
/// carries and the same one every chip in the section uses.
///
/// **It writes `min_grade`, where this control used to write `min_substats`.**
/// The two were taken for one axis on shop gear, measured at 59 of 59 real
/// pieces — and that sample could not hold the case that separates them.
/// Heroic and Epic BOTH roll four substats (150 grade-5 rows at
/// `sub_stat_count` 4..4), so a substat ladder tops out at "Heroic or better"
/// and can never ask for Epic, which is what a player hunting purple-or-red
/// actually wants. `min_substats` is a genuinely different question and stays
/// loadable from `config.toml`; see its own doc.
///
/// **The cells are spelled at this end, and so the ladder draws in every
/// session.** They used to come off the catalog's rarity family, which meant no
/// control at all until a `catalog` message landed — a criterion whose values
/// are ordinals a player must not have to type, reachable only by editing
/// `config.toml`.
pub(super) fn rarity_ladder(ui: &mut egui::Ui, value: &mut Option<u8>) {
    theme::segmented_strip(ui, |ui| {
        for (grade, label) in theme::RARITIES {
            if grade < HUNTED_FLOOR {
                continue;
            }
            let on = *value == Some(grade);
            // The accessible name comes from the label like any
            // `selectable_label`, so nothing has to be restated — unlike
            // [`icon_chip`], whose picture publishes none.
            if ui.selectable_label(on, label).clicked() {
                *value = if on { None } else { Some(grade) };
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
    ui.label(theme::section(label));
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
    ui.label(theme::section("required substats"));
    let mut toggled = None;
    // Salted on the row and not per chip, for the reason `choice_list` states:
    // a `push_id` child never reaches `Layout::next_frame`, so anything drawn
    // inside one is invisible to `horizontal_wrapped`'s wrap. This row carried
    // the same defect and was NOT merely at risk of it — eleven realistic
    // substat labels reached x=608 at the window's fixed 440px, against 414 once
    // the salt moved. It shows less than the sets row only because it is
    // shorter; a ticked chip unfolding its `≥` box and stepper adds width again.
    //
    // Those two now sit directly in the row rather than in a `push_id` of their
    // own, so a wrap can fall between them. That is the cost of the fix here and
    // it is the cheap side of the trade: the pair is ~64px, and grouping them
    // was what put the stepper past the edge in the first place.
    ui.push_id("required substats", |ui| {
        ui.horizontal_wrapped(|ui| {
            for entry in choices {
                let held = reqs.iter().position(|req| req.name == entry.id);
                let mut on = held.is_some();
                if text_chip(ui, &entry.label, &mut on) {
                    toggled = Some(entry.id.clone());
                }
                // The threshold belongs to the chip, so it appears beside the one
                // it applies to and nowhere else.
                if let Some(index) = held {
                    let req = &mut reqs[index];
                    let mut has_min = req.min.is_some();
                    // A chip and not a checkbox: it rides in the same wrapped
                    // row as the values above, where it would otherwise be the
                    // one tick box left among pills.
                    text_chip(ui, "≥", &mut has_min);
                    if has_min {
                        let min = req.min.get_or_insert(seed_for(entry.percent));
                        threshold_field(ui, min, entry.percent);
                    } else {
                        req.min = None;
                    }
                }
            }
        });
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
        assert_eq!(hunt_summary(&named), "Covenant Bookmarks");
        assert_eq!(hunt_summary(&Filter::default()), "nothing selected");
    }

    fn hunting(name: &str) -> Filter {
        Filter {
            names: vec![name.to_owned()],
            ..Filter::default()
        }
    }

    /// Every token the shop sells reads as words, the third one included.
    ///
    /// That third one is the defect the table closes: `HAUL_HEADLINERS` names
    /// two, so `friendpoint_name` used to fall through it to its raw id — the
    /// status line read `Hunting …, friendpoint_name` over a Hunt block that
    /// said "Friendship Points" one panel down.
    #[test]
    fn every_token_the_shop_sells_reads_as_words() {
        for (id, label, _) in HUNT_TOKENS {
            assert_eq!(hunt_summary(&hunting(id)), label);
        }
        // A name nobody can place shows verbatim: it is a criterion the player
        // typed, and words invented for it would hide which one it is.
        assert_eq!(hunt_summary(&hunting("ecq4h_name")), "ecq4h_name");
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
        // The rarity reads as the ladder two panels down spells it: the same
        // criterion named twice on one screen used to read "grade 4+" here and
        // "Heroic" there.
        assert_eq!(hunt_summary(&filter), "3+ substats, ≤300,000 gold, Heroic+");
        // The converse: `min_substats = 0` restricts nothing, so the bar must
        // keep calling it an empty hunt.
        let inert = Filter {
            min_substats: Some(0),
            ..Filter::default()
        };
        assert!(inert.is_unrestricted());
        assert_eq!(hunt_summary(&inert), "nothing selected");
    }

    /// The bar and the ladder name one criterion one way.
    ///
    /// The folded Hunt bar said `grade 5+` over a control spelling Good / Rare /
    /// Heroic / Epic, so the same floor carried two names on one screen — and
    /// the number was the one nobody sees anywhere else in the game.
    ///
    /// All four, though the ladder offers two: `Good+` and `Rare+` are what a
    /// `config.toml` floor below [`HUNTED_FLOOR`] reads as, and they are the
    /// case that would have gone back to an ordinal if the naming table had been
    /// cut down to the control.
    #[test]
    fn a_rarity_floor_reads_in_the_words_the_ladder_offers() {
        for (id, label) in [(2, "Good+"), (3, "Rare+"), (4, "Heroic+"), (5, "Epic+")] {
            let filter = Filter {
                min_grade: Some(id),
                ..Filter::default()
            };
            assert_eq!(hunt_summary(&filter), label);
        }
    }

    /// A floor [`theme::RARITIES`] does not name still names its criterion, as
    /// the ordinal `config.toml` would spell.
    ///
    /// Unreachable from a config file — the loader takes `2..=5` and the table
    /// spells all four — so what this pins is that `grade_label` stays total
    /// rather than guessing a word for a rarity the game has not published.
    #[test]
    fn a_rarity_the_table_cannot_name_still_names_its_criterion() {
        let mythic = Filter {
            min_grade: Some(9),
            ..Filter::default()
        };
        assert_eq!(hunt_summary(&mythic), "grade 9+");
        assert!(
            toml::from_str::<Filter>("min_grade = 9").is_err(),
            "and the loader is what keeps that branch off the config path"
        );
    }

    /// The rarities the window can NAME are exactly the floors the loader
    /// accepts, and this is the UI half of that pairing — the domain half is
    /// `min_grade_outside_the_game_domain_is_refused` in `domain::filter`.
    ///
    /// The two must move together in both directions. A grade the window names
    /// and the loader refuses is a word for a `config.toml` the app cannot start
    /// from; a grade the loader accepts and the table cannot name is a floor the
    /// folded bar can only spell as an ordinal.
    #[test]
    fn the_named_rarities_are_the_grades_the_loader_accepts() {
        for (grade, _) in theme::RARITIES {
            let filter: Filter = toml::from_str(&format!("min_grade = {grade}"))
                .expect("a rarity the window names must be a grade the game has");
            assert_eq!(filter.min_grade, Some(grade));
        }
        let lowest = theme::RARITIES[0].0;
        let highest = theme::RARITIES[theme::RARITIES.len() - 1].0;
        for outside in [lowest - 1, highest + 1] {
            assert!(
                toml::from_str::<Filter>(&format!("min_grade = {outside}")).is_err(),
                "the table must not stop one short of the domain: {outside}"
            );
        }
    }

    /// The ladder offers the top of that domain and stops short of its bottom,
    /// on purpose — and the floors it drops are still floors.
    ///
    /// Offering is narrower than naming, which is the whole shape of this
    /// change: a rung the ladder draws has to be a grade the file accepts, while
    /// a grade the file accepts need not be a rung. What must NOT drift is the
    /// top — a rarity the game publishes above Epic that the ladder cannot ask
    /// for is a hunt only reachable by editing the file, which is what put this
    /// control on screen in the first place.
    #[test]
    fn the_ladder_offers_the_top_of_the_domain_and_stops_short_of_its_bottom() {
        let offered: Vec<(u8, &str)> = theme::RARITIES
            .into_iter()
            .filter(|(grade, _)| *grade >= HUNTED_FLOOR)
            .collect();
        assert_eq!(offered, vec![(4, "Heroic"), (5, "Epic")]);
        let highest = theme::RARITIES[theme::RARITIES.len() - 1].0;
        assert_eq!(
            offered[offered.len() - 1].0,
            highest,
            "the ladder must reach the top rarity the game has"
        );
        // And the two rungs it dropped still load, so the criterion survives the
        // control leaving.
        for dropped in [2, 3] {
            let filter: Filter = toml::from_str(&format!("min_grade = {dropped}"))
                .expect("a floor the ladder stopped offering is still a floor");
            assert_eq!(filter.min_grade, Some(dropped));
        }
    }

    /// Epic is what the reversal bought, so it gets its own line: the ladder's
    /// top cell has to be a floor the file accepts, where the substat ladder it
    /// replaced could only ever ask for four substats — which Heroic already
    /// has.
    #[test]
    fn the_top_rarity_is_epic_and_it_loads() {
        let (top, label) = theme::RARITIES[theme::RARITIES.len() - 1];
        assert_eq!(top, 5);
        assert_eq!(label, "Epic");
        let filter: Filter = toml::from_str("min_grade = 5").expect("Epic is a floor");
        assert_eq!(filter.min_grade, Some(5));
    }

    /// Heroic is a FLOOR and not a bucket: it admits Epic pieces too, which is
    /// why two rungs need no third cell for "either".
    ///
    /// Stated here because the ladder writes the ordinal and nothing on screen
    /// says what the ordinal means — a reader looking at two cells could take
    /// them for two disjoint choices and "fix" the inclusion.
    #[test]
    fn the_lower_rung_admits_the_higher_ones_pieces() {
        use crate::domain::shop::ShopItem;
        let heroic_or_better = Filter {
            min_grade: Some(HUNTED_FLOOR),
            ..Filter::default()
        };
        let epic = ShopItem {
            kind: ItemKind::Equipment,
            grade: Some(5),
            ..ShopItem::default()
        };
        assert!(heroic_or_better.matches(&epic));
    }
}
