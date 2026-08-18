//! Pure projection of the controller for the window: everything one frame
//! shows, copied under a single short lock.
//!
//! Two things are deliberately *not* copied per frame: the slot table's hover
//! tooltip (see [`slot_detail`]) and the slot rows themselves (see
//! [`SlotRows`]). What is left in [`ViewState`] is `Copy` state and `&'static`
//! labels, so building one allocates nothing at all.

use crate::domain::control::{Controller, Limits, Progress, Status};
use crate::domain::shop::{CatalogId, ShopItem};
use crate::render::{HAUL_HEADLINERS, format_item, haul_tally, kind_label, status_summary};

/// Plain per-frame copy of everything the window shows *except* the slot rows;
/// built under the controller lock, rendered after the guard is dropped. Every
/// field is `Copy` or `&'static`, so the copy is free — the rows, which own a
/// name per slot, are cached separately in [`SlotRows`].
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
    /// Gates the welcome screen: an empty [`SlotRows`] alone must not resurrect
    /// it mid-session.
    pub has_snapshot: bool,
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

impl SlotRow {
    /// Projects one slot. The cloned name is the only allocation a frame's
    /// projection makes, which is the whole reason [`SlotRows`] gates it.
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

    /// Whether this row still describes `item` at `index` — [`SlotRows`]'s gate,
    /// and [`SlotRow::project`] mirrored with nothing allocated.
    ///
    /// `self` is destructured rather than read field by field so the two cannot
    /// drift: a field added above is projected but left out of the comparison
    /// here, and an unused binding is a warning, which CI denies. Keep the
    /// bindings in the same order as the declaration so the pairing is readable.
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
/// them moved — the generation-gated shape `ShopApp::journal_cache` already
/// uses, one surface over.
///
/// What it buys is **lock hold**, not CPU: this app repaints at 4 Hz at rest
/// (`request_repaint_after(250ms)`), so a six-row table's worth of `String`s is
/// nothing in itself. But the projection runs inside the controller lock the
/// session loop takes to turn a captured shop into a click job, hover is input,
/// and a pointer resting on the table lifts the repaint rate to the display's.
/// Rebuilding on change instead of on redraw takes the allocator out of that
/// hold entirely — in the steady state the frame's whole lock hold is now
/// comparisons and `Copy` reads.
///
/// The gate is a field-by-field comparison rather than a generation counter or
/// an `Arc::ptr_eq`, because [`Controller::last_snapshot`] hands out
/// `Option<&ShopSnapshot>` over a snapshot stored *inline*: a replacement lands
/// at the same address, so from here there is nothing cheaper that is also
/// correct. Comparing allocates nothing, which is the property that matters
/// under the lock. If the domain ever holds the snapshot behind an `Arc`
/// (own-002's first step, which this file cannot reach), this collapses to one
/// pointer test and the comparison goes away.
#[derive(Default)]
pub(super) struct SlotRows(Vec<SlotRow>);

impl SlotRows {
    /// The cached rows, rendered after the controller guard is dropped.
    pub(super) fn rows(&self) -> &[SlotRow] {
        &self.0
    }

    /// Brings the cache up to date, re-deriving only when the projection
    /// actually moved. Called under the controller lock, beside [`view_state`].
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

    /// Whether every cached row still describes its slot, and there are exactly
    /// as many rows as slots. An absent snapshot projects to no rows, so it is
    /// the empty slice here rather than a case of its own.
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
/// Allocation-free — the rows it used to build live in [`SlotRows`], which the
/// caller syncs inside the same hold.
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

    /// A freshly synced row cache, the way one frame builds it.
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
        let rows = rows(&ctrl);
        assert_eq!(rows.rows()[0].slot, 5);
        assert_eq!(rows.rows()[1].slot, 2);
    }

    #[test]
    fn slot_rows_flag_checklist_rows_as_wanted() {
        let mut ctrl = controller();
        let _ = ctrl.handle(Event::Start { now_ms: 0 });
        // Default filter matches both; only the trackable id enters the
        // checklist — the id-0 sentinel row must never read as wanted.
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
            item: CatalogId::new(42),
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
            id: CatalogId::new(7),
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

    /// The point of the cache, asserted the only way a passing suite cannot fake:
    /// a second sync over an unchanged controller must not build a new `Vec`.
    ///
    /// Capacity is the witness. `sync` fills the cache by `collect`ing an
    /// iterator of known length, so a rebuilt vector's capacity is its length;
    /// growing the live one first makes the two states tell themselves apart
    /// without depending on allocator addresses (a freed buffer can be handed
    /// back at the same address, so `as_ptr` would prove nothing).
    #[test]
    fn slot_rows_are_not_re_derived_while_the_shop_and_the_checklist_hold() {
        let mut ctrl = controller();
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![ShopItem {
                name: Some("Covenant Bookmark".to_owned()),
                price: Some(184_000),
                ..ShopItem::default()
            }]),
            now_ms: 0,
        });
        let mut rows = SlotRows::default();
        rows.sync(&ctrl);
        assert_eq!(rows.rows().len(), 1);
        rows.0.reserve(64);
        let capacity = rows.0.capacity();

        // Ten frames' worth of repaints with nothing new to show.
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

        // A new roll at the same slot: the snapshot is stored inline, so this
        // lands at the same address the old one had — the gate must notice
        // anyway, which is why it compares fields rather than pointers.
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
        // `wanted` is not in the snapshot: a confirmed buy checks the item off
        // while the shop on screen stays byte-for-byte identical. Keying the
        // cache on the shop alone would leave the row green after the purchase.
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
