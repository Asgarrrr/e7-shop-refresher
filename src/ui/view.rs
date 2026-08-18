//! Pure projection of the controller for the window: everything one frame
//! shows, copied under a single short lock.
//!
//! The one thing deliberately *not* in the projection is the slot table's hover
//! tooltip — see [`slot_detail`].

use crate::domain::control::{Controller, Limits, Progress, Status};
use crate::render::{HAUL_HEADLINERS, format_item, haul_tally, kind_label, status_summary};

/// Plain per-frame copy of everything the window shows; built under the
/// controller lock, rendered after the guard is dropped.
pub(super) struct ViewState {
    /// State word (idle/watching/…), carrying the severity color in the bar.
    pub status_word: &'static str,
    /// Secondary clause beside the word: a hint, or the stop reason when
    /// stopped. Muted in the bar. `None` while watching.
    pub status_hint: Option<&'static str>,
    /// The raw status (with its `StopReason`), for the status color — the
    /// word/hint above stay the source of the wording.
    pub status_kind: Status,
    pub progress: Progress,
    pub limits: Limits,
    /// From the controller's enforced meta (debited per advised refresh,
    /// cleared on restart) — not the raw snapshot, which can be stale. The
    /// game calls these "skystones"; the code says "crystals".
    pub crystal_balance: Option<u32>,
    /// Last gold balance echoed by a purchase this run; `None` before the
    /// first buy and again after `Start`.
    pub gold_balance: Option<u32>,
    /// A shop has been captured this session — even a degraded slotless one.
    /// Gates the welcome screen: empty `rows` alone must not resurrect it
    /// mid-session.
    pub has_snapshot: bool,
    pub rows: Vec<SlotRow>,
    /// Confirmed buys this run, per headline token (label, count), in order —
    /// shown even at zero once a run exists, so the player sees the target.
    pub haul: [(&'static str, u32); HAUL_HEADLINERS.len()],
    /// Everything else bought this run, folded into one "+N other" bucket.
    pub haul_others: u32,
}

/// One shop slot as the table shows it.
///
/// The full console line the row shows on hover is **not** a field here: see
/// [`slot_detail`].
pub(super) struct SlotRow {
    pub slot: u8,
    pub kind: &'static str,
    pub name: Option<String>,
    pub price: Option<u32>,
    pub sold_out: bool,
    /// Matched and still to buy: the catalog id sits in the checklist.
    pub wanted: bool,
}

/// The full console line for one slot — the shop table's hover tooltip — built
/// on demand instead of being projected into every [`SlotRow`].
///
/// It used to be a `SlotRow` field, which meant `format_item` ran once per slot
/// on every frame, *inside* the controller lock, for a string at most one row
/// ever reads (only the row under the pointer shows a tooltip). That lock is the
/// same one the session loop takes to turn a captured shop into a click job, and
/// hover state is input — so the pointer moving over the table repaints at
/// display rate, not the 250 ms poll floor, lengthening every hold the session
/// loop competes for. `shop_table` now calls this from inside `on_hover_ui`,
/// which egui invokes only for the hovered widget, so it costs one line per
/// hovered frame and nothing at all otherwise. The same deferral the accessible
/// names in `theme.rs` and `journal.rs` already use.
///
/// `index` is the row's position in the snapshot the frame projected. A shop
/// message landing between that projection and the hover makes the line
/// describe the new roll at that position — a sub-repaint skew on a tooltip,
/// which the table itself resolves on the next poll. Empty when the snapshot no
/// longer has that slot.
pub(super) fn slot_detail(controller: &Controller, index: usize) -> String {
    controller
        .last_snapshot()
        .and_then(|snapshot| snapshot.slots.get(index))
        .map(|item| format_item(item, index))
        .unwrap_or_default()
}

/// Pure extraction: the caller holds the controller lock only for this call.
pub(super) fn view_state(controller: &Controller) -> ViewState {
    let (status_word, status_hint) = status_summary(controller);
    let (haul, haul_others) = haul_tally(controller.haul());
    let checklist = controller.checklist();
    let snapshot = controller.last_snapshot();
    let rows = snapshot
        .map(|snapshot| {
            snapshot
                .slots
                .iter()
                .enumerate()
                .map(|(index, item)| SlotRow {
                    slot: item.effective_slot(index),
                    kind: kind_label(item.kind),
                    name: item.name.clone(),
                    price: item.price,
                    sold_out: item.is_sold_out(),
                    wanted: item.id.is_some_and(|id| checklist.contains(&id)),
                })
                .collect()
        })
        .unwrap_or_default();
    ViewState {
        status_word,
        status_hint,
        status_kind: controller.status(),
        progress: controller.progress(),
        limits: *controller.limits(),
        crystal_balance: controller.refresh_meta().map(|meta| meta.crystal_balance),
        gold_balance: controller.gold_balance(),
        has_snapshot: snapshot.is_some(),
        rows,
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

    #[test]
    fn view_state_on_fresh_controller_is_idle_and_empty() {
        let view = view_state(&controller());
        assert_eq!(view.status_word, "Idle");
        assert_eq!(view.status_hint, Some("ready to start"));
        assert_eq!(view.status_kind, Status::Idle);
        assert!(!view.has_snapshot);
        assert!(view.rows.is_empty());
        assert_eq!(view.crystal_balance, None);
        assert_eq!(view.gold_balance, None);
    }

    #[test]
    fn view_state_rows_use_effective_slot_fallback() {
        let mut ctrl = controller();
        // First slot carries a wire slot, second falls back to its 1-based
        // position. Stored while Idle: storage does not require an armed loop.
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
        let view = view_state(&ctrl);
        assert_eq!(view.rows[0].slot, 5);
        assert_eq!(view.rows[1].slot, 2);
    }

    #[test]
    fn view_state_flags_checklist_rows_as_wanted() {
        let mut ctrl = controller();
        let _ = ctrl.handle(Event::Start { now_ms: 0 });
        // Default filter matches both; only the trackable id enters the
        // checklist — the id-0 sentinel row must never read as wanted.
        let slots = vec![
            ShopItem {
                id: crate::domain::shop::CatalogId::new(42),
                ..ShopItem::default()
            },
            ShopItem {
                id: crate::domain::shop::CatalogId::new(0),
                ..ShopItem::default()
            },
        ];
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(slots),
            now_ms: 1,
        });
        let view = view_state(&ctrl);
        assert!(view.rows[0].wanted);
        assert!(!view.rows[1].wanted);
    }

    #[test]
    fn view_state_flags_sold_out_rows() {
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
        assert!(view_state(&ctrl).rows[0].sold_out);
    }

    #[test]
    fn view_state_copies_refresh_meta_when_present() {
        let mut ctrl = controller();
        let snapshot = ShopSnapshot {
            merchant: Some("Secret Shop".to_owned()),
            slots: vec![ShopItem::default()],
            refresh: Some(RefreshMeta {
                crystal_balance: 95,
                cost: 3,
            }),
        };
        let _ = ctrl.handle(Event::Snapshot {
            snapshot,
            now_ms: 0,
        });
        let view = view_state(&ctrl);
        assert_eq!(view.crystal_balance, Some(95));
    }

    #[test]
    fn view_state_balance_survives_meta_less_snapshot() {
        // The controller keeps its enforced estimate across snapshots that
        // omit meta; the display must show that, not the raw snapshot.
        let mut ctrl = controller();
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: ShopSnapshot {
                merchant: None,
                slots: vec![ShopItem::default()],
                refresh: Some(RefreshMeta {
                    crystal_balance: 95,
                    cost: 3,
                }),
            },
            now_ms: 0,
        });
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![ShopItem::default()]),
            now_ms: 1,
        });
        assert_eq!(view_state(&ctrl).crystal_balance, Some(95));
    }

    #[test]
    fn view_state_balance_cleared_on_restart() {
        // `Start` discards a stale balance; the display must not resurrect it
        // from the stored snapshot.
        let mut ctrl = controller();
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: ShopSnapshot {
                merchant: None,
                slots: vec![ShopItem::default()],
                refresh: Some(RefreshMeta {
                    crystal_balance: 95,
                    cost: 3,
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
            item: crate::domain::shop::CatalogId::new(42),
            gold: Some(1_204_000),
            now_ms: 0,
        });
        assert_eq!(view_state(&ctrl).gold_balance, Some(1_204_000));
    }

    #[test]
    fn view_state_reports_stop_reason_when_stopped() {
        let mut ctrl = controller();
        let _ = ctrl.handle(Event::Start { now_ms: 0 });
        let _ = ctrl.handle(Event::Stop);
        let view = view_state(&ctrl);
        assert_eq!(view.status_word, "Stopped");
        assert_eq!(view.status_kind, Status::Stopped(StopReason::PlayerStopped));
        // The stop reason rides in the hint now, not a separate field.
        assert_eq!(view.status_hint, Some("player stopped"));
    }

    #[test]
    fn slot_detail_matches_format_item() {
        let mut ctrl = controller();
        let item = ShopItem {
            id: crate::domain::shop::CatalogId::new(7),
            slot: 3,
            name: Some("Covenant Bookmark".to_owned()),
            price: Some(184_000),
            ..ShopItem::default()
        };
        let expected = format_item(&item, 0);
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![item]),
            now_ms: 0,
        });
        // Same line the row used to carry as a field, now built per hover.
        assert_eq!(slot_detail(&ctrl, 0), expected);
    }

    #[test]
    fn slot_detail_of_a_vanished_slot_is_empty() {
        // The tooltip is built after the projection lock is released, so the
        // index it is asked about may no longer exist: an absent slot (or an
        // absent snapshot) must read empty, not panic.
        let mut ctrl = controller();
        assert_eq!(slot_detail(&ctrl, 0), "");
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![ShopItem::default()]),
            now_ms: 0,
        });
        assert_eq!(slot_detail(&ctrl, 3), "");
    }
}
