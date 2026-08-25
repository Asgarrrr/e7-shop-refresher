//! Client-side interest filter: the player's criteria for which shop items are
//! worth stopping the refresh loop to buy. Kept on the client so they can be
//! tuned live from the UI.

use serde::{Deserialize, Deserializer, Serialize};

use crate::domain::shop::{Gold, ItemKind, ShopItem, ShopSnapshot};

/// The gear grades the game ships (`config.example.toml` documents the same
/// closed domain). Rejected while deserializing, because a floor outside it
/// fail-closes in `matches` (dropping every item) while `is_unrestricted`
/// counts it as real: the loop arms and refreshes forever without matching.
///
/// The ceiling was 4 until the rarity ladder shipped, on a reading of 59
/// captured pieces where grade and substat count moved together. That sample
/// could not hold a grade 5: it drops at roughly 0.002% per entry. The game's
/// `grade_rate` table maps `grade2`..`grade5` onto 2..5 with no offset and the
/// shop's drop-rate payload lists all four, so Epic is stock the shop sells and
/// `min_grade = 5` has to load.
const GRADE_MIN: u8 = 2;
const GRADE_MAX: u8 = 5;

/// Player criteria. An empty `Vec` or `None` field does not constrain, so a
/// default `Filter` matches every available item; how the set ones combine is
/// [`Self::matches`]' own question, and it is no longer one conjunction.
///
/// Deserialized from the config file's `[filter]` section. Unlike the wire
/// models, unknown keys are rejected: a typo here silently loosens the criteria
/// the refresh loop spends crystals against. The gear criteria a flat `[filter]`
/// used to hold are still read and become one rule — see [`RawFilter`].
///
/// The never-`None` fields skip when empty because `config/persist.rs` replaces
/// the whole `[filter]` section on Apply — without the skips one edit would
/// write inert lines into a file meant to stay as the player wrote it. The
/// container `#[serde(default)]` makes every omission round-trip.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields, try_from = "RawFilter")]
pub struct Filter {
    /// Kept item kinds (any-of); empty keeps all, `Unknown` items included.
    /// Deliberately not a narrower hunt-only enum — see [`hunt_kinds`].
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "hunt_kinds")]
    pub kinds: Vec<ItemKind>,
    /// Kept items (any-of), by exact internal name (`ticketrare_name`, ...);
    /// empty keeps all.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    /// The pieces of gear being hunted, one rule each. An item satisfying ANY
    /// of them answers the gear branch.
    ///
    /// **A list, because a hunt is a list.** These were flat fields, one set of
    /// gear criteria joined by AND, which could only ever describe one kind of
    /// piece — and described it as a product rather than a piece: "boots or
    /// necklace" beside "speed or crit damage" bought a necklace that rolled
    /// speed. A player hunts particular pieces, and each is its own conjunction.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub gear: Vec<GearRule>,
    /// Keep sold-out items (default drops them).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub include_sold_out: bool,
}

/// One piece of gear worth stopping for: every criterion set here has to hold,
/// and an item holding all of them is a hit whatever the other rules say.
///
/// Missing data is fail-closed throughout — an unknown price never satisfies a
/// cap, an unknown grade never clears a floor, an unanswered slot matches no
/// slot criterion. Fail-OPEN would buy across every slot the moment a lookup
/// went missing.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GearRule {
    /// Kept sets (any-of), by exact internal id; empty keeps all.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sets: Vec<String>,
    /// Kept wearable slots (any-of), by exact internal id (`helm`, `boot`, ...);
    /// empty keeps all.
    ///
    /// Fail-closed like [`Self::sets`], and that has a sharper edge here: the
    /// slot is resolved server-side, so a server that could not read its Catalog
    /// sends every item with no [`ShopItem::gear_slot`] and this criterion then
    /// matches nothing at all. The loop keeps refreshing until one of the
    /// player's own stop limits fires — hence the journal line
    /// [`Filter::slots_unanswerable`] backs.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub slots: Vec<String>,
    /// Minimum count of ROLLED substats, [`ShopItem::substats`]' length.
    ///
    /// Config-only: the window offers the rarity ladder instead, and on shop
    /// gear the two say the same thing. A grade rolls exactly `grade - 1`
    /// substats — 7,941 captured items, no exception, and the game's own table
    /// agrees (`db_equip_item.sub_stat_count` is 4 on its 858 grade-5 rows and 3
    /// on its grade-4 ones) — so `min_substats = n` is `min_grade = n + 1` on
    /// anything the shop sells.
    ///
    /// It used to be the wider criterion, on a count that included the main
    /// stat: Heroic and Epic looked alike at four, and the ladder was said to be
    /// the only way to ask for Epic. Both readings came from counting the main
    /// stat as a roll.
    pub min_substats: Option<u8>,
    /// Substats the item must carry, each above its optional threshold. How
    /// several of them combine is [`Self::substat_match`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub required_substats: Vec<SubstatReq>,
    /// Whether [`Self::required_substats`] must ALL hold or any ONE of them.
    ///
    /// A mode and not a criterion: it says how a list combines and cannot
    /// restrict on its own, which is why [`Self::restricts`] does not read it.
    /// `Filter::include_sold_out` is the other field of that shape.
    ///
    /// Defaults to [`SubstatMatch::All`], which is what every filter did before
    /// the mode existed, and is skipped when it holds — so a `config.toml`
    /// written by an older build reloads unchanged and Apply never adds a line
    /// saying what was already true.
    #[serde(skip_serializing_if = "SubstatMatch::is_all")]
    pub substat_match: SubstatMatch,
    /// Inclusive gold cap; an unknown price fails it.
    ///
    /// Belongs to the rule, so it is **not** a global cap: beside a `names`
    /// criterion the named item is bought at any price, because the name branch
    /// answers on its own and no rule is consulted. Surprising and deliberate —
    /// cap a named hunt with the player's own `[limits]`, not with this.
    ///
    /// A [`Gold`], so a crystal budget cannot be written here by mistake.
    /// `#[serde(transparent)]` in both directions, so `config.toml` still
    /// spells this as a bare `max_price = 300000`.
    pub max_price: Option<Gold>,
    /// Inclusive minimum gear grade — 2 Good, 3 Rare, 4 Heroic, 5 Epic; an
    /// unknown grade fails it. A floor outside that domain is refused at parse
    /// time — see `GRADE_MIN`.
    #[serde(deserialize_with = "grade_floor")]
    pub min_grade: Option<u8>,
}

/// The shape a `config.toml` may spell, which is wider than [`Filter`]'s own:
/// the gear criteria were flat keys of `[filter]` before they were a list of
/// rules, and a file that predates the change has to keep loading.
///
/// **Folded, not ignored.** A retired key here still decides what the loop
/// buys, so it cannot go the way of `capture.filter` — dropping it would empty
/// a hunt in silence. The flat keys become one [`GearRule`], which is exactly
/// what they always were, and the next Apply rewrites the section in the new
/// shape (`config/persist.rs` replaces `[filter]` whole).
///
/// Setting both spellings is refused rather than merged: two gear criteria in
/// one file are two hunts, and picking one of them silently is how a player
/// ends up buying against a rule they cannot see.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawFilter {
    #[serde(deserialize_with = "hunt_kinds")]
    kinds: Vec<ItemKind>,
    names: Vec<String>,
    gear: Vec<GearRule>,
    include_sold_out: bool,
    // The retired flat gear keys, in the spelling `[filter]` used to hold.
    sets: Vec<String>,
    slots: Vec<String>,
    min_substats: Option<u8>,
    required_substats: Vec<SubstatReq>,
    substat_match: SubstatMatch,
    max_price: Option<Gold>,
    #[serde(deserialize_with = "grade_floor")]
    min_grade: Option<u8>,
}

impl TryFrom<RawFilter> for Filter {
    type Error = String;

    fn try_from(raw: RawFilter) -> Result<Self, Self::Error> {
        let flat = GearRule {
            sets: raw.sets,
            slots: raw.slots,
            min_substats: raw.min_substats,
            required_substats: raw.required_substats,
            substat_match: raw.substat_match,
            max_price: raw.max_price,
            min_grade: raw.min_grade,
        };
        // `restricts` and not `!= default`: `min_substats = 0` and
        // `substat_match = "all"` say nothing, and a file setting only those has
        // no gear criteria to fold.
        let gear = match (flat.restricts(), raw.gear.is_empty()) {
            (true, true) => vec![flat],
            (true, false) => {
                return Err(
                    "[filter] holds gear criteria of its own beside [[filter.gear]] — move them \
                     into a rule, or delete them"
                        .to_owned(),
                );
            }
            (false, _) => raw.gear,
        };
        Ok(Self {
            kinds: raw.kinds,
            names: raw.names,
            gear,
            include_sold_out: raw.include_sold_out,
        })
    }
}

/// Parses `min_grade`, refusing a floor the game has no grade for. The error
/// names the offending value so `toml` can point at the line.
fn grade_floor<'de, D>(de: D) -> Result<Option<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(grade) = Option::<u8>::deserialize(de)? else {
        return Ok(None);
    };
    if !(GRADE_MIN..=GRADE_MAX).contains(&grade) {
        return Err(serde::de::Error::custom(format!(
            "gear grade {grade} does not exist (expected {GRADE_MIN} to {GRADE_MAX})"
        )));
    }
    Ok(Some(grade))
}

/// The kinds a criterion may name, in the order the Setup tab lists them.
///
/// Public because the checkbox row must be built from it: spelled out there
/// instead, the row once grew a fourth `Unknown` box that wrote
/// `kinds = ["unknown"]` and was refused at the next launch.
pub const HUNTABLE_KINDS: [ItemKind; 3] = [ItemKind::Equipment, ItemKind::Hero, ItemKind::Token];

/// Parses `[filter] kinds`, refusing the wire's catch-all.
///
/// [`ItemKind::Unknown`] keeps a *snapshot* decodable when the server adds an
/// unheard-of kind. As config text, that leniency turns
/// `kinds = ["equipement"]` into a criterion no item satisfies while
/// [`Filter::is_unrestricted`] counts it as real — the loop arms and refreshes
/// forever, debiting crystals, without ever buying.
///
/// Refused at the *boundary*, not by narrowing the field's type: `Unknown` is a
/// meaningful criterion *in the domain* ([`Filter::matches`] compares it
/// against a kind the wire reported, and `Filter::matching_default_items` is
/// built on it). It is ambiguous only as text, where a typo and a deliberate
/// "hunt the kind you cannot name" are the same six bytes.
///
/// It must live here and not in `Config::validate`: the Setup tab reaches the
/// file through `persist::save` with no `Config` in the path.
fn hunt_kinds<'de, D>(de: D) -> Result<Vec<ItemKind>, D::Error>
where
    D: Deserializer<'de>,
{
    // Text first, then `ItemKind`'s own `Deserialize`: the accepted spellings
    // stay whatever `rename_all` produces (never re-listed here, so they cannot
    // drift), and the raw string survives so the error can quote what the
    // player typed — which `serde(other)` would otherwise swallow.
    let raw = Vec::<String>::deserialize(de)?;
    let mut kinds = Vec::with_capacity(raw.len());
    for name in raw {
        let kind =
            ItemKind::deserialize(serde::de::value::StrDeserializer::<D::Error>::new(&name))?;
        if kind == ItemKind::Unknown {
            return Err(serde::de::Error::custom(format!(
                "unrecognized kind {name:?} in [filter] kinds (expected: equipment, hero, token)"
            )));
        }
        kinds.push(kind);
    }
    Ok(kinds)
}

/// How the entries of [`Filter::required_substats`] combine.
///
/// `All` is a piece carrying every substat named — the shape of a hunt for one
/// specific build. `Any` is a piece carrying at least one of them, which is what
/// a player watching for "speed OR crit" is after and what a single conjunction
/// could not spell: with two entries joined by AND, a roll satisfying one is
/// refused, and the shop rolls four substats out of eleven.
///
/// An unspelled variant is refused at parse time rather than folded into a
/// catch-all, for the reason [`hunt_kinds`] gives at length: as text, a typo and
/// a deliberate choice are the same bytes, and the wrong fold here silently
/// widens or narrows what the loop buys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubstatMatch {
    /// Every requirement holds. The behaviour of every filter written before
    /// this mode existed, and the default.
    #[default]
    All,
    /// At least one requirement holds.
    Any,
}

impl SubstatMatch {
    /// Whether this is the default mode. Written for `skip_serializing_if`,
    /// which hands it a reference.
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's skip_serializing_if takes &Self"
    )]
    fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }
}

/// One required substat, by exact internal name (`speed`, `cri`, ...). `min` is
/// an inclusive threshold; `None` means presence is enough. `name` is
/// deliberately required: a nameless requirement would match nothing.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubstatReq {
    pub name: String,
    #[serde(default)]
    pub min: Option<f64>,
}

impl Filter {
    /// Whether this item is worth stopping for.
    ///
    /// The sold-out and `kinds` gates apply to everything. Past them the
    /// criteria fall into two branches — what the item IS (its name) and what
    /// a piece of gear LOOKS LIKE — and an item satisfying either is a hit.
    ///
    /// The OR is the point. A token carries no set and gear carries no token
    /// name, so a single AND over the two branches made "stop on a Covenant
    /// bookmark or a Speed helm" match nothing at all. A branch with no
    /// criterion set does not constrain and does not match, so a filter using
    /// only one branch behaves exactly as it did before this existed.
    pub fn matches(&self, item: &ShopItem) -> bool {
        if !self.include_sold_out && item.is_sold_out() {
            return false;
        }
        if !self.kinds.is_empty() && !self.kinds.contains(&item.kind) {
            return false;
        }
        let named = !self.names.is_empty();
        let geared = self.hunts_gear();
        match (named, geared) {
            (false, false) => true,
            (true, false) => self.name_branch(item),
            (false, true) => self.gear_branch(item),
            (true, true) => self.name_branch(item) || self.gear_branch(item),
        }
    }

    /// Whether any rule restricts. A rule with nothing set matches every piece
    /// of gear, so it must not arm the branch — the GUI adds one the moment the
    /// player opens a card, and an armed branch on an empty rule would buy the
    /// first item of any roll.
    fn hunts_gear(&self) -> bool {
        self.gear.iter().any(GearRule::restricts)
    }

    /// The item IS one of the named things.
    fn name_branch(&self, item: &ShopItem) -> bool {
        item.name
            .as_ref()
            .is_some_and(|name| self.names.contains(name))
    }

    /// The item is one of the pieces being hunted — ANY rule that restricts
    /// accepts it.
    fn gear_branch(&self, item: &ShopItem) -> bool {
        self.gear
            .iter()
            .filter(|rule| rule.restricts())
            .any(|rule| rule.matches(item))
    }

    /// `true` when a rule names a slot and the snapshot answered none — the
    /// shape a Catalog-less server produces, in which [`Self::matches`] cannot
    /// accept anything through that rule.
    ///
    /// A question about a whole roll rather than an item, because one gearless
    /// slot proves nothing: a roll legitimately holds heroes and tokens, which
    /// have no slot and never will. Only *every* slot being unanswered
    /// separates "this roll had no gear" from "nobody can answer about gear",
    /// and even then the first reading stays possible — hence a journal line
    /// rather than a refusal to arm.
    pub fn slots_unanswerable(&self, snapshot: &ShopSnapshot) -> bool {
        self.gear.iter().any(|rule| !rule.slots.is_empty())
            && !snapshot.slots.is_empty()
            && snapshot.slots.iter().all(|item| item.gear_slot.is_none())
    }

    /// `true` when no criterion is set — such a filter matches every available
    /// item, which the relay treats as a configuration error. `include_sold_out`
    /// widens rather than restricts, and a rule with nothing set constrains
    /// nothing; neither counts as a criterion.
    pub fn is_unrestricted(&self) -> bool {
        self.kinds.is_empty() && self.names.is_empty() && !self.hunts_gear()
    }
}

impl GearRule {
    /// Whether this rule constrains at all. Read by [`Filter::matches`] to tell
    /// "no gear constraint" from "a gear constraint this item fails".
    ///
    /// The destructure is load-bearing. [`Self::matches`] enumerates these same
    /// criteria and nothing ties the two lists together, so a field present in
    /// one and missing from the other fails silently in both directions: a
    /// criterion nobody tests, or a rule armed on a test it never runs. Both
    /// fail open, toward buying. Binding every field makes the omission a
    /// compile error instead.
    #[must_use]
    pub fn restricts(&self) -> bool {
        let Self {
            sets,
            slots,
            required_substats,
            min_substats,
            max_price,
            min_grade,
            // A mode, not a criterion: it says how `required_substats` combine,
            // so an empty list stays an empty list under either value and
            // cannot arm this rule. See its own doc.
            substat_match: _,
        } = self;
        !sets.is_empty()
            || !slots.is_empty()
            || !required_substats.is_empty()
            || substat_floor_is_real(*min_substats)
            || max_price.is_some()
            // Any grade floor constrains: `matches` is fail-closed, so it drops
            // every gradeless item (tokens, heroes) even at `min_grade = 2`.
            || min_grade.is_some()
    }

    /// The item looks like this piece. Every criterion set here holds, each
    /// fail-closed on missing data.
    fn matches(&self, item: &ShopItem) -> bool {
        if let Some(min) = self.min_substats
            && item.substats.len() < usize::from(min)
        {
            return false;
        }
        // `is_none_or` is fail-closed: a price the server did not send never
        // satisfies a cap.
        if let Some(max) = self.max_price
            && item.price.is_none_or(|price| price > max)
        {
            return false;
        }
        if let Some(min) = self.min_grade
            && item.grade.is_none_or(|grade| grade < min)
        {
            return false;
        }
        if !self.sets.is_empty() && !item.set.as_ref().is_some_and(|set| self.sets.contains(set)) {
            return false;
        }
        if !self.slots.is_empty()
            && !item
                .gear_slot
                .as_ref()
                .is_some_and(|slot| self.slots.contains(slot))
        {
            return false;
        }
        match self.substat_match {
            SubstatMatch::All => self
                .required_substats
                .iter()
                .all(|req| req.satisfied_by(item)),
            // The emptiness test leads, and it is not a shortcut: `any` over
            // nothing is `false`, so without it a mode a player switched with
            // no requirement listed would refuse every piece of gear — the one
            // asymmetry between the two variants, and the whole of it.
            SubstatMatch::Any => {
                self.required_substats.is_empty()
                    || self
                        .required_substats
                        .iter()
                        .any(|req| req.satisfied_by(item))
            }
        }
    }
}

/// `Some(0)` is not a criterion — the GUI editor produces it in two clicks and
/// it constrains nothing. THE home for that rule, read by [`GearRule::restricts`]
/// and through it by [`Filter::is_unrestricted`]. Spelled twice, the two would
/// drift into a filter that is unrestricted and yet refuses every token.
fn substat_floor_is_real(floor: Option<u8>) -> bool {
    floor.is_some_and(|min| min > 0)
}

#[cfg(test)]
impl Filter {
    /// The single rule this filter holds. Panics on any other count, which is
    /// the point: every caller is asserting about a `[filter]` that folded its
    /// flat gear keys into exactly one, and two would mean the fold split.
    pub(crate) fn only_rule(&self) -> &GearRule {
        match self.gear.as_slice() {
            [rule] => rule,
            other => panic!("expected exactly one gear rule, found {}", other.len()),
        }
    }

    /// Restricted (passes the arming invariant) yet still matching
    /// `ShopItem::default()`, whose kind is `Unknown`.
    pub(crate) fn matching_default_items() -> Self {
        Self {
            kinds: vec![ItemKind::Unknown],
            ..Self::default()
        }
    }
}

impl SubstatReq {
    /// Scans *all* substats of the matching name: an item may list the same
    /// stat twice (a blank entry, then the rolled value).
    ///
    /// **The piece's MAIN stat answers this too**, and that is deliberate:
    /// `speed ≥ 6` is a hunt for speed BOOTS, whose speed is a main stat and
    /// never a roll, and it is the hunt this window exists for. A criterion that
    /// read only [`ShopItem::substats`] would have gone quiet on it the day the
    /// server learned to split the two — the value is unreachable by a roll,
    /// which tops out at 4. `ui::editor::hunt::SUBSTAT_RANGE` sizes its
    /// threshold field over both, for the same reason.
    ///
    /// It is also what keeps an older server working: one that has not learned
    /// the split sends the main stat as `substats[0]`, and either shape answers
    /// the same question here.
    ///
    /// Asking for the two apart is a criterion this does not have yet — see the
    /// `main` a gear rule will carry.
    fn satisfied_by(&self, item: &ShopItem) -> bool {
        item.main_stat
            .iter()
            .chain(item.substats.iter())
            .any(|stat| {
                stat.name == self.name
                    && match self.min {
                        None => true,
                        Some(min) => stat.value.is_some_and(|value| value >= min),
                    }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shop::{PurchaseLimit, Substat};

    fn substat(name: &str, value: Option<f64>) -> Substat {
        Substat {
            name: name.to_owned(),
            value,
        }
    }

    fn equip() -> ShopItem {
        ShopItem {
            id: crate::domain::shop::CatalogId::new(4562),
            slot: 1,
            kind: ItemKind::Equipment,
            name: None,
            price: Some(Gold::new(240_000)),
            grade: Some(3),
            set: Some("set_speed".to_owned()),
            gear_slot: Some("helm".to_owned()),
            // A helm's own stat, which is health and never a roll.
            main_stat: Some(substat("max_hp", Some(472.0))),
            substats: vec![
                substat("speed", Some(15.0)),
                substat("cri", Some(0.03)),
                substat("att", Some(40.0)),
            ],
            limit: None,
        }
    }

    /// A real token: the kind, and none of the gear fields. Every gear
    /// criterion can therefore only ever refuse it, which is what makes it the
    /// item the OR was built for.
    fn token(name: &str) -> ShopItem {
        ShopItem {
            kind: ItemKind::Token,
            name: Some(name.to_owned()),
            price: Some(Gold::new(184_000)),
            set: None,
            gear_slot: None,
            grade: None,
            substats: Vec::new(),
            ..equip()
        }
    }

    fn speed_filter() -> Filter {
        Filter {
            kinds: vec![ItemKind::Equipment],
            gear: vec![GearRule {
                min_substats: Some(3),
                required_substats: vec![SubstatReq {
                    name: "speed".to_owned(),
                    min: Some(15.0),
                }],
                ..GearRule::default()
            }],
            ..Filter::default()
        }
    }

    #[test]
    fn canonical_equip_speed15_3substats_matches() {
        assert!(speed_filter().matches(&equip()));
    }

    #[test]
    fn rejects_speed_below_min() {
        let mut item = equip();
        item.substats[0] = substat("speed", Some(14.0));
        assert!(!speed_filter().matches(&item));
    }

    #[test]
    fn rejects_fewer_than_min_substats() {
        let mut item = equip();
        item.substats.truncate(2); // speed still present, but only 2 substats
        assert!(!speed_filter().matches(&item));
    }

    #[test]
    fn empty_filter_matches_available_item() {
        assert!(Filter::default().matches(&equip()));
    }

    #[test]
    fn min_substats_zero_counts_as_unrestricted() {
        // The GUI editor can produce `Some(0)` in two clicks.
        let noop = Filter {
            gear: vec![GearRule {
                min_substats: Some(0),
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert!(noop.is_unrestricted());
        let real = Filter {
            gear: vec![GearRule {
                min_substats: Some(1),
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert!(!real.is_unrestricted());
    }

    #[test]
    fn unrestricted_detection_ignores_include_sold_out() {
        assert!(Filter::default().is_unrestricted());
        let sold_out_only = Filter {
            include_sold_out: true,
            ..Filter::default()
        };
        assert!(sold_out_only.is_unrestricted());
        assert!(!speed_filter().is_unrestricted());
        let names_only = Filter {
            names: vec!["ticketrare_name".to_owned()],
            ..Filter::default()
        };
        assert!(!names_only.is_unrestricted());
    }

    #[test]
    fn names_any_of_matches() {
        let filter = Filter {
            names: vec![
                "ticketrare_name".to_owned(),
                "ticketspecial_name".to_owned(),
            ],
            ..Filter::default()
        };
        let mut item = equip();
        item.name = Some("ticketrare_name".to_owned());
        assert!(filter.matches(&item));
        item.name = Some("friendpoint_name".to_owned());
        assert!(!filter.matches(&item));
    }

    #[test]
    fn name_none_fails_when_names_filter_active() {
        let filter = Filter {
            names: vec!["ticketrare_name".to_owned()],
            ..Filter::default()
        };
        let mut item = equip();
        item.name = None;
        assert!(!filter.matches(&item));
    }

    #[test]
    fn default_filter_drops_sold_out() {
        let mut item = equip();
        item.limit = Some(PurchaseLimit {
            remaining: 0,
            total: 1,
        });
        assert!(!Filter::default().matches(&item));
    }

    #[test]
    fn include_sold_out_keeps_sold_out() {
        let mut item = equip();
        item.limit = Some(PurchaseLimit {
            remaining: 0,
            total: 1,
        });
        let filter = Filter {
            include_sold_out: true,
            ..Filter::default()
        };
        assert!(filter.matches(&item));
    }

    #[test]
    fn unknown_limit_treated_available() {
        let mut item = equip();
        item.limit = None;
        assert!(Filter::default().matches(&item));
    }

    #[test]
    fn kinds_any_of_matches() {
        let filter = Filter {
            kinds: vec![ItemKind::Hero, ItemKind::Token],
            ..Filter::default()
        };
        let mut item = equip();
        item.kind = ItemKind::Token;
        assert!(filter.matches(&item));
        item.kind = ItemKind::Equipment;
        assert!(!filter.matches(&item));
    }

    #[test]
    fn empty_kinds_allows_unknown() {
        let mut item = equip();
        item.kind = ItemKind::Unknown;
        assert!(Filter::default().matches(&item));
    }

    #[test]
    fn sets_any_of_matches() {
        let filter = Filter {
            gear: vec![GearRule {
                sets: vec!["set_speed".to_owned(), "set_counter".to_owned()],
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert!(filter.matches(&equip()));
    }

    #[test]
    fn set_none_fails_when_set_filter_active() {
        let filter = Filter {
            gear: vec![GearRule {
                sets: vec!["set_speed".to_owned()],
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        let mut item = equip();
        item.set = None;
        assert!(!filter.matches(&item));
    }

    #[test]
    fn set_case_sensitive_no_match() {
        let filter = Filter {
            gear: vec![GearRule {
                sets: vec!["Set_Speed".to_owned()],
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert!(!filter.matches(&equip()));
    }

    #[test]
    fn max_price_inclusive_boundary() {
        let filter = Filter {
            gear: vec![GearRule {
                max_price: Some(Gold::new(240_000)),
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert!(filter.matches(&equip()));
    }

    #[test]
    fn max_price_above_fails() {
        let filter = Filter {
            gear: vec![GearRule {
                max_price: Some(Gold::new(239_999)),
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert!(!filter.matches(&equip()));
    }

    #[test]
    fn max_price_missing_price_fails() {
        let filter = Filter {
            gear: vec![GearRule {
                max_price: Some(Gold::new(240_000)),
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        let mut item = equip();
        item.price = None;
        assert!(!filter.matches(&item));
    }

    #[test]
    fn min_grade_accepts_equal_and_above() {
        let filter = Filter {
            gear: vec![GearRule {
                min_grade: Some(3),
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert!(filter.matches(&equip())); // grade 3, equal to floor
        let mut item = equip();
        item.grade = Some(4);
        assert!(filter.matches(&item)); // grade 4, above floor
    }

    #[test]
    fn min_grade_rejects_below() {
        let filter = Filter {
            gear: vec![GearRule {
                min_grade: Some(4),
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert!(!filter.matches(&equip())); // grade 3, below floor
    }

    #[test]
    fn min_grade_unknown_fails() {
        let filter = Filter {
            gear: vec![GearRule {
                min_grade: Some(3),
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        let mut item = equip();
        item.grade = None;
        assert!(!filter.matches(&item)); // fail-closed: unknown grade fails
    }

    #[test]
    fn min_grade_is_a_real_constraint() {
        let real = Filter {
            gear: vec![GearRule {
                min_grade: Some(4),
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert!(!real.is_unrestricted());
        // Real even at the floor: `matches` fail-closes on a gradeless item, so
        // `min_grade = 2` still drops every token/hero.
        let floor_two = Filter {
            gear: vec![GearRule {
                min_grade: Some(2),
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert!(!floor_two.is_unrestricted());
        let floor_zero = Filter {
            gear: vec![GearRule {
                min_grade: Some(0),
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert!(!floor_zero.is_unrestricted());
    }

    #[test]
    fn substat_req_presence_only_min_none() {
        let filter = Filter {
            gear: vec![GearRule {
                required_substats: vec![SubstatReq {
                    name: "cri".to_owned(),
                    min: None,
                }],
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert!(filter.matches(&equip()));
        let mut item = equip();
        item.substats.retain(|stat| stat.name != "cri");
        assert!(!filter.matches(&item));
    }

    /// The hunt this window exists for: speed boots, where the speed is the
    /// piece's MAIN stat and no roll can reach the value.
    ///
    /// A criterion reading only the rolls would answer `false` here and say
    /// nothing about it — the shop's speed rolls stop at 4, so `speed ≥ 6`
    /// would have gone silently empty the day the server split the two fields.
    #[test]
    fn a_threshold_no_roll_can_reach_is_answered_by_the_main_stat() {
        let hunt = Filter {
            gear: vec![GearRule {
                required_substats: vec![SubstatReq {
                    name: "speed".to_owned(),
                    min: Some(6.0),
                }],
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        let boots = ShopItem {
            gear_slot: Some("boot".to_owned()),
            main_stat: Some(substat("speed", Some(8.0))),
            substats: vec![substat("cri", Some(0.03))],
            ..equip()
        };
        assert!(hunt.matches(&boots));
        // And a piece whose speed is only a roll still fails it, so the
        // criterion is not widened into "carries speed at all".
        let rolled = ShopItem {
            substats: vec![substat("speed", Some(4.0))],
            ..equip()
        };
        assert!(!hunt.matches(&rolled));
    }

    #[test]
    fn substat_req_min_requires_present_value() {
        let filter = Filter {
            gear: vec![GearRule {
                required_substats: vec![SubstatReq {
                    name: "speed".to_owned(),
                    min: Some(15.0),
                }],
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        let mut item = equip();
        item.substats[0] = substat("speed", None);
        assert!(!filter.matches(&item));
    }

    #[test]
    fn substat_req_scans_all_not_first() {
        // A first-match check would grab the blank entry and wrongly reject.
        let filter = Filter {
            gear: vec![GearRule {
                required_substats: vec![SubstatReq {
                    name: "speed".to_owned(),
                    min: Some(15.0),
                }],
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        let mut item = equip();
        item.substats = vec![substat("speed", None), substat("speed", Some(30.0))];
        assert!(filter.matches(&item));
    }

    /// Two requirements, and the mode is the whole difference: a piece with one
    /// of them is a hit under `Any` and a miss under `All`.
    ///
    /// The shop rolls four substats out of eleven, so a two-entry conjunction is
    /// a hunt that almost never fires — which is what `Any` exists for.
    #[test]
    fn the_substat_mode_decides_whether_one_hit_is_enough() {
        let speed_and_crit = vec![
            SubstatReq {
                name: "speed".to_owned(),
                min: None,
            },
            SubstatReq {
                name: "cri_dmg".to_owned(),
                min: None,
            },
        ];
        // `equip()` carries speed and cri, never cri_dmg.
        let all = Filter {
            gear: vec![GearRule {
                required_substats: speed_and_crit.clone(),
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert_eq!(
            all.only_rule().substat_match,
            SubstatMatch::All,
            "the default"
        );
        assert!(!all.matches(&equip()), "one of two is not both");
        let any = Filter {
            gear: vec![GearRule {
                substat_match: SubstatMatch::Any,
                required_substats: speed_and_crit,
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert!(any.matches(&equip()), "one of two is enough");
        // And `Any` still refuses a piece answering neither, or it would be no
        // criterion at all.
        let neither = ShopItem {
            substats: vec![substat("def", Some(30.0))],
            ..equip()
        };
        assert!(!any.matches(&neither));
    }

    /// Each entry's own threshold survives the mode: `Any` asks for one
    /// requirement SATISFIED, not one substat merely present.
    #[test]
    fn the_any_mode_still_applies_each_threshold() {
        let filter = Filter {
            gear: vec![GearRule {
                substat_match: SubstatMatch::Any,
                required_substats: vec![SubstatReq {
                    name: "speed".to_owned(),
                    min: Some(20.0),
                }],
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        // `equip()` rolls speed 15, under the floor.
        assert!(!filter.matches(&equip()));
    }

    /// An empty list does not constrain under EITHER mode — `any` over nothing
    /// is `false`, so a mode switched with no requirement listed would otherwise
    /// refuse every piece of gear the rest of the filter accepted.
    #[test]
    fn switching_the_mode_with_no_requirement_refuses_nothing() {
        let any = Filter {
            gear: vec![GearRule {
                substat_match: SubstatMatch::Any,
                sets: vec!["set_speed".to_owned()],
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert!(any.matches(&equip()));
        // And it is not a criterion of its own: alone it still matches
        // everything, so the loop must keep refusing to arm on it.
        let alone = Filter {
            gear: vec![GearRule {
                substat_match: SubstatMatch::Any,
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert!(alone.is_unrestricted());
        assert!(alone.matches(&token("ticketrare_name")));
    }

    /// The mode round-trips, an unspelled one is refused, and a file that never
    /// mentions it keeps the behaviour every filter had before it existed.
    #[test]
    fn the_substat_mode_is_a_closed_spelling_that_defaults_to_all() {
        assert_eq!(
            GearRule::default().substat_match,
            SubstatMatch::All,
            "a rule that never mentions it"
        );
        for (text, mode) in [("all", SubstatMatch::All), ("any", SubstatMatch::Any)] {
            // Beside a real criterion: a mode is not one, so a rule holding
            // only this would fold away before it could be read back.
            let filter: Filter = toml::from_str(&format!(
                "[[gear]]
slots = [\"boot\"]
substat_match = {text:?}
"
            ))
            .expect("a spelled mode parses");
            assert_eq!(filter.only_rule().substat_match, mode);
            let back: Filter =
                toml::from_str(&toml::to_string(&filter).expect("serialize")).expect("deserialize");
            assert_eq!(back.only_rule().substat_match, mode);
        }
        assert!(
            toml::from_str::<Filter>("substat_match = \"either\"").is_err(),
            "a typo must not fold into a mode nobody chose"
        );
        // The default writes no key: an older `config.toml` must not grow a
        // line on the first Apply saying what was already true.
        let text = toml::to_string(&Filter {
            gear: vec![GearRule {
                required_substats: vec![SubstatReq {
                    name: "speed".to_owned(),
                    min: None,
                }],
                ..GearRule::default()
            }],
            ..Filter::default()
        })
        .expect("serialize");
        assert!(!text.contains("substat_match"), "{text}");
    }

    #[test]
    fn min_substats_counts_duplicates() {
        let filter = Filter {
            gear: vec![GearRule {
                min_substats: Some(3),
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        let mut item = equip();
        item.substats = vec![
            substat("speed", Some(1.0)),
            substat("speed", Some(2.0)),
            substat("speed", Some(3.0)),
        ];
        assert!(filter.matches(&item));
    }

    #[test]
    fn min_grade_outside_the_game_domain_is_refused() {
        // A typo'd floor is fail-closed in `matches` yet counts as real in
        // `is_unrestricted` — an armed loop that never matches.
        for grade in ["0", "1", "6", "44"] {
            let err = toml::from_str::<Filter>(&format!("min_grade = {grade}"))
                .expect_err("out-of-domain grade should be refused");
            assert!(
                err.to_string().contains("does not exist"),
                "error should name the offending grade: {err}"
            );
        }
        // 5 is Epic. It carries four substats exactly as Heroic does, which is
        // why a 59-piece sample of substat counts could not see it; the shop's
        // own drop-rate payload lists it, at roughly 0.002% per entry.
        for grade in [2, 3, 4, 5] {
            let filter: Filter =
                toml::from_str(&format!("min_grade = {grade}")).expect("real grade parses");
            assert_eq!(filter.only_rule().min_grade, Some(grade));
        }
        // Absent stays absent — no key, no rule at all.
        assert!(
            toml::from_str::<Filter>("")
                .expect("empty parses")
                .gear
                .is_empty()
        );
    }

    #[test]
    fn a_kind_the_wire_would_tolerate_is_refused_in_a_config_file() {
        let error = toml::from_str::<Filter>("kinds = [\"equipement\"]")
            .expect_err("a typo must not become a criterion nothing satisfies");
        let message = error.to_string();
        // Names what was typed — which `serde(other)` would swallow — and what
        // is legal.
        assert!(message.contains("equipement"), "{message}");
        for legal in ["equipment", "hero", "token"] {
            assert!(message.contains(legal), "{message}");
        }
        // The catch-all's own spelling goes the same way.
        assert!(toml::from_str::<Filter>("kinds = [\"unknown\"]").is_err());

        // The accepted spellings are `ItemKind`'s own, not a second list.
        for kind in HUNTABLE_KINDS {
            let name = toml::to_string(&Filter {
                kinds: vec![kind],
                ..Filter::default()
            })
            .expect("serialize");
            let back: Filter = toml::from_str(&name).expect("a huntable kind round-trips");
            assert_eq!(back.kinds, vec![kind]);
        }

        // `Unknown` stays a legal criterion in memory — see hunt_kinds.
        let unknown_hunter = Filter::matching_default_items();
        assert_eq!(unknown_hunter.kinds, vec![ItemKind::Unknown]);
        assert!(!unknown_hunter.is_unrestricted());
        assert!(unknown_hunter.matches(&ShopItem::default()));
    }

    #[test]
    fn inert_filter_keys_are_not_serialized() {
        let filter = Filter {
            names: vec!["ticketrare_name".to_owned()],
            ..Filter::default()
        };
        let text = toml::to_string(&filter).expect("serialize");
        assert!(text.contains("ticketrare_name"));
        for inert in ["kinds", "sets", "required_substats", "include_sold_out"] {
            assert!(!text.contains(inert), "{inert} written into {text:?}");
        }
        // And a set value still round-trips, so the skips are not a data loss.
        let filter = Filter {
            include_sold_out: true,
            kinds: vec![ItemKind::Equipment],
            ..Filter::default()
        };
        let text = toml::to_string(&filter).expect("serialize");
        let back: Filter = toml::from_str(&text).expect("deserialize");
        assert_eq!(filter, back);
    }

    #[test]
    fn filter_round_trips_through_toml() {
        let filter = Filter {
            names: vec!["ticketrare_name".to_owned()],
            gear: vec![GearRule {
                min_substats: Some(3),
                max_price: Some(Gold::new(300_000)),
                min_grade: Some(4),
                required_substats: vec![SubstatReq {
                    name: "speed".to_owned(),
                    min: Some(8.0),
                }],
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        let text = toml::to_string(&filter).expect("serialize");
        let back: Filter = toml::from_str(&text).expect("deserialize");
        assert_eq!(filter, back);
    }

    /// Pins the interaction with `shop::sanitized_text`: `names`/`set` are
    /// matched by equality (`Vec::contains`, above), so a sanitized value must
    /// still equal an unsanitized, normal-length config criterion. A criterion
    /// itself longer than `MAX_WIRE_TEXT` would never match post-sanitizing,
    /// but killing one needs both a criterion and a server value each over 120
    /// characters: the longest real value in the repo is 20 characters
    /// (`Wondrous Potion Vial`), the longest set id 11 (`set_counter`),
    /// substat names 5 or fewer. Investigated 2026-08-20, no action.
    #[test]
    fn a_normal_length_name_still_matches_its_criterion_after_sanitizing() {
        use crate::domain::shop::ShopSnapshot;

        let snapshot: ShopSnapshot =
            serde_json::from_str(r#"{"slots":[{"name":"ticketrare_name"}]}"#)
                .expect("snapshot should parse");
        let filter = Filter {
            names: vec!["ticketrare_name".to_owned()],
            ..Filter::default()
        };
        assert!(filter.matches(&snapshot.slots[0]));
    }

    /// The hazard this plan closes: `shop::sanitize_wire_text` used to leave a
    /// trailing space where a control character had been, while
    /// `ui::editor::hunt` trims the player's criterion before storing it. A
    /// name that survives the wire as `"Covenant Bookmark\n"` must still equal
    /// the trimmed criterion the player typed — if the sanitizer's trim ever
    /// regresses, this is the test that notices, not the helper-level one.
    #[test]
    fn a_trimmed_criterion_still_matches_a_name_that_had_trailing_whitespace_on_the_wire() {
        use crate::domain::shop::ShopSnapshot;

        let snapshot: ShopSnapshot =
            serde_json::from_str(r#"{"slots":[{"name":"Covenant Bookmark\n"}]}"#)
                .expect("snapshot should parse");
        let filter = Filter {
            names: vec!["Covenant Bookmark".to_owned()],
            ..Filter::default()
        };
        assert!(filter.matches(&snapshot.slots[0]));
    }

    #[test]
    fn one_failing_criterion_fails_whole() {
        let filter = Filter {
            gear: vec![GearRule {
                max_price: Some(Gold::new(1_000)),
                ..GearRule::default()
            }],
            ..speed_filter()
        };
        assert!(!filter.matches(&equip()));
    }

    /// A token and a gear criterion together used to match NOTHING: every
    /// criterion was joined by AND, so the bookmark failed `sets` and the helm
    /// failed `names`.
    #[test]
    fn a_token_and_a_gear_criterion_match_their_own_items() {
        use crate::domain::shop::ShopSnapshot;

        let roll: ShopSnapshot = serde_json::from_str(
            r#"{"slots":[
                 {"name":"ticketrare_name","kind":"token","price":184000},
                 {"name":"ecq4h_name","kind":"equipment","set":"set_speed",
                  "gear_slot":"helm","grade":4}
               ]}"#,
        )
        .expect("fixture should parse");
        let both = Filter {
            names: vec!["ticketrare_name".to_owned()],
            gear: vec![GearRule {
                sets: vec!["set_speed".to_owned()],
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert!(
            both.matches(&roll.slots[0]),
            "the token should match by name"
        );
        assert!(both.matches(&roll.slots[1]), "the helm should match by set");
    }

    /// Two rules hunt two pieces — and not their product, which is the whole
    /// reason the gear branch became a list.
    ///
    /// Flat, this was one conjunction: slots `[boot, neck]` AND substats
    /// any-of `[speed, cri_dmg]`, which buys a necklace that happened to roll
    /// speed. There was no way to spell "a speed boot or a crit-damage
    /// necklace" at all, and the miss direction spends the player's gold.
    #[test]
    fn two_rules_hunt_two_pieces_rather_than_their_product() {
        let piece = |slot: &str, stat: &str| GearRule {
            slots: vec![slot.to_owned()],
            required_substats: vec![SubstatReq {
                name: stat.to_owned(),
                min: None,
            }],
            ..GearRule::default()
        };
        let filter = Filter {
            gear: vec![piece("boot", "speed"), piece("neck", "cri_dmg")],
            ..Filter::default()
        };
        let gear = |slot: &str, stat: &str| ShopItem {
            gear_slot: Some(slot.to_owned()),
            main_stat: None,
            substats: vec![substat(stat, Some(4.0))],
            ..equip()
        };
        assert!(filter.matches(&gear("boot", "speed")), "the first piece");
        assert!(filter.matches(&gear("neck", "cri_dmg")), "the second");
        // The two cells the product used to buy.
        assert!(!filter.matches(&gear("neck", "speed")));
        assert!(!filter.matches(&gear("boot", "cri_dmg")));
    }

    /// A rule with nothing set arms nothing. The window adds one the moment a
    /// player opens the `+` card, and an armed branch over it would accept the
    /// first item of any roll.
    #[test]
    fn an_empty_rule_neither_arms_nor_matches() {
        let filter = Filter {
            gear: vec![GearRule::default()],
            ..Filter::default()
        };
        assert!(filter.is_unrestricted());
        // Beside a real criterion it must not widen it either: the branch asks
        // every rule that restricts, and this one is not asked at all.
        let named = Filter {
            names: vec!["ticketrare_name".to_owned()],
            gear: vec![GearRule::default()],
            ..Filter::default()
        };
        assert!(!named.matches(&equip()), "a nameless helm answers no name");
        assert!(named.matches(&token("ticketrare_name")));
    }

    /// The gear criteria a flat `[filter]` used to hold still load, as the one
    /// rule they always described — and the file is rewritten in the new shape
    /// the next time it is written.
    #[test]
    fn a_flat_filter_folds_into_one_rule() {
        let filter: Filter = toml::from_str(
            "names = [\"ticketrare_name\"]\nsets = [\"set_speed\"]\nmin_grade = 4\n",
        )
        .expect("a file written before the rules must keep loading");
        assert_eq!(filter.names, vec!["ticketrare_name".to_owned()]);
        let rule = filter.only_rule();
        assert_eq!(rule.sets, vec!["set_speed".to_owned()]);
        assert_eq!(rule.min_grade, Some(4));
        let text = toml::to_string(&filter).expect("serialize");
        assert!(text.contains("[[gear]]"), "{text}");
        let back: Filter = toml::from_str(&text).expect("the new shape reloads");
        assert_eq!(back, filter);
    }

    /// Both spellings at once is refused rather than merged: two gear criteria
    /// in one file are two hunts, and silently picking one is how a player buys
    /// against a rule they cannot see.
    #[test]
    fn a_file_holding_both_spellings_is_refused() {
        let error =
            toml::from_str::<Filter>("sets = [\"set_speed\"]\n\n[[gear]]\nslots = [\"boot\"]\n")
                .expect_err("two gear criteria in one file are ambiguous");
        assert!(error.to_string().contains("gear criteria"), "{error}");
    }

    /// A mode is not a criterion, so a file setting only one folds to no rule
    /// at all — otherwise every such file would grow an inert `[[gear]]` on its
    /// next Apply.
    #[test]
    fn a_mode_alone_does_not_make_a_rule() {
        for text in ["substat_match = \"any\"", "min_substats = 0"] {
            let filter: Filter = toml::from_str(text).expect("parses");
            assert!(filter.gear.is_empty(), "{text} made a rule");
            assert!(filter.is_unrestricted());
        }
    }

    /// A filter naming both branches. Its refusals are pinned separately, by
    /// [`a_two_branch_filter_refuses_an_item_that_answers_neither`].
    fn both_branches() -> Filter {
        Filter {
            names: vec!["ticketrare_name".to_owned()],
            gear: vec![GearRule {
                sets: vec!["set_speed".to_owned()],
                ..GearRule::default()
            }],
            ..Filter::default()
        }
    }

    /// The four cells of the OR. The first three are the compatibility claim:
    /// a filter using at most one branch behaves exactly as it did before the
    /// branches existed.
    #[test]
    fn the_two_branches_cover_their_truth_table() {
        let bookmark = token("ticketrare_name");
        let helm = equip();

        let empty = Filter::default();
        assert!(empty.matches(&bookmark), "no criterion keeps the token");
        assert!(empty.matches(&helm), "no criterion keeps the helm");

        let named = Filter {
            names: vec!["ticketrare_name".to_owned()],
            ..Filter::default()
        };
        assert!(named.matches(&bookmark), "the name branch keeps its token");
        assert!(!named.matches(&helm), "a nameless helm answers no name");

        let geared = Filter {
            gear: vec![GearRule {
                sets: vec!["set_speed".to_owned()],
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert!(!geared.matches(&bookmark), "a setless token answers no set");
        assert!(geared.matches(&helm), "the gear branch keeps its helm");

        let both = both_branches();
        assert!(both.matches(&bookmark), "the token still matches by name");
        assert!(both.matches(&helm), "the helm still matches by set");
    }

    /// The refusing half of the OR, which the truth table's fourth cell cannot
    /// reach — both its items hit, so an arm hardwired to `true` satisfies it.
    /// Two branches widen what is bought; nothing else here pins what a widened
    /// filter still says no to, and the miss direction spends the player's gold.
    #[test]
    fn a_two_branch_filter_refuses_an_item_that_answers_neither() {
        let both = both_branches();
        assert!(
            !both.matches(&token("friendpoint_name")),
            "the friendship bookmark answers neither branch"
        );
        let wrong_set = ShopItem {
            set: Some("set_rage".to_owned()),
            ..equip()
        };
        assert!(
            !both.matches(&wrong_set),
            "a nameless helm of the wrong set answers neither branch"
        );
    }

    /// `min_substats = 0` constrains nothing, so it must not switch the gear
    /// branch on — otherwise a name-only filter would start refusing tokens.
    #[test]
    fn a_zero_substat_floor_does_not_arm_the_gear_branch() {
        let filter = Filter {
            names: vec!["ticketrare_name".to_owned()],
            gear: vec![GearRule {
                min_substats: Some(0),
                ..GearRule::default()
            }],
            ..Filter::default()
        };
        assert!(filter.matches(&token("ticketrare_name")));
    }

    fn slot_filter(slots: &[&str]) -> Filter {
        Filter {
            gear: vec![GearRule {
                slots: slots.iter().map(|s| (*s).to_owned()).collect(),
                ..GearRule::default()
            }],
            ..Filter::default()
        }
    }

    #[test]
    fn a_slot_criterion_is_any_of() {
        // `equip()` is a helm.
        assert!(slot_filter(&["helm"]).matches(&equip()));
        assert!(slot_filter(&["boot", "helm"]).matches(&equip()));
        assert!(!slot_filter(&["boot"]).matches(&equip()));
    }

    #[test]
    fn an_empty_slot_list_does_not_constrain() {
        assert!(slot_filter(&[]).matches(&equip()));
    }

    #[test]
    fn an_unanswered_slot_fails_closed() {
        // A hero, a token, or every item at all when the server had no Catalog.
        // Fail-open would buy across every slot the moment the lookup vanished.
        let slotless = ShopItem {
            gear_slot: None,
            ..equip()
        };
        assert!(!slot_filter(&["helm"]).matches(&slotless));
    }

    #[test]
    fn naming_a_slot_is_a_criterion() {
        assert!(!slot_filter(&["helm"]).is_unrestricted());
        assert!(Filter::default().is_unrestricted());
    }

    /// The journal's cue for the fail-closed edge above. It asks about a ROLL
    /// rather than an item, because one slotless item proves nothing — a roll
    /// legitimately holds heroes and tokens, which never carry a slot.
    ///
    /// Built from JSON rather than from struct literals, so the wire spelling of
    /// `gear_slot` is under test beside the predicate.
    mod slots_unanswerable {
        use super::*;
        use crate::domain::shop::ShopSnapshot;

        fn roll(json: &str) -> ShopSnapshot {
            serde_json::from_str(json).expect("snapshot should parse")
        }

        #[test]
        fn fires_when_the_whole_roll_came_back_slotless() {
            let snapshot = roll(r#"{"slots":[{"name":"a"},{"name":"b"}]}"#);
            assert!(slot_filter(&["helm"]).slots_unanswerable(&snapshot));
        }

        #[test]
        fn stays_quiet_when_any_item_answered() {
            // Five heroes and one helm is a normal roll, not a failure.
            let snapshot = roll(r#"{"slots":[{"name":"a"},{"gear_slot":"helm"}]}"#);
            assert!(!slot_filter(&["helm"]).slots_unanswerable(&snapshot));
        }

        #[test]
        fn stays_quiet_when_no_slot_was_asked_for() {
            let snapshot = roll(r#"{"slots":[{"name":"a"}]}"#);
            assert!(!Filter::default().slots_unanswerable(&snapshot));
        }

        #[test]
        fn stays_quiet_on_an_empty_roll() {
            // Nothing to be unable to answer about; a slotless roll of nothing
            // is a degraded snapshot and a different diagnosis.
            assert!(!slot_filter(&["helm"]).slots_unanswerable(&roll(r#"{"slots":[]}"#)));
        }
    }
}
