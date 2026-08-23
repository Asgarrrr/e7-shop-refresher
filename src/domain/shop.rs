//! The shop model: the trimmed contents the analysis server returns, ready to
//! display and filter. Only this shape crosses the link; how the server
//! produces it is not the client's concern.

use std::num::NonZeroU32;

use serde::{Deserialize, Deserializer, Serialize};

/// One shop roll as the analysis server trimmed it: the merchant, the slots
/// on offer, and the refresh-session facts. Every field is optional on the
/// wire — a degraded message still reaches the view.
#[derive(Debug, Clone, Deserialize)]
pub struct ShopSnapshot {
    #[serde(default, deserialize_with = "sanitized_text")]
    pub merchant: Option<String>,
    /// The slots on offer. Tolerant per element — see [`lenient_slots`] for
    /// why a bad slot is dropped rather than failing the snapshot.
    #[serde(default, deserialize_with = "lenient_slots")]
    pub slots: Vec<ShopItem>,
    /// Refresh-session facts (balance, cost), grouped apart since they are
    /// not shop *contents*. Absent, the cost falls back to the game constant
    /// and only out-of-funds detection is lost.
    #[serde(default, deserialize_with = "object_or_none")]
    pub refresh: Option<RefreshMeta>,
}

/// A global catalog id — the identity the purchase echo uses to name the item
/// the player asked for.
///
/// `NonZeroU32`, because the server spells "no id" as `0`: absence is `None`
/// and nothing else, interpreted only in [`optional_catalog_id`]. A distinct
/// type so an id and an amount cannot be swapped.
///
/// `#[serde(transparent)]`, so the wire shape is a bare number, unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct CatalogId(NonZeroU32);

impl CatalogId {
    /// The id `raw` names, or `None` for the `0` the server sends when it has
    /// none.
    #[must_use]
    pub const fn new(raw: u32) -> Option<Self> {
        match NonZeroU32::new(raw) {
            Some(id) => Some(Self(id)),
            None => None,
        }
    }

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

/// Gold — what a shop item costs, and what a purchase echo reports left in the
/// purse. One of two money ledgers; [`Crystals`] is the other, and they are
/// distinct types so a gold price can never be weighed against a crystal
/// budget.
///
/// **No `Add`/`Sub`/`AddAssign`/`SubAssign`, deliberately.** `Cargo.toml` sets
/// `overflow-checks = true` on the release profile, so `balance - price` would
/// panic on underflow in the shipped binary. Every arithmetic site names its
/// overflow policy instead — [`Gold::saturating_sub`] here,
/// [`Crystals::saturating_add`] / [`Crystals::checked_add`] on the other.
///
/// **`Option<Gold>`, not a `0` sentinel.** Unlike [`CatalogId`], zero is a
/// legitimate value: an empty purse is a *known* balance that must veto a
/// priced buy, while an unknown balance fails open and vetoes nothing. So
/// there is deliberately no `deserialize_with` here — `"gold": 0` decodes to
/// `Some(Gold(0))`.
///
/// `#[serde(transparent)]` in both directions — `Serialize` too, since
/// `Filter::max_price` is a `Gold` that `config::persist` writes back to
/// `config.toml` as a bare `max_price = 300000`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Gold(u32);

impl Gold {
    /// The amount `raw` names. Total, unlike [`CatalogId::new`]: every `u32` is
    /// a gold amount, so there is no sentinel to interpret.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The number, for a wire field or a non-currency comparison. Callerless
    /// on purpose. **Do not use it for arithmetic** — that re-opens the hole
    /// this type closed.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// `self - rhs`, floored at zero. Its one caller
    /// (`Controller::plan_targets`) holds `rhs <= self` only through a separate
    /// affordability test; at zero the next item reads unaffordable.
    #[must_use]
    pub const fn saturating_sub(self, rhs: Self) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }
}

/// Crystals — "skystones" in game. What a shop refresh costs and the player's
/// refresh budget is denominated in; the other half of the pair [`Gold`]
/// documents. This ledger accumulates, so it carries an add where [`Gold`]
/// carries only a subtract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Crystals(u32);

impl Crystals {
    /// The amount `raw` names. Total, like [`Gold::new`], and for the same
    /// reason: zero crystals is a real balance.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The number. Callerless and deliberately so — see [`Gold::get`].
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// `self + rhs`, capped at `u32::MAX`: pinning keeps `stop_reason` halting
    /// rather than panicking on the overflow-checked release profile.
    #[must_use]
    pub const fn saturating_add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }

    /// `self + rhs`, or `None` on overflow. Not interchangeable with
    /// [`Crystals::saturating_add`]: `stop_reason` asks whether the *next*
    /// refresh crosses the budget, and a saturated `u32::MAX` would answer
    /// "over budget" for the wrong reason.
    #[must_use]
    pub const fn checked_add(self, rhs: Self) -> Option<Self> {
        match self.0.checked_add(rhs.0) {
            Some(sum) => Some(Self(sum)),
            None => None,
        }
    }

    /// `self - rhs`, floored at zero. Debits the locally-tracked balance per
    /// advised refresh; at zero `stop_reason` fires out-of-funds on the next
    /// gate.
    #[must_use]
    pub const fn saturating_sub(self, rhs: Self) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }

    /// How far `self` has gone towards `cap`, clamped to `0.0..=1.0` — what the
    /// window's gauge fills to when the crystal budget is the binding limit.
    ///
    /// A method rather than two [`Crystals::get`] calls at the call site: that
    /// getter documents itself as not-for-arithmetic, and a ratio is
    /// arithmetic. Keeping the division in here is what stops a view from
    /// dividing crystals by gold. A zero cap reads as already reached, since a
    /// budget of nothing is spent the moment it exists.
    #[must_use]
    pub fn ratio_of(self, cap: Self) -> f32 {
        if cap.0 == 0 {
            return 1.0;
        }
        (self.0 as f32 / cap.0 as f32).clamp(0.0, 1.0)
    }
}

/// Thousands-grouped, and only that way: one rendering per currency, so no
/// call site can decide grouping on its own (one of the four once did not
/// group at all). The `domain` → `render` call is the one edge in that
/// direction and is deliberate; `grouped` is a pure `u32` → `String`.
impl std::fmt::Display for Gold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::render::grouped(self.0))
    }
}

/// Grouped like [`Gold`], for the reason that impl gives.
impl std::fmt::Display for Crystals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::render::grouped(self.0))
    }
}

/// Reads a wire catalog id: absent, `null` and `0` all mean "no id". The one
/// place a raw id number is interpreted — see [`CatalogId`]. A `0` is folded
/// rather than refused: the message is still good, it just cannot be tied back
/// to a slot.
pub(crate) fn optional_catalog_id<'de, D>(de: D) -> Result<Option<CatalogId>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<u32>::deserialize(de)?.and_then(CatalogId::new))
}

impl ShopSnapshot {
    /// The slot bearing this catalog id, if any. Ids are unique within a
    /// snapshot, so at most one matches; an item whose id the server omitted
    /// cannot be matched by accident, since it has no id to compare.
    pub fn slot_by_id(&self, id: CatalogId) -> Option<&ShopItem> {
        self.slots.iter().find(|item| item.id == Some(id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct RefreshMeta {
    /// Crystal balance after the debit.
    pub crystal_balance: Crystals,
    /// Cost of one manual refresh (3 in the lobby).
    pub cost: Crystals,
}

/// Tolerant optional side-channel object (`refresh`, `limit`): a partial,
/// `null`, or mistyped value degrades to `None` rather than failing the whole
/// snapshot. The value is consumed wholesale first — a bare `?` on the typed
/// parse would abort the surrounding message mid-stream.
///
/// Logged because both fields change behaviour, not just display: a dropped
/// `limit` makes a sold-out slot read buyable (ending in a `Unresponsive` halt
/// that blames the game), a dropped `refresh` disables out-of-funds detection.
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
/// Logged, because a silently shortened substat list fails `min_substats` and
/// every `required_substats` threshold, refreshing past a wanted item.
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

/// Tolerant slot list: an undecodable slot is dropped, a non-array value
/// degrades to empty — the snapshot survives either way.
///
/// A missed match beats the alternative: a bare `Vec<ShopItem>` propagates the
/// element's error up through the internally-tagged `ServerMessage`, where
/// `#[serde(other)] Unknown` cannot catch it (that arm rescues an unknown
/// *tag*, not an undecodable payload), so the whole message is dropped and the
/// armed `Expectation` climbs to `StopReason::Unresponsive`.
///
/// **Hazard, judged remote but real:** [`ShopItem::effective_slot`] falls back
/// to the 1-based position, so a shop that *both* omits `slot` numbers and
/// ships an undecodable slot renumbers past the hole and the actuator clicks
/// one row off — a wrong buy with real gold. Keeping the arity with a default
/// [`ShopItem`] is worse: `dedup::fingerprint` yields `None` when any slot has
/// no id, so one bad scalar would disable duplicate suppression for that roll
/// and every re-delivery would bill another refresh.
///
/// `warn!` once per snapshot naming the arity (the per-slot serde errors stay
/// at `debug!`): the only on-screen trace is a gap in the slot column.
///
/// A non-array `slots` degrades to empty, which does not save the session —
/// `Controller::evaluate_snapshot` returns on a slotless snapshot before
/// disarming the expectation, so the watchdog still climbs to `Unresponsive`.
/// The gain is diagnosis: `last_shop_ms` advances, so the heartbeat can say
/// "server talking, payload unusable" rather than "server mute".
fn lenient_slots<'de, D>(de: D) -> Result<Vec<ShopItem>, D::Error>
where
    D: Deserializer<'de>,
{
    let serde_json::Value::Array(values) = serde_json::Value::deserialize(de)? else {
        tracing::warn!("tolerated a non-array shop slot list — degraded to empty");
        return Ok(Vec::new());
    };
    let offered = values.len();
    let slots: Vec<ShopItem> = values
        .into_iter()
        .filter_map(|value| match serde_json::from_value(value) {
            Ok(item) => Some(item),
            Err(error) => {
                tracing::debug!(%error, "dropped an undecodable shop slot");
                None
            }
        })
        .collect();
    if slots.len() != offered {
        tracing::warn!(
            kept = slots.len(),
            offered,
            "dropped undecodable shop slots — the filter will judge a short shop"
        );
    }
    Ok(slots)
}

/// Cap on any single server-supplied display string.
///
/// Generous against a real merchant or set name and far below anything that
/// could pad a log file: the rotation keeps five files, so an uncapped name is
/// a lever the server can pull to evict a player's real diagnostic history.
const MAX_WIRE_TEXT: usize = 120;

/// Strips control characters (replacing each with a single space), caps the
/// result at [`MAX_WIRE_TEXT`] **characters**, then trims. Shared by
/// [`sanitized_text`] and [`sanitized_required_text`]; returns whether
/// anything changed, so the callers can decide whether to log.
///
/// Replacing rather than deleting a control character keeps `"a\nb"` from
/// becoming `"ab"` — two words the server kept apart should not collide just
/// because the separator was hostile.
///
/// Trimmed because `ui::editor::hunt` trims the player's side of the same
/// comparison (`input.trim()` before a criterion is stored) and
/// `Filter::matches` compares both sides by exact equality — an untrimmed
/// wire value would silently stop matching a criterion the player typed
/// correctly. Trimming happens *after* the cap, not before: truncating at
/// exactly [`MAX_WIRE_TEXT`] characters can itself land on a space (an
/// overlong value cut mid-word), so trimming first could leave that
/// truncation-made space behind untouched.
fn sanitize_wire_text(raw: String) -> (String, bool) {
    let mut changed = false;
    let stripped: String = raw
        .chars()
        .map(|ch| {
            if ch.is_control() {
                changed = true;
                ' '
            } else {
                ch
            }
        })
        .collect();
    let capped: String = if stripped.chars().count() > MAX_WIRE_TEXT {
        changed = true;
        stripped.chars().take(MAX_WIRE_TEXT).collect()
    } else {
        stripped
    };
    let trimmed = capped.trim();
    if trimmed.len() != capped.len() {
        changed = true;
    }
    (trimmed.to_owned(), changed)
}

/// Server-supplied display text, with control characters removed and the
/// length capped. Attaches to `merchant`, `ShopItem::name` and `ShopItem::set`
/// — every optional display string the wire sends.
///
/// These strings reach the rotated log file verbatim as a `tracing` field
/// (`journal::EventLog::emit_at`), and a `\n` in a field breaks the
/// one-event-per-line property the whole file is read on — the same property
/// `lib::fatal` declines to log a multi-line message for. An unbounded name is
/// the other half: five rotated files can be filled on demand.
///
/// Replaces rather than rejects, in keeping with every other tolerant path
/// here: a snapshot with a hostile name still shows the player their shop.
fn sanitized_text<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(raw) = Option::<String>::deserialize(de)? else {
        return Ok(None);
    };
    Ok(Some(sanitize_and_report(raw, "display text")))
}

/// [`sanitized_text`] for a field that is not optional on the Rust side
/// (`Substat::name`) — the wire still owes a string, just not a clean one.
fn sanitized_required_text<'de, D>(de: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(sanitize_and_report(
        String::deserialize(de)?,
        "substat name",
    ))
}

/// [`sanitize_wire_text`] plus the `debug!` that reports a value it had to
/// change — the whole body both `deserialize_with` hooks above used to carry.
///
/// `field` is what tells the two apart in the log. Both hooks passed
/// `"display text"` while the body was written twice, so a mangled
/// `Substat::name` reported itself as a display string; serde hands a
/// `deserialize_with` no field name, so naming it per hook is as close as this
/// can get.
fn sanitize_and_report(raw: String, field: &'static str) -> String {
    let (cleaned, changed) = sanitize_wire_text(raw);
    if changed {
        tracing::debug!(
            field,
            "tolerated a control character or an overlong value in a server-supplied string — sanitized"
        );
    }
    cleaned
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ShopItem {
    /// Ties a purchase confirmation back to the slot the player wanted.
    /// `None` when the server omits it — see [`CatalogId`].
    #[serde(default, deserialize_with = "optional_catalog_id")]
    pub id: Option<CatalogId>,
    /// Shop slot (1..=6); `0` if the server omits it.
    #[serde(default)]
    pub slot: u8,
    /// Defaults to `Unknown` rather than failing the whole message.
    #[serde(default)]
    pub kind: ItemKind,
    #[serde(default, deserialize_with = "sanitized_text")]
    pub name: Option<String>,
    /// Price in gold. `None` fails *open* everywhere it is read — an unknown
    /// price can never be proven unaffordable. A wire `0` is a real price of
    /// zero, not an omission ([`Gold`]).
    #[serde(default)]
    pub price: Option<Gold>,
    /// Gear grade (2, 3, or 4).
    #[serde(default)]
    pub grade: Option<u8>,
    /// Gear set, by internal id (`set_speed`, `set_immune`, ...).
    #[serde(default, deserialize_with = "sanitized_text")]
    pub set: Option<String>,
    /// Substats and their values, keyed by internal stat name. A nameless or
    /// mistyped entry is dropped: it could never match a name-keyed criterion.
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
    /// cannot wrap back into the `0` sentinel. Not injective on malformed
    /// shops — a fallback number can collide with another item's wire slot, so
    /// callers matching by it may over-select.
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
    #[serde(deserialize_with = "sanitized_required_text")]
    pub name: String,
    #[serde(default)]
    pub value: Option<f64>,
}

/// Purchase limit, e.g. "0/1" (sold out) or "1/1" (available). Both fields
/// stay `u32` deliberately: counts of purchases belong to neither money
/// ledger, and the only question asked of them is `remaining == 0`.
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

    /// Fixture amounts, so a test reads `xtl(95)`, not `Crystals::new(95)`.
    const fn xtl(raw: u32) -> Crystals {
        Crystals::new(raw)
    }

    const fn gold(raw: u32) -> Gold {
        Gold::new(raw)
    }

    #[test]
    fn refresh_full_object_parses() {
        let snapshot = parse(r#"{"refresh":{"crystal_balance":95,"cost":3}}"#);
        assert_eq!(
            snapshot.refresh,
            Some(RefreshMeta {
                crystal_balance: xtl(95),
                cost: xtl(3),
            })
        );
    }

    /// Both currencies are bare numbers on the wire and — unlike
    /// [`CatalogId`] — a `0` stays a `0`. See [`Gold`].
    #[test]
    fn a_zero_amount_is_a_real_amount_and_only_absence_is_unknown() {
        let priced = parse(r#"{"slots":[{"price":0}],"refresh":{"crystal_balance":0,"cost":3}}"#);
        assert_eq!(priced.slots[0].price, Some(gold(0)));
        assert_eq!(
            priced.refresh.map(|meta| meta.crystal_balance),
            Some(xtl(0))
        );

        for json in [r#"{"slots":[{}]}"#, r#"{"slots":[{"price":null}]}"#] {
            assert_eq!(parse(json).slots[0].price, None, "{json}");
        }

        // Transparent on the way in: no wrapper object, no truncation.
        assert_eq!(
            parse(r#"{"slots":[{"price":4294967295}]}"#).slots[0].price,
            Some(gold(u32::MAX))
        );
    }

    /// The floors and ceilings each ledger pins. Catches a saturating method
    /// turned wrapping; does not catch an added `Add`/`Sub` (see [`Gold`]).
    #[test]
    fn currency_arithmetic_saturates_at_both_ends() {
        assert_eq!(gold(200_000).saturating_sub(gold(184_000)), gold(16_000));
        assert_eq!(gold(10).saturating_sub(gold(184_000)), gold(0));

        assert_eq!(xtl(3).saturating_add(xtl(3)), xtl(6));
        assert_eq!(xtl(u32::MAX).saturating_add(xtl(1)), xtl(u32::MAX));
        assert_eq!(xtl(6).saturating_sub(xtl(9)), xtl(0));

        assert_eq!(xtl(3).checked_add(xtl(3)), Some(xtl(6)));
        assert_eq!(xtl(u32::MAX).checked_add(xtl(1)), None);
    }

    #[test]
    fn both_currencies_display_thousands_grouped() {
        assert_eq!(gold(1_234_567).to_string(), "1,234,567");
        assert_eq!(xtl(1_000).to_string(), "1,000");
        assert_eq!(gold(0).to_string(), "0");
    }

    #[test]
    fn refresh_partial_object_degrades_to_none() {
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
        assert_eq!(parse(r#"{"refresh":5,"slots":[{}]}"#).refresh, None);
        assert_eq!(parse(r#"{"refresh":"n/a"}"#).refresh, None);
        assert_eq!(parse(r#"{"refresh":[]}"#).refresh, None);
    }

    #[test]
    fn partial_limit_degrades_to_buyable() {
        // Fail-open: a dropped limit must never read as sold out.
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
        // The haul-recording lookup: `0` must never resolve to a slot.
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

    /// Both payloads are shapes the server can emit — a slot past `u8::MAX`, a
    /// stringified price — and each would abort the whole `ServerMessage`,
    /// hence the run. See [`lenient_slots`].
    #[test]
    fn a_bad_scalar_drops_its_slot_and_not_the_snapshot() {
        for json in [
            r#"{"slots":[{"id":101,"slot":1},{"id":102,"slot":300},{"id":103,"slot":3}]}"#,
            r#"{"slots":[{"id":101,"slot":1},{"id":102,"price":"184k"},{"id":103,"slot":3}]}"#,
        ] {
            let snapshot = parse(json);
            let surviving: Vec<Option<CatalogId>> =
                snapshot.slots.iter().map(|item| item.id).collect();
            assert_eq!(
                surviving,
                [CatalogId::new(101), CatalogId::new(103)],
                "{json}"
            );
        }
    }

    /// Does *not* save the session (`evaluate_snapshot` refuses a slotless
    /// snapshot); it pins that the snapshot still arrives, which is what lets
    /// the heartbeat tell a talking server from a mute one.
    #[test]
    fn mistyped_slots_degrade_to_empty() {
        for json in [
            r#"{"slots":"corrupt"}"#,
            r#"{"slots":5}"#,
            r#"{"slots":{"1":{"id":101}}}"#,
        ] {
            assert!(parse(json).slots.is_empty(), "{json}");
        }
        // Absent and null keep working through the field's `default`.
        assert!(parse("{}").slots.is_empty());
    }

    #[test]
    fn mistyped_substats_degrade_to_empty() {
        let snapshot = parse(r#"{"slots":[{"id":9,"substats":"corrupt"}]}"#);
        assert!(snapshot.slots[0].substats.is_empty());
        assert_eq!(snapshot.slots[0].id, CatalogId::new(9));
    }

    /// The end-to-end property the sanitizer exists for: a server-supplied
    /// newline must not be able to open a second line in the journal file.
    /// `render::format_item` is the exemplar consumer — the GUI table, the
    /// tooltip and the console line all go through the same field.
    #[test]
    fn a_newline_in_a_server_name_cannot_reach_a_journal_line() {
        let snapshot = parse(r#"{"slots":[{"name":"before\nafter"}]}"#);
        let line = crate::render::format_item(&snapshot.slots[0], 0);
        assert!(!line.contains('\n'), "{line:?}");
        assert!(line.contains("before after"), "{line:?}");
    }

    #[test]
    fn an_overlong_server_name_is_capped() {
        let long_name = "x".repeat(500);
        let snapshot = parse(&serde_json::json!({"slots": [{"name": long_name}]}).to_string());
        let name = snapshot.slots[0].name.as_deref().expect("name present");
        assert_eq!(name.chars().count(), MAX_WIRE_TEXT);
    }

    #[test]
    fn a_carriage_return_and_a_tab_are_replaced_not_deleted() {
        let snapshot = parse(r#"{"slots":[{"name":"a\r\nb"}]}"#);
        let name = snapshot.slots[0].name.as_deref().expect("name present");
        assert!(!name.chars().any(char::is_control), "{name:?}");
        let a_index = name.find('a').expect("a present");
        let b_index = name.find('b').expect("b present");
        assert!(
            b_index > a_index + 1,
            "a and b should stay separated: {name:?}"
        );
    }

    #[test]
    fn an_ordinary_name_is_returned_unchanged() {
        let snapshot = parse(r#"{"slots":[{"name":"Ancient Coin","set":"set_speed"}]}"#);
        assert_eq!(snapshot.slots[0].name.as_deref(), Some("Ancient Coin"));
        assert_eq!(snapshot.slots[0].set.as_deref(), Some("set_speed"));
    }

    /// `ui::editor::hunt` trims a criterion before storing it (`input.trim()`),
    /// but replacing a control character with a space, as the sanitizer does,
    /// can itself leave a trailing space the wire value never had — a "\n" at
    /// the end of a name becomes a `" "` at the end of a name. Left untrimmed,
    /// that space is the only difference between the sanitized wire value and
    /// the player's trimmed criterion, and `Filter::matches` is exact equality:
    /// the item silently stops matching.
    #[test]
    fn a_trailing_control_character_does_not_leave_a_space_behind() {
        let snapshot = parse(r#"{"slots":[{"name":"Covenant Bookmark\n"}]}"#);
        assert_eq!(snapshot.slots[0].name.as_deref(), Some("Covenant Bookmark"));
    }

    /// The gauge's fill: bounded at both ends, because a budget tightened
    /// below what a run already spent otherwise paints past the panel.
    #[test]
    fn a_spend_ratio_stays_inside_the_gauge() {
        assert_eq!(
            Crystals::new(75).ratio_of(Crystals::new(150)),
            0.5,
            "half a budget"
        );
        assert_eq!(Crystals::new(0).ratio_of(Crystals::new(150)), 0.0);
        assert_eq!(
            Crystals::new(400).ratio_of(Crystals::new(150)),
            1.0,
            "a budget lowered under what is already spent"
        );
    }

    /// A budget of nothing is spent the moment it exists — the alternative is
    /// a division by zero handed to a painter.
    #[test]
    fn a_zero_budget_reads_as_already_reached() {
        assert_eq!(Crystals::new(0).ratio_of(Crystals::new(0)), 1.0);
    }
}
