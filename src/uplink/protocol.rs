//! Messages returned by the analysis server. The client forwards the raw
//! stream and receives these structured replies; the shop payload is the
//! domain model ([`crate::domain::shop`]), this module only the envelopes.

use serde::Deserialize;

use crate::domain::shop::ShopSnapshot;

/// Downstream message from the server to the relay.
///
/// Internally tagged, so the `type` value *is* the wire contract, and
/// `rename_all = "snake_case"` fixes it to `ack` / `shop` / `purchase`. All
/// three strings are pinned by fixtures in this module rather than only
/// asserted through Rust values, because `#[serde(other)]` turns a mismatch on
/// either side into `Unknown` instead of an error — renaming a variant here (or
/// a field in [`crate::domain::shop`]) would otherwise compile, pass every test,
/// and leave the shipped app permanently blind.
///
/// The representation also constrains what may be added: an internally tagged
/// enum cannot carry a newtype variant wrapping a primitive or a sequence. It
/// works today only because [`ShopSnapshot`] and [`PurchaseNotice`] both
/// deserialize from a map — a future `Error(String)` variant would compile and
/// fail at *runtime*, reported once per connection by
/// `websocket::forward`'s decode arm.
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
    /// Unknown type — tolerated (forward compatibility) but not silent:
    /// `websocket::forward` reports it once per connection, because otherwise a
    /// protocol skew reads exactly like a server that stopped talking.
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
    /// Gold balance after the buy (not the price). Always present in
    /// practice; tolerated absent per convention.
    #[serde(default)]
    pub gold: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shop::ItemKind;

    fn parse(json: &str) -> ServerMessage {
        serde_json::from_str(json).expect("message should parse")
    }

    fn shop(json: &str) -> crate::domain::shop::ShopSnapshot {
        let message = parse(json);
        let ServerMessage::Shop(snapshot) = message else {
            panic!("expected Shop, got {message:?}");
        };
        snapshot
    }

    /// The wire contract, end to end: the `shop` tag plus every field of a fully
    /// populated slot. Nothing else in the crate decodes a `ShopSnapshot` from
    /// JSON — every other test builds one as a Rust value — so a rename on
    /// either side of the link is caught here or nowhere.
    #[test]
    fn shop_tag_and_every_item_field_are_the_wire_contract() {
        let snapshot = shop(
            r#"{"type":"shop","merchant":"Secret Shop","slots":[
                 {"id":102,"slot":3,"kind":"equipment","name":"Covenant Bookmark",
                  "price":184000,"grade":4,"set":"set_speed",
                  "substats":[{"name":"speed","value":15.0}],
                  "limit":{"remaining":1,"total":1}}],
               "refresh":{"crystal_balance":95,"cost":3}}"#,
        );

        assert_eq!(snapshot.merchant.as_deref(), Some("Secret Shop"));
        assert_eq!(
            snapshot
                .refresh
                .map(|meta| (meta.crystal_balance, meta.cost)),
            Some((95, 3))
        );
        assert_eq!(snapshot.slots.len(), 1);

        let item = &snapshot.slots[0];
        assert_eq!(item.catalog_id(), Some(102));
        assert_eq!(item.slot, 3);
        assert_eq!(item.kind, ItemKind::Equipment);
        assert_eq!(item.name.as_deref(), Some("Covenant Bookmark"));
        assert_eq!(item.price, Some(184_000));
        assert_eq!(item.grade, Some(4));
        assert_eq!(item.set.as_deref(), Some("set_speed"));
        assert_eq!(item.substats.len(), 1);
        assert_eq!(item.substats[0].name, "speed");
        assert_eq!(item.substats[0].value, Some(15.0));
        assert_eq!(item.limit.map(|l| (l.remaining, l.total)), Some((1, 1)));
        assert!(!item.is_sold_out());

        // The haul-recording lookup, on the only shape it ever sees off the wire.
        assert!(snapshot.slot_by_id(102).is_some());
        assert!(snapshot.slot_by_id(103).is_none());
    }

    /// The three `ItemKind` spellings and the `other` fallback, which decide
    /// whether the filter can ever match an equipment criterion.
    #[test]
    fn item_kind_wire_spellings_are_pinned() {
        let snapshot = shop(
            r#"{"type":"shop","slots":[{"kind":"equipment"},{"kind":"hero"},
                 {"kind":"token"},{"kind":"something_new"}]}"#,
        );
        let kinds: Vec<ItemKind> = snapshot.slots.iter().map(|item| item.kind).collect();
        assert_eq!(
            kinds,
            [
                ItemKind::Equipment,
                ItemKind::Hero,
                ItemKind::Token,
                ItemKind::Unknown
            ]
        );
    }

    #[test]
    fn ack_tag_parses_as_ack_not_unknown() {
        assert!(matches!(parse(r#"{"type":"ack"}"#), ServerMessage::Ack));
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
