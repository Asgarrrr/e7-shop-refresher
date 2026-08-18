//! Snapshot identity for duplicate suppression: an identical re-arrival of
//! an already-acted-on shop must never double-bill a refresh or re-buy.

use std::sync::Arc;

use crate::domain::shop::{CatalogId, ShopSnapshot, Substat};

/// One slot's contribution to a snapshot's identity: the catalog id plus the
/// per-roll fields the filter can match on — a re-roll redrawing the same
/// catalog ids is improbable but possible, and must read as a new shop.
/// `limit` is deliberately excluded: re-opening the shop after a buy
/// re-delivers the same roll with `remaining` decremented, and that must
/// still count as the same shop.
#[derive(Clone, PartialEq)]
pub(super) struct SlotIdentity {
    id: CatalogId,
    price: Option<u32>,
    grade: Option<u8>,
    set: Option<String>,
    substats: Vec<Substat>,
}

/// A snapshot's dedup identity: the ordered [`SlotIdentity`]s, behind an `Arc`.
///
/// Shared rather than owned because `Controller` holds two of these at once (the
/// last roll evaluated and the roll `bought` is scoped to) and they are the same
/// value whenever both are set. The contents are never mutated in place — only
/// compared and replaced wholesale — so the second holder wants a refcount, not
/// a second deep copy of every slot's `set` and `substats` strings.
pub(super) type Fingerprint = Arc<Vec<SlotIdentity>>;

/// Snapshot identity for dedup: the ordered [`SlotIdentity`]s. `None` when
/// any id is the 0 sentinel — omitted ids make shops indistinguishable.
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
