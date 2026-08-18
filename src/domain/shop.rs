//! The shop model: the trimmed contents the analysis server returns, ready to
//! display and filter. Only this shape crosses the link; how the server
//! produces it is not the client's concern.

use std::num::NonZeroU32;

use serde::{Deserialize, Deserializer, Serialize};

/// One shop roll as the analysis server trimmed it: the merchant, the slots on
/// offer, and the refresh-session facts. Every field is optional on the wire —
/// a degraded message still reaches the view rather than failing the link.
#[derive(Debug, Clone, Deserialize)]
pub struct ShopSnapshot {
    #[serde(default)]
    pub merchant: Option<String>,
    #[serde(default)]
    pub slots: Vec<ShopItem>,
    /// Refresh-session facts (balance, cost) — grouped apart because they are
    /// not shop *contents*. Present means both are known; absent, the cost
    /// falls back to the game constant and only out-of-funds detection is
    /// lost.
    #[serde(default, deserialize_with = "object_or_none")]
    pub refresh: Option<RefreshMeta>,
}

/// A global catalog id — the identity the purchase echo uses to name the item
/// the player asked for.
///
/// Two things at once, both of which used to be `u32`:
///
/// 1. **The `0` sentinel is gone.** `NonZeroU32` inside, so "the server omitted
///    the id" is `None` and nothing else. It used to be `id: u32` with `0`
///    standing in for absent, interpreted by a free `shop::catalog_id(id)`
///    documented as *"the only place the `0` sentinel is interpreted — do not
///    re-derive the comparison"*. That contract was broken while it was being
///    written: `Controller::on_purchase` re-derived it as `if item != 0 && …`.
///    A sentinel's whole cost is that every reader has to remember it, and the
///    only fix that does not depend on remembering is not being able to spell it.
///    Both interpreters — the free function and `ShopItem::catalog_id` — are gone
///    with it; the conversion happens once, in [`optional_catalog_id`], at the
///    only place a raw wire number arrives.
/// 2. **It is not a counter.** The id space used to be assignable from any
///    `u32` in the crate: a gold balance, a price, a crystal cost, a refresh
///    count. `checklist`, `bought`, `BuyTarget::id` and `PurchaseNotice::item`
///    all speak this type now, so an id and an amount cannot be swapped.
///
/// `#[serde(transparent)]`, so the wire shape is a bare number, unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct CatalogId(NonZeroU32);

impl CatalogId {
    /// The id `raw` names, or `None` for the `0` the server sends when it has
    /// none. The single interpreter of that number — see the type.
    #[must_use]
    pub const fn new(raw: u32) -> Option<Self> {
        match NonZeroU32::new(raw) {
            Some(id) => Some(Self(id)),
            None => None,
        }
    }

    /// The number, for a wire field or a log line.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

impl std::fmt::Display for CatalogId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Reads a wire catalog id: absent, `null` and `0` all mean "no id".
///
/// The one place a raw id number is interpreted, which is the whole point of
/// [`CatalogId`]. `0` is the server's own spelling of absent, so it has to be
/// accepted and folded here rather than refused — the message is still a good
/// message, it just cannot be tied back to a slot.
pub(crate) fn optional_catalog_id<'de, D>(de: D) -> Result<Option<CatalogId>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<u32>::deserialize(de)?.and_then(CatalogId::new))
}

impl ShopSnapshot {
    /// The slot bearing this catalog id, if any. Ids are unique within a
    /// snapshot (the shop never lists an item twice), so at most one matches.
    /// The single home for a find-by-id; an item whose id the server omitted
    /// can no longer be matched by accident, because it has no id to compare.
    pub fn slot_by_id(&self, id: CatalogId) -> Option<&ShopItem> {
        self.slots.iter().find(|item| item.id == Some(id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct RefreshMeta {
    /// Crystal balance after the debit.
    pub crystal_balance: u32,
    /// Cost of one manual refresh (3 in the lobby).
    pub cost: u32,
}

/// Tolerant optional side-channel object (`refresh`, `limit`): a partial,
/// `null`, or mistyped value degrades to `None` rather than failing the whole
/// snapshot. The value is consumed wholesale first — a bare `?` on the typed
/// parse would abort the surrounding message mid-stream.
///
/// The degradation is *logged*, because both of these fields change what the app
/// does and not just what it shows: a dropped `limit` makes a sold-out slot read
/// buyable (the actuator then clicks Buy, no echo arrives, and the watchdog halts
/// `Unresponsive` blaming the game), and a dropped `refresh` silently disables
/// out-of-funds detection. `debug!` and not `warn!`: the default filter keeps it
/// in the log file, and it is not the player's problem to read.
fn object_or_none<'de, D, T>(de: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(de)?;
    match serde_json::from_value::<T>(value) {
        Ok(parsed) => Ok(Some(parsed)),
        Err(error) => {
            tracing::debug!(
                %error,
                field = std::any::type_name::<T>(),
                "tolerated an undecodable side-channel object — degraded to absent"
            );
            Ok(None)
        }
    }
}

/// Tolerant wire collection (`substats`): an undecodable element is dropped,
/// a non-array value degrades to empty — the containing message survives.
///
/// Logged for the same reason as [`object_or_none`]: a silently shortened
/// substat list quietly fails `min_substats` and every `required_substats`
/// threshold, so the loop refreshes past an item the player wanted.
fn lenient_elements<'de, D, T>(de: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let serde_json::Value::Array(values) = serde_json::Value::deserialize(de)? else {
        tracing::debug!(
            field = std::any::type_name::<T>(),
            "tolerated a non-array wire collection — degraded to empty"
        );
        return Ok(Vec::new());
    };
    Ok(values
        .into_iter()
        .filter_map(|value| match serde_json::from_value(value) {
            Ok(parsed) => Some(parsed),
            Err(error) => {
                tracing::debug!(
                    %error,
                    field = std::any::type_name::<T>(),
                    "dropped an undecodable wire collection element"
                );
                None
            }
        })
        .collect())
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ShopItem {
    /// The item's catalog id, `None` when the server omits it. Lets a purchase
    /// confirmation (whose `item` is this id) be tied back to the slot the player
    /// wanted.
    ///
    /// An `Option<CatalogId>` and not a `u32` with `0` for absent — see
    /// [`CatalogId`] for what that sentinel cost.
    #[serde(default, deserialize_with = "optional_catalog_id")]
    pub id: Option<CatalogId>,
    /// Shop slot (1..=6); `0` if the server omits it.
    #[serde(default)]
    pub slot: u8,
    /// Defaults to `Unknown` rather than failing the whole message.
    #[serde(default)]
    pub kind: ItemKind,
    #[serde(default)]
    pub name: Option<String>,
    /// Price in gold.
    #[serde(default)]
    pub price: Option<u32>,
    /// Gear grade (2, 3, or 4).
    #[serde(default)]
    pub grade: Option<u8>,
    /// Gear set, by internal id (`set_speed`, `set_immune`, ...).
    #[serde(default)]
    pub set: Option<String>,
    /// Substats and their values, keyed by internal stat name. A nameless or
    /// mistyped entry is dropped, not fatal: it could never match a name-keyed
    /// criterion anyway.
    #[serde(default, deserialize_with = "lenient_elements")]
    pub substats: Vec<Substat>,
    /// Fail-open like an absent field: a partial or mistyped limit degrades
    /// to `None` (buyable), matching the server's own omission semantics.
    #[serde(default, deserialize_with = "object_or_none")]
    pub limit: Option<PurchaseLimit>,
}

impl ShopItem {
    /// Sold out when a purchase limit is present and exhausted.
    pub fn is_sold_out(&self) -> bool {
        self.limit.is_some_and(|limit| limit.remaining == 0)
    }

    /// Player-facing slot number: the wire slot, or the 1-based position when
    /// the server omitted it (`slot == 0`), clamped so an oversized shop
    /// cannot wrap back into the `0` sentinel.
    ///
    /// Not injective on malformed shops: a fallback number can collide with
    /// another item's wire slot, so callers matching items by this number may
    /// over-select there.
    ///
    /// # Examples
    ///
    /// ```
    /// use arkyve_refresh_shop::domain::shop::ShopItem;
    ///
    /// // Slot omitted (`0`): the 1-based position stands in.
    /// let omitted = ShopItem::default();
    /// assert_eq!(omitted.effective_slot(0), 1);
    /// assert_eq!(omitted.effective_slot(5), 6);
    ///
    /// // A wire slot always wins, whatever the position.
    /// let numbered = ShopItem {
    ///     slot: 4,
    ///     ..ShopItem::default()
    /// };
    /// assert_eq!(numbered.effective_slot(0), 4);
    ///
    /// // Clamped, never wrapped: an oversized shop must not fall back onto
    /// // the `0` sentinel it is standing in for.
    /// assert_eq!(omitted.effective_slot(300), u8::MAX);
    /// ```
    pub fn effective_slot(&self, index: usize) -> u8 {
        if self.slot == 0 {
            u8::try_from(index + 1).unwrap_or(u8::MAX)
        } else {
            self.slot
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Equipment,
    Hero,
    Token,
    #[default]
    #[serde(other)]
    Unknown,
}

/// One rolled substat of a gear item, by internal stat name. The value is
/// optional: the wire lists blank entries, which no threshold can satisfy.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Substat {
    pub name: String,
    #[serde(default)]
    pub value: Option<f64>,
}

/// Purchase limit, e.g. "0/1" (sold out) or "1/1" (available).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct PurchaseLimit {
    pub remaining: u32,
    pub total: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> ShopSnapshot {
        serde_json::from_str(json).expect("snapshot should parse")
    }

    #[test]
    fn refresh_full_object_parses() {
        let snapshot = parse(r#"{"refresh":{"crystal_balance":95,"cost":3}}"#);
        assert_eq!(
            snapshot.refresh,
            Some(RefreshMeta {
                crystal_balance: 95,
                cost: 3,
            })
        );
    }

    #[test]
    fn refresh_partial_object_degrades_to_none() {
        // A half-shipped `refresh` must not fail the whole snapshot.
        let snapshot = parse(r#"{"refresh":{"crystal_balance":95},"slots":[{}]}"#);
        assert_eq!(snapshot.refresh, None);
        assert_eq!(snapshot.slots.len(), 1);
    }

    #[test]
    fn refresh_null_or_absent_is_none() {
        assert_eq!(parse(r#"{"refresh":null}"#).refresh, None);
        assert_eq!(parse("{}").refresh, None);
    }

    #[test]
    fn refresh_mistyped_degrades_to_none() {
        // The degrade contract covers wrong types, not just partial objects.
        assert_eq!(parse(r#"{"refresh":5,"slots":[{}]}"#).refresh, None);
        assert_eq!(parse(r#"{"refresh":"n/a"}"#).refresh, None);
        assert_eq!(parse(r#"{"refresh":[]}"#).refresh, None);
    }

    #[test]
    fn partial_limit_degrades_to_buyable() {
        // Fail-open like an absent limit: the item stays buyable and the
        // snapshot survives.
        let snapshot = parse(r#"{"slots":[{"id":5,"limit":{"remaining":0}}]}"#);
        assert_eq!(snapshot.slots[0].limit, None);
        assert!(!snapshot.slots[0].is_sold_out());
    }

    #[test]
    fn full_limit_still_parses() {
        let snapshot = parse(r#"{"slots":[{"limit":{"remaining":0,"total":1}}]}"#);
        assert!(snapshot.slots[0].is_sold_out());
    }

    #[test]
    fn bad_substat_entry_is_dropped_not_fatal() {
        let snapshot =
            parse(r#"{"slots":[{"substats":[{"value":4.0},{"name":"speed","value":8.0},7]}]}"#);
        let substats = &snapshot.slots[0].substats;
        assert_eq!(substats.len(), 1);
        assert_eq!(substats[0].name, "speed");
    }

    #[test]
    fn slot_by_id_finds_the_slot_and_the_zero_sentinel_becomes_no_id() {
        // The haul-recording lookup. `0` is the server's spelling of "I have no
        // id for this", so it must never resolve to the slot that carries it —
        // and it cannot, because `CatalogId::new(0)` is `None` and there is no
        // `slot_by_id(0)` left to call.
        let snapshot = parse(r#"{"slots":[{"id":0,"slot":1},{"id":102,"slot":2}]}"#);
        assert_eq!(snapshot.slots[0].id, None, "the 0 folds to absent at parse");
        let hit = snapshot
            .slot_by_id(CatalogId::new(102).expect("102 is not zero"))
            .expect("the slot carrying 102");
        assert_eq!(hit.slot, 2);
        assert_eq!(CatalogId::new(0), None);
        assert!(
            snapshot
                .slot_by_id(CatalogId::new(999).expect("999 is not zero"))
                .is_none()
        );
    }

    #[test]
    fn mistyped_substats_degrade_to_empty() {
        let snapshot = parse(r#"{"slots":[{"id":9,"substats":"corrupt"}]}"#);
        assert!(snapshot.slots[0].substats.is_empty());
        assert_eq!(snapshot.slots[0].id, CatalogId::new(9));
    }
}
