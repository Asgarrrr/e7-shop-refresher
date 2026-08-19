//! The shop model: the trimmed contents the analysis server returns, ready to
//! display and filter. Only this shape crosses the link; how the server
//! produces it is not the client's concern.

use std::num::NonZeroU32;

use serde::{Deserialize, Deserializer, Serialize};

/// One shop roll as the analysis server trimmed it: the merchant, the slots on
/// offer, and the refresh-session facts. Every field is optional on the wire —
/// a degraded message still reaches the view rather than failing the link.
#[derive(Debug, Clone, Deserialize)]
pub struct ShopSnapshot {
    #[serde(default)]
    pub merchant: Option<String>,
    #[serde(default)]
    pub slots: Vec<ShopItem>,
    /// Refresh-session facts (balance, cost) — grouped apart because they are
    /// not shop *contents*. Present means both are known; absent, the cost
    /// falls back to the game constant and only out-of-funds detection is
    /// lost.
    #[serde(default, deserialize_with = "object_or_none")]
    pub refresh: Option<RefreshMeta>,
}

/// A global catalog id — the identity the purchase echo uses to name the item
/// the player asked for.
///
/// Two things at once, both of which used to be `u32`:
///
/// 1. **The `0` sentinel is gone.** `NonZeroU32` inside, so "the server omitted
///    the id" is `None` and nothing else. It used to be `id: u32` with `0`
///    standing in for absent, interpreted by a free `shop::catalog_id(id)`
///    documented as *"the only place the `0` sentinel is interpreted — do not
///    re-derive the comparison"*. That contract was broken while it was being
///    written: `Controller::on_purchase` re-derived it as `if item != 0 && …`.
///    A sentinel's whole cost is that every reader has to remember it, and the
///    only fix that does not depend on remembering is not being able to spell it.
///    Both interpreters — the free function and `ShopItem::catalog_id` — are gone
///    with it; the conversion happens once, in [`optional_catalog_id`], at the
///    only place a raw wire number arrives.
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
/// Both were bare `u32` until this pass, and the two ledgers meet inside one
/// type: `Controller` debits a *gold* price from a *gold* balance in
/// `plan_targets` while comparing a *crystal* spend against a *crystal* budget
/// in `stop_reason`. Nothing but the field names separated them, so
/// `progress.spent >= item.price` — a crystal budget weighed against a gold
/// price — compiled, passed every lint, and would have silently vetoed or
/// authorised every buy of the run. That line is `expected Crystals, found
/// Gold` now, which is the deliverable; the rest of this type is what it took
/// to make that true without loosening anything else.
///
/// # No arithmetic operators, deliberately
///
/// `Add`, `Sub`, `AddAssign` and `SubAssign` are **not** implemented, and their
/// absence is the design rather than an oversight. `Cargo.toml` sets
/// `overflow-checks = true` on the release profile (nine comment lines there
/// argue why: a wrapped counter used to be a silent wrong number in a shipped
/// build, and is a `crash.log` entry now). Under that setting `balance - price`
/// *panics* on underflow in the shipped binary — and both operands come off the
/// wire, where nothing guarantees the order. `plan_targets` does hold
/// `price <= balance`, but only through a separate `affordable` term computed
/// three lines earlier: a non-local invariant that an added clause in `in_reach`
/// could break without touching the subtraction at all. So the operator that
/// would compile into that panic is simply not spellable, and every arithmetic
/// site has to name its overflow policy — [`Gold::saturating_sub`] here,
/// [`Crystals::saturating_add`]/[`Crystals::checked_add`] on the other ledger.
/// The `u32` sites this replaced were already written that way by hand; the type
/// now enforces what the comments asked for.
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
/// `#[serde(transparent)]`, so the wire shape is a bare number, unchanged — and
/// `Serialize` for the same reason, because `Filter::max_price` is a `Gold` that
/// `config::persist` writes back to `config.toml`. Transparent in that direction
/// too: the file still reads `max_price = 300000`, which `config/persist.rs`'s
/// strip test pins by string.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Gold(u32);

impl Gold {
    /// The amount `raw` names.
    ///
    /// Total, unlike [`CatalogId::new`]: every `u32` is a gold amount, so there
    /// is no `Option` to unwrap and no sentinel to interpret.
    ///
    /// There is no longer a *crossing* for it to guard. `Filter::max_price` and
    /// `Limits::max_spend` — the player's two raw ceilings, which briefly stayed
    /// `Option<u32>` and were lifted here at the single comparison that read each
    /// — are now `Option<Gold>` and `Option<Crystals>` themselves, so the two
    /// lifting calls are gone and both comparisons are in-ledger from the parse
    /// onward rather than from one line before the `>`. What remains for this
    /// constructor is fixtures and the `deserialize_with`-free wire path.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The number, for a wire field or a comparison against something that is
    /// not yet a currency.
    ///
    /// It has **no caller in the crate today**, and that is the intended steady
    /// state rather than an oversight to tidy away: every comparison is
    /// in-ledger, every print goes through [`Display`](std::fmt::Display), and
    /// the two raw ceilings that do cross come *in* through [`Gold::new`]
    /// instead of dragging an amount out. It exists so that the next caller who
    /// genuinely needs the number has a documented way to ask, rather than
    /// widening the tuple field to `pub` and losing the type. Reaching for it to
    /// do arithmetic re-opens exactly the hole this type closed — the ledger has
    /// named methods for that.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// `self - rhs`, floored at zero.
    ///
    /// The only subtraction on this ledger, and saturating because its one
    /// caller (`Controller::plan_targets`, debiting each planned buy from the
    /// running balance) holds `rhs <= self` only through a separate
    /// affordability test. At zero the next item simply reads unaffordable,
    /// which is the intended semantics — see the type's note on why `Sub` is
    /// absent.
    #[must_use]
    pub const fn saturating_sub(self, rhs: Self) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }
}

/// Crystals — "skystones" in game. What a shop refresh costs and what the
/// player's refresh budget is denominated in. The other half of the pair
/// [`Gold`] documents; read that type first, everything structural there
/// applies here.
///
/// This ledger is the one that *accumulates*: `Progress::spent` adds a refresh
/// cost per issued refresh and `RefreshMeta::crystal_balance` is debited by the
/// same amount, so it carries an add where [`Gold`] carries only a subtract.
/// Both directions are explicit about overflow for the reason [`Gold`] gives —
/// no operator on this type either.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Crystals(u32);

impl Crystals {
    /// The amount `raw` names. Total, like [`Gold::new`], and for the same
    /// reason: zero crystals is a real balance.
    ///
    /// `Controller::stop_reason` used to lift the player's `[limits] max_spend`
    /// here so the budget comparison `type-004` was filed against had two crystal
    /// operands. The field is a `Crystals` now, so that call is gone and the
    /// comparison is typed from the parse onward — see [`Gold::new`].
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
    /// Distinct from [`Crystals::saturating_add`] on purpose, and the two are
    /// not interchangeable: `stop_reason` asks whether the *next* refresh would
    /// cross the budget, and a saturating answer there reads as "`u32::MAX`,
    /// which is over the budget" — accidentally right, but for the wrong
    /// reason. `None` says "this cannot be computed, so the ceiling is
    /// certainly crossed", which is what the caller actually means.
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
/// `Display` rather than leaving every site to call `render::grouped` itself,
/// because the four places that print an amount — the status-bar balance tiles,
/// the slot table's price column, the console/tooltip item line, and the
/// journal's "bought" line — each decided grouping on their own, and the last
/// one did not group at all: `{gold} gold left` printed `250000` in the journal
/// while the table beside it showed `250,000`. `render::grouped`'s own doc
/// already claimed all of them read the same. Now they do, because there is one
/// rendering per currency and it lives on the currency.
///
/// The call goes `domain` → `render`, the one edge in that direction. Two
/// alternatives were weighed: writing this impl inside `render.rs` (legal — both
/// the trait's implementor and the crate are local — but then a reader of
/// `shop.rs` cannot see that the type has a `Display` at all), and moving
/// `grouped` down here (it also formats refresh counts, which are not money, so
/// it would be a number formatter homed in the shop model). `grouped` is a pure
/// `u32` → `String` with no I/O and no state, so the edge costs nothing beyond
/// its own existence.
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
/// The one place a raw id number is interpreted, which is the whole point of
/// [`CatalogId`]. `0` is the server's own spelling of absent, so it has to be
/// accepted and folded here rather than refused — the message is still a good
/// message, it just cannot be tied back to a slot.
pub(crate) fn optional_catalog_id<'de, D>(de: D) -> Result<Option<CatalogId>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<u32>::deserialize(de)?.and_then(CatalogId::new))
}

impl ShopSnapshot {
    /// The slot bearing this catalog id, if any. Ids are unique within a
    /// snapshot (the shop never lists an item twice), so at most one matches.
    /// The single home for a find-by-id; an item whose id the server omitted
    /// can no longer be matched by accident, because it has no id to compare.
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
/// The degradation is *logged*, because both of these fields change what the app
/// does and not just what it shows: a dropped `limit` makes a sold-out slot read
/// buyable (the actuator then clicks Buy, no echo arrives, and the watchdog halts
/// `Unresponsive` blaming the game), and a dropped `refresh` silently disables
/// out-of-funds detection. `debug!` and not `warn!`: the default filter keeps it
/// in the log file, and it is not the player's problem to read.
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

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ShopItem {
    /// The item's catalog id, `None` when the server omits it. Lets a purchase
    /// confirmation (whose `item` is this id) be tied back to the slot the player
    /// wanted.
    ///
    /// An `Option<CatalogId>` and not a `u32` with `0` for absent — see
    /// [`CatalogId`] for what that sentinel cost.
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
    /// unaffordable. A wire `0` is a real price of zero, not an omission: see
    /// [`Gold`] on why this field has no sentinel fold while `id` does.
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
/// Both fields stay `u32`, deliberately. The currency pass that introduced
/// [`Gold`] and [`Crystals`] listed this type among its sites, but these are
/// *counts of purchases*, not money: they belong to neither ledger, and the only
/// thing ever asked of them is `remaining == 0` ([`ShopItem::is_sold_out`]),
/// which is not a cross-space comparison and cannot be made into one by
/// swapping a field. Typing them would mean a third newtype, with its own
/// arithmetic policy and its own wire fold, filed against a hazard nobody has
/// shown — so it is not done here.
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
    /// (`#[serde(transparent)]`), and — unlike [`CatalogId`] — a `0` stays a `0`.
    ///
    /// The positive half of the "no second sentinel policy" decision argued on
    /// [`Gold`]: a broke player is a *known* balance of zero, and folding it to
    /// `None` would make `plan_targets` fail open and authorise buys that cannot
    /// happen. `null`/absent remains the only spelling of "unknown".
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

    /// The arithmetic each ledger is allowed, and the floors/ceilings it pins.
    /// `Add`/`Sub` are deliberately absent (see [`Gold`]), so these named
    /// methods are the whole surface — a regression that added an operator would
    /// still pass this, but a regression that changed a saturating method into a
    /// wrapping one would not.
    #[test]
    fn currency_arithmetic_saturates_at_both_ends() {
        assert_eq!(gold(200_000).saturating_sub(gold(184_000)), gold(16_000));
        assert_eq!(gold(10).saturating_sub(gold(184_000)), gold(0));

        assert_eq!(xtl(3).saturating_add(xtl(3)), xtl(6));
        assert_eq!(xtl(u32::MAX).saturating_add(xtl(1)), xtl(u32::MAX));
        assert_eq!(xtl(6).saturating_sub(xtl(9)), xtl(0));

        // `checked_add` is not `saturating_add`: `stop_reason` needs "this next
        // refresh cannot be costed" to read as None, not as a pinned u32::MAX
        // that happens to compare over budget.
        assert_eq!(xtl(3).checked_add(xtl(3)), Some(xtl(6)));
        assert_eq!(xtl(u32::MAX).checked_add(xtl(1)), None);
    }

    /// Both currencies render grouped, and only grouped — the property the four
    /// printing sites used to each decide for themselves.
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
        // The haul-recording lookup. `0` is the server's spelling of "I have no
        // id for this", so it must never resolve to the slot that carries it —
        // and it cannot, because `CatalogId::new(0)` is `None` and there is no
        // `slot_by_id(0)` left to call.
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

    #[test]
    fn mistyped_substats_degrade_to_empty() {
        let snapshot = parse(r#"{"slots":[{"id":9,"substats":"corrupt"}]}"#);
        assert!(snapshot.slots[0].substats.is_empty());
        assert_eq!(snapshot.slots[0].id, CatalogId::new(9));
    }
}
