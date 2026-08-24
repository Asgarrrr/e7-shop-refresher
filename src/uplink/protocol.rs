//! Messages returned by the analysis server. The client forwards the raw
//! stream and receives these structured replies; the shop payload is the
//! domain model ([`crate::domain::shop`]), this module only the envelopes.

use serde::{Deserialize, Deserializer};

use crate::domain::shop::{CatalogId, Gold, ShopSnapshot, optional_catalog_id};

/// Downstream message from the server to the relay.
///
/// The `type` value *is* the wire contract, so the three tag strings are pinned
/// by JSON fixtures in this module rather than only asserted through Rust
/// values: `#[serde(other)]` turns a mismatch on either side into `Unknown`
/// instead of an error, so renaming a variant here (or a field in
/// [`crate::domain::shop`]) would otherwise compile, pass every test, and leave
/// the shipped app permanently blind.
///
/// The representation also constrains what may be added: an internally tagged
/// enum cannot carry a newtype variant wrapping a primitive or a sequence, so a
/// future `Error(String)` variant would compile and fail at *runtime*.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Acknowledges a batch of bytes.
    Ack,
    Shop(ShopSnapshot),
    Purchase(PurchaseNotice),
    /// The values a [`crate::domain::filter::Filter`] may name, with the game's
    /// own labels — pushed once when the connection opens.
    ///
    /// It comes from the server because only the server can read the game's
    /// Catalog, and it is a *vocabulary* rather than state: nothing in it
    /// decides anything, it lets the filter editor offer choices instead of a
    /// text box. A server without a Catalog never sends it, so every field
    /// defaults to empty and the editor falls back to free text.
    Catalog(FilterVocabulary),
    /// Unknown type — tolerated (forward compatibility) but not silent:
    /// `websocket::forward` reports it once per connection, because otherwise a
    /// protocol skew reads exactly like a server that stopped talking.
    #[serde(other)]
    Unknown,
}

/// Payload of a `{type:"catalog"}` message: the pickable values, in the order
/// the server sent them.
///
/// Every field is tolerant and defaults to empty. A vocabulary is a convenience
/// for the editor and never an authority — [`crate::domain::filter::Filter`]
/// matches on the raw ids the shop payload carries, so a missing or partial
/// catalog costs a picker, never a verdict. That is also why nothing here is
/// validated against the filter: an id the player already has in `config.toml`
/// must keep working whether or not this list mentions it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct FilterVocabulary {
    /// Wearable gear sets, in the game's own list order.
    #[serde(deserialize_with = "lenient_entries")]
    pub sets: Vec<VocabularyEntry>,
    /// The substats a gear piece can roll. The wire sends the percent-bearing
    /// ones as fractions (`att_rate: 0.03` is 3%), so an editor collecting a
    /// threshold for one of these must store `0.03` and not `3`.
    ///
    /// Which ones those are is [`VocabularyEntry::percent`] and nothing else.
    /// The game also labels them "(%)", but a label is a translation — the
    /// server refuses to derive the flag from one and so must this end.
    #[serde(deserialize_with = "lenient_entries")]
    pub substats: Vec<VocabularyEntry>,
    /// The wearable slots, matching [`crate::domain::shop::ShopItem::gear_slot`].
    #[serde(deserialize_with = "lenient_entries")]
    pub slots: Vec<VocabularyEntry>,
    /// The tokens the shop sells, with the game's own price — the hunt's
    /// subject, offered as cards rather than as ids to type.
    #[serde(deserialize_with = "lenient_tokens")]
    pub tokens: Vec<TokenEntry>,
    /// Gear-set icons, keyed by the same id as [`Self::sets`]: base64 PNGs of
    /// 44x44, some 53 KB for the whole set.
    ///
    /// They ride this message instead of being fetched from the game's CDN
    /// because the relay talks to exactly one host, and a second endpoint would
    /// be a second failure mode and a second set of TLS roots for a 2 KB
    /// picture. Nothing is validated here — not the base64, not the PNG: the
    /// window decodes them (`ui::icons`), and a set with no readable icon draws
    /// as the text chip the editor already falls back to.
    #[serde(deserialize_with = "lenient_icons")]
    pub icons: std::collections::HashMap<String, String>,
}

/// One pickable value: the id a filter matches on, and the words a player reads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct VocabularyEntry {
    pub id: String,
    pub label: String,
    /// `true` where the wire sends this substat as a *fraction*: `att_rate:
    /// 0.03` is 3%, while `att`, `speed`, `def` and `max_hp` are whole numbers.
    ///
    /// Its one reader is `ui::editor::hunt::substat_chips`, which shows and
    /// steps such a threshold in whole percent and stores the fraction
    /// [`crate::domain::filter::Filter::matches`] compares against.
    ///
    /// Absent reads `false`, which is the safe direction: a whole-number
    /// stepper over a fraction is visibly wrong, where the reverse silently
    /// collects a hundredfold.
    #[serde(default)]
    pub percent: bool,
}

/// One token card: what the shop sells it as, and what it costs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TokenEntry {
    pub id: String,
    pub label: String,
    /// The game's list price. Tolerated absent per this module's convention;
    /// [`Gold`], so it cannot be compared against a crystal budget.
    #[serde(default)]
    pub price: Option<Gold>,
}

/// Tolerant vocabulary list: an undecodable entry is dropped and a non-array
/// degrades to empty, so one malformed row cannot cost the whole message.
///
/// Not shared with `shop::lenient_elements` on purpose — that one lives in the
/// domain and this is an envelope concern; the duplication is four lines
/// against an import that would tie the two modules' tolerance together.
fn lenient_entries<'de, D>(de: D) -> Result<Vec<VocabularyEntry>, D::Error>
where
    D: Deserializer<'de>,
{
    let serde_json::Value::Array(values) = serde_json::Value::deserialize(de)? else {
        tracing::debug!("tolerated a non-array vocabulary list — degraded to empty");
        return Ok(Vec::new());
    };
    Ok(values
        .into_iter()
        .filter_map(|value| match serde_json::from_value(value) {
            Ok(parsed) => Some(parsed),
            Err(error) => {
                tracing::debug!(%error, "dropped an undecodable vocabulary entry");
                None
            }
        })
        .collect())
}

/// [`lenient_entries`] for the token list — same tolerance, same reasoning, a
/// second body only because the element type differs.
fn lenient_tokens<'de, D>(de: D) -> Result<Vec<TokenEntry>, D::Error>
where
    D: Deserializer<'de>,
{
    let serde_json::Value::Array(values) = serde_json::Value::deserialize(de)? else {
        tracing::debug!("tolerated a non-array token list — degraded to empty");
        return Ok(Vec::new());
    };
    Ok(values
        .into_iter()
        .filter_map(|value| match serde_json::from_value(value) {
            Ok(parsed) => Some(parsed),
            Err(error) => {
                tracing::debug!(%error, "dropped an undecodable token entry");
                None
            }
        })
        .collect())
}

/// [`lenient_entries`] for the icon table — a non-object degrades to empty and a
/// non-string value drops its entry, so a mistyped blob costs one picture rather
/// than the whole vocabulary. Sharing a body with the two above would mean a
/// generic over both the container and the element; the tolerance is the point
/// and it is four lines.
fn lenient_icons<'de, D>(de: D) -> Result<std::collections::HashMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let serde_json::Value::Object(entries) = serde_json::Value::deserialize(de)? else {
        tracing::debug!("tolerated a non-object icon table — degraded to empty");
        return Ok(std::collections::HashMap::new());
    };
    Ok(entries
        .into_iter()
        .filter_map(|(id, value)| match value {
            serde_json::Value::String(encoded) => Some((id, encoded)),
            _ => {
                tracing::debug!(%id, "dropped a non-string icon");
                None
            }
        })
        .collect())
}

/// Payload of a `{type:"purchase"}` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct PurchaseNotice {
    /// Typed as [`crate::domain::shop::ShopItem`]'s `id` so a purchase echo
    /// cannot be matched against a price or a balance. The server spells "no id"
    /// as `0`; that fold to `None` happens once, in `shop::optional_catalog_id`.
    #[serde(default, deserialize_with = "optional_catalog_id")]
    pub item: Option<CatalogId>,
    /// Gold balance after the buy (not the price). Always present in practice;
    /// tolerated absent per convention.
    ///
    /// [`Gold`], so this balance and the crystal budget the refresh loop
    /// enforces cannot be compared. No sentinel fold, unlike `item` above:
    /// `"gold": 0` is a real, empty purse and must veto the next priced buy,
    /// where an absent key must fail open.
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

    /// Nothing else in the crate decodes a `ShopSnapshot` from JSON — every
    /// other test builds one as a Rust value — so a rename on either side of the
    /// link is caught here or nowhere.
    #[test]
    fn shop_tag_and_every_item_field_are_the_wire_contract() {
        let snapshot = shop(
            r#"{"type":"shop","merchant":"Secret Shop","slots":[
                 {"id":102,"slot":3,"kind":"equipment","name":"Covenant Bookmark",
                  "price":184000,"grade":4,"set":"set_speed",
                  "substats":[{"name":"speed","value":15.0}],
                  "gear_slot":"helm",
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
        assert_eq!(item.gear_slot.as_deref(), Some("helm"));
        assert_eq!(item.substats.len(), 1);
        assert_eq!(item.substats[0].name, "speed");
        assert_eq!(item.substats[0].value, Some(15.0));
        assert_eq!(item.limit.map(|l| (l.remaining, l.total)), Some((1, 1)));
        assert!(!item.is_sold_out());

        let cid = |raw: u32| CatalogId::new(raw).expect("a nonzero fixture id");
        assert!(snapshot.slot_by_id(cid(102)).is_some());
        assert!(snapshot.slot_by_id(cid(103)).is_none());
    }

    /// These spellings decide whether the filter can ever match an equipment
    /// criterion.
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

    /// A `deserialize_with` wrongly copied from `item` onto `gold` would turn a
    /// broke player into an unknown balance, and so authorise every buy.
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
        // Both spellings fold to `None` at the boundary, so `on_purchase` has no
        // sentinel left to re-derive.
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

    /// `domain::shop` pins `lenient_slots` where the decision lives; this
    /// asserts the tolerance is reachable from the wire at all, because the
    /// failure it was written for refused a whole `ServerMessage` — merchant,
    /// refresh meta and five good slots with it.
    ///
    /// `slot: 300` overflows the `u8`, a per-element failure; a `slots` that is
    /// not an array at all is the other half.
    #[test]
    fn one_undecodable_slot_costs_its_slot_and_not_the_message() {
        let snapshot = shop(
            r#"{"type":"shop","merchant":"Secret Shop",
                 "slots":[{"id":101,"slot":1},{"id":102,"slot":300}],
                 "refresh":{"crystal_balance":95,"cost":3}}"#,
        );
        assert_eq!(snapshot.slots.len(), 1, "the readable slot must survive");
        assert_eq!(snapshot.slots[0].id, CatalogId::new(101));
        // Everything outside `slots` is what makes a refused envelope expensive.
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

    fn vocabulary(json: &str) -> FilterVocabulary {
        let message = parse(json);
        let ServerMessage::Catalog(received) = message else {
            panic!("expected Catalog, got {message:?}");
        };
        received
    }

    /// The `catalog` tag and its three field names are the wire contract, and
    /// this is the only place either side's spelling is checked: `#[serde(other)]`
    /// turns a mismatched tag into `Unknown`, which compiles, passes every other
    /// test, and leaves the editor permanently empty.
    #[test]
    fn catalog_tag_and_every_field_are_the_wire_contract() {
        let received = vocabulary(
            r#"{"type":"catalog",
                "sets":[{"id":"set_speed","label":"Speed Set"}],
                "substats":[{"id":"att_rate","label":"Attack(%)"}],
                "slots":[{"id":"helm","label":"Helmet"}]}"#,
        );

        assert_eq!(received.sets[0].id, "set_speed");
        assert_eq!(received.sets[0].label, "Speed Set");
        // The "(%)" is the game's own wording, and it is only wording: what
        // says this substat arrives as a fraction is the `percent` flag, pinned
        // by `catalog_tokens_and_percent_are_the_wire_contract` below.
        assert_eq!(received.substats[0].label, "Attack(%)");
        assert_eq!(received.slots[0].id, "helm");
        assert_eq!(received.slots[0].label, "Helmet");
    }

    #[test]
    fn a_catalog_with_no_fields_is_empty_rather_than_an_error() {
        // What a server with no Catalog would send, if it sent anything at all.
        assert_eq!(
            vocabulary(r#"{"type":"catalog"}"#),
            FilterVocabulary::default()
        );
    }

    #[test]
    fn one_malformed_entry_does_not_cost_the_message() {
        let received = vocabulary(
            r#"{"type":"catalog","sets":[{"id":"set_speed","label":"Speed Set"},{"id":7}]}"#,
        );
        assert_eq!(received.sets.len(), 1);
        assert_eq!(received.sets[0].id, "set_speed");
    }

    /// The token card's three fields and the substat `percent` flag are wire
    /// contract, pinned here because nothing else decodes them.
    #[test]
    fn catalog_tokens_and_percent_are_the_wire_contract() {
        let received = vocabulary(
            r#"{"type":"catalog",
                "tokens":[{"id":"ticketrare_name","label":"Covenant Bookmark","price":184000}],
                "substats":[{"id":"att_rate","label":"Attack(%)","percent":true},
                            {"id":"speed","label":"Speed","percent":false}]}"#,
        );
        assert_eq!(received.tokens[0].id, "ticketrare_name");
        assert_eq!(received.tokens[0].label, "Covenant Bookmark");
        assert_eq!(received.tokens[0].price, Some(Gold::new(184_000)));
        assert!(received.substats[0].percent);
        assert!(!received.substats[1].percent);
    }

    /// An older server sends no `percent`, and a missing flag must read as
    /// "whole number" rather than failing the message.
    #[test]
    fn a_substat_with_no_percent_flag_reads_as_whole() {
        let received =
            vocabulary(r#"{"type":"catalog","substats":[{"id":"speed","label":"Speed"}]}"#);
        assert!(!received.substats[0].percent);
    }

    /// The token list gets its own tolerance, so the sets pin above cannot
    /// stand in for it: a price that overflows [`Gold`] is a per-entry failure,
    /// and the readable cards must outlive it.
    #[test]
    fn one_malformed_token_costs_its_card_and_not_the_message() {
        let received = vocabulary(
            r#"{"type":"catalog",
                "sets":[{"id":"set_speed","label":"Speed Set"}],
                "tokens":[{"id":"ticketrare_name","label":"Covenant Bookmark","price":184000},
                          {"id":"ticketspecial_name"},
                          {"id":"friendpoint_name","label":"Friendship Bookmark",
                           "price":-1}]}"#,
        );
        assert_eq!(received.tokens.len(), 1);
        assert_eq!(received.tokens[0].id, "ticketrare_name");
        assert_eq!(received.sets.len(), 1, "the rest of the message survives");
    }

    /// The icon table's field name is wire contract, and this is the only place
    /// it is checked: the ingest is already sending it, so a spelling that
    /// disagrees leaves every set chip permanently pictureless with nothing to
    /// show for it. The value is passed through verbatim — decoding is
    /// `ui::icons`' job, and this end refuses to have an opinion on it.
    #[test]
    fn catalog_icons_are_the_wire_contract() {
        let received = vocabulary(
            r#"{"type":"catalog",
                "sets":[{"id":"set_speed","label":"Speed Set"}],
                "icons":{"set_speed":"iVBORw=="}}"#,
        );
        assert_eq!(
            received.icons.get("set_speed").map(String::as_str),
            Some("iVBORw==")
        );
    }

    /// An older server sends no icons at all, and the picker must still open.
    #[test]
    fn a_catalog_with_no_icons_still_carries_its_sets() {
        let received =
            vocabulary(r#"{"type":"catalog","sets":[{"id":"set_speed","label":"Speed Set"}]}"#);
        assert!(received.icons.is_empty());
        assert_eq!(received.sets.len(), 1);
    }

    /// The icon table gets the same tolerance as the three lists: a value that
    /// is not a string costs its picture, and a table that is not an object
    /// costs the pictures — never the message.
    #[test]
    fn one_malformed_icon_costs_its_picture_and_not_the_message() {
        let received = vocabulary(
            r#"{"type":"catalog",
                "sets":[{"id":"set_speed","label":"Speed Set"}],
                "icons":{"set_speed":"iVBORw==","set_torn":7,"set_null":null}}"#,
        );
        assert_eq!(received.icons.len(), 1);
        assert!(received.icons.contains_key("set_speed"));
        assert_eq!(received.sets.len(), 1, "the rest of the message survives");

        let mistyped = vocabulary(
            r#"{"type":"catalog","icons":"corrupt","slots":[{"id":"helm","label":"Helmet"}]}"#,
        );
        assert!(mistyped.icons.is_empty());
        assert_eq!(mistyped.slots.len(), 1);
    }

    #[test]
    fn a_mistyped_token_list_degrades_to_empty() {
        let received = vocabulary(r#"{"type":"catalog","tokens":"corrupt"}"#);
        assert!(received.tokens.is_empty());
    }

    #[test]
    fn a_mistyped_list_degrades_to_empty() {
        let received = vocabulary(r#"{"type":"catalog","slots":"corrupt"}"#);
        assert!(received.slots.is_empty());
    }
}
