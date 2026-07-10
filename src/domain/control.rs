//! The refresh-loop controller: a pure state machine that confronts each shop
//! snapshot with the player's [`Filter`] and decides whether to refresh again,
//! pause for a purchase, or stop.
//!
//! Purity: time is injected through the events' `now_ms` (assumed monotonic)
//! and the controller performs no I/O — executing the returned [`Action`]s is
//! the caller's job. The client-side [`Filter`] is authoritative.

use serde::Deserialize;

use crate::domain::filter::Filter;
use crate::domain::shop::{RefreshMeta, ShopSnapshot, SubStat};

/// A refresh always costs 3 crystals (game fact); a wire-sent cost overrides.
const REFRESH_COST_CRYSTALS: u32 = 3;

/// Stop limits, all optional; the loop halts at the first one reached.
///
/// Deserialized from the config file's `[limits]` section; unknown keys are
/// rejected because a misspelled limit is a limit that never triggers.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
    /// A slot matched; waiting for purchases. Auto-resumes when the last
    /// checklist item is bought; a genuinely new shop (in-game refresh,
    /// hourly rotation) is re-evaluated.
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
    Snapshot {
        snapshot: ShopSnapshot,
        now_ms: u64,
    },
    /// A server-confirmed buy: checks the item off the checklist; clearing
    /// the last entry auto-resumes the loop.
    Purchase {
        /// Global catalog id, same space as `ShopItem::id`; `0` when omitted.
        item: u32,
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
/// reaction to a no-match snapshot or the purchase clearing the last
/// checklist entry; duplicate snapshots and snapshots received while
/// unarmed (`Idle`/`Stopped`) never trigger one (they are still stored for
/// the view).
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
    /// Matched-but-unbought catalog ids from the last evaluated snapshot.
    checklist: Vec<u32>,
    /// Identity of the last snapshot evaluated while armed
    /// (`Watching | Paused`): an identical re-arrival is stored for the view
    /// but never re-evaluated, so it cannot double-bill a refresh. Cleared
    /// on `Start` only — surviving the post-buy auto-resume is what mutes a
    /// re-open right after a buy.
    acted_fingerprint: Option<Vec<SlotIdentity>>,
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
            checklist: Vec::new(),
            acted_fingerprint: None,
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

    /// The configured stop limits (immutable for the session).
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Matched-but-unbought catalog ids; untrackable matches (id 0, sold
    /// out) never enter it.
    pub fn checklist(&self) -> &[u32] {
        &self.checklist
    }

    pub fn handle(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::Start { now_ms } => self.on_start(now_ms),
            Event::Stop => self.on_stop(),
            Event::Snapshot { snapshot, now_ms } => self.on_snapshot(snapshot, now_ms),
            Event::Purchase { item, now_ms } => self.on_purchase(item, now_ms),
            Event::FilterChanged(filter) => {
                // Applies from the next *new* snapshot: neither the stored
                // snapshot nor a duplicate re-send is re-evaluated.
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
        self.checklist.clear();
        // A stale identity must not mute the new session's first snapshot.
        self.acted_fingerprint = None;
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

    fn on_snapshot(&mut self, snapshot: ShopSnapshot, now_ms: u64) -> Vec<Action> {
        if snapshot.refresh.is_some() {
            self.refresh_meta = snapshot.refresh;
        }
        if !matches!(self.status, Status::Watching | Status::Paused) {
            self.last_snapshot = Some(snapshot);
            return Vec::new();
        }
        // A slotless snapshot is a degraded message, not shop content.
        if snapshot.slots.is_empty() {
            self.last_snapshot = Some(snapshot);
            return Vec::new();
        }
        // Already acted on: a duplicate must never bill a second refresh
        // or re-alert.
        let fingerprint = fingerprint(&snapshot);
        if fingerprint.is_some() && fingerprint == self.acted_fingerprint {
            self.last_snapshot = Some(snapshot);
            return Vec::new();
        }
        if fingerprint.is_none() && self.status == Status::Paused {
            // Unidentifiable shop (an id is 0): a duplicate cannot be told
            // from a new shop, so never re-evaluate over a pending purchase.
            self.last_snapshot = Some(snapshot);
            return Vec::new();
        }
        if fingerprint.is_some() {
            // A fail-open arrival must not erase the last valid identity:
            // a later verbatim duplicate of that shop must still be muted.
            self.acted_fingerprint = fingerprint;
        }

        let mut matched: Vec<u8> = Vec::new();
        let mut checklist: Vec<u32> = Vec::new();
        for (index, item) in snapshot.slots.iter().enumerate() {
            if self.filter.matches(item) {
                matched.push(item.effective_slot(index));
                // Only ids a purchase echo can actually name: the id-0
                // sentinel never appears in one, and a sold-out slot cannot
                // be bought at all — neither may hold the checklist open.
                if let Some(id) = item.catalog_id()
                    && !item.is_sold_out()
                {
                    checklist.push(id);
                }
            }
        }
        self.last_snapshot = Some(snapshot);
        self.checklist = checklist;

        if matched.is_empty() {
            // A new no-match shop also unpauses (the hourly auto-refresh
            // replaced the matches).
            self.status = Status::Watching;
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

    /// A server-confirmed buy. Only meaningful while `Paused`: checks the
    /// item off the checklist; the buy clearing the last entry resumes the
    /// hunt through the limits gate.
    fn on_purchase(&mut self, item: u32, now_ms: u64) -> Vec<Action> {
        if self.status != Status::Paused {
            return Vec::new();
        }
        // Not on the checklist: an unmatched buy, a replayed echo of a
        // consumed purchase, or the id-0 sentinel.
        let Some(position) = self.checklist.iter().position(|&id| id == item) else {
            return Vec::new();
        };
        self.checklist.swap_remove(position);
        if !self.checklist.is_empty() {
            return Vec::new();
        }
        self.status = Status::Watching;
        self.refresh_or_halt(now_ms)
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

    /// Single emission point: every refresh, including the auto-resume one,
    /// is counted and debited before it goes out.
    fn emit_refresh(&mut self) -> Action {
        self.progress.refreshes += 1;
        let cost = self.refresh_cost();
        self.progress.spent = self.progress.spent.saturating_add(cost);
        if let Some(meta) = self.refresh_meta.as_mut() {
            // Keeps the affordability estimate fresh across snapshots that
            // omit meta; a server-sent meta overwrites it with truth.
            meta.crystal_balance = meta.crystal_balance.saturating_sub(cost);
        }
        Action::Refresh
    }

    /// Spend tracking never waits for a snapshot that carries meta.
    fn refresh_cost(&self) -> u32 {
        self.refresh_meta
            .map_or(REFRESH_COST_CRYSTALS, |meta| meta.cost)
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
                || self
                    .progress
                    .spent
                    .checked_add(self.refresh_cost())
                    .is_none_or(|next| next > max))
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

/// One slot's contribution to a snapshot's identity: the catalog id plus the
/// per-roll fields the filter can match on — a re-roll redrawing the same
/// catalog ids is improbable but possible, and must read as a new shop.
/// `limit` is deliberately excluded: re-opening the shop after a buy
/// re-delivers the same roll with `remaining` decremented, and that must
/// still count as the same shop.
#[derive(PartialEq)]
struct SlotIdentity {
    id: u32,
    price: Option<u32>,
    grade: Option<u8>,
    set: Option<String>,
    substats: Vec<SubStat>,
}

/// Snapshot identity for dedup: the ordered [`SlotIdentity`]s. `None` when
/// any id is the 0 sentinel — omitted ids make shops indistinguishable.
fn fingerprint(snapshot: &ShopSnapshot) -> Option<Vec<SlotIdentity>> {
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

#[cfg(test)]
mod tests {
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

    fn buy(item: u32, now_ms: u64) -> Event {
        Event::Purchase { item, now_ms }
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
    fn duplicate_hit_shop_while_paused_does_not_realert() {
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
        assert_eq!(actions, vec![Action::Alert { slots: vec![3] }]);
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
    fn sold_out_match_excluded_from_checklist() {
        // A sold-out slot can match (include_sold_out) but can never produce
        // a purchase echo: it must not hold the checklist open.
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
        assert_eq!(actions, vec![Action::Alert { slots: vec![3] }]);
        assert_eq!(ctrl.status(), Status::Paused);
        assert!(ctrl.checklist().is_empty()); // only a new shop unpauses
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
            ctrl.handle(snap(with_ids(hit_shop(None)), 1)),
            vec![Action::Alert { slots: vec![3] }]
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
