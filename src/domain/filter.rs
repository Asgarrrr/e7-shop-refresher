//! Client-side interest filter: the player's criteria for which shop items are
//! worth stopping the refresh loop to buy. Kept on the client so they can be
//! tuned live from the UI.

use serde::{Deserialize, Deserializer, Serialize};

use crate::domain::shop::{Gold, ItemKind, ShopItem, ShopSnapshot};

/// The gear grades the game ships (`config.example.toml` documents the same
/// closed domain). Rejected while deserializing, because a floor outside it
/// fail-closes in `matches` (dropping every item) while `is_unrestricted`
/// counts it as real: the loop arms and refreshes forever without matching.
const GRADE_MIN: u8 = 2;
const GRADE_MAX: u8 = 4;

/// Player criteria, combined with a logical AND; an empty `Vec` or `None` field
/// does not constrain, so a default `Filter` matches every available item.
///
/// Missing data is handled asymmetrically on purpose: `max_price` is
/// fail-closed (an unknown price never satisfies a cap), sold-out is fail-open
/// (a missing `limit` counts as buyable).
///
/// Deserialized from the config file's `[filter]` section. Unlike the wire
/// models, unknown keys are rejected: a typo here silently loosens the criteria
/// the refresh loop spends crystals against.
///
/// The four never-`None` fields skip when empty because `config/persist.rs`
/// replaces the whole `[filter]` section on Apply — without the skips one edit
/// would write four inert lines into a file meant to stay as the player wrote
/// it. The container `#[serde(default)]` makes every omission round-trip.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Filter {
    /// Kept item kinds (any-of); empty keeps all, `Unknown` items included.
    /// Deliberately not a narrower hunt-only enum — see [`hunt_kinds`].
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "hunt_kinds")]
    pub kinds: Vec<ItemKind>,
    /// Kept items (any-of), by exact internal name (`ticketrare_name`, ...);
    /// empty keeps all.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    /// Kept sets (any-of), by exact internal id; empty keeps all.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sets: Vec<String>,
    /// Kept wearable slots (any-of), by exact internal id (`helm`, `boot`, ...);
    /// empty keeps all.
    ///
    /// Fail-closed like [`Filter::sets`], and that has a sharper edge here: the
    /// slot is resolved server-side, so a server that could not read its Catalog
    /// sends every item with no [`ShopItem::gear_slot`] and this criterion then
    /// matches nothing at all. The loop keeps refreshing until one of the
    /// player's own stop limits fires. Fail-OPEN would be worse — it would buy
    /// across every slot the moment the lookup went missing — but a session that
    /// finds nothing while a slot is named deserves the journal line
    /// [`Self::slots_unanswerable`] backs.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub slots: Vec<String>,
    /// Minimum substat count (raw list length).
    pub min_substats: Option<u8>,
    /// Substats that must all be present, each above its optional threshold.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub required_substats: Vec<SubstatReq>,
    /// Inclusive gold cap; an unknown price fails it.
    ///
    /// A [`Gold`], so a crystal budget cannot be written here by mistake.
    /// `#[serde(transparent)]` in both directions, so `config.toml` still
    /// spells this as a bare `max_price = 300000`.
    pub max_price: Option<Gold>,
    /// Inclusive minimum gear grade (2, 3, or 4); an unknown grade fails it.
    /// A floor outside that domain is refused at parse time — see `GRADE_MIN`.
    #[serde(deserialize_with = "grade_floor")]
    pub min_grade: Option<u8>,
    /// Keep sold-out items (default drops them).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub include_sold_out: bool,
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
            "gear grade {grade} does not exist (expected {GRADE_MIN}, 3 or {GRADE_MAX})"
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
    pub fn matches(&self, item: &ShopItem) -> bool {
        if !self.include_sold_out && item.is_sold_out() {
            return false;
        }
        if !self.kinds.is_empty() && !self.kinds.contains(&item.kind) {
            return false;
        }
        if !self.names.is_empty()
            && !item
                .name
                .as_ref()
                .is_some_and(|name| self.names.contains(name))
        {
            return false;
        }
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
        self.required_substats
            .iter()
            .all(|req| req.satisfied_by(item))
    }

    /// `true` when this filter names a slot and the snapshot answered none —
    /// the shape a Catalog-less server produces, in which [`Self::matches`]
    /// cannot accept anything.
    ///
    /// A question about a whole roll rather than an item, because one gearless
    /// slot proves nothing: a roll legitimately holds heroes and tokens, which
    /// have no slot and never will. Only *every* slot being unanswered
    /// separates "this roll had no gear" from "nobody can answer about gear",
    /// and even then the first reading stays possible — hence a journal line
    /// rather than a refusal to arm.
    pub fn slots_unanswerable(&self, snapshot: &ShopSnapshot) -> bool {
        !self.slots.is_empty()
            && !snapshot.slots.is_empty()
            && snapshot.slots.iter().all(|item| item.gear_slot.is_none())
    }

    /// `true` when no criterion is set — such a filter matches every available
    /// item, which the relay treats as a configuration error. `include_sold_out`
    /// widens rather than restricts, and `min_substats: 0` constrains nothing;
    /// neither counts as a criterion.
    pub fn is_unrestricted(&self) -> bool {
        self.kinds.is_empty()
            && self.names.is_empty()
            && self.sets.is_empty()
            && self.slots.is_empty()
            && self.min_substats.is_none_or(|min| min == 0)
            && self.required_substats.is_empty()
            && self.max_price.is_none()
            // Any grade floor constrains: `matches` is fail-closed, so it drops
            // every gradeless item (tokens, heroes) even at `min_grade = 2`.
            && self.min_grade.is_none()
    }
}

#[cfg(test)]
impl Filter {
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
    fn satisfied_by(&self, item: &ShopItem) -> bool {
        item.substats.iter().any(|stat| {
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
            substats: vec![
                substat("speed", Some(15.0)),
                substat("cri", Some(0.03)),
                substat("att", Some(40.0)),
            ],
            limit: None,
        }
    }

    fn speed_filter() -> Filter {
        Filter {
            kinds: vec![ItemKind::Equipment],
            min_substats: Some(3),
            required_substats: vec![SubstatReq {
                name: "speed".to_owned(),
                min: Some(15.0),
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
            min_substats: Some(0),
            ..Filter::default()
        };
        assert!(noop.is_unrestricted());
        let real = Filter {
            min_substats: Some(1),
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
            sets: vec!["set_speed".to_owned(), "set_counter".to_owned()],
            ..Filter::default()
        };
        assert!(filter.matches(&equip()));
    }

    #[test]
    fn set_none_fails_when_set_filter_active() {
        let filter = Filter {
            sets: vec!["set_speed".to_owned()],
            ..Filter::default()
        };
        let mut item = equip();
        item.set = None;
        assert!(!filter.matches(&item));
    }

    #[test]
    fn set_case_sensitive_no_match() {
        let filter = Filter {
            sets: vec!["Set_Speed".to_owned()],
            ..Filter::default()
        };
        assert!(!filter.matches(&equip()));
    }

    #[test]
    fn max_price_inclusive_boundary() {
        let filter = Filter {
            max_price: Some(Gold::new(240_000)),
            ..Filter::default()
        };
        assert!(filter.matches(&equip()));
    }

    #[test]
    fn max_price_above_fails() {
        let filter = Filter {
            max_price: Some(Gold::new(239_999)),
            ..Filter::default()
        };
        assert!(!filter.matches(&equip()));
    }

    #[test]
    fn max_price_missing_price_fails() {
        let filter = Filter {
            max_price: Some(Gold::new(240_000)),
            ..Filter::default()
        };
        let mut item = equip();
        item.price = None;
        assert!(!filter.matches(&item));
    }

    #[test]
    fn min_grade_accepts_equal_and_above() {
        let filter = Filter {
            min_grade: Some(3),
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
            min_grade: Some(4),
            ..Filter::default()
        };
        assert!(!filter.matches(&equip())); // grade 3, below floor
    }

    #[test]
    fn min_grade_unknown_fails() {
        let filter = Filter {
            min_grade: Some(3),
            ..Filter::default()
        };
        let mut item = equip();
        item.grade = None;
        assert!(!filter.matches(&item)); // fail-closed: unknown grade fails
    }

    #[test]
    fn min_grade_is_a_real_constraint() {
        let real = Filter {
            min_grade: Some(4),
            ..Filter::default()
        };
        assert!(!real.is_unrestricted());
        // Real even at the floor: `matches` fail-closes on a gradeless item, so
        // `min_grade = 2` still drops every token/hero.
        let floor_two = Filter {
            min_grade: Some(2),
            ..Filter::default()
        };
        assert!(!floor_two.is_unrestricted());
        let floor_zero = Filter {
            min_grade: Some(0),
            ..Filter::default()
        };
        assert!(!floor_zero.is_unrestricted());
    }

    #[test]
    fn substat_req_presence_only_min_none() {
        let filter = Filter {
            required_substats: vec![SubstatReq {
                name: "cri".to_owned(),
                min: None,
            }],
            ..Filter::default()
        };
        assert!(filter.matches(&equip()));
        let mut item = equip();
        item.substats.retain(|stat| stat.name != "cri");
        assert!(!filter.matches(&item));
    }

    #[test]
    fn substat_req_min_requires_present_value() {
        let filter = Filter {
            required_substats: vec![SubstatReq {
                name: "speed".to_owned(),
                min: Some(15.0),
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
            required_substats: vec![SubstatReq {
                name: "speed".to_owned(),
                min: Some(15.0),
            }],
            ..Filter::default()
        };
        let mut item = equip();
        item.substats = vec![substat("speed", None), substat("speed", Some(30.0))];
        assert!(filter.matches(&item));
    }

    #[test]
    fn min_substats_counts_duplicates() {
        let filter = Filter {
            min_substats: Some(3),
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
        for grade in ["0", "1", "5", "44"] {
            let err = toml::from_str::<Filter>(&format!("min_grade = {grade}"))
                .expect_err("out-of-domain grade should be refused");
            assert!(
                err.to_string().contains("does not exist"),
                "error should name the offending grade: {err}"
            );
        }
        for grade in [2, 3, 4] {
            let filter: Filter =
                toml::from_str(&format!("min_grade = {grade}")).expect("real grade parses");
            assert_eq!(filter.min_grade, Some(grade));
        }
        // Absent stays absent — the container default, not the check.
        assert_eq!(
            toml::from_str::<Filter>("")
                .expect("empty parses")
                .min_grade,
            None
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
            min_substats: Some(3),
            max_price: Some(Gold::new(300_000)),
            min_grade: Some(4),
            required_substats: vec![SubstatReq {
                name: "speed".to_owned(),
                min: Some(8.0),
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
            max_price: Some(Gold::new(1_000)),
            ..speed_filter()
        };
        assert!(!filter.matches(&equip()));
    }

    fn slot_filter(slots: &[&str]) -> Filter {
        Filter {
            slots: slots.iter().map(|s| (*s).to_owned()).collect(),
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
