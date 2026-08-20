//! Snapshot identity for duplicate suppression: an identical re-arrival of
//! an already-acted-on shop must never double-bill a refresh or re-buy.

use std::sync::Arc;

use crate::domain::shop::{CatalogId, Gold, ShopSnapshot, Substat};

/// One slot's contribution to a snapshot's identity: the catalog id plus the
/// per-roll fields the filter matches on, since a re-roll can redraw the same
/// ids and must read as a new shop. `limit` is deliberately excluded —
/// re-opening after a buy re-delivers the roll with `remaining` decremented,
/// which is still the same shop.
#[derive(Clone, PartialEq)]
pub(super) struct SlotIdentity {
    id: CatalogId,
    price: Option<Gold>,
    grade: Option<u8>,
    set: Option<String>,
    substats: Vec<Substat>,
}

/// A snapshot's dedup identity: the ordered [`SlotIdentity`]s, behind an `Arc`
/// because `Controller` holds two at once (the last roll evaluated and the roll
/// `bought` is scoped to) and they are the same value whenever both are set.
/// Never mutated in place, so the second holder wants a refcount, not a copy.
pub(super) type Fingerprint = Arc<Vec<SlotIdentity>>;

/// `None` when any id is the 0 sentinel — omitted ids make shops
/// indistinguishable, so dedup must fail open rather than guess.
pub(super) fn fingerprint(snapshot: &ShopSnapshot) -> Option<Fingerprint> {
    let slots: Option<Vec<SlotIdentity>> = snapshot
        .slots
        .iter()
        .map(|item| {
            item.id.map(|id| SlotIdentity {
                id,
                price: item.price,
                grade: item.grade,
                set: item.set.clone(),
                substats: item.substats.clone(),
            })
        })
        .collect();
    slots.map(Arc::new)
}
