//! Pure projection of the controller for the window: everything one frame
//! shows, copied under a single short lock.

use crate::domain::control::{Controller, Limits, Progress, Status};
use crate::render::{describe, format_item, kind_label, merchant_label, status_label};

/// Plain per-frame copy of everything the window shows; built under the
/// controller lock, rendered after the guard is dropped.
pub struct ViewState {
    pub status: &'static str,
    /// The raw status (with its `StopReason`), for the status color — the
    /// label above stays the source of the wording.
    pub status_kind: Status,
    pub stop_reason: Option<&'static str>,
    pub capture_on: bool,
    pub progress: Progress,
    pub limits: Limits,
    pub merchant: String,
    /// From the controller's enforced meta (debited per advised refresh,
    /// cleared on restart) — not the raw snapshot, which can be stale.
    pub crystal_balance: Option<u32>,
    /// Always known: wire meta, else the game constant.
    pub refresh_cost: u32,
    pub rows: Vec<SlotRow>,
}

/// One shop slot as the table shows it.
pub struct SlotRow {
    pub slot: u8,
    pub kind: &'static str,
    pub name: Option<String>,
    pub price: Option<u32>,
    pub sold_out: bool,
    /// Matched and still to buy: the catalog id sits in the checklist.
    pub wanted: bool,
    /// Full console line for the item, shown as the hover tooltip.
    pub detail: String,
}

/// Pure extraction: the caller holds the controller lock only for this call.
pub fn view_state(controller: &Controller, capture_on: bool) -> ViewState {
    let stop_reason = match controller.status() {
        Status::Stopped(reason) => Some(describe(reason)),
        _ => None,
    };
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
                    wanted: item.catalog_id().is_some_and(|id| checklist.contains(&id)),
                    detail: format_item(item, index),
                })
                .collect()
        })
        .unwrap_or_default();
    ViewState {
        status: status_label(controller),
        status_kind: controller.status(),
        stop_reason,
        capture_on,
        progress: controller.progress(),
        limits: controller.limits().clone(),
        merchant: merchant_label(snapshot.and_then(|snapshot| snapshot.merchant.as_deref()))
            .to_owned(),
        crystal_balance: controller.refresh_meta().map(|meta| meta.crystal_balance),
        refresh_cost: controller.refresh_cost(),
        rows,
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
        let view = view_state(&controller(), false);
        assert!(view.status.contains("idle"));
        assert_eq!(view.status_kind, Status::Idle);
        assert_eq!(view.stop_reason, None);
        assert!(!view.capture_on);
        assert!(view.rows.is_empty());
        assert_eq!(view.merchant, "Secret Shop");
        assert_eq!(view.crystal_balance, None);
        // No meta yet: the game-constant fallback, never an unknown cost.
        assert_eq!(view.refresh_cost, 3);
    }

    #[test]
    fn view_state_passes_capture_flag_through() {
        assert!(view_state(&controller(), true).capture_on);
        assert!(!view_state(&controller(), false).capture_on);
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
        ctrl.handle(Event::Snapshot {
            snapshot: shop(slots),
            now_ms: 0,
        });
        let view = view_state(&ctrl, false);
        assert_eq!(view.rows[0].slot, 5);
        assert_eq!(view.rows[1].slot, 2);
    }

    #[test]
    fn view_state_flags_checklist_rows_as_wanted() {
        let mut ctrl = controller();
        ctrl.handle(Event::Start { now_ms: 0 });
        // Default filter matches both; only the trackable id enters the
        // checklist — the id-0 sentinel row must never read as wanted.
        let slots = vec![
            ShopItem {
                id: 42,
                ..ShopItem::default()
            },
            ShopItem {
                id: 0,
                ..ShopItem::default()
            },
        ];
        ctrl.handle(Event::Snapshot {
            snapshot: shop(slots),
            now_ms: 1,
        });
        let view = view_state(&ctrl, true);
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
        ctrl.handle(Event::Snapshot {
            snapshot: shop(slots),
            now_ms: 0,
        });
        assert!(view_state(&ctrl, false).rows[0].sold_out);
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
        ctrl.handle(Event::Snapshot {
            snapshot,
            now_ms: 0,
        });
        let view = view_state(&ctrl, false);
        assert_eq!(view.merchant, "Secret Shop");
        assert_eq!(view.crystal_balance, Some(95));
        assert_eq!(view.refresh_cost, 3);
    }

    #[test]
    fn view_state_balance_survives_meta_less_snapshot() {
        // The controller keeps its enforced estimate across snapshots that
        // omit meta; the display must show that, not the raw snapshot.
        let mut ctrl = controller();
        ctrl.handle(Event::Snapshot {
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
        ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![ShopItem::default()]),
            now_ms: 1,
        });
        assert_eq!(view_state(&ctrl, false).crystal_balance, Some(95));
    }

    #[test]
    fn view_state_balance_cleared_on_restart() {
        // `Start` discards a stale balance; the display must not resurrect it
        // from the stored snapshot.
        let mut ctrl = controller();
        ctrl.handle(Event::Snapshot {
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
        ctrl.handle(Event::Start { now_ms: 1 });
        assert_eq!(view_state(&ctrl, false).crystal_balance, None);
    }

    #[test]
    fn view_state_reports_stop_reason_when_stopped() {
        let mut ctrl = controller();
        ctrl.handle(Event::Start { now_ms: 0 });
        ctrl.handle(Event::Stop);
        let view = view_state(&ctrl, false);
        assert!(view.status.contains("stopped"));
        assert_eq!(view.status_kind, Status::Stopped(StopReason::PlayerStopped));
        assert_eq!(view.stop_reason, Some("player stopped"));
    }

    #[test]
    fn view_state_detail_matches_format_item() {
        let mut ctrl = controller();
        let item = ShopItem {
            id: 7,
            slot: 3,
            name: Some("Covenant Bookmark".to_owned()),
            price: Some(184_000),
            ..ShopItem::default()
        };
        let expected = format_item(&item, 0);
        ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![item]),
            now_ms: 0,
        });
        assert_eq!(view_state(&ctrl, false).rows[0].detail, expected);
    }
}
