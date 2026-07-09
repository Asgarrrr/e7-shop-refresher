//! The shop model: the trimmed contents the analysis server returns, ready to
//! display and filter. Only this shape crosses the link; how the server
//! produces it is not the client's concern.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ShopSnapshot {
    #[serde(default)]
    pub merchant: Option<String>,
    #[serde(default)]
    pub slots: Vec<ShopItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShopItem {
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
    /// Legacy server verdict; the client-side [`crate::domain::filter::Filter`]
    /// is now authoritative. Slated for removal once the controller computes
    /// interest locally.
    #[serde(default)]
    pub interesting: bool,
}

impl ShopItem {
    /// Sold out when a purchase limit is present and exhausted.
    pub fn is_sold_out(&self) -> bool {
        self.limit.is_some_and(|limit| limit.remaining == 0)
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
