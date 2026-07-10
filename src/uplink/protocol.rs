//! Messages returned by the analysis server. The client forwards the raw
//! stream and receives these structured replies; the shop payload is the
//! domain model ([`crate::domain::shop`]), this module only the envelopes.

use serde::Deserialize;

use crate::domain::shop::ShopSnapshot;

/// Downstream message from the server to the relay.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Acknowledges a batch of bytes.
    Ack,
    /// Full shop snapshot; the player's [`crate::domain::filter::Filter`]
    /// decides interest.
    Shop(ShopSnapshot),
    /// Unknown type — ignored (forward compatibility).
    #[serde(other)]
    Unknown,
}
