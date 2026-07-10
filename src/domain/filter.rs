//! Client-side interest filter: the player's criteria for which shop items are
//! worth stopping the refresh loop to buy. Kept on the client so they can be
//! tuned live from the UI.

use serde::Deserialize;

use crate::domain::shop::{ItemKind, ShopItem};

/// Player criteria, all ANDed; an empty `Vec` or `None` field does not
/// constrain, so a default `Filter` matches every available item.
///
/// Missing data is handled asymmetrically on purpose: `max_price` is
/// fail-closed (an unknown price never satisfies a cap), while sold-out is
/// fail-open (a missing `limit` counts as buyable).
///
/// Deserialized from the config file's `[filter]` section. Unlike the wire
/// models, unknown keys are rejected: a typo here silently loosens the
/// criteria the refresh loop spends crystals against.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Filter {
    /// Kept item kinds (any-of); empty keeps all, including `Unknown`.
    pub kinds: Vec<ItemKind>,
    /// Kept items (any-of), by exact internal name (`ticketrare_name`, ...);
    /// empty keeps all.
    pub names: Vec<String>,
    /// Kept sets (any-of), by exact internal id; empty keeps all.
    pub sets: Vec<String>,
    /// Minimum substat count (raw list length).
    pub min_substats: Option<u8>,
    /// Substats that must all be present, each above its optional threshold.
    pub required_substats: Vec<SubstatReq>,
    /// Inclusive gold cap; an unknown price fails it.
    pub max_price: Option<u32>,
    /// Keep sold-out items (default drops them).
    pub include_sold_out: bool,
}

/// One required substat, by exact internal name (`speed`, `cri`, ...). `min` is
/// an inclusive threshold; `None` means presence is enough.
///
/// `name` is deliberately required (no container default): a nameless
/// requirement would silently match nothing.
#[derive(Debug, Clone, PartialEq, Deserialize)]
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
        if let Some(max) = self.max_price
            && item.price.is_none_or(|price| price > max)
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
    use crate::domain::shop::{PurchaseLimit, SubStat};

    fn substat(name: &str, value: Option<f64>) -> SubStat {
        SubStat {
            name: name.to_owned(),
            value,
        }
    }

    fn equip() -> ShopItem {
        ShopItem {
            id: 4562,
            slot: 1,
            kind: ItemKind::Equipment,
            name: None,
            price: Some(240_000),
            grade: Some(3),
            set: Some("set_speed".to_owned()),
            substats: vec![
                substat("speed", Some(15.0)),
                substat("cri", Some(0.03)),
                substat("att", Some(40.0)),
            ],
            required_level: None,
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
            max_price: Some(240_000),
            ..Filter::default()
        };
        assert!(filter.matches(&equip()));
    }

    #[test]
    fn max_price_above_fails() {
        let filter = Filter {
            max_price: Some(239_999),
            ..Filter::default()
        };
        assert!(!filter.matches(&equip()));
    }

    #[test]
    fn max_price_missing_price_fails() {
        let filter = Filter {
            max_price: Some(240_000),
            ..Filter::default()
        };
        let mut item = equip();
        item.price = None;
        assert!(!filter.matches(&item));
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
    fn one_failing_criterion_fails_whole() {
        // Matches the canonical filter on everything but the added price cap.
        let filter = Filter {
            max_price: Some(1_000),
            ..speed_filter()
        };
        assert!(!filter.matches(&equip()));
    }
}
