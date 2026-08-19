//! Client-side interest filter: the player's criteria for which shop items are
//! worth stopping the refresh loop to buy. Kept on the client so they can be
//! tuned live from the UI.

use serde::{Deserialize, Deserializer, Serialize};

use crate::domain::shop::{Gold, ItemKind, ShopItem};

/// The gear grades the game ships (`config.example.toml` documents the same
/// closed domain). A floor outside it is a typo, and a costly one: `matches` is
/// fail-closed on grade, so every item is dropped, while `is_unrestricted`
/// counts any floor as a real criterion — the loop arms and then refreshes
/// forever, debiting crystals, without ever matching. Rejected while
/// deserializing so the invalid value never exists — the same shape, for the same
/// reason, as [`hunt_kinds`] below, which is where the `[filter] kinds` rule now
/// lives too (it used to be a clause in `Config::validate`).
const GRADE_MIN: u8 = 2;
const GRADE_MAX: u8 = 4;

/// Player criteria, combined with a logical AND; an empty `Vec` or `None` field
/// does not constrain, so a default `Filter` matches every available item.
///
/// Missing data is handled asymmetrically on purpose: `max_price` is
/// fail-closed (an unknown price never satisfies a cap), while sold-out is
/// fail-open (a missing `limit` counts as buyable).
///
/// Deserialized from the config file's `[filter]` section. Unlike the wire
/// models, unknown keys are rejected: a typo here silently loosens the
/// criteria the refresh loop spends crystals against.
///
/// The four never-`None` fields are skipped when empty, like `Timings`' eight
/// ranges: `config/persist.rs` replaces the whole `[filter]` section on Apply,
/// and without the skips the first edit of one criterion writes four inert lines
/// into a file that module exists to leave as the player wrote it. The container
/// `#[serde(default)]` makes every omission round-trip.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Filter {
    /// Kept item kinds (any-of); empty keeps all, `Unknown` items included.
    ///
    /// A `Vec<ItemKind>` and not a narrower hunt-only enum, deliberately — see
    /// [`hunt_kinds`], which refuses the catch-all at the *file* boundary where
    /// it is ambiguous, and explains why the field itself stays open.
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "hunt_kinds")]
    pub kinds: Vec<ItemKind>,
    /// Kept items (any-of), by exact internal name (`ticketrare_name`, ...);
    /// empty keeps all.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    /// Kept sets (any-of), by exact internal id; empty keeps all.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sets: Vec<String>,
    /// Minimum substat count (raw list length).
    pub min_substats: Option<u8>,
    /// Substats that must all be present, each above its optional threshold.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub required_substats: Vec<SubstatReq>,
    /// Inclusive gold cap; an unknown price fails it.
    ///
    /// A [`Gold`], so the cap and the price it is weighed against are one type
    /// and a crystal budget cannot be written here by mistake. `Gold` is
    /// `#[serde(transparent)]` in both directions, so `config.toml` still spells
    /// this as a bare `max_price = 300000`.
    pub max_price: Option<Gold>,
    /// Inclusive minimum gear grade (2, 3, or 4); an unknown grade fails it.
    /// A floor outside that domain is refused at parse time — see `GRADE_MIN`.
    #[serde(deserialize_with = "grade_floor")]
    pub min_grade: Option<u8>,
    /// Keep sold-out items (default drops them).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub include_sold_out: bool,
}

/// Parses `min_grade`, refusing a floor the game has no grade for. Absence and
/// `None` pass through; anything else is an error naming the offending value, so
/// `toml` can point at the line.
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
/// Public because the checkbox row is built from it: the list used to be spelled
/// out there, which is how the fourth box — `Unknown`, the one whose only net
/// effect was writing a `kinds = ["unknown"]` the next launch refused — got
/// added in the first place.
pub const HUNTABLE_KINDS: [ItemKind; 3] = [ItemKind::Equipment, ItemKind::Hero, ItemKind::Token];

/// Parses `[filter] kinds`, refusing the wire's catch-all.
///
/// [`ItemKind`] is deliberately lenient: its `#[serde(other)] Unknown` keeps a
/// *snapshot* decodable when the server adds a kind this build has never heard
/// of, which is the forward-compatibility the whole inbound surface is designed
/// for. Arriving as config text, that same leniency turns `kinds = ["equipement"]`
/// into a criterion no item can satisfy while [`Filter::is_unrestricted`] counts
/// it as a real one — so the loop arms and then refreshes forever, debiting
/// crystals, without ever buying. Refused here, naming the value it could not
/// read, so `toml` can point at the line.
///
/// Refused at the *boundary*, and not by narrowing the field's type to a
/// hunt-only enum — which is what the audit asked for, and which was written and
/// then reverted. `ItemKind::Unknown` is a meaningful criterion *in the domain*:
/// [`Filter::matches`] compares it against a kind the wire actually reported, and
/// `Filter::matching_default_items` — the only restricted-yet-matching fixture
/// 30 tests have, because a default [`ShopItem`] has kind `Unknown` — is built on
/// exactly that. It is ambiguous only as text, where a typo and a deliberate
/// "hunt the kind you cannot name" are the same six bytes. So the ambiguity is
/// resolved where it exists.
///
/// Same shape and same reason as [`grade_floor`] above. It replaces a clause in
/// `Config::validate`, which matters for more than tidiness: the Setup tab reaches
/// the file through `persist::save` with no `Config` in the path, and that is the
/// boundary the old clause never covered.
fn hunt_kinds<'de, D>(de: D) -> Result<Vec<ItemKind>, D::Error>
where
    D: Deserializer<'de>,
{
    // Read as text first, then through `ItemKind`'s own `Deserialize`: the
    // accepted spellings stay whatever its `rename_all = "snake_case"` produces
    // (never re-listed here, so they cannot drift), and the raw string survives
    // long enough for the error to quote what the player actually typed — which
    // `serde(other)` would otherwise have swallowed.
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
/// an inclusive threshold; `None` means presence is enough.
///
/// `name` is deliberately required (no container default): a nameless
/// requirement would silently match nothing.
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
        // Both operands are `Gold` from the parse onward — the field is typed,
        // so there is no lifting call here and no `u32`-against-`u32` moment for
        // a crystal budget to slip into. `is_none_or` is the fail-closed half the
        // type doc promises: an item whose price the server did not send never
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
        self.required_substats
            .iter()
            .all(|req| req.satisfied_by(item))
    }

    /// `true` when no criterion is set — such a filter matches every
    /// available item; the relay treats that as a configuration error.
    /// (`include_sold_out` widens, it does not restrict; `min_substats: 0`
    /// constrains nothing and must not count as a criterion either.)
    pub fn is_unrestricted(&self) -> bool {
        self.kinds.is_empty()
            && self.names.is_empty()
            && self.sets.is_empty()
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
    /// `ShopItem::default()` (kind `Unknown`) — the shared fixture for tests
    /// that arm the loop against default items.
    pub(crate) fn matching_default_items() -> Self {
        Self {
            kinds: vec![ItemKind::Unknown],
            ..Self::default()
        }
    }
}

impl SubstatReq {
    /// Scans *all* substats of the matching name, not just the first: an item
    /// may list the same stat twice (e.g. a blank entry then a rolled value).
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
        // Some(0) matches everything: it must not satisfy the mandatory-filter
        // check (the GUI editor can produce it with two clicks).
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
        // Any grade floor is a real constraint: matches() fail-closes on a
        // gradeless item, so even `min_grade = 2` drops every token/hero (grade
        // None) and keeps only grade-2+ gear. It must arm the loop, not read as
        // "matches everything".
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
        // Same stat listed twice: a blank entry before the real rolled value.
        // A first-match check would grab the blank and wrongly reject.
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
        // Documents the raw-length decision: duplicate names still count.
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
        // A typo'd floor is fail-closed in `matches` yet counts as a real
        // criterion in `is_unrestricted`, so it arms a loop that refreshes
        // forever and never matches. Refused at parse time instead.
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
        // The rule used to be a clause in `Config::validate`, which left the Setup
        // tab's write path (`persist::save`, no `Config` in it) uncovered — the
        // path a checkbox once used to write a `kinds = ["unknown"]` that the next
        // launch refused fatally. It lives on the field now.
        let error = toml::from_str::<Filter>("kinds = [\"equipement\"]")
            .expect_err("a typo must not become a criterion nothing satisfies");
        let message = error.to_string();
        // Names what was typed — which `ItemKind`'s `serde(other)` had already
        // swallowed by the time the old check ran — and what is legal.
        assert!(message.contains("equipement"), "{message}");
        for legal in ["equipment", "hero", "token"] {
            assert!(message.contains(legal), "{message}");
        }
        // The catch-all's own spelling goes the same way.
        assert!(toml::from_str::<Filter>("kinds = [\"unknown\"]").is_err());

        // Every huntable kind still parses, and the accepted spellings are
        // `ItemKind`'s own — not a second list inside `hunt_kinds`.
        for kind in HUNTABLE_KINDS {
            let name = toml::to_string(&Filter {
                kinds: vec![kind],
                ..Filter::default()
            })
            .expect("serialize");
            let back: Filter = toml::from_str(&name).expect("a huntable kind round-trips");
            assert_eq!(back.kinds, vec![kind]);
        }

        // And `Unknown` stays a legal criterion *in memory*: it is what
        // `matching_default_items` restricts on, and `matches` compares it against
        // a kind the wire really reported.
        let unknown_hunter = Filter::matching_default_items();
        assert_eq!(unknown_hunter.kinds, vec![ItemKind::Unknown]);
        assert!(!unknown_hunter.is_unrestricted());
        assert!(unknown_hunter.matches(&ShopItem::default()));
    }

    #[test]
    fn inert_filter_keys_are_not_serialized() {
        // `config::persist` replaces the whole `[filter]` section on Apply, so
        // without the skips the first edit of one criterion writes four no-op
        // lines into a file that module exists to leave alone.
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

    #[test]
    fn one_failing_criterion_fails_whole() {
        // Matches the canonical filter on everything but the added price cap.
        let filter = Filter {
            max_price: Some(Gold::new(1_000)),
            ..speed_filter()
        };
        assert!(!filter.matches(&equip()));
    }
}
