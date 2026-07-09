//! Contract of the messages returned by the analysis server.
//!
//! The client sends raw bytes (the undecrypted game stream); the server
//! decrypts, interprets, and replies with these structured messages. The fields
//! mirror a shop item as described in the Secret Shop documentation.

use serde::Deserialize;

/// Downstream message from the server to the relay.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Acknowledges a batch of bytes.
    Ack,
    /// Full shop snapshot decoded by the server.
    Shop(ShopSnapshot),
    /// One or more items worth the player's attention.
    Alert(Alert),
    /// Unknown type — ignored (forward compatibility).
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShopSnapshot {
    #[serde(default)]
    pub merchant: Option<String>,
    #[serde(default)]
    pub slots: Vec<ShopItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Alert {
    pub message: String,
    #[serde(default)]
    pub items: Vec<ShopItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShopItem {
    /// Shop slot (1..=6); `0` if the server omits it.
    #[serde(default)]
    pub slot: u8,
    /// Item type; defaults to `Unknown` rather than failing the whole message.
    #[serde(default)]
    pub kind: ItemKind,
    #[serde(default)]
    pub name: Option<String>,
    /// Price in gold (lobby shop).
    #[serde(default)]
    pub price: Option<u32>,
    /// Gear grade (2, 3, or 4).
    #[serde(default)]
    pub grade: Option<u8>,
    /// Gear set (Speed, Critical, ...).
    #[serde(default)]
    pub set: Option<String>,
    #[serde(default)]
    pub substats: Vec<SubStat>,
    #[serde(default)]
    pub required_level: Option<u8>,
    #[serde(default)]
    pub limit: Option<PurchaseLimit>,
    /// Server verdict: this item is worth attention.
    #[serde(default)]
    pub interesting: bool,
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
