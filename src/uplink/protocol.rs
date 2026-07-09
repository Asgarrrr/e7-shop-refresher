//! Messages returned by the analysis server. The client forwards the raw
//! stream and receives these structured replies; the shop payload is the
//! domain model ([`crate::domain::shop`]), this module only the envelopes.

use serde::Deserialize;

use crate::domain::shop::{ShopItem, ShopSnapshot};

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
pub struct Alert {
    pub message: String,
    #[serde(default)]
    pub items: Vec<ShopItem>,
}
