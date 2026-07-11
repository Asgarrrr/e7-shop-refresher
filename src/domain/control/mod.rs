//! The refresh-loop controller: a pure state machine that confronts each shop
//! snapshot with the player's [`Filter`] and decides whether to refresh again,
//! pause for a purchase, or stop.
//!
//! Purity: time is injected through the events' `now_ms` (assumed monotonic)
//! and the controller performs no I/O — executing the returned [`Action`]s is
//! the caller's job. The client-side [`Filter`] is authoritative.

mod dedup;
#[cfg(test)]
mod tests;

use serde::Deserialize;

use crate::domain::filter::Filter;
use crate::domain::shop::{RefreshMeta, ShopSnapshot};

use dedup::{SlotIdentity, fingerprint};

/// A refresh always costs 3 crystals (game fact); a wire-sent cost overrides.
const REFRESH_COST_CRYSTALS: u32 = 3;

/// Stop limits, all optional; the loop halts at the first one reached.
///
/// Deserialized from the config file's `[limits]` section; unknown keys are
/// rejected because a misspelled limit is a limit that never triggers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
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
    /// The relay pipeline ended (capture or uplink gone) while armed: the
    /// player did not stop the hunt and must not be told they did.
    SessionEnded,
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
    /// The player asked to stop.
    Stop,
    /// The relay is going away underneath the loop (uplink or capture gone):
    /// same halt as `Stop`, honest label.
    Shutdown,
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
    /// The player retuned the stop limits mid-session (from the GUI).
    LimitsChanged(Limits),
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
    /// The event was rejected; nothing changed. Callers render the reason —
    /// enforcement and messaging come from the same decision.
    Refused(RefusalReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    /// Arming with — or swapping to — a filter that matches everything.
    UnrestrictedFilter,
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

    /// The active stop limits.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The active interest filter.
    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    /// The refresh meta the stop logic enforces: server-sent, locally debited
    /// per advised refresh, discarded as stale on `Start`. This — not the raw
    /// snapshot — is what a view must display.
    pub fn refresh_meta(&self) -> Option<RefreshMeta> {
        self.refresh_meta
    }

    /// Matched-but-unbought catalog ids; untrackable matches (id 0, sold
    /// out) never enter it.
    pub fn checklist(&self) -> &[u32] {
        &self.checklist
    }

    pub fn handle(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::Start { now_ms } => self.on_start(now_ms),
            Event::Stop => self.on_halt_request(StopReason::PlayerStopped),
            Event::Shutdown => self.on_halt_request(StopReason::SessionEnded),
            Event::Snapshot { snapshot, now_ms } => self.on_snapshot(snapshot, now_ms),
            Event::Purchase { item, now_ms } => self.on_purchase(item, now_ms),
            Event::FilterChanged(filter) => {
                // An unrestricted filter is never accepted — armed, it would
                // match every slot of every shop. Accepted swaps apply from
                // the next *new* snapshot: neither the stored snapshot nor a
                // duplicate re-send is re-evaluated.
                if filter.is_unrestricted() {
                    return vec![Action::Refused(RefusalReason::UnrestrictedFilter)];
                }
                self.filter = filter;
                Vec::new()
            }
            Event::LimitsChanged(limits) => {
                // Applies at the next check-point (snapshot or tick): the
                // event itself never halts retroactively.
                self.limits = limits;
                Vec::new()
            }
            Event::Tick { now_ms } => self.on_tick(now_ms),
        }
    }

    /// Never refreshes: opening the shop is free and yields the first
    /// snapshot. Ignored mid-session so a stray `Start` cannot reset
    /// counters, and refused while the filter is unrestricted — the loop
    /// must never hunt without a target, whoever sent the event.
    fn on_start(&mut self, now_ms: u64) -> Vec<Action> {
        if !matches!(self.status, Status::Idle | Status::Stopped(_)) {
            return Vec::new();
        }
        if self.filter.is_unrestricted() {
            return vec![Action::Refused(RefusalReason::UnrestrictedFilter)];
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

    /// Idempotent once stopped (the original reason is not relabelled), and a
    /// no-op while Idle: a session that never ran did not stop — the
    /// invariant lives here, not at the callers, so every Stop producer
    /// (console, GUI button, teardown) gets it for free.
    fn on_halt_request(&mut self, reason: StopReason) -> Vec<Action> {
        if !matches!(self.status, Status::Watching | Status::Paused) {
            return Vec::new();
        }
        self.halt(reason)
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

    /// A tick is a check-point: a limit tightened mid-session (or an elapsed
    /// timer) takes effect here, without waiting for the next snapshot.
    ///
    /// While `Watching`, every stop reason applies — an already-exceeded
    /// `max_refreshes`/`max_spend`/`max_matches` must not linger with the gate
    /// on. While `Paused` the loop is waiting on a purchase, so only the
    /// timeout applies; the other limits are re-checked when the buy or a new
    /// shop resumes the hunt (`refresh_or_halt`), which avoids abandoning a
    /// still-buyable pause on, say, an out-of-crystals estimate.
    fn on_tick(&mut self, now_ms: u64) -> Vec<Action> {
        match self.status {
            Status::Watching => match self.stop_reason(now_ms) {
                Some(reason) => self.halt(reason),
                None => Vec::new(),
            },
            Status::Paused if self.duration_elapsed(now_ms) => self.halt(StopReason::Timeout),
            _ => Vec::new(),
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
    pub fn refresh_cost(&self) -> u32 {
        self.refresh_meta
            .map_or(REFRESH_COST_CRYSTALS, |meta| meta.cost)
    }

    fn halt(&mut self, reason: StopReason) -> Vec<Action> {
        self.status = Status::Stopped(reason);
        // A stopped hunt has no pending purchases: snapshots stored while
        // Stopped are never evaluated, and catalog ids are stable per item,
        // so a surviving checklist would keep painting yesterday's matches
        // as "wanted" in the view.
        self.checklist.clear();
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
