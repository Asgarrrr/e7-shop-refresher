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
    #[serde(default)]
    pub merchant: Option<String>,
    /// The slots on offer. Tolerant per element — see [`lenient_slots`], which
    /// carries the argument for dropping a bad slot rather than failing the
    /// snapshot the way this field used to.
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
/// Two things at once, both of which used to be `u32`:
///
/// 1. **The `0` sentinel is gone.** `NonZeroU32` inside, so "the server omitted
///    the id" is `None` and nothing else. It used to be `id: u32` with `0` for
///    absent, interpreted by a free `shop::catalog_id(id)` documented as the
///    only place to do so — a contract broken while it was written:
///    `Controller::on_purchase` re-derived it as `if item != 0 && …`. A
///    sentinel's cost is that every reader has to remember it; the fix is
///    making it unspellable. Both old interpreters are gone with it; the
///    conversion now happens once, in [`optional_catalog_id`], where a raw
///    wire number arrives.
/// 2. **It is not a counter.** The id space used to be assignable from any
///    `u32` in the crate: a gold balance, a price, a crystal cost, a refresh
///    count. `checklist`, `bought`, `BuyTarget::id` and `PurchaseNotice::item`
///    all speak this type now, so an id and an amount cannot be swapped.
///
/// `#[serde(transparent)]`, so the wire shape is a bare number, unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct CatalogId(NonZeroU32);

impl CatalogId {
    /// The id `raw` names, or `None` for the `0` the server sends when it has
    /// none. The single interpreter of that number — see the type.
    #[must_use]
    pub const fn new(raw: u32) -> Option<Self> {
        match NonZeroU32::new(raw) {
            Some(id) => Some(Self(id)),
            None => None,
        }
    }

    /// The number, for a wire field or a log line.
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
/// purse. One of the two money ledgers this crate runs; [`Crystals`] is the
/// other, and keeping them apart is the whole reason both exist.
///
/// Both were bare `u32` until this pass: `Controller` debits a *gold* price
/// from a *gold* balance in `plan_targets` while comparing a *crystal* spend
/// against a *crystal* budget in `stop_reason`, and nothing but field names
/// separated them. `progress.spent >= item.price` — a crystal budget weighed
/// against a gold price — compiled, passed every lint, and would have
/// silently vetoed or authorised every buy of the run. That line is
/// `expected Crystals, found Gold` now; the rest of this type is what it
/// took to make that true without loosening anything else.
///
/// # No arithmetic operators, deliberately
///
/// `Add`, `Sub`, `AddAssign` and `SubAssign` are not implemented; their
/// absence is the design, not an oversight. `Cargo.toml` sets
/// `overflow-checks = true` on the release profile (a wrapped counter used
/// to be a silent wrong number in a shipped build), so `balance - price`
/// panics on underflow in the shipped binary, and both operands come off
/// the wire where nothing guarantees the order. `plan_targets` holds
/// `price <= balance` only through a separate `affordable` term computed
/// three lines earlier — a non-local invariant `in_reach` could break
/// without touching the subtraction. So the operator is not spellable, and
/// every arithmetic site names its overflow policy —
/// [`Gold::saturating_sub`] here, [`Crystals::saturating_add`] /
/// [`Crystals::checked_add`] on the other ledger.
///
/// # `Option<Gold>`, not a `0` sentinel
///
/// [`CatalogId`] folds absent / `null` / `0` to `None` because `0` is the
/// server's own spelling of "no id" — the value space has no legitimate zero.
/// Money does. An empty purse is a *known* balance of zero and must veto a
/// priced buy; an unknown balance must fail open and veto nothing. Folding `0`
/// to `None` here would turn "the player is broke" into "we have no idea", i.e.
/// authorise exactly the buys that cannot happen. So `Gold` wraps a plain `u32`,
/// absence rides in the `Option` around it, and there is deliberately **no**
/// `deserialize_with` on the wire fields: `"gold": 0` decodes to
/// `Some(Gold(0))`. That is the same sentinel policy as `CatalogId`, not a
/// second one — fold the sentinel where the value space has no zero, keep the
/// zero where it means something.
///
/// `#[serde(transparent)]`, so the wire shape is a bare number, unchanged —
/// and `Serialize` too, since `Filter::max_price` is a `Gold` that
/// `config::persist` writes back to `config.toml` as a bare
/// `max_price = 300000`, which `config/persist.rs`'s strip test pins by
/// string.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Gold(u32);

impl Gold {
    /// The amount `raw` names.
    ///
    /// Total, unlike [`CatalogId::new`]: every `u32` is a gold amount, so there
    /// is no `Option` to unwrap and no sentinel to interpret.
    ///
    /// `Filter::max_price` and `Limits::max_spend` — the player's two raw
    /// ceilings — are `Option<Gold>` and `Option<Crystals>` themselves now, so
    /// both comparisons are in-ledger from the parse onward. What remains for
    /// this constructor is fixtures and the `deserialize_with`-free wire path.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The number, for a wire field or a comparison against something that is
    /// not yet a currency.
    ///
    /// Has no caller in the crate today — the intended steady state, not an
    /// oversight: every comparison is in-ledger, every print goes through
    /// [`Display`](std::fmt::Display), and the two raw ceilings that do cross
    /// come in through [`Gold::new`] instead of dragging an amount out. It
    /// exists so a caller who genuinely needs the number has a documented way
    /// to ask, rather than widening the tuple field to `pub`. **Do not use it
    /// for arithmetic** — that re-opens the hole this type closed; the ledger
    /// has named methods for that.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// `self - rhs`, floored at zero.
    ///
    /// The only subtraction on this ledger, saturating because its one caller
    /// (`Controller::plan_targets`, debiting each planned buy from the running
    /// balance) holds `rhs <= self` only through a separate affordability
    /// test. At zero the next item simply reads unaffordable — see the type's
    /// note on why `Sub` is absent.
    #[must_use]
    pub const fn saturating_sub(self, rhs: Self) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }
}

/// Crystals — "skystones" in game. What a shop refresh costs and the
/// player's refresh budget is denominated in. The other half of the pair
/// [`Gold`] documents.
///
/// This ledger *accumulates*: `Progress::spent` adds a refresh cost per
/// issued refresh and `RefreshMeta::crystal_balance` is debited by the same
/// amount, so it carries an add where [`Gold`] carries only a subtract. Both
/// directions are explicit about overflow for the reason [`Gold`] gives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Crystals(u32);

impl Crystals {
    /// The amount `raw` names. Total, like [`Gold::new`], and for the same
    /// reason: zero crystals is a real balance.
    ///
    /// `Controller::stop_reason` used to lift `[limits] max_spend` here so the
    /// budget comparison `type-004` was filed against had two crystal operands.
    /// The field is a `Crystals` now, so that call is gone — see [`Gold::new`].
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The number. Callerless and deliberately so — see [`Gold::get`], which
    /// carries the argument for both.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// `self + rhs`, capped at `u32::MAX`.
    ///
    /// Spend accumulation: a run long enough to overflow a `u32` of crystals
    /// has already tripped every budget the player could set, so pinning at the
    /// ceiling keeps `stop_reason` halting rather than panicking on the
    /// overflow-checked release profile.
    #[must_use]
    pub const fn saturating_add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }

    /// `self + rhs`, or `None` on overflow.
    ///
    /// Not interchangeable with [`Crystals::saturating_add`]: `stop_reason`
    /// asks whether the *next* refresh would cross the budget, and a
    /// saturating answer there reads as "`u32::MAX`, over budget" —
    /// accidentally right, for the wrong reason. `None` says "this cannot be
    /// computed, so the ceiling is certainly crossed", which is what the
    /// caller means.
    #[must_use]
    pub const fn checked_add(self, rhs: Self) -> Option<Self> {
        match self.0.checked_add(rhs.0) {
            Some(sum) => Some(Self(sum)),
            None => None,
        }
    }

    /// `self - rhs`, floored at zero. Debits the locally-tracked balance per
    /// advised refresh; a balance already at zero stays there and
    /// `stop_reason`'s out-of-funds test fires on the next gate.
    #[must_use]
    pub const fn saturating_sub(self, rhs: Self) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }
}

/// A gold amount renders as a thousands-grouped decimal, and only that way.
///
/// `Display` rather than leaving every site to call `render::grouped` itself:
/// the four places that print an amount — the status-bar balance tiles, the
/// slot table's price column, the console/tooltip item line, and the
/// journal's "bought" line — each decided grouping on their own, and the last
/// one did not group at all: `{gold} gold left` printed `250000` in the
/// journal while the table beside it showed `250,000`. Now there is one
/// rendering per currency, living on the currency.
///
/// The call goes `domain` → `render`, the one edge in that direction. Two
/// alternatives: writing this impl inside `render.rs` (legal, but then a
/// reader of `shop.rs` cannot see the type has a `Display` at all), or moving
/// `grouped` down here (it also formats refresh counts, which are not money,
/// so it would be a number formatter homed in the shop model). `grouped` is a
/// pure `u32` → `String` with no I/O and no state.
impl std::fmt::Display for Gold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::render::grouped(self.0))
    }
}

/// Grouped like [`Gold`] — see that impl for why the rendering lives on the
/// currency and not at the four call sites.
impl std::fmt::Display for Crystals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&crate::render::grouped(self.0))
    }
}

/// Reads a wire catalog id: absent, `null` and `0` all mean "no id".
///
/// The one place a raw id number is interpreted — see [`CatalogId`]. `0` is
/// the server's own spelling of absent, so it is accepted and folded here
/// rather than refused: the message is still good, it just cannot be tied
/// back to a slot.
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
/// Logged, because both fields change app behavior, not just what it shows:
/// a dropped `limit` makes a sold-out slot read buyable (the actuator clicks
/// Buy, no echo arrives, and the watchdog halts `Unresponsive` blaming the
/// game), and a dropped `refresh` silently disables out-of-funds detection.
/// `debug!` not `warn!`: the default filter keeps it in the log file.
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
///
/// Logged for the same reason as [`object_or_none`]: a silently shortened
/// substat list quietly fails `min_substats` and every `required_substats`
/// threshold, so the loop refreshes past an item the player wanted.
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
/// Its own function rather than [`lenient_elements`], which its body nearly
/// repeats, because the argument for dropping a *slot* is not the argument for
/// dropping a substat, and because the loss worth logging here is the arity,
/// which needs counting rather than listing.
///
/// # Why dropping beats the failure it replaces
///
/// Dropping a slot is a real loss, and a worse one than [`lenient_elements`]
/// takes: the filter then sees five slots and calls that the shop, so a match
/// in the sixth is missed and the loop refreshes past the item the player was
/// hunting, with nothing on screen saying a slot went missing. What makes it
/// the better branch is what the alternative actually was. A bare
/// `Vec<ShopItem>` propagates an element's error up through the
/// internally-tagged `ServerMessage`, where `#[serde(other)] Unknown` cannot
/// catch it — that arm rescues an unknown *tag*, not a payload that failed to
/// decode — so `websocket::forward` dropped the whole message at `debug!`. No
/// snapshot reached the view, `last_shop_ms` never advanced, and the
/// `Expectation` armed by the last refresh climbed the ladder to
/// `StopReason::Unresponsive`.
///
/// So the choice was never one missed match against none. It was one missed
/// match against six missed matches *and* a stopped run *and* a halt reason
/// that sends the player to look at Epic Seven — check the window, restart the
/// client — when what happened is that the analysis server put a `300` in a
/// `u8`. Failing loudly is only worth something when it points at the right
/// thing.
///
/// # What it costs, named rather than waved past
///
/// [`ShopItem::effective_slot`] falls back to the 1-based *position* when the
/// server omits `slot`, so a shop that both omits slot numbers and ships one
/// undecodable slot renumbers everything after the hole and the actuator
/// clicks one row off — a wrong buy with real gold, not a missed one. That
/// needs two independent server faults at once and `slot` is pinned by the
/// wire fixture in `uplink::protocol`, so it is judged remote; it is written
/// down because it is the only branch here that spends money rather than
/// losing an opportunity.
///
/// Substituting a default [`ShopItem`] for the bad one — keeping the arity, so
/// nothing renumbers — is the obvious answer to that and is worse:
/// `dedup::fingerprint` yields `None` when any slot has no id, and a
/// placeholder has none, so one bad scalar would switch duplicate suppression
/// off for that roll and every re-delivery of the same shop would bill another
/// refresh in crystals. A certain recurring spend to buy off a remote wrong
/// click is the wrong trade.
///
/// # What the log and the player see
///
/// One `warn!` per snapshot naming the arity, not one per dropped slot: what a
/// reader of the log file the README asks players to send needs is that the
/// shop was six slots and five arrived, and a server stuck on a bad scalar
/// re-sends one on every roll. `warn!` rather than the `debug!` both siblings
/// use because this is `websocket::forward`'s dialect line's class of event —
/// client and server disagree about the wire and the symptom is silence —
/// while the per-slot serde errors stay at `debug!`, which the default filter
/// still files. On screen the only trace is a gap in the slot column, and only
/// when the survivors carry wire `slot` numbers of their own: weak evidence,
/// but it is what a player who counts to six in the game window would have to
/// go on.
///
/// A wholly non-array `slots` degrades to empty, and be clear about what that
/// buys: `Controller::evaluate_snapshot` treats a slotless snapshot as a
/// degraded message and returns before it disarms the expectation, so the
/// watchdog still climbs to `Unresponsive`. The gain is diagnosis, not the
/// session — `last_shop_ms` advances, so the heartbeat can read "server
/// talking, payload unusable" instead of "server mute", which are exactly the
/// two cases that line exists to separate.
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

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ShopItem {
    /// The item's catalog id, `None` when the server omits it — ties a
    /// purchase confirmation back to the slot the player wanted. See
    /// [`CatalogId`] for the sentinel policy.
    #[serde(default, deserialize_with = "optional_catalog_id")]
    pub id: Option<CatalogId>,
    /// Shop slot (1..=6); `0` if the server omits it.
    #[serde(default)]
    pub slot: u8,
    /// Defaults to `Unknown` rather than failing the whole message.
    #[serde(default)]
    pub kind: ItemKind,
    #[serde(default)]
    pub name: Option<String>,
    /// Price in gold. `None` when the server omits it, which fails *open*
    /// everywhere it is read — an unknown price can never be proven
    /// unaffordable. A wire `0` is a real price of zero, not an omission —
    /// see [`Gold`].
    #[serde(default)]
    pub price: Option<Gold>,
    /// Gear grade (2, 3, or 4).
    #[serde(default)]
    pub grade: Option<u8>,
    /// Gear set, by internal id (`set_speed`, `set_immune`, ...).
    #[serde(default)]
    pub set: Option<String>,
    /// Substats and their values, keyed by internal stat name. A nameless or
    /// mistyped entry is dropped, not fatal: it could never match a name-keyed
    /// criterion anyway.
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
    /// cannot wrap back into the `0` sentinel.
    ///
    /// Not injective on malformed shops: a fallback number can collide with
    /// another item's wire slot, so callers matching items by this number may
    /// over-select there.
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
    pub name: String,
    #[serde(default)]
    pub value: Option<f64>,
}

/// Purchase limit, e.g. "0/1" (sold out) or "1/1" (available).
///
/// Both fields stay `u32`, deliberately: these are *counts of purchases*,
/// not money, so they belong to neither ledger, and the only thing ever
/// asked of them is `remaining == 0` ([`ShopItem::is_sold_out`]) — not a
/// cross-space comparison. A third newtype would need its own arithmetic
/// policy and wire fold, filed against a hazard nobody has shown.
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

    /// Fixture amounts, so a test reads `xtl(95)` rather than
    /// `Crystals::new(95)` six times a line — the same shorthand the control
    /// and session suites give [`CatalogId`] with `cid`.
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

    /// The wire shape of both currencies is a bare number
    /// (`#[serde(transparent)]`), and — unlike [`CatalogId`] — a `0` stays a
    /// `0`. See [`Gold`] for why.
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

        // Transparent on the way in: no wrapper object, and a large amount is
        // not truncated on its way into the newtype.
        assert_eq!(
            parse(r#"{"slots":[{"price":4294967295}]}"#).slots[0].price,
            Some(gold(u32::MAX))
        );
    }

    /// The arithmetic each ledger is allowed, and the floors/ceilings it
    /// pins. `Add`/`Sub` are deliberately absent (see [`Gold`]): a regression
    /// that added an operator would still pass this, but one that changed a
    /// saturating method into a wrapping one would not.
    #[test]
    fn currency_arithmetic_saturates_at_both_ends() {
        assert_eq!(gold(200_000).saturating_sub(gold(184_000)), gold(16_000));
        assert_eq!(gold(10).saturating_sub(gold(184_000)), gold(0));

        assert_eq!(xtl(3).saturating_add(xtl(3)), xtl(6));
        assert_eq!(xtl(u32::MAX).saturating_add(xtl(1)), xtl(u32::MAX));
        assert_eq!(xtl(6).saturating_sub(xtl(9)), xtl(0));

        // See Crystals::checked_add: None, not a pinned u32::MAX that happens
        // to compare over budget.
        assert_eq!(xtl(3).checked_add(xtl(3)), Some(xtl(6)));
        assert_eq!(xtl(u32::MAX).checked_add(xtl(1)), None);
    }

    /// Both currencies render grouped, and only grouped — see the `Display`
    /// impl for why.
    #[test]
    fn both_currencies_display_thousands_grouped() {
        assert_eq!(gold(1_234_567).to_string(), "1,234,567");
        assert_eq!(xtl(1_000).to_string(), "1,000");
        assert_eq!(gold(0).to_string(), "0");
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

    #[test]
    fn refresh_mistyped_degrades_to_none() {
        // The degrade contract covers wrong types, not just partial objects.
        assert_eq!(parse(r#"{"refresh":5,"slots":[{}]}"#).refresh, None);
        assert_eq!(parse(r#"{"refresh":"n/a"}"#).refresh, None);
        assert_eq!(parse(r#"{"refresh":[]}"#).refresh, None);
    }

    #[test]
    fn partial_limit_degrades_to_buyable() {
        // Fail-open like an absent limit: the item stays buyable and the
        // snapshot survives.
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
        // The haul-recording lookup. `0` must never resolve to a slot — and
        // cannot, since `CatalogId::new(0)` is `None`.
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

    /// The failure this file's last intolerant collection used to cause. Both
    /// payloads are shapes the server can actually emit — a slot number past
    /// `u8::MAX` and a stringified price — and each used to abort the whole
    /// `ShopSnapshot`, hence the whole `ServerMessage`, hence the run. See
    /// [`lenient_slots`] for why the surviving slots are worth more than the
    /// halt.
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

    /// The whole-collection half of the degrade, like
    /// [`mistyped_substats_degrade_to_empty`] one level down. This one does
    /// *not* save the session — `evaluate_snapshot` refuses a slotless
    /// snapshot — so what it pins is that the snapshot still arrives, which is
    /// what lets the heartbeat tell a talking server from a mute one.
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
}
