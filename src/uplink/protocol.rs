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
    /// A successful buy echoed by the server.
    Purchase(PurchaseNotice),
    /// Unknown type — ignored (forward compatibility).
    #[serde(other)]
    Unknown,
}

/// Payload of a `{type:"purchase"}` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct PurchaseNotice {
    /// Global catalog id — same id space as [`crate::domain::shop::ShopItem`]'s
    /// `id`; `0` when the server omits it.
    #[serde(default)]
    pub item: u32,
    /// Gold paid. Always present in practice; tolerated absent per convention.
    #[serde(default)]
    pub gold: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> ServerMessage {
        serde_json::from_str(json).expect("message should parse")
    }

    #[test]
    fn purchase_full_message_parses() {
        let message = parse(r#"{"type":"purchase","item":102,"gold":250000}"#);
        let ServerMessage::Purchase(notice) = message else {
            panic!("expected Purchase, got {message:?}");
        };
        assert_eq!(
            notice,
            PurchaseNotice {
                item: 102,
                gold: Some(250_000),
            }
        );
    }

    #[test]
    fn purchase_gold_absent_tolerated() {
        let message = parse(r#"{"type":"purchase","item":102}"#);
        let ServerMessage::Purchase(notice) = message else {
            panic!("expected Purchase, got {message:?}");
        };
        assert_eq!(notice.gold, None);
    }

    #[test]
    fn purchase_item_absent_defaults_to_zero() {
        let message = parse(r#"{"type":"purchase"}"#);
        let ServerMessage::Purchase(notice) = message else {
            panic!("expected Purchase, got {message:?}");
        };
        assert_eq!(notice.item, 0);
    }

    #[test]
    fn unknown_type_still_falls_back() {
        let message = parse(r#"{"type":"telemetry","whatever":1}"#);
        assert!(matches!(message, ServerMessage::Unknown));
    }
}
