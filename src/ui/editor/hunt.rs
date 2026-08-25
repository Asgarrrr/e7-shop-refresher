//! Hunt: the item-interest criteria — the section's summary and its leaf
//! widgets. Works on a `Filter` field or a scratch buffer handed in by the
//! caller; `hunt_body` stays in the shell because it reaches four drafts.

use eframe::egui;

use super::super::theme;
use super::{arm_optional, count_label, optional_field};
use crate::domain::filter::{Filter, GearRule, SubstatMatch, SubstatReq};
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

/// What the shop rolls for each substat: the lowest and the highest value it
/// has been seen to sell, in the unit the WIRE sends — a fraction for the
/// percent-bearing ones, a whole number for the rest.
///
/// The low end is where a freshly armed `≥` starts, so arming a requirement
/// excludes nothing and every step from there restricts. The high end is where
/// its field stops: a threshold above the top roll is a hunt that cannot fire,
/// and a drag that can reach one is a trap the control need not offer.
///
/// **Measured on the wire, on 7,941 pieces of gear across 2,428 captured
/// `random_shop` responses** (`E7_Datamine/mitm_tcp3333_live/by_cmd`, all four
/// grades, Epic included). Every one of the 19,628 values is a **whole number in
/// the unit the game shows** — 0 exceptions, and no zeroes either. That is what
/// [`threshold_field`] rests on: a substat is not a continuous quantity, and a
/// control that steps it in tenths offers two thresholds out of three that no
/// roll can tell apart.
///
/// **The range spans the piece's MAIN stat as well, because the list does.** The
/// wire's `data.op` leads with the main stat — it is `op[0]` on all 7,941 items,
/// it is the part's own stat every time (`att` on a weapon, `max_hp` on a helm),
/// and it takes one of exactly three values per part, one per item level the
/// shop sells: 66/88/100 for a weapon's attack, 352/472/540 for a helm's health.
/// `data.g` counts it too, which is why grade 2/3/4/5 gives `op` lengths
/// 2/3/4/5 for 1/2/3/4 rolled substats. The rolled substats alone stop far
/// lower — `att` at 45, `max_hp` at 190, `speed` at 4 — so a ceiling taken from
/// them would refuse `speed ≥ 6`, which is a hunt for speed boots and the one
/// most players open this window to set up. [`crate::domain::filter::SubstatReq`]
/// scans the whole list, so the field's domain is the whole list's.
///
/// **Not read off `db_equip_stat`, and that is a correction rather than a
/// preference.** Joining that table's `val_min`/`val_max` through
/// `db_equip_item.sub_stat` gives a domain three to five times too small — `att`
/// to 32 where a weapon's own attack reads 100 — so whatever those rows
/// describe, it is not what a shop piece carries on the wire. The wire is the
/// authority here; the DB was the plausible-looking wrong source.
///
/// **One shop level, and that is the caveat.** All 2,428 rolls were taken at
/// `rshop_level` 13, where the shop sells gear of level 55, 70 and 85. The
/// ceilings above are that level-85 top row; a shop level the player has not
/// reached could sell higher. Which is why the CEILING is a convenience the
/// field clamps to and never a filtering rule, and why [`SHOWN_FLOOR`] rather
/// than this table's low end is what a field floors at.
///
/// Ids and not labels, because a label is a translation — the same reason
/// [`VocabularyEntry::percent`] is a flag on the wire rather than a "(%)" read
/// off the words.
const SUBSTAT_RANGE: [(&str, f64, f64); 11] = [
    ("att", 18.0, 45.0),
    ("att_rate", 0.03, 0.08),
    ("def", 15.0, 32.0),
    ("def_rate", 0.03, 0.08),
    ("max_hp", 88.0, 190.0),
    ("max_hp_rate", 0.03, 0.08),
    ("speed", 1.0, 4.0),
    ("cri", 0.02, 0.05),
    ("cri_dmg", 0.03, 0.07),
    ("acc", 0.03, 0.08),
    ("res", 0.03, 0.08),
];

/// The range for a substat [`SUBSTAT_RANGE`] does not name: one unit up from
/// nothing, and open to the top of its family's unit.
///
/// Reachable from a `config.toml` naming a stat the game has since renamed, and
/// from a catalog that offers one. Deliberately the widest range rather than a
/// guessed one — this end cannot know what such a stat rolls, and a seed or a
/// cap invented for it would be a number with nothing behind it.
const UNNAMED_SEED_PERCENT: f64 = 0.01;
const UNNAMED_SEED_WHOLE: f64 = 1.0;

/// The floor of every threshold field, in the unit it SHOWS.
///
/// One and not the substat's own lowest roll, though that number is right
/// there in [`SUBSTAT_RANGE`]: the shop's gear scales with the player's shop
/// level and every captured roll comes from one level, so flooring at the
/// measured minimum would forbid exactly the thresholds a weaker shop needs —
/// `max_hp ≥ 40` is meaningless at level 13 and is the whole useful range
/// somewhere below it. What this rules out is what the player asked it to: a
/// threshold of zero, or of less.
const SHOWN_FLOOR: i32 = 1;

/// The top of a percent field with no measured ceiling. Not a roll and not a
/// measurement: a rate is a fraction of a whole, and no piece of gear can carry
/// more than the whole of one.
const PERCENT_CEILING: f64 = 1.0;

/// The range for one substat: [`SUBSTAT_RANGE`]'s, else its family's widest.
///
/// One spelling, called from the chip (which holds the entry) and from the
/// normalising pass (which has to look the entry up).
fn range_for(id: &str, percent: bool) -> (f64, f64) {
    SUBSTAT_RANGE
        .iter()
        .find(|(name, _, _)| *name == id)
        .map_or_else(
            || {
                if percent {
                    (UNNAMED_SEED_PERCENT, PERCENT_CEILING)
                } else {
                    (UNNAMED_SEED_WHOLE, f64::from(i32::MAX))
                }
            },
            |(_, seed, ceiling)| (*seed, *ceiling),
        )
}

/// Where a freshly armed `≥` starts: the lowest roll the shop sells for that
/// substat.
fn seed_for(id: &str, percent: bool) -> f64 {
    range_for(id, percent).0
}

/// Drag steps, in the unit the field SHOWS. Both cross their domain in a few
/// hundred pixels: the percent one is ten points wide, the whole one runs to
/// the hundreds.
const PERCENT_STEP: f64 = 0.1;
const WHOLE_STEP: f64 = 0.5;

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
    // One rule reads as itself, several as a tally: three pieces' worth of
    // criteria spelled out would crowd every other part off the bar, and the
    // body is one click away.
    match filter.gear.as_slice() {
        [] => {}
        [only] => parts.extend(rule_parts(only)),
        several => parts.push(count_label(several.len(), "piece", "pieces")),
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

/// One gear rule's criteria, in the words the folded bar uses.
///
/// Every criterion [`GearRule::restricts`] counts has to appear, or a rule that
/// arms the loop can fold up as nothing — the defect `min_grade` and `slots`
/// each shipped once, and `every_criterion_has_a_case_here` is the tripwire.
fn rule_parts(rule: &GearRule) -> Vec<String> {
    let mut parts = Vec::new();
    if !rule.sets.is_empty() {
        parts.push(count_label(rule.sets.len(), "set", "sets"));
    }
    if let Some(slot) = &rule.slot {
        // NAMED and not counted, unlike the sets and stats around it, because
        // there is exactly one of it — "1 slot" would spend a summary line
        // saying only that a part was chosen. The words are the wire's own id
        // (`boot`, `neck`), since this takes the filter and nothing else and the
        // game's word for a part is the server's to send; the ids are short and
        // read as the part, where `acc` reads "Effectiveness". A reader who
        // wants the game's word opens the section, where the strip paints it.
        parts.push(slot.clone());
    }
    if !rule.mains.is_empty() {
        // Counted and not named, like the sets and slots above it: the words
        // for a stat are the server's (`acc` reads "Effectiveness"), and this
        // takes the filter and nothing else.
        parts.push(count_label(rule.mains.len(), "main stat", "main stats"));
    }
    if !rule.required_substats.is_empty() {
        let tally = count_label(rule.required_substats.len(), "substat", "substats");
        // The mode is named only where it changes the predicate. Over one
        // requirement `all` and `any` are the same question, and "any of 1
        // substat" would be a distinction the engine does not make.
        parts.push(
            if rule.substat_match == SubstatMatch::Any && rule.required_substats.len() > 1 {
                format!("any of {tally}")
            } else {
                tally
            },
        );
    }
    // `Some(0)` constrains nothing and `restricts` refuses to count it, so
    // naming it would be the mirror-image lie.
    if let Some(min) = rule.min_substats.filter(|min| *min > 0) {
        parts.push(format!("{min}+ substats"));
    }
    if let Some(max) = rule.max_price {
        // `Gold` groups itself, so this reads like the shop table's price
        // column rather than a bare seven-digit number.
        parts.push(format!("≤{max} gold"));
    }
    if let Some(min) = rule.min_grade {
        parts.push(grade_label(min));
    }
    parts
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
///
/// `qualifier` is what a row adds to its chips' accessible names, and it exists
/// because two rows on this surface read the SAME vocabulary: the main stat and
/// the required substats are both picked out of `FilterVocabulary::substats`.
/// Left unqualified they publish eleven pairs of identically named checkboxes —
/// a screen reader announces "Speed" twice with nothing between them saying
/// which is the piece and which is the roll, and `get_by_label` answers a
/// duplicate by panicking rather than by picking (see [`token_card`], which
/// carries the same rule for the opposite reason). It qualifies the NAME and
/// never the chip's own words: the window is pinned at
/// [`crate::ui::WINDOW_WIDTH`] and eleven chips wearing "Speed main stat" would
/// not fit the row.
pub(super) fn choice_list(
    ui: &mut egui::Ui,
    label: &str,
    values: &mut Vec<String>,
    choices: &[VocabularyEntry],
    icons: &SetIcons,
    qualifier: Option<&str>,
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
                    Some(texture) => icon_chip(ui, texture, &entry.label, qualifier, &mut on),
                    None => text_chip(ui, &entry.label, qualifier, &mut on),
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
fn text_chip(ui: &mut egui::Ui, label: &str, qualifier: Option<&str>, on: &mut bool) -> bool {
    let response = theme::chip(ui, egui::Button::new(label), *on);
    if response.clicked() {
        *on = !*on;
    }
    // After the toggle, so the node states the value the click produced rather
    // than the one it replaced.
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Checkbox,
            ui.is_enabled(),
            *on,
            chip_name(label, qualifier),
        )
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
fn icon_chip(
    ui: &mut egui::Ui,
    texture: &egui::TextureHandle,
    label: &str,
    qualifier: Option<&str>,
    on: &mut bool,
) -> bool {
    let picture = egui::Image::new(egui::load::SizedTexture::from_handle(texture))
        .fit_to_exact_size(egui::vec2(SET_ICON, SET_ICON));
    let response = theme::chip(ui, egui::Button::image(picture), *on).on_hover_text(label);
    if response.clicked() {
        *on = !*on;
    }
    // After the toggle above, so the node states the value the click produced
    // rather than the one it replaced.
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Checkbox,
            ui.is_enabled(),
            *on,
            chip_name(label, qualifier),
        )
    });
    response.clicked()
}

/// One chip's accessible name: its own words, plus the criterion's where two
/// rows offer the same ones ([`choice_list`]'s `qualifier`).
///
/// Built inside the [`egui::Response::widget_info`] closure at both call sites,
/// which is what makes the allocation free: egui runs that closure only while
/// AccessKit is live, a harness is reading, or the widget was just clicked —
/// never on the sixty frames a second nobody is listening to.
///
/// The value's own words LEAD, so the name opens with the label the chip paints.
/// A name starting with the criterion would announce eleven chips as "main stat
/// …" before saying which one, and would stop carrying its visible label as a
/// prefix.
fn chip_name(label: &str, qualifier: Option<&str>) -> String {
    qualifier.map_or_else(
        || label.to_owned(),
        |qualifier| format!("{label} {qualifier}"),
    )
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

/// The word a piece that names no part wears.
///
/// It needs one at all because the cell has to say something, and "any part" is
/// what an unset [`GearRule::slot`] means: the rule's other criteria apply to
/// every piece of gear on the board.
const ANY_PART: &str = "Any part";

/// The strip cell that drafts a piece nobody has committed to yet.
///
/// The `+` alone was mute beside cells that now carry words, and the verb is
/// what tells a reader the cell adds rather than selects — the second half of
/// the same defect the numbered cells had.
const ADD_PIECE: &str = "+ add";

/// The word for the part a piece names: the game's own where the server sent
/// one, the raw id where it did not, [`ANY_PART`] where no part is named.
///
/// The id is the fallback for the reason [`token_label`]'s is — a criterion the
/// vocabulary cannot place has to stay readable as the thing it is, and a word
/// invented for it would hide which part is being hunted. That case is also the
/// whole of what a Catalog-less server can show, since the words for the six
/// parts are the server's and not this end's (see [`HUNT_TOKENS`] on which two
/// families are spelled here and why the slots are not among them).
pub(super) fn part_label(slot: Option<&str>, parts: &[VocabularyEntry]) -> String {
    let Some(id) = slot else {
        return ANY_PART.to_owned();
    };
    parts
        .iter()
        .find(|entry| entry.id == id)
        .map_or_else(|| id.to_owned(), |entry| entry.label.clone())
}

/// The pieces being hunted, as one segmented strip: a cell per rule wearing the
/// name of its part, a `+ add` cell to draft one, and a `remove` verb acting on
/// the one on screen.
///
/// A strip and not a stack of cards, because the window is pinned at
/// [`crate::ui::WINDOW_WIDTH`] and one rule's criteria already fill a panel:
/// three rules unfolded at once would be three screens of scrolling to compare
/// two chips. The strip is the same grammar the rarity ladder and the timing
/// presets wear, so an exclusive choice looks like every other one here.
///
/// **The cells carry their part's name, where they used to carry `1` and `2`.**
/// A number says nothing about what the cell holds, so clicking through was the
/// only way to learn which piece was which — and a strip reading `[Ring]
/// [Necklace]` needs no legend at all. It is the same criterion the slot ladder
/// one block down writes, which is what makes the strip a readout of the choice
/// rather than a second one.
///
/// **Every cell's accessible name carries its POSITION, and the painted words
/// never do.** Two pieces may legitimately name the same part — a ring of one
/// set beside a ring of another — and two may name none, so the words alone are
/// not unique on a surface where `get_by_label` answers a duplicate by panicking
/// rather than by picking. The main-stat row answered its own collision the same
/// way, with a qualifier on the name and not on the chip ([`choice_list`]); here
/// the qualifier has to be the position, because that is the only thing that
/// tells two cells of one part apart. It also settles the collision with the
/// slot ladder below, whose cells wear exactly these words unqualified.
///
/// The `+ add` cell only moves the index past the last rule — see
/// `EditorState::gear_index` — and `remove` is a [`theme::bare_verb`] rather
/// than a cell, because it acts on the selection instead of being one.
///
/// **The verb sits on the header line and the strip takes a row of its own, and
/// that is the width decision.** A cell wearing "Necklace" is four times the
/// width of `1`, and the number of cells is unbounded — two rules may name one
/// part, so a hunt is not capped at the six the game wears. Sharing one line,
/// the row went past the window at EIGHT pieces of the longest part name
/// (`remove` ending at 447px against 440) and the strip's own cells at nine. On
/// its own row the strip wraps inside its frame instead
/// ([`theme::segmented_strip`]), so no count can push anything out, and the verb
/// stops sliding rightwards as pieces are added.
/// `the_piece_strip_stays_inside_the_window` pins both halves. Measured at three
/// pieces, which is what a real hunt looks like: the strip ends at 176px of the
/// 440 and the verb sits at 388..432; the strip breaks its line at nine cells.
pub(super) fn piece_strip(
    ui: &mut egui::Ui,
    rules: &mut Vec<GearRule>,
    index: &mut usize,
    parts: &[VocabularyEntry],
) {
    // A rule a config file holds and the window cannot reach would filter while
    // being invisible and unremovable — the defect `unoffered_rows` exists to
    // close for a value the vocabulary cannot name. Clamped rather than
    // asserted: the count changes under the index whenever a rule is removed.
    *index = (*index).min(rules.len());
    ui.horizontal(|ui| {
        ui.label(theme::section("pieces"));
        // Only over a rule that exists: the draft cell has nothing to remove,
        // and a disabled verb beside it would be a control that never acts.
        if *index < rules.len() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::bare_verb(ui, "remove").clicked() {
                    rules.remove(*index);
                    *index = (*index).min(rules.len());
                }
            });
        }
    });
    theme::segmented_strip(ui, |ui| {
        // One cell per rule, then the one being drafted — `None` is the cell
        // past the last rule, and the only one with no position to be named by.
        for (cell, rule) in rules.iter().map(Some).chain([None]).enumerate() {
            let label = rule.map_or_else(
                || ADD_PIECE.to_owned(),
                |rule| part_label(rule.slot.as_deref(), parts),
            );
            let on = *index == cell;
            let enabled = ui.is_enabled();
            let response = ui.selectable_label(on, egui::RichText::new(&label).small());
            // Restated for the reason [`text_chip`] carries, with the position
            // folded in: a `selectable_label` names itself after the words it
            // paints, and those are not unique here.
            response.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::SelectableLabel,
                    enabled,
                    on,
                    piece_name(&label, rule.is_some().then_some(cell + 1)),
                )
            });
            if response.clicked() {
                *index = cell;
            }
        }
    });
    ui.add_space(theme::SP_SM);
}

/// One strip cell's accessible name: its own words, then its position — the
/// order [`chip_name`] states, so the name still opens with what the cell
/// paints. The draft cell has no position, because it is not a piece yet.
fn piece_name(label: &str, ordinal: Option<usize>) -> String {
    ordinal.map_or_else(
        || label.to_owned(),
        |position| format!("{label} piece {position}"),
    )
}

/// The part a piece is, as one exclusive strip: a cell per wearable slot the
/// server named, in the game's own words.
///
/// [`theme::segmented_strip`] and not a chip row, because a piece has exactly
/// one part. A chip row says "any of these", and that reading is the defect
/// [`GearRule::slot`] closed — ticking two slots beside two main stats bought
/// four combinations where the player meant two. The rarity ladder one block
/// down already wears this grammar, and clicking the active cell clears the
/// part, so every state the control reaches is reachable back out of.
///
/// **A slot the vocabulary cannot name gets a cell of its own, spelled as the
/// raw id.** [`unoffered_rows`] is what does that everywhere else on this
/// surface and cannot do it here: those rows edit a `Vec`, and this criterion is
/// one value. Folding the stray into the strip answers the same need with no
/// second widget — the criterion is visible, and the click that clears it is the
/// way out.
///
/// Against a server with no Catalog and no part held there is nothing to offer,
/// so the strip is skipped rather than painted as an empty box. That withdraws
/// the typed entry the free-text fallback used to give this criterion; the piece
/// strip above is what keeps a config-set part visible and removable in that
/// state, since its cells fall back to the same raw id.
pub(super) fn slot_ladder(
    ui: &mut egui::Ui,
    value: &mut Option<String>,
    choices: &[VocabularyEntry],
) {
    let stray = value
        .clone()
        .filter(|id| !choices.iter().any(|entry| entry.id == *id));
    if choices.is_empty() && stray.is_none() {
        return;
    }
    theme::segmented_strip(ui, |ui| {
        for (id, label) in choices
            .iter()
            .map(|entry| (entry.id.as_str(), entry.label.as_str()))
            .chain(stray.as_deref().map(|id| (id, id)))
        {
            let on = value.as_deref() == Some(id);
            // The accessible name comes from the label like any
            // `selectable_label`, as on the rarity ladder: the words a cell
            // paints are unique across the vocabulary, and the piece strip above
            // is the one that has to qualify.
            if ui.selectable_label(on, label).clicked() {
                *value = if on { None } else { Some(id.to_owned()) };
            }
        }
    });
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

/// Required substats: how the list combines, then one chip per offered substat
/// — a pill while it is not asked for, and a box holding its name, its `≥` and
/// its threshold once it is ([`required_substat`]).
///
/// The threshold is the number the WIRE sends, which is not the number a player
/// thinks in: a percent-bearing substat arrives as a fraction (`att_rate` is
/// `0.03` for 3%). The vocabulary states which are which — `VocabularyEntry`'s
/// `percent` — so such a requirement is shown and stepped in whole percent and
/// stores the fraction [`Filter::matches`] compares against; see
/// [`threshold_field`].
///
/// A requirement the vocabulary cannot name gets a removable row instead of a
/// chip, for the reason [`unoffered_rows`] gives — and, unlike the other two
/// lists, it also needs its threshold normalised, which is why that pass leads
/// the function rather than living in the chip.
pub(super) fn substat_chips(
    ui: &mut egui::Ui,
    reqs: &mut Vec<SubstatReq>,
    mode: &mut SubstatMatch,
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
            // percent-bearing substat lands on its lowest roll rather than on
            // `1.0` — which is 100%, the very threshold nothing in the game
            // satisfies. A requirement the vocabulary cannot name states no
            // family and takes the whole-number fallback, which is what every
            // requirement had here before the flag was read at all.
            let percent = choices
                .iter()
                .any(|entry| entry.id == req.name && entry.percent);
            req.min = Some(seed_for(&req.name, percent));
        }
    }
    substat_header(ui, mode);
    let mut toggled = None;
    // Salted on the row and not per chip, for the reason `choice_list` states:
    // a `push_id` child never reaches `Layout::next_frame`, so anything drawn
    // inside one is invisible to `horizontal_wrapped`'s wrap. This row carried
    // the same defect and was NOT merely at risk of it — eleven realistic
    // substat labels reached x=608 at the window's fixed 440px, against 414 once
    // the salt moved. It shows less than the sets row only because it is
    // shorter; a ticked chip unfolding its threshold adds width again.
    //
    // A requirement's own cells sit directly in the row for the same reason —
    // see [`theme::joined_run`], which is where a grouping `Ui` would have
    // reintroduced exactly this — and [`required_substat`] asks the row for
    // their width before drawing so a wrap cannot fall between them.
    ui.push_id("required substats", |ui| {
        ui.horizontal_wrapped(|ui| {
            for entry in choices {
                match reqs.iter().position(|req| req.name == entry.id) {
                    // Not asked for: a lone pill, like every other offered
                    // value on this screen.
                    None => {
                        let mut on = false;
                        // Unqualified, and the main-stat row above is the one
                        // that names its chips after its criterion. One of the
                        // two sharing this vocabulary has to, and it is not
                        // this one: a value here is drawn two ways — a bare
                        // pill, or [`required_substat`]'s three-cell run — so
                        // qualifying it means qualifying two names plus the
                        // `≥` beside them, where the main stat has one shape.
                        if text_chip(ui, &entry.label, None, &mut on) {
                            toggled = Some(entry.id.clone());
                        }
                    }
                    // Asked for: the name, the sign and the number as one box,
                    // so the threshold appears on the requirement it applies to
                    // and can be read as belonging to it.
                    Some(index) => {
                        if required_substat(ui, &entry.label, &mut reqs[index].min, entry) {
                            toggled = Some(entry.id.clone());
                        }
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

/// The width the value cell takes, and the only part of a run that cannot be
/// measured off its text: a [`egui::DragValue`] is sized by the number it
/// currently holds, and the number a drag is about to produce is wider.
///
/// Measured at 49px on the widest text the field can be asked to render — four
/// whole digits, against the `100%` a percent one tops out at — and spelled with
/// headroom over it, because the two failures are not symmetric: too small
/// splits the control across a wrap, too large only breaks the line one chip
/// early. `the_value_cell_fits_the_width_it_reserves` holds it to what the field
/// actually takes.
const VALUE_CELL: f32 = 56.0;

/// The horizontal room one required substat needs, in one piece.
///
/// The run goes into the caller's own wrapped row rather than into a child `Ui`
/// — [`theme::joined_run`] gives the reason at length — so nothing stops
/// `horizontal_wrapped` breaking the line between two cells and leaving a
/// fragment of the box on each. The control asks the row for this much first.
///
/// Computed rather than spelled, because the first cell is a NAME: the eleven
/// the game rolls run from "Speed" to "Critical Hit Damage", and one constant
/// wide enough for the longest would wrap the row several chips early.
fn run_width(ui: &egui::Ui, label: &str, armed: bool) -> f32 {
    cell_width(ui, label) + cell_width(ui, "≥") + if armed { VALUE_CELL } else { 0.0 }
}

/// One text cell's width: its galley, plus the padding [`theme::joined_run`]
/// puts either side of it.
///
/// The chip box and not the caller's own: the run restyles the `Ui` it draws
/// into, so measuring against `ui.spacing()` here would read a padding this cell
/// never wears.
fn cell_width(ui: &egui::Ui, text: &str) -> f32 {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font, theme::INK);
    galley.size().x + 2.0 * theme::SP_SM
}

/// One required substat: its name, the `≥` that qualifies it and the value the
/// sign applies to, as ONE box.
///
/// They were three neighbours — a pill, eight pixels of nothing, a `≥` pill,
/// eight more, a stepper — for two facts about one substat, and nothing on
/// screen said which chip the number belonged to. A row of several requirements
/// read as a list of six or nine loose controls. Joined, the run is one object
/// per requirement: the lit cells are what is being asked for, the ground cell
/// is the number being asked.
///
/// **Each cell keeps the click it always had.** The name drops the requirement,
/// the sign arms and disarms the threshold. The value cell cannot double as the
/// switch — a [`egui::DragValue`] spends its own click opening text entry, so
/// arming on it would be a gesture fighting the widget's.
///
/// Answers whether the NAME was clicked, i.e. whether the requirement should go.
fn required_substat(
    ui: &mut egui::Ui,
    label: &str,
    min: &mut Option<f64>,
    entry: &VocabularyEntry,
) -> bool {
    let armed = min.is_some();
    if ui.available_width() < run_width(ui, label, armed) {
        ui.end_row();
    }
    let enabled = ui.is_enabled();
    let (seed, ceiling) = range_for(&entry.id, entry.percent);
    let (name, sign) = theme::joined_run(ui, |ui| {
        theme::joined_cell(ui, theme::joint(true, false), theme::CELL_LIT);
        let name = ui.add(egui::Button::new(label));
        // The sign is lit only while it holds a threshold, so an unarmed run
        // reads as a name with a dim invitation on its end rather than as two
        // things equally chosen.
        theme::joined_cell(
            ui,
            theme::joint(false, !armed),
            if armed {
                theme::CELL_LIT
            } else {
                theme::CELL_GROUND
            },
        );
        let sign = ui.add(egui::Button::new("≥"));
        if let Some(stored) = min.as_mut() {
            theme::joined_cell(ui, theme::joint(false, true), theme::CELL_GROUND);
            threshold_field(ui, stored, entry.percent, ceiling);
        }
        (name, sign)
    });
    // Both names are written back for the reason [`text_chip`] carries: a bare
    // `Button` states a `WidgetType::Button`, and these two toggle, so each is
    // restated as the checkbox it behaves like — with the same duplicate event.
    //
    // The name cell is drawn only while the requirement is held, so it is
    // selected unless this very click is the one taking it away.
    name.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, !name.clicked(), label)
    });
    // And the sign states the value its own click produced, not the one it
    // replaced: `!=` on two bools is the flip.
    sign.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Checkbox,
            enabled,
            armed != sign.clicked(),
            "≥",
        )
    });
    if sign.clicked() {
        *min = if armed { None } else { Some(seed) };
    }
    name.clicked()
}

/// The header of the substat block: what the criterion is, and how its entries
/// combine.
///
/// The mode belongs on the header rather than beside the chips, because it is
/// the one control here that is about the LIST and not about a substat. It is
/// drawn whether or not anything is required: it says what a second chip would
/// mean before there is a second chip, and a control that appears halfway
/// through picking moves the row under the pointer.
///
/// [`theme::segmented_strip`] for the reason the rarity ladder takes it — the
/// choice is exclusive, and this window has one grammar for that. There is no
/// way back OUT of a mode, unlike the ladder: `all` is a value and not the
/// absence of one, and the filter is in one of the two at all times.
///
/// **Its words are `.small()`, where the ladder's are body text.** At the body
/// size the strip stood taller than the block's own header and read as the
/// title of it — an object announcing the section rather than a modifier on the
/// list below. The rarity ladder is a criterion in its own right and keeps the
/// larger type; this one qualifies a list that is already on screen.
fn substat_header(ui: &mut egui::Ui, mode: &mut SubstatMatch) {
    ui.horizontal(|ui| {
        ui.label(theme::section("required substats"));
        theme::segmented_strip(ui, |ui| {
            for (value, label, hint) in [
                (
                    SubstatMatch::All,
                    "all",
                    "every substat ticked has to be on the piece",
                ),
                (SubstatMatch::Any, "any", "one of them is enough"),
            ] {
                if ui
                    .selectable_label(*mode == value, egui::RichText::new(label).small())
                    .on_hover_text(hint)
                    .clicked()
                {
                    *mode = value;
                }
            }
        });
    });
}

/// The `≥` stepper for one required substat, in the unit a player reads.
///
/// A percent-bearing substat is shown and stepped in whole percent and STORED as
/// the fraction the filter compares against: `3%` on screen is `0.03` in
/// `config.toml` and `0.03` on the wire. A whole one is its own unit and passes
/// through.
///
/// **The value is an integer, and that is a fact about the game and not a
/// rounding taste.** Every one of the 19,628 rolls measured on the wire is a
/// whole number in the unit shown — see [`SUBSTAT_RANGE`] — so on an `f64` field
/// two thirds of the values a drag lands on are thresholds no roll can be told
/// apart by, and `2.7%` reads as a precision the shop does not have. The stepper
/// is therefore typed `i32`: egui rounds a drag over an integer, where
/// `fixed_decimals` would only have hidden the fraction it kept storing.
///
/// **It is bounded at both ends, asymmetrically.** The floor is
/// [`SHOWN_FLOOR`] — a threshold of zero is not one — and the ceiling is the
/// highest roll the shop has been seen to sell for THIS substat, above which no
/// piece can ever match. Neither bound filters anything: they are where the drag
/// stops, and a value a `config.toml` already carries outside them survives
/// untouched (`clamp_existing_to_range(false)`).
///
/// The conversion sits on a local shown value rather than in `custom_formatter`
/// / `custom_parser`, and that is the point of the split: those two convert the
/// TEXT while leaving `speed` — and the range below — in wire units, so the unit
/// would be spelled in two places and only one of them would move in a diff.
/// Here every number the widget touches is in percent and the two lines that
/// cross the boundary sit next to each other. `currency_row` lends an optional
/// currency field its raw number the same way, for the same reason.
///
/// **A value already stored is shown rounded and left alone.** `config.toml` can
/// carry `min = 0.027`, which this field reads as `3%`; the write-back only
/// fires on a change, so nothing rewrites a player's file behind them — the rule
/// `seeded_zero_limit_is_not_silently_clamped` states for every bounded field in
/// this window. It costs nothing in accuracy: no roll sits between 2% and 3%, so
/// the number on screen and the number in the file accept exactly the same
/// pieces.
///
/// It also cannot be the SOURCE of a non-finite threshold — an `i32` has no NaN
/// — which is what the `f64` field before it needed an `is_finite` guard for.
/// The normalising pass leading [`substat_chips`] still owns the ones that
/// arrive from `config.toml`, and still owns the whole list, including the
/// requirements no chip is drawn for.
fn threshold_field(ui: &mut egui::Ui, stored: &mut f64, percent: bool, ceiling: f64) {
    let scale = if percent { 100.0 } else { 1.0 };
    // Saturating on the way in, so a threshold from a file no field can hold
    // still draws something a player can drag out of.
    let mut shown = (*stored * scale).round() as i32;
    let response = ui.add(
        egui::DragValue::new(&mut shown)
            .speed(if percent { PERCENT_STEP } else { WHOLE_STEP })
            .range(SHOWN_FLOOR..=(ceiling * scale).round() as i32)
            // The pair every bounded field here takes: a value the file already
            // carried is not clamped, a dragged or typed one is.
            .clamp_existing_to_range(false)
            .suffix(if percent { "%" } else { "" }),
    );
    // Only on a change, so an untouched threshold is never round-tripped
    // through the scale and back.
    if response.changed() {
        *stored = f64::from(shown) / scale;
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
    use egui_kittest::{
        Harness,
        kittest::{NodeT as _, Queryable as _},
    };

    use super::*;
    use crate::domain::shop::{Gold, ItemKind};

    /// The eleven substats the game rolls, at their real label lengths and with
    /// their real families — the row the threshold control has to fit into.
    ///
    /// Spelled here rather than taken from a fixture because what these pin is
    /// WIDTH: a shorter stand-in row would pass a layout the live vocabulary
    /// pushes off the window.
    const LIVE_SUBSTATS: [(&str, &str, bool); 11] = [
        ("att", "Attack", false),
        ("att_rate", "Attack(%)", true),
        ("def", "Defense", false),
        ("def_rate", "Defense(%)", true),
        ("max_hp", "Health", false),
        ("max_hp_rate", "Health(%)", true),
        ("speed", "Speed", false),
        ("cri", "Critical Hit Chance", true),
        ("cri_dmg", "Critical Hit Damage", true),
        ("acc", "Effectiveness", true),
        ("res", "Effect Resistance", true),
    ];

    fn live_choices() -> Vec<VocabularyEntry> {
        LIVE_SUBSTATS
            .into_iter()
            .map(|(id, label, percent)| VocabularyEntry {
                id: id.to_owned(),
                label: label.to_owned(),
                percent,
            })
            .collect()
    }

    /// The substat row at the window's own width, which `main.rs` pins as both
    /// the minimum and the maximum inner size — a control laid out with room to
    /// spare here would wrap on the player's screen.
    fn substat_row<'a>(
        reqs: &'a mut Vec<SubstatReq>,
        choices: &'a [VocabularyEntry],
    ) -> Harness<'a> {
        let mut mode = SubstatMatch::default();
        let mut harness = Harness::builder()
            .with_size(egui::vec2(crate::ui::WINDOW_WIDTH, 600.0))
            .build_ui(move |ui| substat_chips(ui, reqs, &mut mode, choices));
        harness.run();
        harness
    }

    /// The accessibility box of the node a label names.
    fn node_box(harness: &Harness<'_>, label: &str) -> egui::accesskit::Rect {
        harness
            .get_by_label(label)
            .accesskit_node()
            .bounding_box()
            .expect("egui gives every node its bounds")
    }

    /// The name, the sign and the value are ONE control: all three cells touch.
    ///
    /// A gap anywhere in the run is the old loose-controls reading coming back
    /// with rounder corners — and it is also how a wrap gets in, since the run
    /// is reserved as one width and only a run that is actually contiguous is
    /// covered by it.
    #[test]
    fn a_required_substat_is_one_box_from_its_name_to_its_value() {
        let choices = live_choices();
        let mut reqs = vec![SubstatReq {
            name: "speed".to_owned(),
            min: Some(3.0),
        }];
        let harness = substat_row(&mut reqs, &choices);
        let name = node_box(&harness, "Speed");
        let sign = node_box(&harness, "≥");
        let value = harness
            .get_by_role(egui::accesskit::Role::SpinButton)
            .accesskit_node()
            .bounding_box()
            .expect("egui gives every node its bounds");
        for (left, right, seam) in [(name, sign, "name/sign"), (sign, value, "sign/value")] {
            assert!(
                (right.x0 - left.x1).abs() < 1.0,
                "the {seam} cells should share an edge: {:.1} then {:.1}",
                left.x1,
                right.x0
            );
        }
    }

    /// The one cell of a run whose width cannot be measured off its text fits
    /// what [`VALUE_CELL`] reserves for it — at the widest threshold the field
    /// can hold, not at the seeded one.
    ///
    /// A value cell wider than its reservation is a run wider than the row was
    /// asked for, which is the wrap [`run_width`] exists to prevent falling
    /// through the middle of the control.
    #[test]
    fn the_value_cell_fits_the_width_it_reserves() {
        let choices = live_choices();
        // 100% on a percent stat and a four-digit whole one: the widest text a
        // `DragValue` here can be asked to render, well past any real roll.
        let mut reqs = vec![
            SubstatReq {
                name: "cri_dmg".to_owned(),
                min: Some(1.0),
            },
            SubstatReq {
                name: "max_hp".to_owned(),
                min: Some(9999.0),
            },
        ];
        let harness = substat_row(&mut reqs, &choices);
        for field in harness.query_all_by_role(egui::accesskit::Role::SpinButton) {
            let cell = field
                .accesskit_node()
                .bounding_box()
                .expect("egui gives every node its bounds");
            let width = cell.x1 - cell.x0;
            assert!(
                width <= f64::from(VALUE_CELL),
                "a value cell takes {width:.0}px and reserves {VALUE_CELL}"
            );
        }
    }

    /// And a run never wraps between its own cells, on the worst row there is:
    /// every one of the game's eleven substats required, every one of them with
    /// a threshold armed.
    ///
    /// The window is pinned at `WINDOW_WIDTH`, so this row DOES wrap — several
    /// times. What must not happen is a break inside a run, which
    /// [`run_width`]'s reservation is the only thing preventing: three cells on
    /// one line is what says the reservation still covers what it reserves for.
    #[test]
    fn a_required_substat_never_wraps_between_its_cells() {
        let choices = live_choices();
        let mut reqs: Vec<SubstatReq> = LIVE_SUBSTATS
            .into_iter()
            .map(|(id, _, percent)| SubstatReq {
                name: id.to_owned(),
                min: Some(seed_for(id, percent)),
            })
            .collect();
        let harness = substat_row(&mut reqs, &choices);
        let signs: Vec<egui::accesskit::Rect> = harness
            .query_all_by_label("≥")
            .map(|cell| {
                cell.accesskit_node()
                    .bounding_box()
                    .expect("egui gives every node its bounds")
            })
            .collect();
        let values: Vec<egui::accesskit::Rect> = harness
            .query_all_by_role(egui::accesskit::Role::SpinButton)
            .map(|cell| {
                cell.accesskit_node()
                    .bounding_box()
                    .expect("egui gives every node its bounds")
            })
            .collect();
        assert_eq!(signs.len(), LIVE_SUBSTATS.len(), "one sign per requirement");
        assert_eq!(values.len(), LIVE_SUBSTATS.len(), "one value per armed one");
        for (index, (_, label, _)) in LIVE_SUBSTATS.into_iter().enumerate() {
            let name = node_box(&harness, label);
            // The vocabulary's order is the draw order, so the nth sign and the
            // nth value belong to the nth name.
            for (cell, part) in [(signs[index], "sign"), (values[index], "value")] {
                assert!(
                    (cell.y0 - name.y0).abs() < 1.0,
                    "{label}'s {part} wrapped onto another line: name at y={:.0}, {part} at y={:.0}",
                    name.y0,
                    cell.y0
                );
            }
        }
    }

    /// And the run never lands past the window, on the worst row there
    /// is: every one of the game's eleven substats required, every one of them
    /// with a threshold armed.
    ///
    /// It is the armed twin of `every_substat_chip_stays_inside_the_window`,
    /// which draws the same row with no requirement in it and so measures none
    /// of this. The window is pinned at `WINDOW_WIDTH` and cannot be widened,
    /// so a pair that overflows is a pair to make narrower.
    #[test]
    fn an_armed_threshold_never_lands_past_the_window() {
        let choices = live_choices();
        let mut reqs: Vec<SubstatReq> = LIVE_SUBSTATS
            .into_iter()
            .map(|(id, _, percent)| SubstatReq {
                name: id.to_owned(),
                min: Some(seed_for(id, percent)),
            })
            .collect();
        let harness = substat_row(&mut reqs, &choices);
        let edge = f64::from(crate::ui::WINDOW_WIDTH);
        let mut overflowing: Vec<String> = Vec::new();
        for (_, label, _) in LIVE_SUBSTATS {
            let chip = node_box(&harness, label);
            if chip.x1 > edge {
                overflowing.push(format!("{label} ends at {:.0}", chip.x1));
            }
        }
        for (index, cell) in harness
            .query_all_by_label("≥")
            .chain(harness.query_all_by_role(egui::accesskit::Role::SpinButton))
            .enumerate()
        {
            let right = cell
                .accesskit_node()
                .bounding_box()
                .expect("egui gives every node its bounds")
                .x1;
            if right > edge {
                overflowing.push(format!("threshold cell {index} ends at {right:.0}"));
            }
        }
        assert!(
            overflowing.is_empty(),
            "the armed substat row laid out past the window: {overflowing:?}"
        );
    }

    /// Every substat the game rolls has a range of its OWN, both ends whole in
    /// the unit shown, positive, and the right way round.
    ///
    /// Four claims, one table. The completeness is the tripwire: a substat
    /// missing from [`SUBSTAT_RANGE`] still works — it falls back to its
    /// family's widest — so nothing else would notice `max_hp` arming at 1,
    /// an eighty-eighth of the smallest roll the shop sells, or its field
    /// letting a drag reach a threshold no piece can meet. The rest is what the
    /// field's shape rests on: a fractional bound would put the integer stepper
    /// on a value it cannot reach, and an inverted pair would give
    /// `DragValue::range` an empty domain to clamp into.
    #[test]
    fn every_substat_the_game_rolls_has_a_whole_positive_range_of_its_own() {
        for (id, label, percent) in LIVE_SUBSTATS {
            assert!(
                SUBSTAT_RANGE.iter().any(|(name, _, _)| *name == id),
                "{label} would fall back to its family's widest range"
            );
            let (seed, ceiling) = range_for(id, percent);
            assert!(seed > 0.0, "{label} seeds at {seed}");
            assert!(seed <= ceiling, "{label} ranges {seed}..={ceiling}");
            let scale = if percent { 100.0 } else { 1.0 };
            for (value, end) in [(seed, "seed"), (ceiling, "ceiling")] {
                let shown = value * scale;
                assert!(
                    (shown - shown.round()).abs() < 1e-9,
                    "{label}'s {end} is {shown} in the unit a player reads"
                );
            }
        }
        assert_eq!(
            SUBSTAT_RANGE.len(),
            LIVE_SUBSTATS.len(),
            "the table names exactly the substats the game rolls"
        );
    }

    /// The substat row with one requirement in it, ready to be clicked.
    fn one_requirement<'a>(
        reqs: &'a mut Vec<SubstatReq>,
        mode: &'a mut SubstatMatch,
        choices: &'a [VocabularyEntry],
    ) -> Harness<'a> {
        let mut harness = Harness::builder()
            .with_size(egui::vec2(crate::ui::WINDOW_WIDTH, 600.0))
            .build_ui(|ui| substat_chips(ui, reqs, mode, choices));
        harness.run();
        harness
    }

    /// Arming `≥` seeds the lowest roll of THAT substat, not of its family.
    ///
    /// `max_hp` is the case that separates them: the family seed was 1, and the
    /// smallest health roll the shop sells is 88 — so a freshly armed threshold
    /// sat eighty-seven points below anything it could ever exclude, and the
    /// whole first half of the drag did nothing at all.
    #[test]
    fn arming_a_threshold_seeds_the_lowest_roll_of_that_substat() {
        let choices = live_choices();
        for (id, floor) in [("max_hp", 88.0), ("att", 18.0), ("cri", 0.02)] {
            let mut reqs = vec![SubstatReq {
                name: id.to_owned(),
                min: None,
            }];
            let mut mode = SubstatMatch::default();
            let mut harness = one_requirement(&mut reqs, &mut mode, &choices);
            harness.get_by_label("≥").click();
            harness.run();
            drop(harness);
            assert_eq!(reqs[0].min, Some(floor), "{id}");
        }
    }

    /// The field a player drags stops one unit above nothing and at the highest
    /// roll the shop sells for THAT substat.
    ///
    /// Read off the widget's own published bounds rather than off
    /// [`SUBSTAT_RANGE`], so what this pins is the WIRING: the table can hold
    /// the right numbers while the field is built with a family default, and
    /// only the node the player actually drags can tell the two apart. The two
    /// ends are asymmetric on purpose — see [`SHOWN_FLOOR`].
    #[test]
    fn the_field_stops_at_one_and_at_the_highest_roll_the_shop_sells() {
        let choices = live_choices();
        // In the unit the field SHOWS: 7 for a 7% ceiling, 190 for 190 health.
        // These are the ROLL tops — a speed boot's own 8 is a main stat, and a
        // field reaching it would offer four notches no roll can land in.
        for (id, ceiling) in [("speed", 4.0), ("max_hp", 190.0), ("cri_dmg", 7.0)] {
            let percent = LIVE_SUBSTATS
                .into_iter()
                .find_map(|(name, _, percent)| (name == id).then_some(percent))
                .expect("the fixture names every substat");
            let mut reqs = vec![SubstatReq {
                name: id.to_owned(),
                min: Some(seed_for(id, percent)),
            }];
            let harness = substat_row(&mut reqs, &choices);
            let field = harness
                .get_by_role(egui::accesskit::Role::SpinButton)
                .accesskit_node();
            assert_eq!(
                field.min_numeric_value(),
                Some(f64::from(SHOWN_FLOOR)),
                "{id} floors below one"
            );
            assert_eq!(field.max_numeric_value(), Some(ceiling), "{id}");
        }
    }

    /// A threshold reads as a whole number, and a finer one already in the file
    /// is rounded on screen without being rewritten underneath the player.
    ///
    /// The two halves are one policy. The game rolls whole percents, so `2.7%`
    /// on screen offers a precision the shop does not have; and `0.027` accepts
    /// exactly the pieces `0.03` does, so showing the round number tells the
    /// truth about what the filter DOES while leaving `config.toml` as it was
    /// written.
    #[test]
    fn a_threshold_reads_as_a_whole_number_without_rewriting_the_file() {
        let choices = live_choices();
        let mut reqs = vec![SubstatReq {
            name: "att_rate".to_owned(),
            min: Some(0.027),
        }];
        let harness = substat_row(&mut reqs, &choices);
        let field = harness
            .get_by_role(egui::accesskit::Role::SpinButton)
            .accesskit_node();
        assert_eq!(field.numeric_value(), Some(3.0), "what the player reads");
        assert_eq!(
            field.value().as_deref(),
            Some("3%"),
            "and how it is written"
        );
        drop(harness);
        assert_eq!(
            reqs[0].min,
            Some(0.027),
            "a render must not rewrite a threshold the player did not touch"
        );
    }

    /// The mode strip writes the filter's own mode, and starts on the value the
    /// filter holds.
    #[test]
    fn the_mode_strip_writes_how_the_requirements_combine() {
        let choices = live_choices();
        let mut reqs = vec![SubstatReq {
            name: "speed".to_owned(),
            min: None,
        }];
        let mut mode = SubstatMatch::All;
        let mut harness = one_requirement(&mut reqs, &mut mode, &choices);
        harness.get_by_label("any").click();
        harness.run();
        drop(harness);
        assert_eq!(mode, SubstatMatch::Any);
        // And back, so every state the control reaches is reachable out of.
        let mut harness = one_requirement(&mut reqs, &mut mode, &choices);
        harness.get_by_label("all").click();
        harness.run();
        drop(harness);
        assert_eq!(mode, SubstatMatch::All);
    }

    /// The folded bar says the mode where the mode decides anything.
    ///
    /// A hunt that changed from "both of these" to "either of these" and folded
    /// up reading the same words would be the summary lying about the criterion
    /// the loop spends crystals against — the defect `min_grade` and `slots`
    /// each shipped once already.
    #[test]
    fn the_summary_names_the_substat_mode_where_it_changes_the_hunt() {
        let two = vec![
            SubstatReq {
                name: "speed".to_owned(),
                min: None,
            },
            SubstatReq {
                name: "cri".to_owned(),
                min: None,
            },
        ];
        let all = Filter {
            gear: vec![GearRule {
                required_substats: two.clone(),
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert_eq!(hunt_summary(&all), "2 substats");
        let any = Filter {
            gear: vec![GearRule {
                substat_match: SubstatMatch::Any,
                required_substats: two.clone(),
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert_eq!(hunt_summary(&any), "any of 2 substats");
        // Over one requirement the two modes are the same predicate, so the bar
        // states the tally alone rather than a distinction the engine does not
        // make.
        let lone = Filter {
            gear: vec![GearRule {
                substat_match: SubstatMatch::Any,
                required_substats: two[..1].to_vec(),
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert_eq!(hunt_summary(&lone), "1 substat");
    }

    /// Several pieces fold up as a tally, one folds up as itself.
    ///
    /// Three pieces' worth of criteria spelled out would crowd every other part
    /// off a bar that already caps at three, and the tally still says the hunt
    /// restricts — which is the one thing the bar must never get wrong.
    #[test]
    fn several_pieces_fold_up_as_a_tally() {
        let piece = |slot: &str| GearRule {
            slot: Some(slot.to_owned()),
            ..GearRule::default()
        };
        let one = Filter {
            gear: vec![piece("boot")],
            ..Filter::default()
        };
        // The part is NAMED where a lone piece folds up as itself: a rule holds
        // one, so a tally could only ever read "1 slot". The words are the
        // wire's id — see `rule_parts`.
        assert_eq!(hunt_summary(&one), "boot");
        let two = Filter {
            gear: vec![piece("boot"), piece("neck")],
            ..Filter::default()
        };
        assert_eq!(hunt_summary(&two), "2 pieces");
        assert!(!two.is_unrestricted());
    }

    /// The six parts the game wears, at their real label lengths — the strip
    /// and the ladder both have to fit them at the window's fixed width, and a
    /// shorter stand-in would pass a layout the live vocabulary pushes off it.
    const LIVE_PARTS: [(&str, &str); 6] = [
        ("weapon", "Weapon"),
        ("helm", "Helmet"),
        ("armor", "Armor"),
        ("neck", "Necklace"),
        ("ring", "Ring"),
        ("boot", "Boots"),
    ];

    fn live_parts() -> Vec<VocabularyEntry> {
        LIVE_PARTS
            .into_iter()
            .map(|(id, label)| VocabularyEntry {
                id: id.to_owned(),
                label: label.to_owned(),
                percent: false,
            })
            .collect()
    }

    fn piece_of(slot: Option<&str>) -> GearRule {
        GearRule {
            slot: slot.map(str::to_owned),
            ..GearRule::default()
        }
    }

    /// The piece strip at the window's own width, which `main.rs` pins as both
    /// the minimum and the maximum inner size.
    fn strip<'a>(
        rules: &'a mut Vec<GearRule>,
        index: &'a mut usize,
        parts: &'a [VocabularyEntry],
    ) -> Harness<'a> {
        let mut harness = Harness::builder()
            .with_size(egui::vec2(crate::ui::WINDOW_WIDTH, 600.0))
            .build_ui(move |ui| piece_strip(ui, rules, index, parts));
        harness.run();
        harness
    }

    /// Each cell wears the name of its part, so the strip reads without a
    /// legend — the numbers it drew before said nothing about what a cell held,
    /// and clicking through was the only way to find out.
    #[test]
    fn every_piece_cell_wears_the_name_of_its_part() {
        let parts = live_parts();
        let mut rules = vec![piece_of(Some("ring")), piece_of(Some("neck"))];
        let mut index = 0;
        let harness = strip(&mut rules, &mut index, &parts);
        for (label, position) in [("Ring", 1), ("Necklace", 2)] {
            assert_eq!(
                harness
                    .get_all_by_label(&format!("{label} piece {position}"))
                    .count(),
                1,
                "{label} should name its own cell"
            );
        }
        // And the draft cell says what it does, where a bare `+` did not.
        assert_eq!(harness.get_all_by_label(ADD_PIECE).count(), 1);
    }

    /// A piece naming no part still needs a word, and two of them are legal —
    /// so the position is what tells the cells apart.
    ///
    /// `get_by_label` answers a duplicate by panicking rather than by picking,
    /// and a screen reader announces two identical names as one repeated
    /// control. The main-stat row hit exactly this and answered it with a
    /// qualifier on the NAME, never on the painted words
    /// (`the_two_rows_over_one_vocabulary_name_their_chips_apart`); here the
    /// qualifier has to be the position, because two pieces may also name the
    /// SAME part — a ring of one set beside a ring of another — which no other
    /// qualifier separates.
    #[test]
    fn two_pieces_wearing_one_word_are_still_two_named_cells() {
        let parts = live_parts();
        for pair in [
            [piece_of(None), piece_of(None)],
            [piece_of(Some("ring")), piece_of(Some("ring"))],
        ] {
            let word = part_label(pair[0].slot.as_deref(), &parts);
            let mut rules = pair.to_vec();
            let mut index = 0;
            let harness = strip(&mut rules, &mut index, &parts);
            for position in [1, 2] {
                assert_eq!(
                    harness
                        .get_all_by_label(&format!("{word} piece {position}"))
                        .count(),
                    1,
                    "{word} #{position} should be one named cell of its own"
                );
            }
            // The words themselves are what the cell PAINTS, and they are the
            // same on both — which is why the name cannot be them alone.
            assert_eq!(word, part_label(pair[1].slot.as_deref(), &parts));
        }
    }

    /// Clicking a cell selects that piece; clicking `remove` drops the selected
    /// one and leaves the rest.
    #[test]
    fn the_strip_selects_a_piece_and_removes_the_selected_one() {
        let parts = live_parts();
        let mut rules = vec![piece_of(Some("ring")), piece_of(Some("neck"))];
        let mut index = 0;
        let mut harness = strip(&mut rules, &mut index, &parts);
        harness.get_by_label("Necklace piece 2").click();
        harness.run();
        drop(harness);
        assert_eq!(index, 1);
        let mut harness = strip(&mut rules, &mut index, &parts);
        harness.get_by_label("remove").click();
        harness.run();
        drop(harness);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].slot.as_deref(), Some("ring"), "the other one");
        assert_eq!(index, 1, "and the draft cell is what is left selected");
    }

    /// The draft cell has nothing to remove, so the verb is not drawn over it.
    #[test]
    fn the_draft_cell_offers_no_remove() {
        let parts = live_parts();
        let mut rules = vec![piece_of(Some("ring"))];
        let mut index = 1;
        let harness = strip(&mut rules, &mut index, &parts);
        assert_eq!(harness.query_all_by_label("remove").count(), 0);
    }

    /// Neither the strip nor its verb may land past the window, at any number
    /// of pieces.
    ///
    /// The window is pinned at `WINDOW_WIDTH` and cannot be widened. Sharing one
    /// line with the strip, `remove` measured 447px at eight pieces of the
    /// longest part name — which is why it has a fixed home on the header line
    /// and the strip wraps inside its own frame ([`theme::segmented_strip`]).
    /// Twelve is past anything a hunt reaches and is the point: the count is
    /// unbounded, since two rules may name one part.
    #[test]
    fn the_piece_strip_stays_inside_the_window() {
        let parts = live_parts();
        for count in [3_usize, 12] {
            let mut rules: Vec<GearRule> = (0..count).map(|_| piece_of(Some("neck"))).collect();
            let mut index = 0;
            let harness = strip(&mut rules, &mut index, &parts);
            let edge = f64::from(crate::ui::WINDOW_WIDTH);
            let mut overflowing: Vec<String> = Vec::new();
            let mut names: Vec<String> = (1..=count)
                .map(|position| format!("Necklace piece {position}"))
                .collect();
            names.push(ADD_PIECE.to_owned());
            names.push("remove".to_owned());
            for name in &names {
                let cell = node_box(&harness, name);
                if cell.x1 > edge {
                    overflowing.push(format!("{name} ends at {:.0}", cell.x1));
                }
            }
            assert!(
                overflowing.is_empty(),
                "the strip laid out past the window at {count} pieces: {overflowing:?}"
            );
        }
    }

    /// The slot picker is exclusive, writes the wire id, and every state it
    /// reaches is reachable back out of.
    #[test]
    fn the_slot_ladder_names_one_part_and_writes_its_id() {
        let parts = live_parts();
        let mut value: Option<String> = None;
        let harness = ladder(&mut value, &parts);
        // Every offered part is a named cell — the whole reason this is not an
        // `egui::ComboBox`, which contributes no accessibility node at all.
        for (_, label) in LIVE_PARTS {
            assert_eq!(harness.get_all_by_label(label).count(), 1, "{label}");
        }
        drop(harness);
        click_cell(&mut value, &parts, "Necklace");
        assert_eq!(value.as_deref(), Some("neck"), "the id, never the word");
        // A second part replaces the first rather than joining it: a piece has
        // one part, which is the whole of what `GearRule::slot` says.
        click_cell(&mut value, &parts, "Ring");
        assert_eq!(value.as_deref(), Some("ring"));
        // And the active cell clears it.
        click_cell(&mut value, &parts, "Ring");
        assert_eq!(value, None);
    }

    /// The slot picker at the window's own width.
    fn ladder<'a>(value: &'a mut Option<String>, parts: &'a [VocabularyEntry]) -> Harness<'a> {
        let mut harness = Harness::builder()
            .with_size(egui::vec2(crate::ui::WINDOW_WIDTH, 600.0))
            .build_ui(move |ui| slot_ladder(ui, value, parts));
        harness.run();
        harness
    }

    /// Click one of its cells and let the write land, harness dropped so the
    /// value can be read back.
    fn click_cell(value: &mut Option<String>, parts: &[VocabularyEntry], label: &str) {
        let mut harness = ladder(value, parts);
        harness.get_by_label(label).click();
        harness.run();
        drop(harness);
    }

    /// The ladder holds the six parts the game wears inside the window.
    #[test]
    fn every_slot_cell_stays_inside_the_window() {
        let parts = live_parts();
        let mut value: Option<String> = None;
        let harness = ladder(&mut value, &parts);
        let overflowing: Vec<String> = LIVE_PARTS
            .into_iter()
            .map(|(_, label)| (label, node_box(&harness, label).x1))
            .filter(|(_, right)| *right > f64::from(crate::ui::WINDOW_WIDTH))
            .map(|(label, right)| format!("{label} ends at {right:.0}"))
            .collect();
        assert!(
            overflowing.is_empty(),
            "the slot ladder laid out past the window: {overflowing:?}"
        );
    }

    /// A part the vocabulary cannot name keeps a cell of its own, spelled as
    /// the raw id, and the click that clears it is the way out.
    ///
    /// It is the `unoffered_rows` promise for a criterion that holds ONE value:
    /// a part written into `config.toml` before the catalog arrived, or one the
    /// game has since renamed, must not filter while being invisible and
    /// unremovable. With no catalog at all it is the whole control.
    #[test]
    fn a_part_the_catalog_cannot_name_keeps_a_cell_of_its_own() {
        for choices in [Vec::new(), live_parts()] {
            let mut value = Some("gauntlet".to_owned());
            let harness = ladder(&mut value, &choices);
            assert_eq!(harness.get_all_by_label("gauntlet").count(), 1);
            drop(harness);
            click_cell(&mut value, &choices, "gauntlet");
            assert_eq!(value, None, "clearing it is the way out");
        }
    }

    /// With nothing offered and nothing held there is no strip at all, rather
    /// than an empty box. The piece strip above is what still names a part in
    /// that state — see `part_label`'s fallback.
    #[test]
    fn a_ladder_with_nothing_to_offer_draws_nothing() {
        let mut value: Option<String> = None;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(crate::ui::WINDOW_WIDTH, 600.0))
            .build_ui(|ui| {
                slot_ladder(ui, &mut value, &[]);
                ui.label("after");
            });
        harness.run();
        let after = node_box(&harness, "after");
        assert!(
            after.y0 < 12.0,
            "an empty ladder should take no room: the next widget starts at {:.0}",
            after.y0
        );
    }

    /// The words a cell paints are the game's where the server sent them, the
    /// raw id where it did not, and a word of their own where no part is named.
    #[test]
    fn a_part_reads_as_the_game_names_it_then_as_its_id() {
        let parts = live_parts();
        assert_eq!(part_label(Some("neck"), &parts), "Necklace");
        assert_eq!(part_label(Some("gauntlet"), &parts), "gauntlet");
        assert_eq!(part_label(None, &parts), ANY_PART);
        // With no catalog every part falls back to its id, which is the whole
        // of what a Catalog-less server can show.
        assert_eq!(part_label(Some("neck"), &[]), "neck");
    }

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
                gear: vec![GearRule {
                    sets: vec!["set_speed".to_owned()],
                    ..GearRule::default()
                }],
                ..Filter::default()
            },
            Filter {
                gear: vec![GearRule {
                    slot: Some("helm".to_owned()),
                    ..GearRule::default()
                }],
                ..Filter::default()
            },
            Filter {
                gear: vec![GearRule {
                    required_substats: vec![SubstatReq {
                        name: "speed".to_owned(),
                        min: None,
                    }],
                    ..GearRule::default()
                }],
                ..Filter::default()
            },
            Filter {
                gear: vec![GearRule {
                    mains: vec!["speed".to_owned()],
                    ..GearRule::default()
                }],
                ..Filter::default()
            },
            Filter {
                gear: vec![GearRule {
                    min_substats: Some(3),
                    ..GearRule::default()
                }],
                ..Filter::default()
            },
            Filter {
                gear: vec![GearRule {
                    max_price: Some(Gold::new(300_000)),
                    ..GearRule::default()
                }],
                ..Filter::default()
            },
            Filter {
                gear: vec![GearRule {
                    min_grade: Some(4),
                    ..GearRule::default()
                }],
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
    /// It counts across BOTH structs, since a criterion may be added to either:
    /// `Filter`'s own keys less the `gear` container, plus the keys of the one
    /// rule inside it. A criterion added to `GearRule` and left out of
    /// [`rule_parts`] is the same defect as one added to `Filter`, and the
    /// container itself is not a criterion — it is where they live.
    ///
    /// Two fields have no case, and both are MODES rather than criteria:
    /// `include_sold_out` widens rather than restricts, and `substat_match` says
    /// how a list combines. `Filter::is_unrestricted` ignores both, and so must
    /// the summary — neither can turn an empty hunt into a real one. Each is
    /// skipped at its default, so a filter carrying neither writes no key and
    /// the count below still enumerates exactly the criteria.
    #[test]
    fn every_criterion_has_a_case_here() {
        let all = Filter {
            kinds: vec![ItemKind::Equipment],
            names: vec!["ticketrare_name".to_owned()],
            include_sold_out: false,
            gear: vec![GearRule {
                substat_match: SubstatMatch::All,
                sets: vec!["set_speed".to_owned()],
                slot: Some("helm".to_owned()),
                mains: vec!["speed".to_owned()],
                required_substats: vec![SubstatReq {
                    name: "speed".to_owned(),
                    min: None,
                }],
                min_substats: Some(3),
                max_price: Some(Gold::new(300_000)),
                min_grade: Some(4),
                // Both literals are exhaustive on purpose — no `..default()`.
                // A field added to either struct fails to compile HERE, which
                // lands the author in this test before the count can drift.
            }],
        };
        // Counted off the TABLE, not off the text: `required_substats` writes a
        // `[[required_substats]]` array-of-tables whose own `name =` line would
        // be counted as one more key by anything reading the serialized string.
        let written = toml::Value::try_from(&all).expect("a filter should serialize");
        let table = written.as_table().expect("a filter is a table");
        let rule = table["gear"].as_array().expect("gear is an array")[0]
            .as_table()
            .expect("a rule is a table");
        // Less one for `gear` itself: the container is where criteria live, not
        // a criterion.
        let keys = table.len() - 1 + rule.len();
        assert_eq!(
            keys,
            one_per_criterion().len(),
            "a criterion was added to `Filter` or `GearRule` — add or drop its \
             case in `one_per_criterion`, and name it in `hunt_summary`:\n{written:?}"
        );
    }

    #[test]
    fn the_numeric_criteria_read_in_the_shop_table_s_terms() {
        let filter = Filter {
            gear: vec![GearRule {
                min_grade: Some(4),
                max_price: Some(Gold::new(300_000)),
                min_substats: Some(3),
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        // The rarity reads as the ladder two panels down spells it: the same
        // criterion named twice on one screen used to read "grade 4+" here and
        // "Heroic" there.
        assert_eq!(hunt_summary(&filter), "3+ substats, ≤300,000 gold, Heroic+");
        // The converse: `min_substats = 0` restricts nothing, so the bar must
        // keep calling it an empty hunt.
        let inert = Filter {
            gear: vec![GearRule {
                min_substats: Some(0),
                ..GearRule::default()
            }],
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
                gear: vec![GearRule {
                    min_grade: Some(id),
                    ..GearRule::default()
                }],
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
            gear: vec![GearRule {
                min_grade: Some(9),
                ..GearRule::default()
            }],
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
            assert_eq!(filter.only_rule().min_grade, Some(grade));
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
            assert_eq!(filter.only_rule().min_grade, Some(dropped));
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
        assert_eq!(filter.only_rule().min_grade, Some(5));
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
            gear: vec![GearRule {
                min_grade: Some(HUNTED_FLOOR),
                ..GearRule::default()
            }],
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
