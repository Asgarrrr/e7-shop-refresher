//! egui window over the session: copies the controller state once per frame,
//! renders it, and pushes player commands through the existing channel. No
//! egui type crosses into `app.rs` or the domain.

use crate::app::{describe, format_item, kind_label, status_label};
use crate::domain::control::{Controller, Limits, Progress, Status};

/// Plain per-frame copy of everything the window shows; built under the
/// controller lock, rendered after the guard is dropped.
pub struct ViewState {
    pub status: &'static str,
    pub stop_reason: Option<&'static str>,
    pub capture_on: bool,
    pub progress: Progress,
    pub limits: Limits,
    pub merchant: Option<String>,
    pub crystal_balance: Option<u32>,
    pub refresh_cost: Option<u32>,
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
                    detail: format_item(item),
                })
                .collect()
        })
        .unwrap_or_default();
    let refresh = snapshot.and_then(|snapshot| snapshot.refresh);
    ViewState {
        status: status_label(controller),
        stop_reason,
        capture_on,
        progress: controller.progress(),
        limits: controller.limits().clone(),
        merchant: snapshot.and_then(|snapshot| snapshot.merchant.clone()),
        crystal_balance: refresh.map(|meta| meta.crystal_balance),
        refresh_cost: refresh.map(|meta| meta.cost),
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::control::Event;
    use crate::domain::filter::Filter;
    use crate::domain::shop::{PurchaseLimit, RefreshMeta, ShopItem, ShopSnapshot};

    fn controller() -> Controller {
        Controller::new(Filter::default(), Limits::default())
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
        assert_eq!(view.stop_reason, None);
        assert!(!view.capture_on);
        assert!(view.rows.is_empty());
        assert_eq!(view.merchant, None);
        assert_eq!(view.crystal_balance, None);
        assert_eq!(view.refresh_cost, None);

        assert!(view_state(&controller(), true).capture_on);
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
        assert_eq!(view.merchant.as_deref(), Some("Secret Shop"));
        assert_eq!(view.crystal_balance, Some(95));
        assert_eq!(view.refresh_cost, Some(3));
    }

    #[test]
    fn view_state_reports_stop_reason_when_stopped() {
        let mut ctrl = controller();
        ctrl.handle(Event::Start { now_ms: 0 });
        ctrl.handle(Event::Stop);
        let view = view_state(&ctrl, false);
        assert!(view.status.contains("stopped"));
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
        let expected = format_item(&item);
        ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![item]),
            now_ms: 0,
        });
        assert_eq!(view_state(&ctrl, false).rows[0].detail, expected);
    }
}
