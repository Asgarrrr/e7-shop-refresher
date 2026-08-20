//! Pure projection of the controller for the window: everything one frame
//! shows, copied under a single short lock. The hover tooltip
//! ([`slot_detail`]) and the slot rows ([`SlotRows`]) are deliberately not
//! copied per frame, so building a [`ViewState`] allocates nothing.

use crate::domain::control::{Controller, Limits, Progress, Status};
use crate::domain::shop::{CatalogId, Crystals, Gold, ShopItem};
use crate::render::{
    HAUL_HEADLINERS, format_item, haul_tally, kind_label, merchant_label, status_summary,
};

/// Every field is `Copy` or `&'static`, so the per-frame copy is free.
pub(super) struct ViewState {
    pub status_word: &'static str,
    /// A hint, or the stop reason; `None` while watching.
    pub status_hint: Option<&'static str>,
    /// For the status color only — the two above own the wording.
    pub status_kind: Status,
    pub progress: Progress,
    pub limits: Limits,
    /// From the controller's enforced meta, not the raw snapshot.
    pub crystal_balance: Option<Crystals>,
    /// Last gold balance echoed by a purchase; `None` before the first buy and
    /// again after `Start`.
    pub gold_balance: Option<Gold>,
    /// A shop has been captured this session, even a degraded slotless one.
    /// Gates the welcome screen: empty rows alone must not resurrect it.
    pub has_snapshot: bool,
    /// Confirmed buys this run, per headline token, shown even at zero once a
    /// run exists.
    pub haul: [(&'static str, u32); HAUL_HEADLINERS.len()],
    /// Everything else bought this run, folded into one "+N other" bucket.
    pub haul_others: u32,
}

/// One shop slot as the table shows it.
pub(super) struct SlotRow {
    pub slot: u8,
    pub kind: &'static str,
    pub name: Option<String>,
    pub price: Option<Gold>,
    pub sold_out: bool,
    /// Matched and still to buy: the catalog id sits in the checklist.
    pub wanted: bool,
}

impl SlotRow {
    /// The cloned name is the only allocation a frame's projection makes —
    /// which is why [`SlotRows`] gates it.
    fn project(item: &ShopItem, index: usize, checklist: &[CatalogId]) -> Self {
        Self {
            slot: item.effective_slot(index),
            kind: kind_label(item.kind),
            name: item.name.clone(),
            price: item.price,
            sold_out: item.is_sold_out(),
            wanted: item.id.is_some_and(|id| checklist.contains(&id)),
        }
    }

    /// Whether this row still describes `item` at `index` — [`SlotRows`]'s
    /// gate, mirroring [`SlotRow::project`] with nothing allocated.
    ///
    /// `self` is destructured rather than read field by field so that a field
    /// added above but left out here becomes an unused binding, which CI denies.
    fn matches(&self, item: &ShopItem, index: usize, checklist: &[CatalogId]) -> bool {
        let Self {
            slot,
            kind,
            name,
            price,
            sold_out,
            wanted,
        } = self;
        *slot == item.effective_slot(index)
            && *kind == kind_label(item.kind)
            && name.as_deref() == item.name.as_deref()
            && *price == item.price
            && *sold_out == item.is_sold_out()
            && *wanted == item.id.is_some_and(|id| checklist.contains(&id))
    }
}

/// The slot table's rows, re-derived only when the shop or the checklist behind
/// them moved. This buys lock hold, not CPU: the projection runs inside the
/// controller lock the session loop needs, and hover lifts the repaint rate
/// above the 4 Hz idle poll.
///
/// The gate compares fields rather than a generation counter or `Arc::ptr_eq`:
/// [`Controller::last_snapshot`] stores the snapshot inline, so a replacement
/// lands at the same address — nothing cheaper is also correct.
#[derive(Default)]
pub(super) struct SlotRows(Vec<SlotRow>);

impl SlotRows {
    /// The cached rows, rendered after the controller guard is dropped.
    pub(super) fn rows(&self) -> &[SlotRow] {
        &self.0
    }

    /// Brings the cache up to date, re-deriving only when the projection moved.
    pub(super) fn sync(&mut self, controller: &Controller) {
        if self.is_current(controller) {
            return;
        }
        let checklist = controller.checklist();
        self.0 = controller
            .last_snapshot()
            .map_or_else(Vec::new, |snapshot| {
                snapshot
                    .slots
                    .iter()
                    .enumerate()
                    .map(|(index, item)| SlotRow::project(item, index, checklist))
                    .collect()
            });
    }

    /// Whether every cached row still describes its slot, with as many rows
    /// as slots. An absent snapshot projects to the empty slice.
    fn is_current(&self, controller: &Controller) -> bool {
        let slots = controller
            .last_snapshot()
            .map_or(&[][..], |snapshot| &snapshot.slots);
        if self.0.len() != slots.len() {
            return false;
        }
        let checklist = controller.checklist();
        self.0
            .iter()
            .zip(slots)
            .enumerate()
            .all(|(index, (row, item))| row.matches(item, index, checklist))
    }
}

/// The shop table's hover tooltip, built on demand rather than projected into
/// every [`SlotRow`]: as a field, `format_item` ran once per slot per frame
/// inside the controller lock, at hover's display repaint rate, for a string
/// only one row ever reads.
///
/// `index` is the row's position in the projected snapshot, so a message
/// landing between projection and hover describes the new roll — a sub-repaint
/// skew resolved on the next poll. Empty when that slot is gone.
pub(super) fn slot_detail(controller: &Controller, index: usize) -> String {
    controller
        .last_snapshot()
        .and_then(|snapshot| snapshot.slots.get(index))
        .map(|item| format_item(item, index))
        .unwrap_or_default()
}

/// The Shop tab's heading: the merchant name, or the shared fallback. Built on
/// demand like [`slot_detail`] rather than folded into [`ViewState`] — a
/// `String` field there would break the "every field is `Copy` or `&'static`"
/// invariant this module opens with, and would allocate every frame even while
/// the merchant sits idle between rolls. Named `merchant_heading` rather than
/// `merchant_label` so it does not collide with [`crate::render::merchant_label`],
/// the fallback it calls through to — the same one the console dump reads, so
/// the two never disagree.
pub(super) fn merchant_heading(controller: &Controller) -> String {
    let merchant = controller
        .last_snapshot()
        .and_then(|snapshot| snapshot.merchant.as_deref());
    merchant_label(merchant).to_owned()
}

/// Pure extraction, allocation-free: the caller holds the controller lock
/// only for this call.
pub(super) fn view_state(controller: &Controller) -> ViewState {
    let (status_word, status_hint) = status_summary(controller);
    let (haul, haul_others) = haul_tally(controller.haul());
    ViewState {
        status_word,
        status_hint,
        status_kind: controller.status(),
        progress: controller.progress(),
        limits: *controller.limits(),
        crystal_balance: controller.refresh_meta().map(|meta| meta.crystal_balance),
        gold_balance: controller.gold_balance(),
        has_snapshot: controller.last_snapshot().is_some(),
        haul,
        haul_others,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::control::{Event, StopReason};
    use crate::domain::filter::Filter;
    use crate::domain::shop::{PurchaseLimit, RefreshMeta, ShopItem, ShopSnapshot};

    fn controller() -> Controller {
        Controller::new(Filter::matching_default_items(), Limits::default())
    }

    fn shop(slots: Vec<ShopItem>) -> ShopSnapshot {
        ShopSnapshot {
            merchant: None,
            slots,
            refresh: None,
        }
    }

    fn rows(controller: &Controller) -> SlotRows {
        let mut rows = SlotRows::default();
        rows.sync(controller);
        rows
    }

    #[test]
    fn view_state_on_fresh_controller_is_idle_and_empty() {
        let ctrl = controller();
        let view = view_state(&ctrl);
        assert_eq!(view.status_word, "Idle");
        assert_eq!(view.status_hint, Some("ready to start"));
        assert_eq!(view.status_kind, Status::Idle);
        assert!(!view.has_snapshot);
        assert!(rows(&ctrl).rows().is_empty());
        assert_eq!(view.crystal_balance, None);
        assert_eq!(view.gold_balance, None);
    }

    #[test]
    fn slot_rows_use_effective_slot_fallback() {
        let mut ctrl = controller();
        // The second slot carries no wire slot and falls back to its 1-based
        // position.
        let slots = vec![
            ShopItem {
                slot: 5,
                ..ShopItem::default()
            },
            ShopItem {
                slot: 0,
                ..ShopItem::default()
            },
        ];
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(slots),
            now_ms: 0,
        });
        let rows = rows(&ctrl);
        assert_eq!(rows.rows()[0].slot, 5);
        assert_eq!(rows.rows()[1].slot, 2);
    }

    #[test]
    fn slot_rows_flag_checklist_rows_as_wanted() {
        let mut ctrl = controller();
        let _ = ctrl.handle(Event::Start { now_ms: 0 });
        // The default filter matches both, but only a trackable id enters the
        // checklist: the id-0 sentinel row must never read as wanted.
        let slots = vec![
            ShopItem {
                id: CatalogId::new(42),
                ..ShopItem::default()
            },
            ShopItem {
                id: CatalogId::new(0),
                ..ShopItem::default()
            },
        ];
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(slots),
            now_ms: 1,
        });
        let rows = rows(&ctrl);
        assert!(rows.rows()[0].wanted);
        assert!(!rows.rows()[1].wanted);
    }

    #[test]
    fn slot_rows_flag_sold_out_rows() {
        let mut ctrl = controller();
        let slots = vec![ShopItem {
            limit: Some(PurchaseLimit {
                remaining: 0,
                total: 1,
            }),
            ..ShopItem::default()
        }];
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(slots),
            now_ms: 0,
        });
        assert!(rows(&ctrl).rows()[0].sold_out);
    }

    #[test]
    fn view_state_copies_refresh_meta_when_present() {
        let mut ctrl = controller();
        let snapshot = ShopSnapshot {
            merchant: Some("Secret Shop".to_owned()),
            slots: vec![ShopItem::default()],
            refresh: Some(RefreshMeta {
                crystal_balance: Crystals::new(95),
                cost: Crystals::new(3),
            }),
        };
        let _ = ctrl.handle(Event::Snapshot {
            snapshot,
            now_ms: 0,
        });
        let view = view_state(&ctrl);
        assert_eq!(view.crystal_balance, Some(Crystals::new(95)));
    }

    #[test]
    fn view_state_balance_survives_meta_less_snapshot() {
        let mut ctrl = controller();
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: ShopSnapshot {
                merchant: None,
                slots: vec![ShopItem::default()],
                refresh: Some(RefreshMeta {
                    crystal_balance: Crystals::new(95),
                    cost: Crystals::new(3),
                }),
            },
            now_ms: 0,
        });
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![ShopItem::default()]),
            now_ms: 1,
        });
        assert_eq!(view_state(&ctrl).crystal_balance, Some(Crystals::new(95)));
    }

    #[test]
    fn view_state_balance_cleared_on_restart() {
        // `Start` discards a stale balance, and it must not resurrect from the
        // stored snapshot.
        let mut ctrl = controller();
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: ShopSnapshot {
                merchant: None,
                slots: vec![ShopItem::default()],
                refresh: Some(RefreshMeta {
                    crystal_balance: Crystals::new(95),
                    cost: Crystals::new(3),
                }),
            },
            now_ms: 0,
        });
        let _ = ctrl.handle(Event::Start { now_ms: 1 });
        assert_eq!(view_state(&ctrl).crystal_balance, None);
    }

    #[test]
    fn view_state_surfaces_gold_balance_from_a_purchase() {
        let mut ctrl = controller();
        assert_eq!(view_state(&ctrl).gold_balance, None);
        let _ = ctrl.handle(Event::Purchase {
            item: CatalogId::new(42),
            gold: Some(Gold::new(1_204_000)),
            now_ms: 0,
        });
        assert_eq!(view_state(&ctrl).gold_balance, Some(Gold::new(1_204_000)));
    }

    #[test]
    fn view_state_reports_stop_reason_when_stopped() {
        let mut ctrl = controller();
        let _ = ctrl.handle(Event::Start { now_ms: 0 });
        let _ = ctrl.handle(Event::Stop);
        let view = view_state(&ctrl);
        assert_eq!(view.status_word, "Stopped");
        assert_eq!(view.status_kind, Status::Stopped(StopReason::PlayerStopped));
        // The stop reason rides in the hint, not a separate field.
        assert_eq!(view.status_hint, Some("player stopped"));
    }

    #[test]
    fn slot_detail_matches_format_item() {
        let mut ctrl = controller();
        let item = ShopItem {
            id: CatalogId::new(7),
            slot: 3,
            name: Some("Covenant Bookmark".to_owned()),
            price: Some(Gold::new(184_000)),
            ..ShopItem::default()
        };
        let expected = format_item(&item, 0);
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![item]),
            now_ms: 0,
        });
        assert_eq!(slot_detail(&ctrl, 0), expected);
    }

    #[test]
    fn slot_detail_of_a_vanished_slot_is_empty() {
        // The index may no longer exist by the time this is called.
        let mut ctrl = controller();
        assert_eq!(slot_detail(&ctrl, 0), "");
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![ShopItem::default()]),
            now_ms: 0,
        });
        assert_eq!(slot_detail(&ctrl, 3), "");
    }

    /// Asserted through capacity, not identity: `sync` `collect`s an iterator
    /// of known length, so a rebuilt vector's capacity equals its length, and
    /// growing the live one first tells the two apart. `as_ptr` would prove
    /// nothing — a freed buffer can come back at the same address.
    #[test]
    fn slot_rows_are_not_re_derived_while_the_shop_and_the_checklist_hold() {
        let mut ctrl = controller();
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![ShopItem {
                name: Some("Covenant Bookmark".to_owned()),
                price: Some(Gold::new(184_000)),
                ..ShopItem::default()
            }]),
            now_ms: 0,
        });
        let mut rows = SlotRows::default();
        rows.sync(&ctrl);
        assert_eq!(rows.rows().len(), 1);
        rows.0.reserve(64);
        let capacity = rows.0.capacity();

        // Repaints with nothing new to show.
        for _ in 0..10 {
            rows.sync(&ctrl);
        }

        assert_eq!(
            rows.0.capacity(),
            capacity,
            "the rows were re-derived on a frame that had no reason to"
        );
        assert!(rows.is_current(&ctrl));
    }

    #[test]
    fn slot_rows_are_re_derived_when_the_shop_rolls_over() {
        let mut ctrl = controller();
        let roll = |name: &str| {
            shop(vec![ShopItem {
                name: Some(name.to_owned()),
                ..ShopItem::default()
            }])
        };
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: roll("Covenant Bookmark"),
            now_ms: 0,
        });
        let mut rows = SlotRows::default();
        rows.sync(&ctrl);

        // A new roll lands at the same address, and the gate must notice.
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: roll("Mystic Medal"),
            now_ms: 1,
        });
        assert!(!rows.is_current(&ctrl));
        rows.sync(&ctrl);
        assert_eq!(rows.rows()[0].name.as_deref(), Some("Mystic Medal"));
    }

    #[test]
    fn slot_rows_are_re_derived_when_the_checklist_moves_under_them() {
        // `wanted` is not in the snapshot, so keying on the shop alone would
        // leave the row green after the purchase.
        let mut ctrl = controller();
        let id = CatalogId::new(42);
        let _ = ctrl.handle(Event::Start { now_ms: 0 });
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![ShopItem {
                id,
                ..ShopItem::default()
            }]),
            now_ms: 1,
        });
        let mut rows = SlotRows::default();
        rows.sync(&ctrl);
        assert!(rows.rows()[0].wanted);

        let _ = ctrl.handle(Event::Purchase {
            item: id,
            gold: None,
            now_ms: 2,
        });

        assert!(!rows.is_current(&ctrl));
        rows.sync(&ctrl);
        assert!(!rows.rows()[0].wanted);
    }
}
