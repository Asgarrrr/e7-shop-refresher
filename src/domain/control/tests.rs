//! Behavioural specification for the refresh-loop controller: what each
//! [`Event`] does to the state machine, and which [`Action`]s come back.
//! Ordered roughly as the loop runs.
//!
//! Time is a plain `now_ms` argument, so every deadline here is exact and
//! nothing sleeps.

use super::*;
use crate::domain::shop::ItemKind::{self, Equipment, Token};
use crate::domain::shop::{CatalogId, Crystals, Gold, PurchaseLimit, ShopItem};

/// A fixture crystal amount, so a budget assertion still reads as one line.
const fn xtl(raw: u32) -> Crystals {
    Crystals::new(raw)
}

/// A fixture gold amount. Separate from [`xtl`] on purpose: a test that means
/// gold has to say gold, the property under test in half this file.
const fn gold(raw: u32) -> Gold {
    Gold::new(raw)
}

fn item(slot: u8, kind: ItemKind) -> ShopItem {
    ShopItem {
        slot,
        kind,
        ..ShopItem::default()
    }
}

fn shop(kinds: &[ItemKind], refresh: Option<RefreshMeta>) -> ShopSnapshot {
    ShopSnapshot {
        merchant: None,
        slots: kinds
            .iter()
            .enumerate()
            .map(|(index, &kind)| item(index as u8 + 1, kind))
            .collect(),
        refresh,
    }
}

/// Six slots, none matching the equipment filter.
fn dud_shop(refresh: Option<RefreshMeta>) -> ShopSnapshot {
    shop(&[Token; 6], refresh)
}

/// Slot 3 matches the equipment filter.
fn hit_shop(refresh: Option<RefreshMeta>) -> ShopSnapshot {
    shop(&[Token, Token, Equipment, Token, Token, Token], refresh)
}

fn meta(crystal_balance: u32, cost: u32) -> RefreshMeta {
    RefreshMeta {
        crystal_balance: xtl(crystal_balance),
        cost: xtl(cost),
    }
}

/// A fixture catalog id. Panics on `0`, which [`CatalogId`] makes unspellable.
fn cid(id: u32) -> CatalogId {
    CatalogId::new(id).expect("a fixture catalog id is never zero")
}

/// Assigns stable non-zero catalog ids (`100 + index`) so dedup and the
/// checklist engage; `hit_shop`'s matching slot 3 gets id 102.
fn with_ids(mut snapshot: ShopSnapshot) -> ShopSnapshot {
    for (index, item) in snapshot.slots.iter_mut().enumerate() {
        item.id = Some(cid(100 + index as u32));
    }
    snapshot
}

fn snap(snapshot: ShopSnapshot, now_ms: u64) -> Event {
    Event::Snapshot { snapshot, now_ms }
}

fn target(slot: u8, id: Option<u32>) -> BuyTarget {
    BuyTarget {
        slot,
        id: id.map(cid),
    }
}

/// A purchase echo naming `item`; the idless case has its own fixture below.
fn buy(item: u32, now_ms: u64) -> Event {
    Event::Purchase {
        item: Some(cid(item)),
        gold: None,
        now_ms,
    }
}

/// A purchase echo the server sent no id with.
fn buy_unidentified(now_ms: u64) -> Event {
    Event::Purchase {
        item: None,
        gold: None,
        now_ms,
    }
}

fn buy_with_gold(item: u32, balance: u32, now_ms: u64) -> Event {
    Event::Purchase {
        item: Some(cid(item)),
        gold: Some(gold(balance)),
        now_ms,
    }
}

fn tick(now_ms: u64) -> Event {
    Event::Tick { now_ms }
}

fn controller(limits: Limits) -> Controller {
    let filter = Filter {
        kinds: vec![Equipment],
        ..Filter::default()
    };
    Controller::new(filter, limits)
}

/// A controller already started at t=0.
fn started(limits: Limits) -> Controller {
    let mut ctrl = controller(limits);
    assert!(ctrl.handle(Event::Start { now_ms: 0 }).is_empty());
    ctrl
}

/// A controller started at t=0 with the recovery watchdog armed.
fn recovering(limits: Limits) -> Controller {
    let mut ctrl = controller(limits);
    ctrl.set_recovery(true);
    assert!(ctrl.handle(Event::Start { now_ms: 0 }).is_empty());
    ctrl
}

#[test]
fn start_arms_watching_without_refresh() {
    let mut ctrl = controller(Limits::default());
    let actions = ctrl.handle(Event::Start { now_ms: 100 });
    assert!(actions.is_empty());
    assert_eq!(ctrl.status(), Status::Watching);
    assert_eq!(ctrl.progress().refreshes, 0);
}

#[test]
fn start_refused_while_filter_unrestricted() {
    // The invariant lives here, not in the callers.
    let mut ctrl = Controller::new(Filter::default(), Limits::default());
    assert_eq!(
        ctrl.handle(Event::Start { now_ms: 0 }),
        vec![Action::Refused(RefusalReason::UnrestrictedFilter)]
    );
    assert_eq!(ctrl.status(), Status::Idle);
}

#[test]
fn unrestricted_filter_swap_is_ignored() {
    let mut ctrl = started(Limits::default());
    assert_eq!(
        ctrl.handle(Event::FilterChanged(Filter::default())),
        vec![Action::Refused(RefusalReason::UnrestrictedFilter)]
    );
    assert!(!ctrl.filter().is_unrestricted());
    // The old criteria keep hunting: slot 3 still matches.
    let actions = ctrl.handle(snap(hit_shop(None), 1));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(3, None)]
        }],
        "the equipment filter must still be active"
    );
}

#[test]
fn is_refusal_flags_only_the_refused_verdict() {
    // The app reads acceptance from this verdict, not the action count: an
    // empty list contains no refusal, so the event applied.
    assert!(Action::Refused(RefusalReason::UnrestrictedFilter).is_refusal());
    assert!(!Action::Refresh.is_refusal());
    assert!(
        !Action::Buy {
            targets: vec![target(1, None)]
        }
        .is_refusal()
    );
    assert!(!Action::Halt(StopReason::PlayerStopped).is_refusal());
}

#[test]
fn first_snapshot_no_match_emits_single_refresh() {
    let mut ctrl = started(Limits::default());
    let actions = ctrl.handle(snap(dud_shop(Some(meta(100, 3))), 1));
    assert_eq!(actions, vec![Action::Refresh]);
    assert_eq!(ctrl.progress().refreshes, 1);
    assert_eq!(ctrl.progress().spent, xtl(3));
}

#[test]
fn limits_changed_applies_to_next_check() {
    let mut ctrl = started(Limits::default());
    assert_eq!(ctrl.handle(snap(dud_shop(None), 1)), vec![Action::Refresh]);
    let tightened = Limits {
        max_refreshes: Some(1),
        ..Limits::default()
    };
    assert!(ctrl.handle(Event::LimitsChanged(tightened)).is_empty());
    let actions = ctrl.handle(snap(dud_shop(None), 2));
    assert_eq!(actions, vec![Action::Halt(StopReason::MaxRefreshes)]);
    assert_eq!(ctrl.status(), Status::Stopped(StopReason::MaxRefreshes));
}

#[test]
fn limits_changed_never_halts_immediately() {
    let mut ctrl = started(Limits::default());
    assert_eq!(ctrl.handle(snap(dud_shop(None), 1)), vec![Action::Refresh]);
    // Already over the new ceiling, yet nothing happens until the next
    // check-point.
    let tightened = Limits {
        max_refreshes: Some(1),
        ..Limits::default()
    };
    assert!(ctrl.handle(Event::LimitsChanged(tightened)).is_empty());
    assert_eq!(ctrl.status(), Status::Watching);
}

#[test]
fn tick_enforces_a_tightened_count_limit_while_watching() {
    let mut ctrl = started(Limits::default());
    // One refresh done, then the player tightens the ceiling below it.
    assert_eq!(ctrl.handle(snap(dud_shop(None), 1)), vec![Action::Refresh]);
    let _ = ctrl.handle(Event::LimitsChanged(Limits {
        max_refreshes: Some(1),
        ..Limits::default()
    }));
    assert_eq!(ctrl.status(), Status::Watching);
    // No new shop will come to carry the gate: the tick has to.
    let actions = ctrl.handle(Event::Tick { now_ms: 2 });
    assert_eq!(actions, vec![Action::Halt(StopReason::MaxRefreshes)]);
    assert_eq!(ctrl.status(), Status::Stopped(StopReason::MaxRefreshes));
}

#[test]
fn tick_while_paused_ignores_non_timeout_limits() {
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.status(), Status::Paused);
    // Tightening a count limit must not abandon a buyable pause at a tick;
    // the purchase/next-shop path re-checks it.
    let _ = ctrl.handle(Event::LimitsChanged(Limits {
        max_refreshes: Some(0),
        ..Limits::default()
    }));
    assert!(ctrl.handle(Event::Tick { now_ms: 2 }).is_empty());
    assert_eq!(ctrl.status(), Status::Paused);
}

#[test]
fn snapshot_match_pauses_without_refresh() {
    let mut ctrl = started(Limits::default());
    let actions = ctrl.handle(snap(hit_shop(None), 1));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(3, None)]
        }]
    );
    assert_eq!(ctrl.status(), Status::Paused);
    assert_eq!(ctrl.progress().refreshes, 0);
}

#[test]
fn auto_resume_refresh_is_counted() {
    let mut ctrl = started(Limits {
        max_refreshes: Some(2),
        ..Limits::default()
    });
    // Distinct ids per round: identical re-sends would be deduped.
    for round in 0..3u32 {
        let mut shop = with_ids(hit_shop(None));
        for item in &mut shop.slots {
            item.id = item.id.map(|id| cid(id.get() + round * 100));
        }
        let now = u64::from(round) * 2;
        let _ = ctrl.handle(snap(shop, now + 1));
        let actions = ctrl.handle(buy(102 + round * 100, now + 2));
        if round < 2 {
            assert_eq!(actions, vec![Action::Refresh]);
        } else {
            // The third one must hit the limit, not silently overshoot.
            assert_eq!(actions, vec![Action::Halt(StopReason::MaxRefreshes)]);
        }
    }
    assert_eq!(ctrl.progress().refreshes, 2);
}

#[test]
fn purchase_clears_one_item_stays_paused() {
    let mut ctrl = started(Limits::default());
    let two_hits = with_ids(shop(
        &[Equipment, Token, Equipment, Token, Token, Token],
        None,
    ));
    let _ = ctrl.handle(snap(two_hits, 1));
    assert_eq!(ctrl.status(), Status::Paused);
    assert!(ctrl.handle(buy(100, 2)).is_empty());
    assert_eq!(ctrl.status(), Status::Paused);
    assert_eq!(ctrl.checklist(), &[cid(102)]);
}

#[test]
fn last_purchase_auto_resumes_with_refresh() {
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.status(), Status::Paused);
    let actions = ctrl.handle(buy(102, 2));
    assert_eq!(actions, vec![Action::Refresh]);
    assert_eq!(ctrl.status(), Status::Watching);
    assert_eq!(ctrl.progress().refreshes, 1);
}

#[test]
fn auto_resume_respects_limits() {
    let mut ctrl = started(Limits {
        max_refreshes: Some(0),
        ..Limits::default()
    });
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    let actions = ctrl.handle(buy(102, 2));
    assert_eq!(actions, vec![Action::Halt(StopReason::MaxRefreshes)]);
    assert_eq!(ctrl.progress().refreshes, 0);
}

#[test]
fn stop_clears_the_checklist() {
    // Ids are stable per item, so a checklist outliving the hunt would keep
    // flagging yesterday's matches as "wanted" in the view.
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.checklist(), &[cid(102)]);
    let _ = ctrl.handle(Event::Stop);
    assert!(ctrl.checklist().is_empty());
}

#[test]
fn max_matches_halts_after_the_goal_item_is_bought() {
    let mut ctrl = started(Limits {
        max_matches: Some(1),
        ..Limits::default()
    });
    // The match trips max_matches, but the found item is the point: buy first.
    let actions = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(3, Some(102))]
        }]
    );
    assert_eq!(ctrl.status(), Status::Paused);
    let actions = ctrl.handle(buy(102, 2));
    assert_eq!(actions, vec![Action::Halt(StopReason::MaxMatches)]);
    assert!(ctrl.checklist().is_empty());
}

#[test]
fn purchase_of_unknown_id_ignored() {
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert!(ctrl.handle(buy(999, 2)).is_empty());
    assert_eq!(ctrl.status(), Status::Paused);
    assert_eq!(ctrl.checklist(), &[cid(102)]);
}

#[test]
fn purchase_ignored_unless_paused() {
    let mut watching = started(Limits::default());
    assert!(watching.handle(buy(102, 1)).is_empty());
    assert_eq!(watching.status(), Status::Watching);
    assert_eq!(watching.progress().refreshes, 0);

    let mut idle = controller(Limits::default());
    assert!(idle.handle(buy(102, 1)).is_empty());
    assert_eq!(idle.status(), Status::Idle);
}

#[test]
fn replayed_echo_of_consumed_purchase_is_ignored() {
    let mut ctrl = started(Limits::default());
    let two_hits = with_ids(shop(
        &[Equipment, Token, Equipment, Token, Token, Token],
        None,
    ));
    let _ = ctrl.handle(snap(two_hits, 1));
    assert!(ctrl.handle(buy(100, 2)).is_empty());
    // The replay must not stand in for the remaining item's buy.
    assert!(ctrl.handle(buy(100, 3)).is_empty());
    assert_eq!(ctrl.status(), Status::Paused);
    assert_eq!(ctrl.checklist(), &[cid(102)]);
}

#[test]
fn an_idless_match_pauses_until_a_new_shop() {
    let mut ctrl = started(Limits::default());
    // The bare fixture leaves every id absent, so no echo can name the match.
    let _ = ctrl.handle(snap(hit_shop(None), 1));
    assert_eq!(ctrl.status(), Status::Paused);
    assert!(ctrl.checklist().is_empty());
    // No echo can clear an untrackable match; only a new shop unpauses.
    assert!(ctrl.handle(buy_unidentified(2)).is_empty());
    assert_eq!(ctrl.status(), Status::Paused);
    let actions = ctrl.handle(snap(with_ids(dud_shop(None)), 3));
    assert_eq!(actions, vec![Action::Refresh]);
    assert_eq!(ctrl.status(), Status::Watching);
}

#[test]
fn duplicate_snapshot_while_armed_emits_nothing() {
    let mut ctrl = started(Limits::default());
    assert_eq!(
        ctrl.handle(snap(with_ids(dud_shop(None)), 1)),
        vec![Action::Refresh]
    );
    let mut resend = with_ids(dud_shop(None));
    resend.merchant = Some("resend".to_owned());
    assert!(ctrl.handle(snap(resend, 2)).is_empty());
    assert_eq!(ctrl.progress().refreshes, 1);
    // Still stored for the view.
    assert_eq!(
        ctrl.last_snapshot().unwrap().merchant.as_deref(),
        Some("resend")
    );
}

#[test]
fn duplicate_snapshot_still_absorbs_refresh_meta() {
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    // The deduped re-send still carries truth: a balance too low to
    // refresh must gate the auto-resume that follows.
    let actions = ctrl.handle(snap(with_ids(hit_shop(Some(meta(2, 3)))), 2));
    assert!(actions.is_empty()); // deduped
    let actions = ctrl.handle(buy(102, 3));
    assert_eq!(actions, vec![Action::Halt(StopReason::OutOfFunds)]);
}

#[test]
fn duplicate_hit_shop_while_paused_does_not_rematch() {
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.progress().matches_found, 1);
    assert!(ctrl.handle(snap(with_ids(hit_shop(None)), 2)).is_empty());
    assert_eq!(ctrl.progress().matches_found, 1);
    assert_eq!(ctrl.checklist(), &[cid(102)]);
    assert_eq!(ctrl.status(), Status::Paused);
}

#[test]
fn reopened_shop_after_buy_does_not_double_refresh() {
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.handle(buy(102, 2)), vec![Action::Refresh]);
    // A re-open re-delivers the roll with the bought slot decremented: same
    // identity (`limit` is excluded), so the surviving fingerprint mutes it.
    let mut reopened = with_ids(hit_shop(None));
    reopened.slots[2].limit = Some(PurchaseLimit {
        remaining: 0,
        total: 1,
    });
    assert!(ctrl.handle(snap(reopened, 3)).is_empty());
    assert_eq!(ctrl.progress().refreshes, 1);
    // Muted by dedup before the bought set is even consulted: no re-pause,
    // no rebuilt checklist.
    assert_eq!(ctrl.status(), Status::Watching);
    assert!(ctrl.checklist().is_empty());
}

#[test]
fn restart_reopen_does_not_rebuy_a_bought_item() {
    // The wire never says sold-out and `Start` clears the dedup identity, so
    // only the roll-scoped `bought` set stops the re-click.
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.handle(buy(102, 2)), vec![Action::Refresh]);
    let _ = ctrl.handle(Event::Stop);
    let _ = ctrl.handle(Event::Start { now_ms: 3 });
    // The match still shows, but the bought slot is dead stock: display-only.
    let actions = ctrl.handle(snap(with_ids(hit_shop(None)), 4));
    assert_eq!(
        actions,
        vec![
            Action::Buy {
                targets: vec![target(3, None)]
            },
            Action::Refresh,
        ]
    );
    assert_eq!(ctrl.status(), Status::Watching);
    assert!(ctrl.checklist().is_empty());
}

#[test]
fn restart_reopen_keeps_unbought_match_clickable() {
    let mut ctrl = started(Limits::default());
    let two_hits = with_ids(shop(
        &[Equipment, Token, Equipment, Token, Token, Token],
        None,
    ));
    let _ = ctrl.handle(snap(two_hits.clone(), 1)); // checklist [100, 102]
    assert!(ctrl.handle(buy(100, 2)).is_empty());
    let _ = ctrl.handle(Event::Stop);
    let _ = ctrl.handle(Event::Start { now_ms: 3 });
    // Only the bought slot is excluded; the other match stays clickable.
    let actions = ctrl.handle(snap(two_hits, 4));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(1, None), target(3, Some(102))]
        }]
    );
    assert_eq!(ctrl.status(), Status::Paused);
    assert_eq!(ctrl.checklist(), &[cid(102)]);
}

#[test]
fn new_roll_makes_a_bought_id_buyable_again() {
    // Ids are stable per item type, so a new roll relisting one is fresh stock.
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.handle(buy(102, 2)), vec![Action::Refresh]);
    // Same catalog ids, changed per-roll field: a re-roll, not a re-open.
    let mut reroll = with_ids(hit_shop(None));
    reroll.slots[0].price = Some(gold(120_000));
    let actions = ctrl.handle(snap(reroll, 3));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(3, Some(102))]
        }]
    );
    assert_eq!(ctrl.status(), Status::Paused);
    assert_eq!(ctrl.checklist(), &[cid(102)]);
}

#[test]
fn an_idless_echo_never_enters_the_bought_set() {
    // `bought` is the re-buy guard: an entry nothing can match only grows it.
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(hit_shop(None), 1)); // untrackable match: Paused
    assert!(ctrl.handle(buy_unidentified(2)).is_empty());
    assert!(ctrl.bought.is_empty());
}

#[test]
fn fail_open_reevaluation_does_not_rebuy() {
    // An unidentifiable snapshot re-evaluates while Watching, but must neither
    // reset `bought` nor re-click a bought slot.
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.handle(buy(102, 2)), vec![Action::Refresh]);
    let mut holed = with_ids(hit_shop(None));
    holed.slots[5].id = None; // fingerprint gone: fail-open re-evaluation
    let actions = ctrl.handle(snap(holed, 3));
    assert_eq!(
        actions,
        vec![
            Action::Buy {
                targets: vec![target(3, None)]
            },
            Action::Refresh,
        ]
    );
    assert_eq!(ctrl.progress().refreshes, 2);
}

#[test]
fn an_idless_slot_disables_dedup() {
    let mut ctrl = started(Limits::default());
    let mut holed = with_ids(dud_shop(None));
    holed.slots[5].id = None;
    assert_eq!(ctrl.handle(snap(holed.clone(), 1)), vec![Action::Refresh]);
    // No usable identity: the identical re-send evaluates again (fail open).
    assert_eq!(ctrl.handle(snap(holed, 2)), vec![Action::Refresh]);
    assert_eq!(ctrl.progress().refreshes, 2);
}

#[test]
fn new_shop_while_paused_no_match_refreshes() {
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.status(), Status::Paused);
    // Hourly auto-refresh replaced the shop: different ids, no match.
    let mut fresh = with_ids(dud_shop(None));
    for item in &mut fresh.slots {
        item.id = item.id.map(|id| cid(id.get() + 100));
    }
    let actions = ctrl.handle(snap(fresh, 2));
    assert_eq!(actions, vec![Action::Refresh]);
    assert_eq!(ctrl.status(), Status::Watching);
}

#[test]
fn new_shop_while_paused_rebuilds_checklist() {
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1)); // checklist [102]
    let mut fresh = with_ids(hit_shop(None));
    for item in &mut fresh.slots {
        item.id = item.id.map(|id| cid(id.get() + 100)); // new shop, matching slot now id 202
    }
    let actions = ctrl.handle(snap(fresh, 2));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(3, Some(202))]
        }]
    );
    assert_eq!(ctrl.checklist(), &[cid(202)]);
    // The stale id is gone; only the new one clears the pause.
    assert!(ctrl.handle(buy(102, 3)).is_empty());
    assert_eq!(ctrl.status(), Status::Paused);
    assert_eq!(ctrl.handle(buy(202, 4)), vec![Action::Refresh]);
    assert_eq!(ctrl.status(), Status::Watching);
}

#[test]
fn pre_start_snapshot_then_identical_after_start_is_evaluated() {
    let mut ctrl = controller(Limits::default());
    assert!(ctrl.handle(snap(with_ids(dud_shop(None)), 1)).is_empty());
    let _ = ctrl.handle(Event::Start { now_ms: 2 });
    let actions = ctrl.handle(snap(with_ids(dud_shop(None)), 3));
    assert_eq!(actions, vec![Action::Refresh]);
}

#[test]
fn restart_evaluates_same_shop_again() {
    let mut ctrl = started(Limits::default());
    assert_eq!(
        ctrl.handle(snap(with_ids(dud_shop(None)), 1)),
        vec![Action::Refresh]
    );
    let _ = ctrl.handle(Event::Stop);
    let _ = ctrl.handle(Event::Start { now_ms: 2 });
    // `Start` cleared the fingerprint: the same shop opens the new session.
    let actions = ctrl.handle(snap(with_ids(dud_shop(None)), 3));
    assert_eq!(actions, vec![Action::Refresh]);
}

fn empty_shop() -> ShopSnapshot {
    ShopSnapshot {
        merchant: None,
        slots: Vec::new(),
        refresh: None,
    }
}

#[test]
fn empty_snapshot_while_paused_is_stored_not_evaluated() {
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.status(), Status::Paused);
    // A degraded slotless message must not wipe the pending checklist.
    assert!(ctrl.handle(snap(empty_shop(), 2)).is_empty());
    assert_eq!(ctrl.status(), Status::Paused);
    assert_eq!(ctrl.checklist(), &[cid(102)]);
    // Still stored for the view.
    assert!(ctrl.last_snapshot().unwrap().slots.is_empty());
}

#[test]
fn empty_snapshot_never_advises_refresh() {
    let mut ctrl = started(Limits::default());
    assert!(ctrl.handle(snap(empty_shop(), 1)).is_empty());
    assert_eq!(ctrl.progress().refreshes, 0);
}

#[test]
fn unidentifiable_snapshot_while_paused_stored_only() {
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.status(), Status::Paused);
    // All ids omitted: a duplicate cannot be told from a new shop, so
    // nothing may be re-evaluated over the pending purchase.
    assert!(ctrl.handle(snap(dud_shop(None), 2)).is_empty());
    assert_eq!(ctrl.status(), Status::Paused);
    assert_eq!(ctrl.checklist(), &[cid(102)]);
    assert_eq!(ctrl.progress().refreshes, 0);
}

#[test]
fn same_ids_new_roll_is_evaluated() {
    let mut ctrl = started(Limits::default());
    assert_eq!(
        ctrl.handle(snap(with_ids(dud_shop(None)), 1)),
        vec![Action::Refresh]
    );
    // A paid re-roll can redraw the same catalog ids: a changed per-roll
    // field (here the price) makes it a new shop, not a duplicate.
    let mut reroll = with_ids(dud_shop(None));
    reroll.slots[0].price = Some(gold(120_000));
    assert_eq!(ctrl.handle(snap(reroll, 2)), vec![Action::Refresh]);
    assert_eq!(ctrl.progress().refreshes, 2);
}

#[test]
fn fail_open_snapshot_keeps_last_identity() {
    let mut ctrl = started(Limits::default());
    assert_eq!(
        ctrl.handle(snap(with_ids(dud_shop(None)), 1)),
        vec![Action::Refresh]
    );
    // An unidentifiable shop evaluates (fail open) but must not erase
    // the remembered identity...
    let mut holed = with_ids(dud_shop(None));
    holed.slots[5].id = None;
    assert_eq!(ctrl.handle(snap(holed, 2)), vec![Action::Refresh]);
    // ...so a stale verbatim duplicate of the first shop is still muted.
    assert!(ctrl.handle(snap(with_ids(dud_shop(None)), 3)).is_empty());
    assert_eq!(ctrl.progress().refreshes, 2);
}

#[test]
fn sold_out_only_match_keeps_hunting() {
    // Nobody can buy a sold-out slot, so pausing would park the loop until the
    // hourly rotation: show the match and hunt on, in the same batch.
    let filter = Filter {
        kinds: vec![Equipment],
        include_sold_out: true,
        ..Filter::default()
    };
    let mut ctrl = Controller::new(filter, Limits::default());
    let _ = ctrl.handle(Event::Start { now_ms: 0 });
    let mut shop = with_ids(hit_shop(None));
    shop.slots[2].limit = Some(PurchaseLimit {
        remaining: 0,
        total: 1,
    });
    let actions = ctrl.handle(snap(shop, 1));
    assert_eq!(
        actions,
        vec![
            Action::Buy {
                targets: vec![target(3, None)]
            },
            Action::Refresh,
        ]
    );
    assert_eq!(ctrl.status(), Status::Watching);
    assert!(ctrl.checklist().is_empty());
}

#[test]
fn untrackable_but_in_stock_match_still_pauses() {
    // One match is sold out, the other idless but in stock: the player can
    // still buy that one, so the manual-flow pause stays.
    let filter = Filter {
        kinds: vec![Equipment],
        include_sold_out: true,
        ..Filter::default()
    };
    let mut ctrl = Controller::new(filter, Limits::default());
    let _ = ctrl.handle(Event::Start { now_ms: 0 });
    let mut two_hits = shop(&[Equipment, Equipment, Token, Token, Token, Token], None);
    two_hits.slots[0].id = Some(cid(100));
    two_hits.slots[0].limit = Some(PurchaseLimit {
        remaining: 0,
        total: 1,
    });
    let actions = ctrl.handle(snap(two_hits, 1));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(1, None), target(2, None)]
        }]
    );
    assert_eq!(ctrl.status(), Status::Paused);
}

#[test]
fn echoed_gold_blocks_unaffordable_next_match() {
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.status(), Status::Paused);
    assert_eq!(
        ctrl.handle(buy_with_gold(102, 100_000, 2)),
        vec![Action::Refresh]
    );
    // 100k echoed, next match priced 184k: shown, never clicked, hunt continues.
    let mut pricey = with_ids(hit_shop(None));
    for item in &mut pricey.slots {
        item.id = item.id.map(|id| cid(id.get() + 100));
    }
    pricey.slots[2].price = Some(gold(184_000));
    let actions = ctrl.handle(snap(pricey, 3));
    assert_eq!(
        actions,
        vec![
            Action::Buy {
                targets: vec![target(3, None)]
            },
            Action::Refresh,
        ]
    );
    assert_eq!(ctrl.status(), Status::Watching);
    assert!(ctrl.checklist().is_empty());
}

#[test]
fn gold_debits_cumulatively_within_one_shop() {
    // 200k on hand, two 184k matches: the first buy spends the purse.
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    let _ = ctrl.handle(buy_with_gold(102, 200_000, 2));
    let mut twins = with_ids(shop(
        &[Equipment, Equipment, Token, Token, Token, Token],
        None,
    ));
    for item in &mut twins.slots {
        item.id = item.id.map(|id| cid(id.get() + 100));
    }
    twins.slots[0].price = Some(gold(184_000));
    twins.slots[1].price = Some(gold(184_000));
    let actions = ctrl.handle(snap(twins, 3));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(1, Some(200)), target(2, None)]
        }]
    );
    assert_eq!(ctrl.checklist(), &[cid(200)]);
    assert_eq!(ctrl.status(), Status::Paused);
}

#[test]
fn unknown_gold_restricts_nothing() {
    // No echo yet: the estimate fails open, never vetoing on ignorance.
    let mut ctrl = started(Limits::default());
    let mut priced = with_ids(hit_shop(None));
    priced.slots[2].price = Some(gold(999_999_999));
    let actions = ctrl.handle(snap(priced, 1));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(3, Some(102))]
        }]
    );
    assert_eq!(ctrl.status(), Status::Paused);
}

/// Why [`Gold`](crate::domain::shop::Gold) does not fold `0` to `None` the way
/// [`CatalogId`] does; `unknown_gold_restricts_nothing` pins the other half.
/// Collapsed, a broke player's shop would read as fully buyable.
#[test]
fn a_zero_gold_balance_is_known_and_vetoes_every_priced_match() {
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    // The buy drains the purse, and the echoed zero is a fact.
    let _ = ctrl.handle(buy_with_gold(102, 0, 2));
    assert_eq!(ctrl.gold_balance(), Some(gold(0)));

    let mut priced = with_ids(hit_shop(None));
    for item in &mut priced.slots {
        item.id = item.id.map(|id| cid(id.get() + 100));
    }
    priced.slots[2].price = Some(gold(1));
    let actions = ctrl.handle(snap(priced, 3));
    // Shown, never clicked, hunt goes on — as for any unaffordable match.
    assert_eq!(
        actions,
        vec![
            Action::Buy {
                targets: vec![target(3, None)]
            },
            Action::Refresh,
        ]
    );
    assert!(ctrl.checklist().is_empty());
}

#[test]
fn unknown_price_with_known_gold_fails_open() {
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    let _ = ctrl.handle(buy_with_gold(102, 10, 2)); // 10 gold left, known
    // The next match omits its price, so it cannot be proven unaffordable.
    let mut fresh = with_ids(hit_shop(None));
    for item in &mut fresh.slots {
        item.id = item.id.map(|id| cid(id.get() + 100));
    }
    let actions = ctrl.handle(snap(fresh, 3));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(3, Some(202))]
        }]
    );
    assert_eq!(ctrl.status(), Status::Paused);
}

#[test]
fn start_forgets_the_gold_estimate() {
    // Stale gold from the previous session must not veto the new one's buys.
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    let _ = ctrl.handle(buy_with_gold(102, 10, 2));
    let _ = ctrl.handle(Event::Stop);
    let _ = ctrl.handle(Event::Start { now_ms: 3 });
    let mut pricey = with_ids(hit_shop(None));
    pricey.slots[2].price = Some(gold(184_000));
    let actions = ctrl.handle(snap(pricey, 4));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(3, Some(102))]
        }]
    );
    assert_eq!(ctrl.status(), Status::Paused);
}

#[test]
fn snapshot_before_start_ignored() {
    let mut ctrl = controller(Limits::default());
    let actions = ctrl.handle(snap(hit_shop(None), 1));
    assert!(actions.is_empty());
    assert_eq!(ctrl.status(), Status::Idle);
    assert!(ctrl.last_snapshot().is_some());
}

#[test]
fn start_while_watching_is_ignored() {
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(dud_shop(Some(meta(100, 3))), 1));
    let actions = ctrl.handle(Event::Start { now_ms: 50 });
    assert!(actions.is_empty());
    assert_eq!(ctrl.status(), Status::Watching);
    assert_eq!(ctrl.progress().refreshes, 1); // counters survived
}

#[test]
fn stop_is_idempotent_when_stopped() {
    let mut ctrl = started(Limits::default());
    let actions = ctrl.handle(Event::Stop);
    assert_eq!(actions, vec![Action::Halt(StopReason::PlayerStopped)]);
    assert!(ctrl.handle(Event::Stop).is_empty());

    // An earlier reason is not relabelled to PlayerStopped.
    let mut ctrl = started(Limits {
        max_refreshes: Some(0),
        ..Limits::default()
    });
    let _ = ctrl.handle(snap(dud_shop(None), 1));
    assert!(ctrl.handle(Event::Stop).is_empty());
    assert_eq!(ctrl.status(), Status::Stopped(StopReason::MaxRefreshes));
}

#[test]
fn stop_while_idle_is_a_no_op() {
    // A session that never ran did not stop: no producer may turn Idle into
    // "stopped: player stopped".
    let mut ctrl = controller(Limits::default());
    assert!(ctrl.handle(Event::Stop).is_empty());
    assert_eq!(ctrl.status(), Status::Idle);
    assert!(ctrl.handle(Event::Shutdown).is_empty());
    assert_eq!(ctrl.status(), Status::Idle);
    assert!(ctrl.handle(Event::ActuatorFailed).is_empty());
    assert_eq!(ctrl.status(), Status::Idle);
}

#[test]
fn shutdown_halts_with_an_honest_label() {
    // The pipeline dying underneath an armed loop is not the player's stop.
    let mut ctrl = started(Limits::default());
    assert_eq!(
        ctrl.handle(Event::Shutdown),
        vec![Action::Halt(StopReason::SessionEnded)]
    );
    assert_eq!(ctrl.status(), Status::Stopped(StopReason::SessionEnded));

    // And like Stop, it never relabels an existing reason.
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(Event::Stop);
    assert!(ctrl.handle(Event::Shutdown).is_empty());
    assert_eq!(ctrl.status(), Status::Stopped(StopReason::PlayerStopped));
}

#[test]
fn actuator_failure_halts_with_an_honest_label() {
    // The clicker refusing to act is not the player's stop.
    let mut ctrl = started(Limits::default());
    assert_eq!(
        ctrl.handle(Event::ActuatorFailed),
        vec![Action::Halt(StopReason::ActuatorFailed)]
    );
    assert_eq!(ctrl.status(), Status::Stopped(StopReason::ActuatorFailed));

    // Like Stop and Shutdown: never relabels an earlier reason.
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(Event::Stop);
    assert!(ctrl.handle(Event::ActuatorFailed).is_empty());
    assert_eq!(ctrl.status(), Status::Stopped(StopReason::PlayerStopped));
}

#[test]
fn max_refreshes_emits_exactly_n_then_halts() {
    let mut ctrl = started(Limits {
        max_refreshes: Some(3),
        ..Limits::default()
    });
    for now in 1..=3 {
        assert_eq!(
            ctrl.handle(snap(dud_shop(None), now)),
            vec![Action::Refresh]
        );
    }
    let actions = ctrl.handle(snap(dud_shop(None), 4));
    assert_eq!(actions, vec![Action::Halt(StopReason::MaxRefreshes)]);
    assert_eq!(ctrl.progress().refreshes, 3);
}

#[test]
fn max_refreshes_zero_blocks_first() {
    let mut ctrl = started(Limits {
        max_refreshes: Some(0),
        ..Limits::default()
    });
    let actions = ctrl.handle(snap(dud_shop(None), 1));
    assert_eq!(actions, vec![Action::Halt(StopReason::MaxRefreshes)]);
    assert_eq!(ctrl.progress().refreshes, 0);
}

#[test]
fn max_spend_is_hard_ceiling_no_overshoot() {
    // Budget 7, cost 3: two refreshes (6 spent) fit, a third would cross.
    let mut ctrl = started(Limits {
        max_spend: Some(xtl(7)),
        ..Limits::default()
    });
    assert_eq!(
        ctrl.handle(snap(dud_shop(Some(meta(100, 3))), 1)),
        vec![Action::Refresh]
    );
    assert_eq!(
        ctrl.handle(snap(dud_shop(Some(meta(97, 3))), 2)),
        vec![Action::Refresh]
    );
    let actions = ctrl.handle(snap(dud_shop(Some(meta(94, 3))), 3));
    assert_eq!(actions, vec![Action::Halt(StopReason::MaxSpend)]);
    assert_eq!(ctrl.progress().spent, xtl(6));
}

#[test]
fn out_of_funds_when_balance_below_cost() {
    let mut ctrl = started(Limits::default());
    let actions = ctrl.handle(snap(dud_shop(Some(meta(2, 3))), 1));
    assert_eq!(actions, vec![Action::Halt(StopReason::OutOfFunds)]);
}

#[test]
fn balance_equal_to_cost_still_affords_one_refresh() {
    // Boundary of out_of_funds_when_balance_below_cost: the gate is `<`, not
    // `<=`, so the last affordable refresh still goes out.
    let mut ctrl = started(Limits::default());
    assert_eq!(
        ctrl.handle(snap(dud_shop(Some(meta(3, 3))), 1)),
        vec![Action::Refresh]
    );
}

#[test]
fn a_budget_that_divides_by_the_cost_is_fully_spent() {
    // The look-ahead gate is `>`, not `>=`, so an exactly-divisible budget is
    // spent to the last crystal rather than leaving one refresh unbought.
    let mut ctrl = started(Limits {
        max_spend: Some(xtl(6)),
        ..Limits::default()
    });
    assert_eq!(
        ctrl.handle(snap(dud_shop(Some(meta(100, 3))), 1)),
        vec![Action::Refresh]
    );
    assert_eq!(
        ctrl.handle(snap(dud_shop(Some(meta(97, 3))), 2)),
        vec![Action::Refresh],
        "spent 3 + cost 3 reaches the ceiling of 6 without crossing it"
    );
    assert_eq!(ctrl.progress().spent, xtl(6));
    assert_eq!(
        ctrl.handle(snap(dud_shop(Some(meta(94, 3))), 3)),
        vec![Action::Halt(StopReason::MaxSpend)]
    );
}

#[test]
fn stop_reason_priority_order() {
    // Pins the *ordering* of `stop_reason`'s clauses, which is exactly the kind
    // of thing that gets reordered by accident.
    let all_limits = Limits {
        max_refreshes: Some(0),
        max_spend: Some(xtl(0)),
        max_matches: None,
        max_duration_ms: Some(0),
    };
    let cases = [
        (
            all_limits,
            Some(meta(2, 3)),
            StopReason::OutOfFunds,
            "everything triggered at once: OutOfFunds wins",
        ),
        (
            all_limits,
            None,
            StopReason::MaxRefreshes,
            "no funds info: MaxRefreshes beats MaxSpend and Timeout",
        ),
        (
            Limits {
                max_spend: Some(xtl(0)),
                max_duration_ms: Some(0),
                ..Limits::default()
            },
            None,
            StopReason::MaxSpend,
            "MaxSpend beats Timeout",
        ),
        (
            Limits {
                max_matches: Some(0),
                max_duration_ms: Some(0),
                ..Limits::default()
            },
            None,
            StopReason::MaxMatches,
            "MaxMatches beats Timeout",
        ),
    ];
    for (limits, refresh, expected, label) in cases {
        let mut ctrl = started(limits);
        assert_eq!(
            ctrl.handle(snap(dud_shop(refresh), 100)),
            vec![Action::Halt(expected)],
            "{label}"
        );
    }
}

#[test]
fn max_matches_zero_blocks_first_refresh() {
    // Symmetric with max_refreshes_zero: gated before anything is spent.
    let mut ctrl = started(Limits {
        max_matches: Some(0),
        ..Limits::default()
    });
    let actions = ctrl.handle(snap(dud_shop(None), 1));
    assert_eq!(actions, vec![Action::Halt(StopReason::MaxMatches)]);
    assert_eq!(ctrl.progress().refreshes, 0);
}

#[test]
fn out_of_funds_uses_debited_balance_estimate() {
    // Meta arrives once (balance 9, cost 3) and never again.
    let mut ctrl = started(Limits::default());
    assert_eq!(
        ctrl.handle(snap(dud_shop(Some(meta(9, 3))), 1)),
        vec![Action::Refresh]
    );
    assert_eq!(ctrl.handle(snap(dud_shop(None), 2)), vec![Action::Refresh]);
    assert_eq!(ctrl.handle(snap(dud_shop(None), 3)), vec![Action::Refresh]);
    let actions = ctrl.handle(snap(dud_shop(None), 4));
    assert_eq!(actions, vec![Action::Halt(StopReason::OutOfFunds)]);
    assert_eq!(ctrl.progress().refreshes, 3);
}

#[test]
fn max_spend_enforced_without_meta_via_constant_cost() {
    // Budget 7, no meta ever: the constant 3 tracks spend from the first
    // refresh — two fit, a third would cross.
    let mut ctrl = started(Limits {
        max_spend: Some(xtl(7)),
        ..Limits::default()
    });
    assert_eq!(ctrl.handle(snap(dud_shop(None), 1)), vec![Action::Refresh]);
    assert_eq!(ctrl.handle(snap(dud_shop(None), 2)), vec![Action::Refresh]);
    assert_eq!(ctrl.progress().spent, xtl(6));
    let actions = ctrl.handle(snap(dud_shop(None), 3));
    assert_eq!(actions, vec![Action::Halt(StopReason::MaxSpend)]);
}

#[test]
fn wire_cost_overrides_the_constant() {
    // Cost 5 against a budget of 7: one refresh fits, the next would cross.
    let mut ctrl = started(Limits {
        max_spend: Some(xtl(7)),
        ..Limits::default()
    });
    assert_eq!(
        ctrl.handle(snap(dud_shop(Some(meta(100, 5))), 1)),
        vec![Action::Refresh]
    );
    assert_eq!(ctrl.progress().spent, xtl(5));
    let actions = ctrl.handle(snap(dud_shop(Some(meta(95, 5))), 2));
    assert_eq!(actions, vec![Action::Halt(StopReason::MaxSpend)]);
}

#[test]
fn a_wire_cost_of_zero_falls_back_to_the_constant() {
    // Zero switches both money gates off, so it is refused. Budget 7 with the
    // constant 3 gives the same schedule as the no-meta test above — the point.
    let mut ctrl = started(Limits {
        max_spend: Some(xtl(7)),
        ..Limits::default()
    });
    assert_eq!(
        ctrl.handle(snap(dud_shop(Some(meta(95, 0))), 1)),
        vec![Action::Refresh]
    );
    assert_eq!(
        ctrl.handle(snap(dud_shop(Some(meta(95, 0))), 2)),
        vec![Action::Refresh]
    );
    assert_eq!(
        ctrl.progress().spent,
        xtl(6),
        "spend must accumulate, or max_spend is unreachable"
    );
    assert_eq!(
        ctrl.handle(snap(dud_shop(Some(meta(95, 0))), 3)),
        vec![Action::Halt(StopReason::MaxSpend)]
    );
}

#[test]
fn a_wire_cost_of_zero_still_reaches_out_of_funds() {
    // `stop_reason` weighs the balance against the floored cost: against the
    // wire's zero, `balance < 0` is never true and the loop never stops.
    let mut ctrl = started(Limits::default());
    let actions = ctrl.handle(snap(dud_shop(Some(meta(2, 0))), 1));
    assert_eq!(actions, vec![Action::Halt(StopReason::OutOfFunds)]);
    assert_eq!(ctrl.progress().refreshes, 0);
}

#[test]
fn matches_found_increments_by_matched_len() {
    let mut ctrl = started(Limits::default());
    let two_hits = shop(&[Equipment, Token, Equipment, Token, Token, Token], None);
    let actions = ctrl.handle(snap(two_hits, 1));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(1, None), target(3, None)]
        }]
    );
    assert_eq!(ctrl.progress().matches_found, 2);
}

#[test]
fn max_matches_boundary_uses_ge() {
    // Exact-boundary case (== max); the strict-overshoot half of `>=` is
    // pinned by single_shop_multi_match_stops.
    let mut ctrl = started(Limits {
        max_matches: Some(2),
        ..Limits::default()
    });
    assert_eq!(
        ctrl.handle(snap(with_ids(hit_shop(None)), 1)),
        vec![Action::Buy {
            targets: vec![target(3, Some(102))]
        }]
    );
    assert_eq!(ctrl.status(), Status::Paused);
    assert_eq!(ctrl.handle(buy(102, 2)), vec![Action::Refresh]);
    let mut second = with_ids(hit_shop(None));
    for item in &mut second.slots {
        item.id = item.id.map(|id| cid(id.get() + 100)); // a new roll, not a deduped re-send
    }
    let actions = ctrl.handle(snap(second, 3));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(3, Some(202))]
        }]
    );
    assert_eq!(ctrl.status(), Status::Paused);
    assert_eq!(
        ctrl.handle(buy(202, 4)),
        vec![Action::Halt(StopReason::MaxMatches)]
    );
    assert_eq!(ctrl.status(), Status::Stopped(StopReason::MaxMatches));
}

#[test]
fn single_shop_multi_match_overshoot_halts_at_next_gate() {
    let mut ctrl = started(Limits {
        max_matches: Some(2),
        ..Limits::default()
    });
    let triple = with_ids(shop(
        &[Equipment, Equipment, Equipment, Token, Token, Token],
        None,
    ));
    // Three matches overshoot the limit of two, but the pause still comes
    // first: the found items must be buyable before the loop stops.
    let actions = ctrl.handle(snap(triple, 1));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![
                target(1, Some(100)),
                target(2, Some(101)),
                target(3, Some(102))
            ]
        }]
    );
    assert_eq!(ctrl.progress().matches_found, 3);
    assert_eq!(ctrl.status(), Status::Paused);
    let _ = ctrl.handle(buy(100, 2));
    let _ = ctrl.handle(buy(101, 3));
    // The last buy resumes through the gate, which trips the exceeded limit.
    assert_eq!(
        ctrl.handle(buy(102, 4)),
        vec![Action::Halt(StopReason::MaxMatches)]
    );
}

#[test]
fn timeout_fires_via_tick_while_paused() {
    let mut ctrl = started(Limits {
        max_duration_ms: Some(1_000),
        ..Limits::default()
    });
    let _ = ctrl.handle(snap(hit_shop(None), 10));
    assert_eq!(ctrl.status(), Status::Paused);
    assert!(ctrl.handle(Event::Tick { now_ms: 999 }).is_empty());
    let actions = ctrl.handle(Event::Tick { now_ms: 1_000 });
    assert_eq!(actions, vec![Action::Halt(StopReason::Timeout)]);
    assert_eq!(ctrl.status(), Status::Stopped(StopReason::Timeout));
}

#[test]
fn an_elapsed_duration_halts_at_the_refresh_gate() {
    // The copy of the deadline check that guards the emission point: without
    // it, an arrival after the deadline buys one more refresh.
    let mut ctrl = started(Limits {
        max_duration_ms: Some(1_000),
        ..Limits::default()
    });
    assert_eq!(
        ctrl.handle(snap(dud_shop(None), 1_000)),
        vec![Action::Halt(StopReason::Timeout)]
    );
    assert_eq!(ctrl.progress().refreshes, 0);
}

#[test]
fn tick_before_start_is_noop() {
    let mut ctrl = controller(Limits {
        max_duration_ms: Some(0),
        ..Limits::default()
    });
    assert!(ctrl.handle(Event::Tick { now_ms: u64::MAX }).is_empty());
    assert_eq!(ctrl.status(), Status::Idle);
}

#[test]
fn now_before_started_no_underflow() {
    let mut ctrl = controller(Limits {
        max_duration_ms: Some(100),
        ..Limits::default()
    });
    let _ = ctrl.handle(Event::Start { now_ms: 1_000 });
    assert!(ctrl.handle(Event::Tick { now_ms: 500 }).is_empty());
    assert_eq!(ctrl.status(), Status::Watching);
}

#[test]
fn filter_changed_applies_next_snapshot_only() {
    let mut ctrl = started(Limits::default());
    assert_eq!(ctrl.handle(snap(dud_shop(None), 1)), vec![Action::Refresh]);
    let token_filter = Filter {
        kinds: vec![Token],
        ..Filter::default()
    };
    assert!(ctrl.handle(Event::FilterChanged(token_filter)).is_empty());
    assert_eq!(ctrl.status(), Status::Watching);
    let actions = ctrl.handle(snap(dud_shop(None), 2));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: (1..=6).map(|slot| target(slot, None)).collect()
        }]
    );
    assert_eq!(ctrl.status(), Status::Paused);
}

#[test]
fn buy_target_slot_falls_back_to_position_when_zero() {
    let mut ctrl = started(Limits::default());
    let mut shop = hit_shop(None);
    shop.slots[2].slot = 0; // server omitted the slot number
    let actions = ctrl.handle(snap(shop, 1));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(3, None)]
        }]
    );
}

#[test]
fn buy_target_slot_clamps_oversized_position() {
    // The fallback must saturate, not overflow (debug panic) or wrap to 0.
    let mut ctrl = started(Limits::default());
    let mut slots: Vec<ShopItem> = (0..300).map(|_| item(0, Token)).collect();
    slots[299] = item(0, Equipment);
    let oversized = ShopSnapshot {
        merchant: None,
        slots,
        refresh: None,
    };
    let actions = ctrl.handle(snap(oversized, 1));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(u8::MAX, None)]
        }]
    );
}

#[test]
fn buy_targets_align_with_checklist() {
    // Three matches: trackable, idless, sold out. Exactly the `id: Some`
    // targets form the checklist, so the actuator clicks what the resume awaits.
    let filter = Filter {
        kinds: vec![Equipment],
        include_sold_out: true,
        ..Filter::default()
    };
    let mut ctrl = Controller::new(filter, Limits::default());
    let _ = ctrl.handle(Event::Start { now_ms: 0 });
    let mut shop = shop(
        &[Equipment, Equipment, Equipment, Token, Token, Token],
        None,
    );
    shop.slots[0].id = Some(cid(100));
    shop.slots[2].limit = Some(PurchaseLimit {
        remaining: 0,
        total: 1,
    });
    shop.slots[2].id = Some(cid(102));
    let actions = ctrl.handle(snap(shop, 1));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(1, Some(100)), target(2, None), target(3, None)]
        }]
    );
    assert_eq!(ctrl.checklist(), &[cid(100)]);
}

#[test]
fn recovery_disabled_never_arms_the_watchdog() {
    // Off is player-paced advice and DryRun yields no wire feedback: deadlines
    // would self-halt both.
    let mut ctrl = started(Limits::default());
    assert_eq!(
        ctrl.handle(snap(with_ids(dud_shop(None)), 1)),
        vec![Action::Refresh]
    );
    assert!(ctrl.handle(tick(1_000_000)).is_empty());
    assert_eq!(ctrl.status(), Status::Watching);

    // Same for a pending purchase.
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.status(), Status::Paused);
    assert!(ctrl.handle(tick(1_000_000)).is_empty());
    assert_eq!(ctrl.status(), Status::Paused);
}

#[test]
fn snapshot_watchdog_reclicks_confirm_then_reissues_then_halts() {
    let mut ctrl = recovering(Limits::default());
    assert_eq!(
        ctrl.handle(snap(with_ids(dud_shop(None)), 1)),
        vec![Action::Refresh]
    );
    // Deadline not reached: quiet.
    assert!(ctrl.handle(tick(10_000)).is_empty());
    // Miss #1: free blind confirm re-click.
    assert_eq!(
        ctrl.handle(tick(past_rung(1))),
        vec![Action::Recover(Recovery::ConfirmRefresh)]
    );
    // Miss #2: paid re-issue, counted and debited like any refresh.
    assert_eq!(
        ctrl.handle(tick(past_rung(2))),
        vec![Action::Recover(Recovery::Refresh)]
    );
    assert_eq!(ctrl.progress().refreshes, 2);
    assert_eq!(ctrl.progress().spent, xtl(6));
    // Miss #3: honest halt.
    assert_eq!(
        ctrl.handle(tick(past_rung(3))),
        vec![Action::Halt(StopReason::Unresponsive)]
    );
    assert_eq!(ctrl.status(), Status::Stopped(StopReason::Unresponsive));
    assert_eq!(ctrl.expectation, None);
}

/// **The reason `set_recovery(false)` is not a bare field write.** `watchdog`
/// has no notion of "armed": `recovery` gates *setting* deadlines, not
/// honouring them. A deadline left behind by a disarm would climb the ladder
/// and re-issue paid clicks for a rehearsal nobody is watching.
#[test]
fn disarming_recovery_drops_the_deadline_it_was_waiting_on() {
    let mut ctrl = recovering(Limits::default());
    assert_eq!(
        ctrl.handle(snap(with_ids(dud_shop(None)), 1)),
        vec![Action::Refresh]
    );
    assert!(
        ctrl.expectation.is_some(),
        "the issued refresh must have opened one"
    );

    ctrl.set_recovery(false);
    assert_eq!(ctrl.expectation, None);
    assert!(!ctrl.is_recovery_enabled());

    // The rung that fired in `watchdog_reissue_respects_limits` is now silent.
    assert!(
        ctrl.handle(tick(past_rung(1))).is_empty(),
        "a disarmed watchdog must not climb"
    );
    assert!(ctrl.handle(tick(past_rung(2))).is_empty());
}

/// Re-arming opens no deadline of its own: the proof the cleared expectation
/// waited on belonged to a job that ran in the other mode. The *next* issued
/// refresh is what re-opens one.
#[test]
fn rearming_recovery_does_not_resurrect_the_cleared_deadline() {
    let mut ctrl = recovering(Limits::default());
    let _ = ctrl.handle(snap(with_ids(dud_shop(None)), 1));
    ctrl.set_recovery(false);
    ctrl.set_recovery(true);

    assert!(ctrl.is_recovery_enabled());
    assert_eq!(ctrl.expectation, None, "re-arming restores nothing");
    assert!(
        ctrl.handle(tick(past_rung(3))).is_empty(),
        "there is no missed proof to escalate on"
    );
}

#[test]
fn watchdog_reissue_respects_limits() {
    // The paid rung goes through the same gate as any refresh, so an exhausted
    // limit halts honestly instead of double-rolling.
    let mut ctrl = recovering(Limits {
        max_refreshes: Some(1),
        ..Limits::default()
    });
    assert_eq!(
        ctrl.handle(snap(with_ids(dud_shop(None)), 1)),
        vec![Action::Refresh]
    );
    // The free rung still fires: it spends nothing.
    assert_eq!(
        ctrl.handle(tick(past_rung(1))),
        vec![Action::Recover(Recovery::ConfirmRefresh)]
    );
    // The paid rung would cross the ceiling: honest halt, no double-roll.
    assert_eq!(
        ctrl.handle(tick(past_rung(2))),
        vec![Action::Halt(StopReason::MaxRefreshes)]
    );
    assert_eq!(ctrl.progress().refreshes, 1);
    assert!(ctrl.handle(tick(40_000)).is_empty());
}

#[test]
fn new_snapshot_rearms_a_fresh_snapshot_deadline() {
    let mut ctrl = recovering(Limits::default());
    assert_eq!(
        ctrl.handle(snap(with_ids(dud_shop(None)), 1)),
        vec![Action::Refresh]
    );
    // The awaited proof arrives, and the advised refresh arms a fresh window.
    let mut next = with_ids(dud_shop(None));
    for item in &mut next.slots {
        item.id = item.id.map(|id| cid(id.get() + 100));
    }
    assert_eq!(ctrl.handle(snap(next, 9_000)), vec![Action::Refresh]);
    // The original deadline passes silently...
    assert!(ctrl.handle(tick(10_500)).is_empty());
    // ...the fresh one fires.
    assert_eq!(
        ctrl.handle(tick(19_000)),
        vec![Action::Recover(Recovery::ConfirmRefresh)]
    );
}

#[test]
fn duplicate_snapshot_keeps_the_snapshot_expectation() {
    // A re-open is not the awaited new shop, so the deadline keeps running.
    let mut ctrl = recovering(Limits::default());
    assert_eq!(
        ctrl.handle(snap(with_ids(dud_shop(None)), 1)),
        vec![Action::Refresh]
    );
    assert!(
        ctrl.handle(snap(with_ids(dud_shop(None)), 5_000))
            .is_empty()
    );
    assert_eq!(
        ctrl.handle(tick(past_rung(1))),
        vec![Action::Recover(Recovery::ConfirmRefresh)]
    );
}

#[test]
fn slotless_snapshot_keeps_the_snapshot_expectation() {
    // A degraded message is not shop content: the refresh's proof is still owed.
    let mut ctrl = recovering(Limits::default());
    assert_eq!(
        ctrl.handle(snap(with_ids(dud_shop(None)), 1)),
        vec![Action::Refresh]
    );
    assert!(ctrl.handle(snap(empty_shop(), 5_000)).is_empty());
    assert_eq!(
        ctrl.handle(tick(past_rung(1))),
        vec![Action::Recover(Recovery::ConfirmRefresh)]
    );
}

#[test]
fn purchase_watchdog_ladder_reissues_outstanding_buys() {
    let mut ctrl = recovering(Limits::default());
    assert_eq!(
        ctrl.handle(snap(with_ids(hit_shop(None)), 1)),
        vec![Action::Buy {
            targets: vec![target(3, Some(102))]
        }]
    );
    // Miss #1: free blind confirm re-click.
    assert_eq!(
        ctrl.handle(tick(past_rung(1))),
        vec![Action::Recover(Recovery::ConfirmBuy)]
    );
    // Miss #2: the outstanding buys re-issued by identity.
    assert_eq!(
        ctrl.handle(tick(past_rung(2))),
        vec![Action::Recover(Recovery::Buy {
            targets: vec![target(3, Some(102))]
        })]
    );
    // Miss #3: honest halt.
    assert_eq!(
        ctrl.handle(tick(past_rung(3))),
        vec![Action::Halt(StopReason::Unresponsive)]
    );
    assert_eq!(ctrl.status(), Status::Stopped(StopReason::Unresponsive));
}

#[test]
fn accepted_echo_resets_the_purchase_deadline_and_attempt() {
    let mut ctrl = recovering(Limits::default());
    let two_hits = with_ids(shop(
        &[Equipment, Token, Equipment, Token, Token, Token],
        None,
    ));
    let _ = ctrl.handle(snap(two_hits, 1)); // checklist [100, 102]
    assert_eq!(
        ctrl.handle(tick(past_rung(1))),
        vec![Action::Recover(Recovery::ConfirmBuy)]
    );
    // Proof of life: the ladder restarts at rung zero with a full window.
    assert!(ctrl.handle(buy(100, 12_000)).is_empty());
    assert!(ctrl.handle(tick(21_000)).is_empty());
    assert_eq!(
        ctrl.handle(tick(22_000)),
        vec![Action::Recover(Recovery::ConfirmBuy)]
    );
}

#[test]
fn pause_without_checklist_never_arms_the_watchdog() {
    // An untrackable match pauses for the player, not the game: no echo can
    // arrive, so a deadline would always halt the session.
    let mut ctrl = recovering(Limits::default());
    let _ = ctrl.handle(snap(hit_shop(None), 1)); // fixture ids all 0
    assert_eq!(ctrl.status(), Status::Paused);
    assert!(ctrl.checklist().is_empty());
    assert!(ctrl.handle(tick(1_000_000)).is_empty());
    assert_eq!(ctrl.status(), Status::Paused);
}

#[test]
fn dead_stock_batch_arms_snapshot_not_purchase() {
    // Nothing is buyable, so the deadline watches for the next shop, not for
    // echoes that cannot come.
    let filter = Filter {
        kinds: vec![Equipment],
        include_sold_out: true,
        ..Filter::default()
    };
    let mut ctrl = Controller::new(filter, Limits::default());
    ctrl.set_recovery(true);
    let _ = ctrl.handle(Event::Start { now_ms: 0 });
    let mut shop = with_ids(hit_shop(None));
    shop.slots[2].limit = Some(PurchaseLimit {
        remaining: 0,
        total: 1,
    });
    let actions = ctrl.handle(snap(shop, 1));
    assert_eq!(
        actions,
        vec![
            Action::Buy {
                targets: vec![target(3, None)]
            },
            Action::Refresh,
        ]
    );
    assert_eq!(
        ctrl.handle(tick(past_rung(1))),
        vec![Action::Recover(Recovery::ConfirmRefresh)]
    );
}

#[test]
fn buy_reissue_ignores_a_mid_pause_filter_swap() {
    // Targets are rebuilt from the checklist by identity, so a new filter must
    // not redraw or drop what the pause is waiting on.
    let mut ctrl = recovering(Limits::default());
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1)); // checklist [102]
    let token_filter = Filter {
        kinds: vec![Token],
        ..Filter::default()
    };
    assert!(ctrl.handle(Event::FilterChanged(token_filter)).is_empty());
    let _ = ctrl.handle(tick(past_rung(1))); // rung 1: confirm re-click
    assert_eq!(
        ctrl.handle(tick(past_rung(2))),
        vec![Action::Recover(Recovery::Buy {
            targets: vec![target(3, Some(102))]
        })]
    );
}

#[test]
fn halt_clears_the_expectation() {
    let mut ctrl = recovering(Limits::default());
    let _ = ctrl.handle(snap(with_ids(dud_shop(None)), 1));
    let _ = ctrl.handle(Event::Stop);
    assert_eq!(ctrl.expectation, None);
    assert!(ctrl.handle(tick(past_rung(1))).is_empty());
}

#[test]
fn restart_carries_no_stale_expectation() {
    let mut ctrl = recovering(Limits::default());
    let _ = ctrl.handle(snap(with_ids(dud_shop(None)), 1));
    let _ = ctrl.handle(Event::Stop);
    let _ = ctrl.handle(Event::Start { now_ms: 2 });
    // A leftover deadline here would recover a click nobody sent.
    assert_eq!(ctrl.expectation, None);
    assert!(ctrl.handle(tick(past_rung(1))).is_empty());
    assert_eq!(ctrl.status(), Status::Watching);
}

#[test]
fn max_matches_resolution_halt_clears_the_expectation() {
    // The pause resolves by buying and the resume gate halts on the reached
    // limit: that halt must also disarm the purchase deadline.
    let mut ctrl = recovering(Limits {
        max_matches: Some(1),
        ..Limits::default()
    });
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1)); // Purchase armed
    assert_eq!(
        ctrl.handle(buy(102, 2)),
        vec![Action::Halt(StopReason::MaxMatches)]
    );
    assert_eq!(ctrl.expectation, None);
    assert!(ctrl.handle(tick(20_000)).is_empty());
}

#[test]
fn timeout_beats_the_watchdog_on_the_same_tick() {
    // Both deadlines lapse together: the honest label is the player's own
    // limit, not the game's silence.
    let mut ctrl = recovering(Limits {
        // Derived, not re-typed: the coincidence with rung 1 is the point.
        max_duration_ms: Some(past_rung(1)),
        ..Limits::default()
    });
    let _ = ctrl.handle(snap(with_ids(hit_shop(None)), 1)); // Paused, both deadlines at rung 1
    assert_eq!(
        ctrl.handle(tick(past_rung(1))),
        vec![Action::Halt(StopReason::Timeout)]
    );

    // Same while Watching on a refresh in flight.
    let mut ctrl = recovering(Limits {
        max_duration_ms: Some(past_rung(1)),
        ..Limits::default()
    });
    let _ = ctrl.handle(snap(with_ids(dud_shop(None)), 1));
    assert_eq!(
        ctrl.handle(tick(past_rung(1))),
        vec![Action::Halt(StopReason::Timeout)]
    );
}

#[test]
fn link_down_suspends_the_watchdog() {
    // The reconnect backoff caps above the whole ladder: left running, an
    // outage escalates into a paid double-roll and a halt blaming the game.
    let mut ctrl = recovering(Limits::default());
    let _ = ctrl.handle(snap(with_ids(dud_shop(None)), 1));
    assert!(ctrl.handle(Event::LinkDown).is_empty());
    assert!(ctrl.handle(tick(50_000)).is_empty());
    assert_eq!(ctrl.status(), Status::Watching);
}

#[test]
fn link_up_regrants_a_full_deadline() {
    let mut ctrl = recovering(Limits::default());
    let _ = ctrl.handle(snap(with_ids(dud_shop(None)), 1));
    // One rung climbed before the outage.
    assert_eq!(
        ctrl.handle(tick(past_rung(1))),
        vec![Action::Recover(Recovery::ConfirmRefresh)]
    );
    let _ = ctrl.handle(Event::LinkDown);
    assert!(ctrl.handle(tick(25_000)).is_empty());
    // A fresh window, but the climbed rung is kept: the retry never got its
    // answer, so the next miss escalates.
    assert!(ctrl.handle(Event::LinkUp { now_ms: 25_000 }).is_empty());
    assert!(ctrl.handle(tick(34_999)).is_empty());
    assert_eq!(
        ctrl.handle(tick(35_000)),
        vec![Action::Recover(Recovery::Refresh)]
    );
}

#[test]
fn purchase_echo_while_watching_leaves_snapshot_expectation_alone() {
    // A stray echo is not the proof a refresh waits on.
    let mut ctrl = recovering(Limits::default());
    let _ = ctrl.handle(snap(with_ids(dud_shop(None)), 1));
    assert!(ctrl.handle(buy(999, 2)).is_empty());
    assert_eq!(
        ctrl.handle(tick(past_rung(1))),
        vec![Action::Recover(Recovery::ConfirmRefresh)]
    );
}

/// Slots carrying ids *and* names, so an id-only echo resolves to a wire name.
fn named_shop() -> ShopSnapshot {
    let named = |id: u32, name: Option<&str>| ShopItem {
        id: Some(cid(id)),
        name: name.map(str::to_owned),
        ..ShopItem::default()
    };
    ShopSnapshot {
        merchant: None,
        slots: vec![
            named(100, Some("ticketrare_name")),
            named(101, Some("ticketspecial_name")),
            named(102, Some("Wondrous Potion Vial")),
            named(103, None),
        ],
        refresh: None,
    }
}

#[test]
fn haul_tallies_bought_items_by_resolved_name() {
    let mut ctrl = controller(Limits::default());
    let _ = ctrl.handle(snap(named_shop(), 0));
    let _ = ctrl.handle(buy(100, 1)); // covenant
    let _ = ctrl.handle(buy(101, 2)); // mystic
    let _ = ctrl.handle(buy(102, 3)); // named, but not a headliner
    let _ = ctrl.handle(buy(103, 4)); // nameless
    let haul = ctrl.haul();
    assert_eq!(haul.count("ticketrare_name"), 1);
    assert_eq!(haul.count("ticketspecial_name"), 1);
    // The unlisted named buy and the nameless one both fall in the bucket.
    assert_eq!(haul.others(&["ticketrare_name", "ticketspecial_name"]), 2);
}

#[test]
fn haul_counts_a_repeated_echo_once_per_roll() {
    let mut ctrl = controller(Limits::default());
    let _ = ctrl.handle(snap(named_shop(), 0));
    let _ = ctrl.handle(buy(100, 1));
    let _ = ctrl.handle(buy(100, 2));
    assert_eq!(ctrl.haul().count("ticketrare_name"), 1);
}

#[test]
fn haul_resets_on_start() {
    let mut ctrl = controller(Limits::default());
    let _ = ctrl.handle(snap(named_shop(), 0));
    let _ = ctrl.handle(buy(100, 1));
    assert_eq!(ctrl.haul().count("ticketrare_name"), 1);
    assert!(ctrl.handle(Event::Start { now_ms: 2 }).is_empty());
    assert_eq!(ctrl.haul().count("ticketrare_name"), 0);
}

#[test]
fn haul_ignores_a_buy_after_the_run_stops() {
    // A buy after the stop is the player's own, not the loop's.
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(named_shop(), 0));
    let _ = ctrl.handle(buy(100, 1));
    assert_eq!(ctrl.haul().count("ticketrare_name"), 1);
    let _ = ctrl.handle(Event::Stop);
    let _ = ctrl.handle(buy(101, 2));
    assert_eq!(ctrl.haul().count("ticketspecial_name"), 0);
}

#[test]
fn haul_drops_a_stale_echo_after_the_roll_rotates() {
    // An echo landing after the stock rotated finds no slot for its id, and is
    // dropped rather than bucketed as a phantom Other.
    let mut ctrl = started(Limits::default());
    let _ = ctrl.handle(snap(named_shop(), 0));
    let _ = ctrl.handle(buy(100, 1));
    assert_eq!(ctrl.haul().count("ticketrare_name"), 1);
    // A new roll (fresh ids) rotates the stock; `bought` clears.
    let roll_b = ShopSnapshot {
        merchant: None,
        slots: vec![ShopItem {
            id: Some(cid(200)),
            ..ShopItem::default()
        }],
        refresh: None,
    };
    let _ = ctrl.handle(snap(roll_b, 2));
    let _ = ctrl.handle(buy(100, 3));
    assert_eq!(ctrl.haul().count("ticketrare_name"), 1);
    assert_eq!(
        ctrl.haul()
            .others(&["ticketrare_name", "ticketspecial_name"]),
        0
    );
}
