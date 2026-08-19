//! Messages returned by the analysis server. The client forwards the raw
//! stream and receives these structured replies; the shop payload is the
//! domain model ([`crate::domain::shop`]), this module only the envelopes.

use serde::Deserialize;

use crate::domain::shop::{CatalogId, Gold, ShopSnapshot, optional_catalog_id};

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
    /// The bought item's catalog id — the same space as
    /// [`crate::domain::shop::ShopItem`]'s `id`, and now the same type, so a
    /// purchase echo cannot be matched against a price or a balance. `None` when
    /// the server omits it, which it spells as `0`; that fold happens once, in
    /// `shop::optional_catalog_id`.
    #[serde(default, deserialize_with = "optional_catalog_id")]
    pub item: Option<CatalogId>,
    /// Gold balance after the buy (not the price). Always present in
    /// practice; tolerated absent per convention.
    ///
    /// [`Gold`], so this balance and the crystal budget the refresh loop
    /// enforces cannot be compared — the two ledgers were one `u32` apart until
    /// the currency pass. No `deserialize_with` and no sentinel fold, unlike
    /// `item` above: `"gold": 0` is a real, empty purse and must veto the next
    /// priced buy, where an absent key must fail open. See [`Gold`].
    #[serde(default)]
    pub gold: Option<Gold>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shop::{Crystals, ItemKind};

    fn parse(json: &str) -> ServerMessage {
        serde_json::from_str(json).expect("message should parse")
    }

    fn shop(json: &str) -> ShopSnapshot {
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
            Some((Crystals::new(95), Crystals::new(3)))
        );
        assert_eq!(snapshot.slots.len(), 1);

        let item = &snapshot.slots[0];
        assert_eq!(item.id, CatalogId::new(102));
        assert_eq!(item.slot, 3);
        assert_eq!(item.kind, ItemKind::Equipment);
        assert_eq!(item.name.as_deref(), Some("Covenant Bookmark"));
        assert_eq!(item.price, Some(Gold::new(184_000)));
        assert_eq!(item.grade, Some(4));
        assert_eq!(item.set.as_deref(), Some("set_speed"));
        assert_eq!(item.substats.len(), 1);
        assert_eq!(item.substats[0].name, "speed");
        assert_eq!(item.substats[0].value, Some(15.0));
        assert_eq!(item.limit.map(|l| (l.remaining, l.total)), Some((1, 1)));
        assert!(!item.is_sold_out());

        // The haul-recording lookup, on the only shape it ever sees off the wire.
        let cid = |raw: u32| CatalogId::new(raw).expect("a nonzero fixture id");
        assert!(snapshot.slot_by_id(cid(102)).is_some());
        assert!(snapshot.slot_by_id(cid(103)).is_none());
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
                item: CatalogId::new(102),
                gold: Some(Gold::new(250_000)),
            }
        );
    }

    /// The two currencies keep the wire shape they had as `u32`s: a bare
    /// number, with `0` a real amount rather than the "absent" the `item`
    /// sentinel spells. Pinned here because this module is the only place the
    /// crate decodes a purchase notice off JSON, so a `deserialize_with`
    /// wrongly copied from `item` onto `gold` — which would turn a broke player
    /// into an unknown balance, and so authorise every buy — is caught here or
    /// nowhere.
    #[test]
    fn a_zero_gold_balance_is_an_empty_purse_not_an_absent_one() {
        let ServerMessage::Purchase(broke) = parse(r#"{"type":"purchase","item":102,"gold":0}"#)
        else {
            panic!("expected Purchase");
        };
        assert_eq!(broke.gold, Some(Gold::new(0)));

        let ServerMessage::Purchase(silent) =
            parse(r#"{"type":"purchase","item":102,"gold":null}"#)
        else {
            panic!("expected Purchase");
        };
        assert_eq!(silent.gold, None);
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
    fn purchase_item_absent_and_the_zero_sentinel_both_read_as_no_id() {
        // The wire spells "I have no id for this buy" two ways — the key absent,
        // and the key present as `0`. Both fold to `None` at the boundary, so
        // `on_purchase` has no sentinel left to re-derive.
        for json in [
            r#"{"type":"purchase"}"#,
            r#"{"type":"purchase","item":0}"#,
            r#"{"type":"purchase","item":null}"#,
        ] {
            let message = parse(json);
            let ServerMessage::Purchase(notice) = message else {
                panic!("expected Purchase, got {message:?} for {json}");
            };
            assert_eq!(notice.item, None, "{json}");
        }
    }

    /// Slot tolerance at the envelope, which is where the outage happened.
    ///
    /// `domain::shop`'s own tests pin `lenient_slots` at the `ShopSnapshot`
    /// level, and that is where the decision lives — but the failure was a whole
    /// `ServerMessage` refused, taking the merchant, the refresh meta and the
    /// five good slots with it, and this module is the only place the crate
    /// decodes one off JSON. The inner test cannot pass while the outer one
    /// fails, so this is not a second opinion; it is the assertion that the
    /// tolerance is reachable from the wire at all.
    ///
    /// `slot: 300` overflows the `u8`, which is a per-element failure. A `slots`
    /// that is not an array at all is the other half: the whole list degrades to
    /// empty, and the message still arrives.
    #[test]
    fn one_undecodable_slot_costs_its_slot_and_not_the_message() {
        let snapshot = shop(
            r#"{"type":"shop","merchant":"Secret Shop",
                 "slots":[{"id":101,"slot":1},{"id":102,"slot":300}],
                 "refresh":{"crystal_balance":95,"cost":3}}"#,
        );
        assert_eq!(snapshot.slots.len(), 1, "the readable slot must survive");
        assert_eq!(snapshot.slots[0].id, CatalogId::new(101));
        // The fields that used to die with the message: everything outside
        // `slots` is what makes a refused envelope expensive.
        assert_eq!(snapshot.merchant.as_deref(), Some("Secret Shop"));
        assert_eq!(
            snapshot.refresh.map(|meta| meta.cost),
            Some(Crystals::new(3))
        );

        let scalar = shop(r#"{"type":"shop","merchant":"Secret Shop","slots":7}"#);
        assert!(scalar.slots.is_empty());
        assert_eq!(scalar.merchant.as_deref(), Some("Secret Shop"));
    }

    #[test]
    fn unknown_type_still_falls_back() {
        let message = parse(r#"{"type":"telemetry","whatever":1}"#);
        assert!(matches!(message, ServerMessage::Unknown));
    }
}
