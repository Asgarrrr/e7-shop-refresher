//! Snapshot identity for duplicate suppression: an identical re-arrival of
//! an already-acted-on shop must never double-bill a refresh or re-buy.

use crate::domain::shop::{ShopSnapshot, SubStat};

/// One slot's contribution to a snapshot's identity: the catalog id plus the
/// per-roll fields the filter can match on — a re-roll redrawing the same
/// catalog ids is improbable but possible, and must read as a new shop.
/// `limit` is deliberately excluded: re-opening the shop after a buy
/// re-delivers the same roll with `remaining` decremented, and that must
/// still count as the same shop.
#[derive(PartialEq)]
pub(super) struct SlotIdentity {
    id: u32,
    price: Option<u32>,
    grade: Option<u8>,
    set: Option<String>,
    substats: Vec<SubStat>,
}

/// Snapshot identity for dedup: the ordered [`SlotIdentity`]s. `None` when
/// any id is the 0 sentinel — omitted ids make shops indistinguishable.
pub(super) fn fingerprint(snapshot: &ShopSnapshot) -> Option<Vec<SlotIdentity>> {
    snapshot
        .slots
        .iter()
        .map(|item| {
            item.catalog_id().map(|id| SlotIdentity {
                id,
                price: item.price,
                grade: item.grade,
                set: item.set.clone(),
                substats: item.substats.clone(),
            })
        })
        .collect()
}
