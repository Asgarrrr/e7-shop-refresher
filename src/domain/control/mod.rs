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
mod watchdog;

// Re-exported for `app::session`'s test suite, which drives the same recovery
// ladder from the outside and would otherwise re-spell its deadlines as
// literals. `watchdog` itself stays private: only the derived deadline escapes,
// never the two windows it is derived from.
#[cfg(test)]
pub(crate) use watchdog::past_rung;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::filter::Filter;
use crate::domain::shop::{RefreshMeta, ShopSnapshot, catalog_id};

use dedup::{Fingerprint, fingerprint};
use watchdog::Expectation;

/// A refresh always costs 3 crystals (game fact); a wire-sent cost overrides.
const REFRESH_COST_CRYSTALS: u32 = 3;

/// A watchdog-issued retry. Deliberately distinct from bare
/// `Refresh`/`Buy`: recovery reaches the caller from a tick, which carries
/// no game-animation trigger — bare variants there would render as advice
/// and submit nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// Blind confirm re-click after a missed refresh confirm — free and
    /// safe: nothing clickable sits under the modal zone when it is closed
    /// (player-confirmed game fact).
    ConfirmRefresh,
    /// Blind confirm re-click after a missed buy confirm.
    ConfirmBuy,
    /// Full refresh re-issue, counted and debited like any other refresh.
    Refresh,
    /// Full re-issue of the outstanding buys, rebuilt by identity from the
    /// checklist — never re-filtered.
    Buy { targets: Vec<BuyTarget> },
}

/// Stop limits, all optional; the loop halts at the first one reached.
///
/// Deserialized from the config file's `[limits]` section; unknown keys are
/// rejected because a misspelled limit is a limit that never triggers.
/// `Copy` because it is four `Option`s of plain integers (40 bytes, no
/// allocation): the GUI passes it around per frame and a `.clone()` there was
/// only ever noise.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_refreshes: Option<u32>,
    /// Crystal budget — a hard ceiling: a refresh that would cross it is
    /// never issued.
    pub max_spend: Option<u32>,
    /// Matched items, cumulative — not purchases. Reached by a match, the
    /// halt waits for that match's pause to resolve: the found items are
    /// bought, then the loop stops instead of resuming.
    pub max_matches: Option<u32>,
    pub max_duration_ms: Option<u64>,
}

/// Why a hunt stopped. Every variant is player-facing (rendered by
/// `render::describe`), so the label must stay honest about *who* stopped it:
/// the player, a machine fault, or a limit the player set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    PlayerStopped,
    /// The relay pipeline ended (capture or uplink gone) while armed: the
    /// player did not stop the hunt and must not be told they did.
    SessionEnded,
    /// The click executor could not act safely: a machine fault, not the
    /// player's stop.
    ActuatorFailed,
    /// The game answered nothing through the whole recovery ladder.
    Unresponsive,
    OutOfFunds,
    MaxRefreshes,
    MaxSpend,
    MaxMatches,
    Timeout,
}

/// Where the refresh loop is. Only `Watching` and `Paused` are *armed*: every
/// gate in this module tests for that pair, and events arriving outside it are
/// stored for the view but never acted on.
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

/// Everything that can move the loop: player commands, decoded wire arrivals,
/// link transitions, and the clock. The only input to [`Controller::handle`], and
/// the only way time enters the state machine (`now_ms`, assumed monotonic).
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
    /// The click executor cannot act safely: same halt, honest label.
    ActuatorFailed,
    Snapshot {
        snapshot: ShopSnapshot,
        now_ms: u64,
    },
    /// A server-confirmed buy: checks the item off the checklist; clearing
    /// the last entry auto-resumes the loop.
    Purchase {
        /// Global catalog id, same space as `ShopItem::id`; `0` when omitted.
        item: u32,
        /// Gold balance after the buy — feeds the affordability planning of
        /// the next matches.
        gold: Option<u32>,
        now_ms: u64,
    },
    FilterChanged(Filter),
    /// The player retuned the stop limits mid-session (from the GUI).
    LimitsChanged(Limits),
    /// The uplink dropped: no proof can arrive, so the watchdog must not
    /// escalate over a dead wire (the reconnect backoff alone outlasts the
    /// whole ladder).
    LinkDown,
    /// The uplink is back; a pending expectation gets a fresh full deadline.
    LinkUp {
        now_ms: u64,
    },
    /// Lets the duration limit expire outside snapshots (e.g. while `Paused`).
    Tick {
        now_ms: u64,
    },
}

/// One matched slot. `slot` is the sorted display position (1..=6 on a
/// well-formed shop; row = slot − 1). `id` is `Some` exactly when the item is
/// buyable AND a purchase echo can name it — i.e. exactly the checklist
/// entries; an `id: None` target is display-only, never clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuyTarget {
    pub slot: u8,
    pub id: Option<u32>,
}

/// Actions are to be consumed in order: a `Buy` can precede another action
/// in the same batch and both matter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Refresh,
    /// The matched slots to buy.
    Buy {
        targets: Vec<BuyTarget>,
    },
    /// A watchdog retry to execute (recovery-enabled sessions only).
    Recover(Recovery),
    Halt(StopReason),
    /// The event was rejected; nothing changed. Callers render the reason —
    /// enforcement and messaging come from the same decision.
    Refused(RefusalReason),
}

impl Action {
    /// Whether this action is the domain's explicit rejection verdict. Callers
    /// deciding "was the event applied?" test for this, never the emptiness of
    /// the action list — an accepted event that grows an action later must not
    /// silently read as refused.
    pub fn is_refusal(&self) -> bool {
        matches!(self, Action::Refused(_))
    }
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
    /// Items matched, not purchases.
    pub matches_found: u32,
}

/// Confirmed purchases this run, tallied by the bought item's wire name so a
/// view can group them into headline tokens (Covenant, Mystic, …) and a
/// generic bucket. Buys that carry no name in their roll land in `untitled`.
/// Reset on `Start`, like `Progress`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Haul {
    named: BTreeMap<String, u32>,
    untitled: u32,
}

impl Haul {
    /// How many bought under this exact wire name.
    pub fn count(&self, name: &str) -> u32 {
        self.named.get(name).copied().unwrap_or(0)
    }

    /// Everything not one of `known`: named-but-unlisted buys plus the
    /// unresolved/nameless ones. The view passes its headline names here.
    pub fn others(&self, known: &[&str]) -> u32 {
        let bucketed = self
            .named
            .iter()
            .filter(|(name, _)| !known.contains(&name.as_str()))
            .map(|(_, count)| *count)
            .fold(0u32, u32::saturating_add);
        bucketed.saturating_add(self.untitled)
    }

    /// Record one confirmed buy, keyed by the item's name when it resolved.
    fn record(&mut self, name: Option<String>) {
        match name {
            Some(name) => {
                let count = self.named.entry(name).or_insert(0);
                *count = count.saturating_add(1);
            }
            None => self.untitled = self.untitled.saturating_add(1),
        }
    }
}

/// Invariant: refreshes are reactive — one is only requested in reaction to
/// a no-match snapshot or the purchase clearing the last checklist entry;
/// duplicate snapshots and snapshots received while unarmed
/// (`Idle`/`Stopped`) never trigger one (they are still stored for the
/// view).
pub struct Controller {
    filter: Filter,
    limits: Limits,
    status: Status,
    started_at: Option<u64>,
    progress: Progress,
    /// Confirmed buys this run, grouped by item name for the haul readout.
    haul: Haul,
    /// Last balance/cost seen this session, locally debited per refresh so
    /// the affordability estimate survives snapshots that omit meta; a
    /// server-sent meta overwrites the estimate. Forgotten on `Start`.
    refresh_meta: Option<RefreshMeta>,
    /// Last gold balance echoed by a purchase; the next matches' buys are
    /// planned against it. `None` (nothing echoed yet) restricts nothing,
    /// and `Start` forgets it — a stale balance must not veto buys.
    gold_balance: Option<u32>,
    last_snapshot: Option<ShopSnapshot>,
    /// Matched-but-unbought catalog ids from the last evaluated snapshot.
    checklist: Vec<u32>,
    /// Identity of the last snapshot evaluated while armed
    /// (`Watching | Paused`): an identical re-arrival is stored for the view
    /// but never re-evaluated, so it cannot double-bill a refresh. Cleared
    /// on `Start` only — surviving the post-buy auto-resume is what mutes a
    /// re-open right after a buy.
    acted_fingerprint: Option<Fingerprint>,
    /// Catalog ids bought in the current roll. The wire never says sold-out
    /// and ids are stable per item type, so this is the only guard against
    /// re-buying an already-bought slot. Both fields survive `Start`: a
    /// restart clears `acted_fingerprint`, and keying the clear off that
    /// alone would let a same-roll re-open wrongly forget the buys.
    bought: Vec<u32>,
    /// The roll `bought` is scoped to; a snapshot with a different identity
    /// is fresh stock and empties the set. Shares the `Arc` with
    /// `acted_fingerprint` rather than deep-copying every slot's strings — the
    /// two hold the same value whenever both are set.
    bought_fingerprint: Option<Fingerprint>,
    /// Watchdog armed. Only ever true for live actuation: Off is
    /// player-paced advice and `DryRun` never yields wire feedback — deadlines
    /// would self-halt both.
    recovery: bool,
    /// The proof the watchdog currently waits on; `None` = quiet.
    expectation: Option<Expectation>,
    /// Deadlines only run while the uplink can deliver the proof.
    link_up: bool,
}

impl Controller {
    pub fn new(filter: Filter, limits: Limits) -> Self {
        Self {
            filter,
            limits,
            status: Status::Idle,
            started_at: None,
            progress: Progress::default(),
            haul: Haul::default(),
            refresh_meta: None,
            gold_balance: None,
            last_snapshot: None,
            checklist: Vec::new(),
            acted_fingerprint: None,
            bought: Vec::new(),
            bought_fingerprint: None,
            recovery: false,
            expectation: None,
            link_up: true,
        }
    }

    /// Arms the recovery watchdog: every issued refresh/buy gets an
    /// expectation deadline, escalating nudge → re-issue → honest halt.
    /// Called once at wiring time, only when the actuator really clicks
    /// (`Mode::Live`).
    pub fn enable_recovery(&mut self) {
        self.recovery = true;
    }

    /// Whether the recovery watchdog is armed (set once at wiring time).
    pub fn is_recovery_enabled(&self) -> bool {
        self.recovery
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

    pub fn haul(&self) -> &Haul {
        &self.haul
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

    /// Last gold balance echoed by a purchase this run, if any; `None` before
    /// the first buy and again after `Start` clears it. What a view displays.
    pub fn gold_balance(&self) -> Option<u32> {
        self.gold_balance
    }

    /// The returned actions **are** the decision; the state mutation alone is
    /// not the output. `clippy::must_use_candidate` structurally cannot flag a
    /// `&mut self` method, so this attribute is the only thing that will ever
    /// catch a dropped refresh or a dropped buy.
    #[must_use = "these actions are the decision — dropping them loses the refresh or the buy"]
    pub fn handle(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::Start { now_ms } => self.on_start(now_ms),
            Event::Stop => self.on_halt_request(StopReason::PlayerStopped),
            Event::Shutdown => self.on_halt_request(StopReason::SessionEnded),
            Event::ActuatorFailed => self.on_halt_request(StopReason::ActuatorFailed),
            Event::Snapshot { snapshot, now_ms } => self.on_snapshot(snapshot, now_ms),
            Event::Purchase { item, gold, now_ms } => self.on_purchase(item, gold, now_ms),
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
            Event::LinkDown => {
                self.link_up = false;
                Vec::new()
            }
            Event::LinkUp { now_ms } => self.on_link_up(now_ms),
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
        self.haul = Haul::default(); // last run's haul is not this run's
        self.started_at = Some(now_ms);
        self.refresh_meta = None; // a stale balance must not stop the new session
        self.gold_balance = None; // nor a stale purse veto its buys
        self.checklist.clear();
        // A stale identity must not mute the new session's first snapshot.
        self.acted_fingerprint = None;
        // Nor may a stale deadline fire into the new session.
        self.expectation = None;
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
        // Decide against a borrow, then store once: every snapshot is kept for
        // the view whatever branch decided, so no per-path store can drift.
        let actions = self.evaluate_snapshot(&snapshot, now_ms);
        self.last_snapshot = Some(snapshot);
        actions
    }

    /// The refresh/buy/mute decision for a snapshot already recognised as
    /// current. Mutates loop state but never stores the snapshot — its caller
    /// owns the single store point.
    fn evaluate_snapshot(&mut self, snapshot: &ShopSnapshot, now_ms: u64) -> Vec<Action> {
        if !matches!(self.status, Status::Watching | Status::Paused) {
            return Vec::new();
        }
        // A slotless snapshot is a degraded message, not shop content.
        if snapshot.slots.is_empty() {
            return Vec::new();
        }
        // Already acted on: a duplicate must never bill a second refresh
        // or re-buy.
        let fingerprint = fingerprint(snapshot);
        if fingerprint.is_some() && fingerprint == self.acted_fingerprint {
            return Vec::new();
        }
        if fingerprint.is_none() && self.status == Status::Paused {
            // Unidentifiable shop (an id is 0): a duplicate cannot be told
            // from a new shop, so never re-evaluate over a pending purchase.
            return Vec::new();
        }
        if fingerprint.is_some() {
            // A fail-open arrival must not erase the last valid identity:
            // a later verbatim duplicate of that shop must still be muted.
            self.acted_fingerprint = fingerprint;
            if self.acted_fingerprint != self.bought_fingerprint {
                self.bought.clear(); // new roll = fresh stock
                // A refcount bump, not a second deep copy of the roll.
                self.bought_fingerprint = self.acted_fingerprint.clone();
            }
        }
        // The awaited proof (or its superseder) arrived: disarm the watchdog.
        // Duplicates and degraded arrivals returned above — a re-open is not
        // a re-roll. Pause-entry and emit_refresh re-arm below.
        self.expectation = None;

        let (targets, buyable) = self.plan_targets(snapshot);
        // Alignment by construction: the checklist is exactly the clickable
        // targets, so an actuator buying `id: Some` targets clears the pause.
        self.checklist = targets.iter().filter_map(|target| target.id).collect();

        if targets.is_empty() {
            // A new no-match shop also unpauses (the hourly auto-refresh
            // replaced the matches).
            self.status = Status::Watching;
            return self.refresh_or_halt(now_ms);
        }
        self.progress.matches_found = self
            .progress
            .matches_found
            .saturating_add(targets.len() as u32);
        if !buyable {
            // Every match is dead stock — sold out or beyond the known gold:
            // nobody can buy any of it, and pausing would park the loop until
            // the hourly rotation. Show the match and keep hunting.
            self.status = Status::Watching;
            let mut actions = vec![Action::Buy { targets }];
            actions.extend(self.refresh_or_halt(now_ms));
            return actions;
        }
        // A match means a purchase to make: never refresh over it — and
        // never halt over it either. The matched items are the hunt's very
        // goal, so a reached `max_matches` does not fire here: the pause
        // resolves first (the items get bought) and the limit lands at the
        // next gate, which re-checks every stop reason.
        self.status = Status::Paused;
        if self.recovery && !self.checklist.is_empty() {
            // Only echo-clearable pauses get a deadline: an untrackable
            // (empty-checklist) pause waits on the player, not the game.
            self.expectation = Some(Expectation::purchase(now_ms));
        }
        vec![Action::Buy { targets }]
    }

    /// The matched slots, each clickable or display-only, planned against
    /// the last echoed gold balance debited in click order (the second 184k
    /// bookmark of a 200k purse is out of reach). An unknown balance or
    /// price fails open. The flag says whether anything at all is in reach
    /// of a buy — the tool's or the player's.
    fn plan_targets(&self, snapshot: &ShopSnapshot) -> (Vec<BuyTarget>, bool) {
        let mut targets = Vec::new();
        let mut gold = self.gold_balance;
        let mut buyable = false;
        for (index, item) in snapshot.slots.iter().enumerate() {
            if !self.filter.matches(item) {
                continue;
            }
            let affordable = match (gold, item.price) {
                (Some(balance), Some(price)) => price <= balance,
                _ => true,
            };
            let already_bought = item
                .catalog_id()
                .is_some_and(|id| self.bought.contains(&id));
            let in_reach = !item.is_sold_out() && affordable && !already_bought;
            if in_reach && let (Some(balance), Some(price)) = (gold, item.price) {
                // `saturating_sub`, not `-`: both operands are wire-supplied, and
                // `price <= balance` only holds here through `affordable` above —
                // a non-local invariant an added `in_reach` term could break. At
                // zero the next item simply reads unaffordable, which is the
                // intended semantics.
                gold = Some(balance.saturating_sub(price));
            }
            buyable |= in_reach;
            targets.push(BuyTarget {
                slot: item.effective_slot(index),
                // Only ids a purchase echo can actually name AND a buy can
                // actually land: the id-0 sentinel never appears in an echo,
                // and a sold-out, unaffordable or already-bought slot cannot
                // be bought — none may hold the checklist open (nor be
                // clicked).
                id: item.catalog_id().filter(|_| in_reach),
            });
        }
        (targets, buyable)
    }

    /// A server-confirmed buy: records the echoed gold balance, then — only
    /// meaningful while `Paused` — checks the item off the checklist; the
    /// buy clearing the last entry resumes the hunt through the limits gate.
    fn on_purchase(&mut self, item: u32, gold: Option<u32>, now_ms: u64) -> Vec<Action> {
        if gold.is_some() {
            // The echoed balance is truth whatever the status: the next
            // matches' buys are planned against it.
            self.gold_balance = gold;
        }
        // Like gold, a buy is truth whatever the status: the slot is spent
        // for the rest of the roll. The id-0 sentinel never names an item —
        // asked of `shop::catalog_id`, the single interpreter, rather than
        // re-derived as `item != 0` here.
        // The `bought` guard also dedups a replayed echo, so a buy is counted
        // at most once per roll (a genuine re-buy in a fresh roll clears
        // `bought` and counts again — two items obtained).
        if let Some(item) = catalog_id(item)
            && !self.bought.contains(&item)
        {
            self.bought.push(item);
            // `bought` takes every buy as roll truth (dedup, re-open guard);
            // the haul is narrower — the *run's* take. Record only while a
            // run is live (a manual buy after a stop is the player's, not the
            // loop's) and only when the id still sits in the current roll: a
            // stale echo whose roll rotated out before it landed resolves to
            // no slot, and bucketing it would double-count the buy as a
            // phantom Other.
            if !matches!(self.status, Status::Stopped(_))
                && let Some(slot) = self.last_snapshot.as_ref().and_then(|s| s.slot_by_id(item))
            {
                self.haul.record(slot.name.clone());
            }
        }
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
            if self.recovery {
                // Proof of life: the game is delivering echoes, so the
                // remaining buys restart the ladder with a full deadline.
                self.expectation = Some(Expectation::purchase(now_ms));
            }
            return Vec::new();
        }
        self.status = Status::Watching;
        self.refresh_or_halt(now_ms)
    }

    /// A tick is a check-point: a limit tightened mid-session (or an elapsed
    /// timer) takes effect here, without waiting for the next snapshot.
    ///
    /// While `Watching` quietly (no expectation pending), every stop reason
    /// applies — an already-exceeded `max_refreshes`/`max_spend`/
    /// `max_matches` must not linger with the gate on. While a refresh is in
    /// flight the count/spend gates move into the ladder's own
    /// `refresh_or_halt`: the awaited roll is already paid for and may hold
    /// a match, so only time may halt over it — mirroring `Paused`, where
    /// only the timeout applies lest a still-buyable pause be abandoned on,
    /// say, an out-of-crystals estimate. Time first, watchdog second.
    fn on_tick(&mut self, now_ms: u64) -> Vec<Action> {
        match self.status {
            Status::Watching if self.expectation.is_none() => match self.stop_reason(now_ms) {
                Some(reason) => self.halt(reason),
                None => Vec::new(),
            },
            Status::Watching | Status::Paused if self.has_duration_elapsed(now_ms) => {
                self.halt(StopReason::Timeout)
            }
            Status::Watching | Status::Paused => self.watchdog(now_ms),
            // Named, not `_`: the tick is the only time-driven check-point, so a
            // new `Status` falling in here would get no limit enforcement and no
            // watchdog for as long as it lasted. Make it a compile error.
            Status::Idle | Status::Stopped(_) => Vec::new(),
        }
    }

    /// The gate in front of the single emission point: halt at the first
    /// limit reached, refresh otherwise.
    fn refresh_or_halt(&mut self, now_ms: u64) -> Vec<Action> {
        match self.stop_reason(now_ms) {
            Some(reason) => self.halt(reason),
            None => vec![self.emit_refresh(now_ms)],
        }
    }

    /// Single emission point: every refresh, including the auto-resume one,
    /// is counted and debited before it goes out — and, with recovery on,
    /// expected to produce a snapshot in time.
    fn emit_refresh(&mut self, now_ms: u64) -> Action {
        self.progress.refreshes = self.progress.refreshes.saturating_add(1);
        let cost = self.refresh_cost();
        self.progress.spent = self.progress.spent.saturating_add(cost);
        if let Some(meta) = self.refresh_meta.as_mut() {
            // Keeps the affordability estimate fresh across snapshots that
            // omit meta; a server-sent meta overwrites it with truth.
            meta.crystal_balance = meta.crystal_balance.saturating_sub(cost);
        }
        if self.recovery {
            self.expectation = Some(Expectation::snapshot(now_ms));
        }
        Action::Refresh
    }

    /// Spend tracking never waits for a snapshot that carries meta. Internal:
    /// no view reads it since the top bar dropped the cost readout — re-expose
    /// it when the Stats tab needs it again.
    fn refresh_cost(&self) -> u32 {
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
        // Every stop path (Unresponsive included) disarms the watchdog.
        self.expectation = None;
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
        if self.has_duration_elapsed(now_ms) {
            return Some(StopReason::Timeout);
        }
        None
    }

    /// `saturating_sub`: a `now_ms` reported earlier than the start counts as
    /// zero elapsed rather than underflowing.
    fn has_duration_elapsed(&self, now_ms: u64) -> bool {
        self.limits.max_duration_ms.is_some_and(|max| {
            self.started_at
                .is_some_and(|started| now_ms.saturating_sub(started) >= max)
        })
    }
}
