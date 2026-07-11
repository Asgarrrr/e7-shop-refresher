use super::*;
use crate::domain::shop::ItemKind::{self, Equipment, Token};
use crate::domain::shop::{PurchaseLimit, ShopItem};

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
        crystal_balance,
        cost,
    }
}

/// Assigns stable non-zero catalog ids (`100 + index`) so dedup and the
/// checklist engage; `hit_shop`'s matching slot 3 gets id 102.
fn with_ids(mut snapshot: ShopSnapshot) -> ShopSnapshot {
    for (index, item) in snapshot.slots.iter_mut().enumerate() {
        item.id = 100 + index as u32;
    }
    snapshot
}

fn snap(snapshot: ShopSnapshot, now_ms: u64) -> Event {
    Event::Snapshot { snapshot, now_ms }
}

fn target(slot: u8, id: Option<u32>) -> BuyTarget {
    BuyTarget { slot, id }
}

fn buy(item: u32, now_ms: u64) -> Event {
    Event::Purchase {
        item,
        gold: None,
        now_ms,
    }
}

fn buy_with_gold(item: u32, gold: u32, now_ms: u64) -> Event {
    Event::Purchase {
        item,
        gold: Some(gold),
        now_ms,
    }
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
    // The invariant lives here, not in the callers: no command producer
    // may arm a hunt-everything loop, and the refusal is explicit so the
    // caller can render the reason.
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
    // The app reads acceptance from this verdict, not the action count: only
    // Action::Refused is a refusal, everything else (including an empty list,
    // which contains no refusal) means the event applied.
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
    assert_eq!(ctrl.progress().spent, 3);
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
    ctrl.handle(Event::LimitsChanged(Limits {
        max_refreshes: Some(1),
        ..Limits::default()
    }));
    assert_eq!(ctrl.status(), Status::Watching);
    // Without any new shop, the next tick must halt: the gate can't linger on.
    let actions = ctrl.handle(Event::Tick { now_ms: 2 });
    assert_eq!(actions, vec![Action::Halt(StopReason::MaxRefreshes)]);
    assert_eq!(ctrl.status(), Status::Stopped(StopReason::MaxRefreshes));
}

#[test]
fn tick_while_paused_ignores_non_timeout_limits() {
    let mut ctrl = started(Limits::default());
    ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.status(), Status::Paused);
    // Tightening a count limit must not abandon a buyable pause at a tick;
    // the purchase/next-shop path re-checks it.
    ctrl.handle(Event::LimitsChanged(Limits {
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
            item.id += round * 100;
        }
        let now = u64::from(round) * 2;
        ctrl.handle(snap(shop, now + 1));
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
    ctrl.handle(snap(two_hits, 1));
    assert_eq!(ctrl.status(), Status::Paused);
    assert!(ctrl.handle(buy(100, 2)).is_empty());
    assert_eq!(ctrl.status(), Status::Paused);
    assert_eq!(ctrl.checklist(), &[102]);
}

#[test]
fn last_purchase_auto_resumes_with_refresh() {
    let mut ctrl = started(Limits::default());
    ctrl.handle(snap(with_ids(hit_shop(None)), 1));
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
    ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    let actions = ctrl.handle(buy(102, 2));
    assert_eq!(actions, vec![Action::Halt(StopReason::MaxRefreshes)]);
    assert_eq!(ctrl.progress().refreshes, 0);
}

#[test]
fn stop_clears_the_checklist() {
    // Catalog ids are stable per item and snapshots stored while Stopped are
    // never evaluated: a checklist outliving the hunt would keep flagging
    // yesterday's matches as "wanted" in the view.
    let mut ctrl = started(Limits::default());
    ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.checklist(), &[102]);
    ctrl.handle(Event::Stop);
    assert!(ctrl.checklist().is_empty());
}

#[test]
fn max_matches_halts_after_the_goal_item_is_bought() {
    let mut ctrl = started(Limits {
        max_matches: Some(1),
        ..Limits::default()
    });
    // The match trips max_matches, but the found item is the point of the
    // hunt: pause and buy it first.
    let actions = ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(3, Some(102))]
        }]
    );
    assert_eq!(ctrl.status(), Status::Paused);
    // The buy clears the pause; the resume gate halts instead of refreshing.
    let actions = ctrl.handle(buy(102, 2));
    assert_eq!(actions, vec![Action::Halt(StopReason::MaxMatches)]);
    assert!(ctrl.checklist().is_empty());
}

#[test]
fn purchase_of_unknown_id_ignored() {
    let mut ctrl = started(Limits::default());
    ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert!(ctrl.handle(buy(999, 2)).is_empty());
    assert_eq!(ctrl.status(), Status::Paused);
    assert_eq!(ctrl.checklist(), &[102]);
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
    ctrl.handle(snap(two_hits, 1));
    assert!(ctrl.handle(buy(100, 2)).is_empty());
    // The wire may replay an echo: the id already left the checklist,
    // so the duplicate must not stand in for the remaining item's buy.
    assert!(ctrl.handle(buy(100, 3)).is_empty());
    assert_eq!(ctrl.status(), Status::Paused);
    assert_eq!(ctrl.checklist(), &[102]);
}

#[test]
fn zero_id_matches_pause_until_new_shop() {
    let mut ctrl = started(Limits::default());
    ctrl.handle(snap(hit_shop(None), 1)); // fixture ids are all 0
    assert_eq!(ctrl.status(), Status::Paused);
    assert!(ctrl.checklist().is_empty());
    // No echo can clear an untrackable match; only a new shop unpauses.
    assert!(ctrl.handle(buy(0, 2)).is_empty());
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
    ctrl.handle(snap(with_ids(hit_shop(None)), 1));
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
    ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.progress().matches_found, 1);
    assert!(ctrl.handle(snap(with_ids(hit_shop(None)), 2)).is_empty());
    assert_eq!(ctrl.progress().matches_found, 1);
    assert_eq!(ctrl.checklist(), &[102]);
    assert_eq!(ctrl.status(), Status::Paused);
}

#[test]
fn reopened_shop_after_buy_does_not_double_refresh() {
    let mut ctrl = started(Limits::default());
    ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.handle(buy(102, 2)), vec![Action::Refresh]);
    // Re-opening the shop after the buy re-delivers the same roll with
    // the bought slot decremented: same identity (limit is excluded),
    // so the fingerprint that survived the auto-resume mutes it.
    let mut reopened = with_ids(hit_shop(None));
    reopened.slots[2].limit = Some(PurchaseLimit {
        remaining: 0,
        total: 1,
    });
    assert!(ctrl.handle(snap(reopened, 3)).is_empty());
    assert_eq!(ctrl.progress().refreshes, 1);
}

#[test]
fn zero_id_slot_disables_dedup() {
    let mut ctrl = started(Limits::default());
    let mut holed = with_ids(dud_shop(None));
    holed.slots[5].id = 0;
    assert_eq!(ctrl.handle(snap(holed.clone(), 1)), vec![Action::Refresh]);
    // No usable identity: the identical re-send evaluates again (fail open).
    assert_eq!(ctrl.handle(snap(holed, 2)), vec![Action::Refresh]);
    assert_eq!(ctrl.progress().refreshes, 2);
}

#[test]
fn new_shop_while_paused_no_match_refreshes() {
    let mut ctrl = started(Limits::default());
    ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.status(), Status::Paused);
    // Hourly auto-refresh replaced the shop: different ids, no match.
    let mut fresh = with_ids(dud_shop(None));
    for item in &mut fresh.slots {
        item.id += 100;
    }
    let actions = ctrl.handle(snap(fresh, 2));
    assert_eq!(actions, vec![Action::Refresh]);
    assert_eq!(ctrl.status(), Status::Watching);
}

#[test]
fn new_shop_while_paused_rebuilds_checklist() {
    let mut ctrl = started(Limits::default());
    ctrl.handle(snap(with_ids(hit_shop(None)), 1)); // checklist [102]
    let mut fresh = with_ids(hit_shop(None));
    for item in &mut fresh.slots {
        item.id += 100; // new shop, matching slot now id 202
    }
    let actions = ctrl.handle(snap(fresh, 2));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(3, Some(202))]
        }]
    );
    assert_eq!(ctrl.checklist(), &[202]);
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
    ctrl.handle(Event::Start { now_ms: 2 });
    // Stored but never acted on: the same shop must evaluate after start.
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
    ctrl.handle(Event::Stop);
    ctrl.handle(Event::Start { now_ms: 2 });
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
    ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.status(), Status::Paused);
    // A degraded slotless message must not wipe the pending checklist.
    assert!(ctrl.handle(snap(empty_shop(), 2)).is_empty());
    assert_eq!(ctrl.status(), Status::Paused);
    assert_eq!(ctrl.checklist(), &[102]);
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
    ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.status(), Status::Paused);
    // All ids omitted: a duplicate cannot be told from a new shop, so
    // nothing may be re-evaluated over the pending purchase.
    assert!(ctrl.handle(snap(dud_shop(None), 2)).is_empty());
    assert_eq!(ctrl.status(), Status::Paused);
    assert_eq!(ctrl.checklist(), &[102]);
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
    reroll.slots[0].price = Some(120_000);
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
    holed.slots[5].id = 0;
    assert_eq!(ctrl.handle(snap(holed, 2)), vec![Action::Refresh]);
    // ...so a stale verbatim duplicate of the first shop is still muted.
    assert!(ctrl.handle(snap(with_ids(dud_shop(None)), 3)).is_empty());
    assert_eq!(ctrl.progress().refreshes, 2);
}

#[test]
fn sold_out_only_match_keeps_hunting() {
    // A sold-out slot can match (include_sold_out) but nobody can buy it:
    // pausing would park the loop until the hourly rotation, so the match
    // is shown and the hunt continues in the same batch.
    let filter = Filter {
        kinds: vec![Equipment],
        include_sold_out: true,
        ..Filter::default()
    };
    let mut ctrl = Controller::new(filter, Limits::default());
    ctrl.handle(Event::Start { now_ms: 0 });
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
    // One matched slot is sold out, the other has no id but is in stock:
    // the player can still buy the latter, so the manual-flow pause stays.
    let filter = Filter {
        kinds: vec![Equipment],
        include_sold_out: true,
        ..Filter::default()
    };
    let mut ctrl = Controller::new(filter, Limits::default());
    ctrl.handle(Event::Start { now_ms: 0 });
    let mut two_hits = shop(&[Equipment, Equipment, Token, Token, Token, Token], None);
    two_hits.slots[0].id = 100;
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
    ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    assert_eq!(ctrl.status(), Status::Paused);
    // The buy echoes a 100k balance...
    assert_eq!(
        ctrl.handle(buy_with_gold(102, 100_000, 2)),
        vec![Action::Refresh]
    );
    // ...so the next match, priced 184k, is beyond reach: shown, never
    // clicked, and the hunt continues.
    let mut pricey = with_ids(hit_shop(None));
    for item in &mut pricey.slots {
        item.id += 100;
    }
    pricey.slots[2].price = Some(184_000);
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
    // 200k on hand, two 184k matches: the first is clickable, the second is
    // not — the first buy will have spent the purse.
    let mut ctrl = started(Limits::default());
    ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    ctrl.handle(buy_with_gold(102, 200_000, 2));
    let mut twins = with_ids(shop(
        &[Equipment, Equipment, Token, Token, Token, Token],
        None,
    ));
    for item in &mut twins.slots {
        item.id += 100;
    }
    twins.slots[0].price = Some(184_000);
    twins.slots[1].price = Some(184_000);
    let actions = ctrl.handle(snap(twins, 3));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(1, Some(200)), target(2, None)]
        }]
    );
    assert_eq!(ctrl.checklist(), &[200]);
    assert_eq!(ctrl.status(), Status::Paused);
}

#[test]
fn unknown_gold_restricts_nothing() {
    // No echo yet: a priced match stays clickable — the estimate fails
    // open, it never vetoes buys on ignorance.
    let mut ctrl = started(Limits::default());
    let mut priced = with_ids(hit_shop(None));
    priced.slots[2].price = Some(999_999_999);
    let actions = ctrl.handle(snap(priced, 1));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(3, Some(102))]
        }]
    );
    assert_eq!(ctrl.status(), Status::Paused);
}

#[test]
fn unknown_price_with_known_gold_fails_open() {
    let mut ctrl = started(Limits::default());
    ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    ctrl.handle(buy_with_gold(102, 10, 2)); // 10 gold left, known
    // The next match omits its price: it cannot be proven unaffordable,
    // so it stays clickable.
    let mut fresh = with_ids(hit_shop(None));
    for item in &mut fresh.slots {
        item.id += 100;
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
    ctrl.handle(snap(with_ids(hit_shop(None)), 1));
    ctrl.handle(buy_with_gold(102, 10, 2));
    ctrl.handle(Event::Stop);
    ctrl.handle(Event::Start { now_ms: 3 });
    let mut pricey = with_ids(hit_shop(None));
    pricey.slots[2].price = Some(184_000);
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
    ctrl.handle(snap(dud_shop(Some(meta(100, 3))), 1));
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
    ctrl.handle(snap(dud_shop(None), 1));
    assert!(ctrl.handle(Event::Stop).is_empty());
    assert_eq!(ctrl.status(), Status::Stopped(StopReason::MaxRefreshes));
}

#[test]
fn stop_while_idle_is_a_no_op() {
    // A session that never ran did not stop: no Stop producer (console, GUI
    // button, teardown) may turn Idle into "stopped: player stopped".
    let mut ctrl = controller(Limits::default());
    assert!(ctrl.handle(Event::Stop).is_empty());
    assert_eq!(ctrl.status(), Status::Idle);
    assert!(ctrl.handle(Event::Shutdown).is_empty());
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
    ctrl.handle(Event::Stop);
    assert!(ctrl.handle(Event::Shutdown).is_empty());
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
        max_spend: Some(7),
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
    assert_eq!(ctrl.progress().spent, 6);
}

#[test]
fn out_of_funds_when_balance_below_cost() {
    let mut ctrl = started(Limits::default());
    let actions = ctrl.handle(snap(dud_shop(Some(meta(2, 3))), 1));
    assert_eq!(actions, vec![Action::Halt(StopReason::OutOfFunds)]);

    // Boundary: balance == cost still affords one refresh.
    let mut ctrl = started(Limits::default());
    assert_eq!(
        ctrl.handle(snap(dud_shop(Some(meta(3, 3))), 1)),
        vec![Action::Refresh]
    );
}

#[test]
fn stop_reason_priority_order() {
    let all_limits = Limits {
        max_refreshes: Some(0),
        max_spend: Some(0),
        max_matches: None,
        max_duration_ms: Some(0),
    };

    // Everything triggered at once: OutOfFunds wins.
    let mut ctrl = started(all_limits.clone());
    let actions = ctrl.handle(snap(dud_shop(Some(meta(2, 3))), 100));
    assert_eq!(actions, vec![Action::Halt(StopReason::OutOfFunds)]);

    // No funds info: MaxRefreshes beats MaxSpend and Timeout.
    let mut ctrl = started(all_limits);
    let actions = ctrl.handle(snap(dud_shop(None), 100));
    assert_eq!(actions, vec![Action::Halt(StopReason::MaxRefreshes)]);

    // MaxSpend beats Timeout.
    let mut ctrl = started(Limits {
        max_spend: Some(0),
        max_duration_ms: Some(0),
        ..Limits::default()
    });
    let actions = ctrl.handle(snap(dud_shop(None), 100));
    assert_eq!(actions, vec![Action::Halt(StopReason::MaxSpend)]);

    // MaxMatches beats Timeout.
    let mut ctrl = started(Limits {
        max_matches: Some(0),
        max_duration_ms: Some(0),
        ..Limits::default()
    });
    let actions = ctrl.handle(snap(dud_shop(None), 100));
    assert_eq!(actions, vec![Action::Halt(StopReason::MaxMatches)]);
}

#[test]
fn max_matches_zero_blocks_first_refresh() {
    // Symmetric with max_refreshes_zero: an already-reached matches limit
    // gates the loop before it spends anything.
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
    // Meta arrives once (balance 9, cost 3) then never again: the debited
    // estimate must still trigger OutOfFunds, not refresh forever.
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
    // Budget 7, no meta ever: the constant 3-crystal cost tracks spend
    // from the very first refresh — two fit, a third would cross.
    let mut ctrl = started(Limits {
        max_spend: Some(7),
        ..Limits::default()
    });
    assert_eq!(ctrl.handle(snap(dud_shop(None), 1)), vec![Action::Refresh]);
    assert_eq!(ctrl.handle(snap(dud_shop(None), 2)), vec![Action::Refresh]);
    assert_eq!(ctrl.progress().spent, 6);
    let actions = ctrl.handle(snap(dud_shop(None), 3));
    assert_eq!(actions, vec![Action::Halt(StopReason::MaxSpend)]);
}

#[test]
fn wire_cost_overrides_the_constant() {
    // A server-sent cost of 5 replaces the constant: one refresh fits
    // the budget of 7, the next would cross.
    let mut ctrl = started(Limits {
        max_spend: Some(7),
        ..Limits::default()
    });
    assert_eq!(
        ctrl.handle(snap(dud_shop(Some(meta(100, 5))), 1)),
        vec![Action::Refresh]
    );
    assert_eq!(ctrl.progress().spent, 5);
    let actions = ctrl.handle(snap(dud_shop(Some(meta(95, 5))), 2));
    assert_eq!(actions, vec![Action::Halt(StopReason::MaxSpend)]);
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
        item.id += 100; // a new roll, not a deduped re-send
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
    ctrl.handle(buy(100, 2));
    ctrl.handle(buy(101, 3));
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
    ctrl.handle(snap(hit_shop(None), 10));
    assert_eq!(ctrl.status(), Status::Paused);
    assert!(ctrl.handle(Event::Tick { now_ms: 999 }).is_empty());
    let actions = ctrl.handle(Event::Tick { now_ms: 1_000 });
    assert_eq!(actions, vec![Action::Halt(StopReason::Timeout)]);
    assert_eq!(ctrl.status(), Status::Stopped(StopReason::Timeout));
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
    ctrl.handle(Event::Start { now_ms: 1_000 });
    // A now_ms earlier than the start saturates to zero elapsed.
    assert!(ctrl.handle(Event::Tick { now_ms: 500 }).is_empty());
    assert_eq!(ctrl.status(), Status::Watching);
}

#[test]
fn filter_changed_applies_next_snapshot_only() {
    let mut ctrl = started(Limits::default());
    assert_eq!(ctrl.handle(snap(dud_shop(None), 1)), vec![Action::Refresh]);
    // No re-evaluation of the stored snapshot on the spot.
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
    // Three matches: trackable, id omitted, sold out. Exactly the targets
    // with `id: Some` form the checklist — the actuator clicks what the
    // auto-resume waits on, nothing else.
    let filter = Filter {
        kinds: vec![Equipment],
        include_sold_out: true,
        ..Filter::default()
    };
    let mut ctrl = Controller::new(filter, Limits::default());
    ctrl.handle(Event::Start { now_ms: 0 });
    let mut shop = shop(
        &[Equipment, Equipment, Equipment, Token, Token, Token],
        None,
    );
    shop.slots[0].id = 100;
    shop.slots[2].limit = Some(PurchaseLimit {
        remaining: 0,
        total: 1,
    });
    shop.slots[2].id = 102;
    let actions = ctrl.handle(snap(shop, 1));
    assert_eq!(
        actions,
        vec![Action::Buy {
            targets: vec![target(1, Some(100)), target(2, None), target(3, None)]
        }]
    );
    assert_eq!(ctrl.checklist(), &[100]);
}
