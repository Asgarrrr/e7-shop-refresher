//! The refresh-loop controller: a pure state machine that confronts each shop
//! snapshot with the player's [`Filter`] and decides whether to refresh again,
//! pause for a purchase, or stop.
//!
//! Purity: time is injected through the events' `now_ms` (assumed monotonic)
//! and the controller performs no I/O — executing the returned [`Action`]s is
//! the caller's job. The legacy server verdict (`ShopItem::interesting`) is
//! never read; the client-side [`Filter`] is authoritative.

use crate::domain::filter::Filter;
use crate::domain::shop::{RefreshMeta, ShopSnapshot};

/// Stop limits, all optional; the loop halts at the first one reached.
#[derive(Debug, Clone, Default)]
pub struct Limits {
    pub max_refreshes: Option<u32>,
    /// Crystal budget — a hard ceiling: a refresh that would cross it is
    /// never issued.
    pub max_spend: Option<u32>,
    /// Matched (alerted) items, cumulative — not purchases.
    pub max_matches: Option<u32>,
    pub max_duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    PlayerStopped,
    OutOfFunds,
    MaxRefreshes,
    MaxSpend,
    MaxMatches,
    Timeout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Watching,
    /// A slot matched; waiting for the player to buy, then `Resume`.
    Paused,
    Stopped(StopReason),
}

#[derive(Debug, Clone)]
pub enum Event {
    /// Arms the watch. The caller must deliver a snapshot *after* this:
    /// pre-Start snapshots are stored for the view, never replayed.
    Start {
        now_ms: u64,
    },
    Stop,
    /// After a purchase: back to the hunt.
    Resume {
        now_ms: u64,
    },
    Snapshot {
        snapshot: ShopSnapshot,
        now_ms: u64,
    },
    FilterChanged(Filter),
    /// Lets the duration limit expire outside snapshots (e.g. while `Paused`).
    Tick {
        now_ms: u64,
    },
}

/// Actions are to be consumed in order: on the max-matches path an `Alert`
/// precedes the `Halt` and both matter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Refresh,
    /// Shop slot numbers (1..=6 on a well-formed shop) that matched.
    Alert {
        slots: Vec<u8>,
    },
    Halt(StopReason),
}

/// Counters exposed to the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    pub refreshes: u32,
    /// Crystals committed to refreshes.
    pub spent: u32,
    /// Items matched (alerted), not purchases.
    pub matches_found: u32,
}

/// Invariant: the relay stays passive — a refresh is only requested in
/// reaction to a no-match snapshot or an explicit `Resume`; snapshots received
/// outside `Watching` never trigger one (they are still stored for the view).
pub struct Controller {
    filter: Filter,
    limits: Limits,
    status: Status,
    started_at: Option<u64>,
    progress: Progress,
    /// Last balance/cost seen this session, locally debited per refresh so
    /// the affordability estimate survives snapshots that omit meta; a
    /// server-sent meta overwrites the estimate. Forgotten on `Start`.
    refresh_meta: Option<RefreshMeta>,
    last_snapshot: Option<ShopSnapshot>,
}

impl Controller {
    pub fn new(filter: Filter, limits: Limits) -> Self {
        Self {
            filter,
            limits,
            status: Status::Idle,
            started_at: None,
            progress: Progress::default(),
            refresh_meta: None,
            last_snapshot: None,
        }
    }

    pub fn status(&self) -> Status {
        self.status
    }

    pub fn last_snapshot(&self) -> Option<&ShopSnapshot> {
        self.last_snapshot.as_ref()
    }

    pub fn progress(&self) -> Progress {
        self.progress
    }

    /// `false` when `max_spend` is set but no snapshot has carried a
    /// [`RefreshMeta`] yet: with the cost unknown, spend cannot be tracked
    /// (and `OutOfFunds` cannot trigger either).
    pub fn limits_enforceable(&self) -> bool {
        self.limits.max_spend.is_none() || self.refresh_meta.is_some()
    }

    pub fn handle(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::Start { now_ms } => self.on_start(now_ms),
            Event::Stop => self.on_stop(),
            Event::Resume { now_ms } => self.on_resume(now_ms),
            Event::Snapshot { snapshot, now_ms } => self.on_snapshot(snapshot, now_ms),
            Event::FilterChanged(filter) => {
                // Applies from the next snapshot; no re-evaluation of the last.
                self.filter = filter;
                Vec::new()
            }
            Event::Tick { now_ms } => self.on_tick(now_ms),
        }
    }

    /// Never refreshes: opening the shop is free and yields the first
    /// snapshot. Ignored mid-session so a stray `Start` cannot reset counters.
    fn on_start(&mut self, now_ms: u64) -> Vec<Action> {
        if !matches!(self.status, Status::Idle | Status::Stopped(_)) {
            return Vec::new();
        }
        self.progress = Progress::default();
        self.started_at = Some(now_ms);
        self.refresh_meta = None; // a stale balance must not stop the new session
        // last_snapshot is kept: restarting the watch does not change the shop.
        self.status = Status::Watching;
        Vec::new()
    }

    /// Idempotent once stopped: the original reason is not relabelled.
    fn on_stop(&mut self) -> Vec<Action> {
        if matches!(self.status, Status::Stopped(_)) {
            return Vec::new();
        }
        self.halt(StopReason::PlayerStopped)
    }

    /// The refresh is blind by design: snapshots that arrived while `Paused`
    /// were stored for the view but not re-matched — resuming asserts the
    /// player is done with the current shop.
    fn on_resume(&mut self, now_ms: u64) -> Vec<Action> {
        if self.status != Status::Paused {
            return Vec::new();
        }
        self.status = Status::Watching;
        self.refresh_or_halt(now_ms)
    }

    fn on_snapshot(&mut self, snapshot: ShopSnapshot, now_ms: u64) -> Vec<Action> {
        if snapshot.refresh.is_some() {
            self.refresh_meta = snapshot.refresh;
        }
        if self.status != Status::Watching {
            self.last_snapshot = Some(snapshot);
            return Vec::new();
        }
        let matched: Vec<u8> = snapshot
            .slots
            .iter()
            .enumerate()
            .filter(|(_, item)| self.filter.matches(item))
            .map(|(index, item)| {
                // Position fallback when the server omits the slot; clamped
                // so an oversized shop cannot wrap back into the `0` sentinel.
                if item.slot == 0 {
                    u8::try_from(index + 1).unwrap_or(u8::MAX)
                } else {
                    item.slot
                }
            })
            .collect();
        self.last_snapshot = Some(snapshot);

        if matched.is_empty() {
            return self.refresh_or_halt(now_ms);
        }
        // A match means a purchase to make: never refresh over it.
        self.progress.matches_found = self
            .progress
            .matches_found
            .saturating_add(matched.len() as u32);
        let alert = Action::Alert { slots: matched };
        if self
            .limits
            .max_matches
            .is_some_and(|max| self.progress.matches_found >= max)
        {
            let mut actions = vec![alert];
            actions.extend(self.halt(StopReason::MaxMatches));
            actions
        } else {
            self.status = Status::Paused;
            vec![alert]
        }
    }

    fn on_tick(&mut self, now_ms: u64) -> Vec<Action> {
        if !matches!(self.status, Status::Watching | Status::Paused) {
            return Vec::new();
        }
        if self.duration_elapsed(now_ms) {
            self.halt(StopReason::Timeout)
        } else {
            Vec::new()
        }
    }

    /// The gate in front of the single emission point: halt at the first
    /// limit reached, refresh otherwise.
    fn refresh_or_halt(&mut self, now_ms: u64) -> Vec<Action> {
        match self.stop_reason(now_ms) {
            Some(reason) => self.halt(reason),
            None => vec![self.emit_refresh()],
        }
    }

    /// Single emission point: every refresh, including the one after a
    /// `Resume`, is counted before it goes out.
    fn emit_refresh(&mut self) -> Action {
        self.progress.refreshes += 1;
        if let Some(meta) = self.refresh_meta.as_mut() {
            self.progress.spent = self.progress.spent.saturating_add(meta.cost);
            // Keeps the affordability estimate fresh across snapshots that
            // omit meta; a server-sent meta overwrites it with truth.
            meta.crystal_balance = meta.crystal_balance.saturating_sub(meta.cost);
        }
        Action::Refresh
    }

    fn halt(&mut self, reason: StopReason) -> Vec<Action> {
        self.status = Status::Stopped(reason);
        vec![Action::Halt(reason)]
    }

    /// Checked before every refresh; the order below is the priority order.
    fn stop_reason(&self, now_ms: u64) -> Option<StopReason> {
        if let Some(meta) = self.refresh_meta
            && meta.crystal_balance < meta.cost
        {
            return Some(StopReason::OutOfFunds);
        }
        if let Some(max) = self.limits.max_refreshes
            && self.progress.refreshes >= max
        {
            return Some(StopReason::MaxRefreshes);
        }
        // Hard ceiling: also stop when the *next* refresh would cross it.
        if let Some(max) = self.limits.max_spend
            && (self.progress.spent >= max
                || self.refresh_meta.is_some_and(|meta| {
                    self.progress
                        .spent
                        .checked_add(meta.cost)
                        .is_none_or(|next| next > max)
                }))
        {
            return Some(StopReason::MaxSpend);
        }
        if let Some(max) = self.limits.max_matches
            && self.progress.matches_found >= max
        {
            return Some(StopReason::MaxMatches);
        }
        if self.duration_elapsed(now_ms) {
            return Some(StopReason::Timeout);
        }
        None
    }

    /// `saturating_sub`: a `now_ms` reported earlier than the start counts as
    /// zero elapsed rather than underflowing.
    fn duration_elapsed(&self, now_ms: u64) -> bool {
        self.limits.max_duration_ms.is_some_and(|max| {
            self.started_at
                .is_some_and(|started| now_ms.saturating_sub(started) >= max)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::shop::ItemKind::{self, Equipment, Token};
    use crate::domain::shop::ShopItem;

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

    fn snap(snapshot: ShopSnapshot, now_ms: u64) -> Event {
        Event::Snapshot { snapshot, now_ms }
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
    fn first_snapshot_no_match_emits_single_refresh() {
        let mut ctrl = started(Limits::default());
        let actions = ctrl.handle(snap(dud_shop(Some(meta(100, 3))), 1));
        assert_eq!(actions, vec![Action::Refresh]);
        assert_eq!(ctrl.progress().refreshes, 1);
        assert_eq!(ctrl.progress().spent, 3);
    }

    #[test]
    fn snapshot_match_alerts_and_pauses_without_refresh() {
        let mut ctrl = started(Limits::default());
        let actions = ctrl.handle(snap(hit_shop(None), 1));
        assert_eq!(actions, vec![Action::Alert { slots: vec![3] }]);
        assert_eq!(ctrl.status(), Status::Paused);
        assert_eq!(ctrl.progress().refreshes, 0);
    }

    #[test]
    fn resume_refresh_is_counted() {
        let mut ctrl = started(Limits {
            max_refreshes: Some(2),
            ..Limits::default()
        });
        for now in [1, 3] {
            ctrl.handle(snap(hit_shop(None), now));
            let actions = ctrl.handle(Event::Resume { now_ms: now + 1 });
            assert_eq!(actions, vec![Action::Refresh]);
        }
        assert_eq!(ctrl.progress().refreshes, 2);
        // The third one must hit the limit, not silently overshoot it.
        ctrl.handle(snap(hit_shop(None), 5));
        let actions = ctrl.handle(Event::Resume { now_ms: 6 });
        assert_eq!(actions, vec![Action::Halt(StopReason::MaxRefreshes)]);
    }

    #[test]
    fn snapshot_ignored_while_paused() {
        let mut ctrl = started(Limits::default());
        ctrl.handle(snap(hit_shop(None), 1));
        let actions = ctrl.handle(snap(dud_shop(None), 2));
        assert!(actions.is_empty());
        assert_eq!(ctrl.status(), Status::Paused);
        assert_eq!(ctrl.progress().refreshes, 0);
        // Still stored for the view.
        assert_eq!(ctrl.last_snapshot().unwrap().slots[2].kind, Token);
    }

    #[test]
    fn resume_ignored_unless_paused() {
        let mut watching = started(Limits::default());
        assert!(watching.handle(Event::Resume { now_ms: 1 }).is_empty());
        assert_eq!(watching.progress().refreshes, 0);

        let mut idle = controller(Limits::default());
        assert!(idle.handle(Event::Resume { now_ms: 1 }).is_empty());
        assert_eq!(idle.status(), Status::Idle);
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
    fn spend_limit_unenforceable_when_refresh_meta_absent() {
        let mut ctrl = started(Limits {
            max_spend: Some(6),
            ..Limits::default()
        });
        assert!(!ctrl.limits_enforceable());
        // Cost unknown: refreshes flow, spend cannot be tracked.
        for now in 1..=5 {
            assert_eq!(
                ctrl.handle(snap(dud_shop(None), now)),
                vec![Action::Refresh]
            );
        }
        assert_eq!(ctrl.progress().spent, 0);

        // The first snapshot carrying the cost engages the limit.
        assert_eq!(
            ctrl.handle(snap(dud_shop(Some(meta(100, 3))), 6)),
            vec![Action::Refresh]
        );
        assert!(ctrl.limits_enforceable());
        assert_eq!(ctrl.progress().spent, 3);
        assert_eq!(
            ctrl.handle(snap(dud_shop(Some(meta(97, 3))), 7)),
            vec![Action::Refresh]
        );
        let actions = ctrl.handle(snap(dud_shop(Some(meta(94, 3))), 8));
        assert_eq!(actions, vec![Action::Halt(StopReason::MaxSpend)]);
    }

    #[test]
    fn matches_found_increments_by_matched_len() {
        let mut ctrl = started(Limits::default());
        let two_hits = shop(&[Equipment, Token, Equipment, Token, Token, Token], None);
        let actions = ctrl.handle(snap(two_hits, 1));
        assert_eq!(actions, vec![Action::Alert { slots: vec![1, 3] }]);
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
            ctrl.handle(snap(hit_shop(None), 1)),
            vec![Action::Alert { slots: vec![3] }]
        );
        assert_eq!(ctrl.status(), Status::Paused);
        ctrl.handle(Event::Resume { now_ms: 2 });
        let actions = ctrl.handle(snap(hit_shop(None), 3));
        assert_eq!(
            actions,
            vec![
                Action::Alert { slots: vec![3] },
                Action::Halt(StopReason::MaxMatches),
            ]
        );
        assert_eq!(ctrl.status(), Status::Stopped(StopReason::MaxMatches));
    }

    #[test]
    fn single_shop_multi_match_stops() {
        let mut ctrl = started(Limits {
            max_matches: Some(2),
            ..Limits::default()
        });
        let triple = shop(
            &[Equipment, Equipment, Equipment, Token, Token, Token],
            None,
        );
        let actions = ctrl.handle(snap(triple, 1));
        assert_eq!(
            actions,
            vec![
                Action::Alert {
                    slots: vec![1, 2, 3]
                },
                Action::Halt(StopReason::MaxMatches),
            ]
        );
        assert_eq!(ctrl.progress().matches_found, 3);
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
            vec![Action::Alert {
                slots: vec![1, 2, 3, 4, 5, 6]
            }]
        );
        assert_eq!(ctrl.status(), Status::Paused);
    }

    #[test]
    fn alert_slot_falls_back_to_position_when_zero() {
        let mut ctrl = started(Limits::default());
        let mut shop = hit_shop(None);
        shop.slots[2].slot = 0; // server omitted the slot number
        let actions = ctrl.handle(snap(shop, 1));
        assert_eq!(actions, vec![Action::Alert { slots: vec![3] }]);
    }

    #[test]
    fn alert_slot_clamps_oversized_position() {
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
            vec![Action::Alert {
                slots: vec![u8::MAX]
            }]
        );
    }
}
