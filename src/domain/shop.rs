//! The shop model: the trimmed contents the analysis server returns, ready to
//! display and filter. Only this shape crosses the link; how the server
//! produces it is not the client's concern.

use serde::{Deserialize, Deserializer};

#[derive(Debug, Clone, Deserialize)]
pub struct ShopSnapshot {
    #[serde(default)]
    pub merchant: Option<String>,
    #[serde(default)]
    pub slots: Vec<ShopItem>,
    /// Refresh-session facts (balance, cost) — grouped apart because they are
    /// not shop *contents*. Present means both are known; absent means neither
    /// is, which makes crystal limits unenforceable.
    #[serde(default, deserialize_with = "refresh_or_none")]
    pub refresh: Option<RefreshMeta>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct RefreshMeta {
    /// Crystal balance after the debit.
    pub crystal_balance: u32,
    /// Cost of one manual refresh (3 in the lobby).
    pub cost: u32,
}

/// Degrades a partial `refresh` object (or `null`) to `None` rather than
/// failing the whole snapshot, like the rest of the model.
fn refresh_or_none<'de, D: Deserializer<'de>>(de: D) -> Result<Option<RefreshMeta>, D::Error> {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct Partial {
        crystal_balance: Option<u32>,
        cost: Option<u32>,
    }
    Ok(Option::<Partial>::deserialize(de)?.and_then(|partial| {
        Some(RefreshMeta {
            crystal_balance: partial.crystal_balance?,
            cost: partial.cost?,
        })
    }))
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ShopItem {
    /// Wire item id; `0` if the server omits it. Lets a purchase confirmation
    /// (whose `item` is this id) be tied back to the slot the player wanted.
    #[serde(default)]
    pub id: u32,
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
    /// Substats and their values, keyed by internal stat name.
    #[serde(default)]
    pub substats: Vec<SubStat>,
    #[serde(default)]
    pub required_level: Option<u8>,
    #[serde(default)]
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
    pub fn effective_slot(&self, index: usize) -> u8 {
        if self.slot == 0 {
            u8::try_from(index + 1).unwrap_or(u8::MAX)
        } else {
            self.slot
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Equipment,
    Hero,
    Token,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubStat {
    pub name: String,
    #[serde(default)]
    pub value: Option<f64>,
}

/// Purchase limit, e.g. "0/1" (sold out) or "1/1" (available).
#[derive(Debug, Clone, Copy, Deserialize)]
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
}
