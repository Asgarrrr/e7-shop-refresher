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

// For `app::session`'s tests, which drive the same ladder from outside and
// would otherwise re-spell its deadlines as literals. Only the derived
// deadline escapes, never the two windows behind it.
#[cfg(test)]
pub(crate) use watchdog::past_rung;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::filter::Filter;
use crate::domain::shop::{CatalogId, Crystals, Gold, RefreshMeta, ShopSnapshot};

use dedup::{Fingerprint, fingerprint};
use watchdog::Expectation;

/// A refresh always costs 3 crystals (game fact); a wire-sent cost overrides.
const REFRESH_COST_CRYSTALS: Crystals = Crystals::new(3);

/// A watchdog-issued retry. Distinct from bare `Refresh`/`Buy` because
/// recovery reaches the caller from a tick, which carries no game-animation
/// trigger — bare variants there would render as advice and submit nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// Blind confirm re-click after a missed refresh confirm. Free and safe:
    /// nothing clickable sits under a closed modal zone (game fact).
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
///
/// # There is no gold limit here, and that is deliberate
///
/// The currency this section bounds is crystals ([`Self::max_spend`]), which
/// only refreshes spend. Gold is spent by *buys*, and a run's worst-case gold
/// outlay is bounded from the other two knobs instead:
/// `max_matches × Filter::max_price` — the per-item gold ceiling lives in
/// `[filter]`, because an item above it never matches and so is never bought,
/// and [`Self::max_matches`] bounds how many matches a run can make. **Both
/// default to `None`, so a config setting neither has no gold ceiling at
/// all.**
///
/// A `max_gold_spend` key would have to accumulate gold actually spent, and
/// the only wire signal for it is `PurchaseNotice::gold` — the balance
/// *after* the buy, not the price, optional and failing open when absent, and
/// carrying `item: Option<CatalogId>` so the purchase is not always
/// attributable to a priced item. A limit built on that would silently not
/// limit, which is the exact failure this type's own `deny_unknown_fields`
/// exists to prevent. The gap is that nothing tells a player the product
/// above is their worst case — a sentence, which `config.example.toml` and
/// the README now carry, not a field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_refreshes: Option<u32>,
    /// Crystal budget — a hard ceiling: a refresh that would cross it is never
    /// issued. Typed as [`Crystals`] so the comparison against
    /// [`Progress::spent`] is unwritable in the wrong currency;
    /// `#[serde(transparent)]`, so `config.toml` still says `max_spend = 300`.
    pub max_spend: Option<Crystals>,
    /// Matched items, cumulative — not purchases. Reached by a match, the halt
    /// waits for that match's pause to resolve: the items are bought, then the
    /// loop stops instead of resuming.
    pub max_matches: Option<u32>,
    pub max_duration_ms: Option<u64>,
}

/// Why a hunt stopped. Every variant is player-facing (rendered by
/// `render::stop_reason_label`), so the label must stay honest about *who*
/// stopped it: the player, a machine fault, or a limit the player set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    PlayerStopped,
    /// The relay pipeline ended (capture or uplink gone) while armed — not the
    /// player's stop, and they must not be told it was.
    SessionEnded,
    /// The click executor could not act safely: a machine fault.
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
/// link transitions, and the clock. The only input to [`Controller::handle`],
/// and the only way time enters the machine (`now_ms`, assumed monotonic).
#[derive(Debug, Clone)]
pub enum Event {
    /// Arms the watch. The caller must deliver a snapshot *after* this:
    /// pre-Start snapshots are stored for the view, never replayed.
    Start {
        now_ms: u64,
    },
    Stop,
    /// The relay is going away underneath the loop: same halt as `Stop`,
    /// honest label.
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
        /// The bought item's catalog id, `None` when the server omitted it.
        item: Option<CatalogId>,
        /// Gold balance after the buy, planning the next matches. `None` fails
        /// open; `Some(Gold::new(0))` is an empty purse that vetoes every
        /// priced match.
        gold: Option<Gold>,
        now_ms: u64,
    },
    FilterChanged(Filter),
    /// The player retuned the stop limits mid-session (from the GUI).
    LimitsChanged(Limits),
    /// The uplink dropped: no proof can arrive, so the watchdog must not
    /// escalate — the reconnect backoff alone outlasts the whole ladder.
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

/// One matched slot. `slot` is the display position (1..=6 on a well-formed
/// shop; row = slot − 1). `id` is `Some` exactly when the item is buyable AND
/// an echo can name it — i.e. exactly the checklist entries; an `id: None`
/// target is display-only, never clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuyTarget {
    pub slot: u8,
    pub id: Option<CatalogId>,
}

/// Actions are to be consumed in order: a `Buy` can precede another action
/// in the same batch and both matter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Refresh,
    Buy {
        targets: Vec<BuyTarget>,
    },
    /// A watchdog retry to execute (recovery-enabled sessions only).
    Recover(Recovery),
    Halt(StopReason),
    /// The event was rejected; nothing changed. Callers render the reason, so
    /// enforcement and messaging come from the same decision.
    Refused(RefusalReason),
}

impl Action {
    /// The domain's explicit rejection verdict. "Was the event applied?" tests
    /// for this, never the emptiness of the action list — an accepted event
    /// that grows an action later must not silently read as refused.
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
    pub spent: Crystals,
    /// Items matched, not purchases.
    pub matches_found: u32,
}

/// Confirmed purchases this run, tallied by the bought item's wire name so a
/// view can group them into headline tokens (Covenant, Mystic, …) and a
/// generic bucket. Nameless buys land in `untitled`. Reset on `Start`.
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

/// Invariant: refreshes are reactive — one is requested only for a no-match
/// snapshot or the purchase clearing the last checklist entry. Duplicates and
/// snapshots arriving unarmed (`Idle`/`Stopped`) never trigger one, though
/// they are still stored for the view.
pub struct Controller {
    filter: Filter,
    limits: Limits,
    status: Status,
    started_at: Option<u64>,
    progress: Progress,
    /// Confirmed buys this run, grouped by item name for the haul readout.
    haul: Haul,
    /// Last balance/cost seen this session, locally debited per refresh so the
    /// estimate survives snapshots that omit meta; a server-sent meta
    /// overwrites it. Forgotten on `Start`.
    refresh_meta: Option<RefreshMeta>,
    /// Last gold balance echoed by a purchase; the next buys are planned
    /// against it. `None` restricts nothing, and `Start` forgets it — a stale
    /// balance must not veto buys.
    gold_balance: Option<Gold>,
    last_snapshot: Option<ShopSnapshot>,
    /// Matched-but-unbought catalog ids from the last evaluated snapshot.
    checklist: Vec<CatalogId>,
    /// Identity of the last snapshot evaluated while armed: an identical
    /// re-arrival is stored for the view but never re-evaluated, so it cannot
    /// double-bill a refresh. Cleared on `Start` only — surviving the post-buy
    /// auto-resume is what mutes a re-open right after a buy.
    acted_fingerprint: Option<Fingerprint>,
    /// Catalog ids bought in the current roll. The wire never says sold-out and
    /// ids are stable per item type, so this is the only guard against
    /// re-buying a bought slot. Survives `Start`, which clears
    /// `acted_fingerprint`: keying the clear off that alone would let a
    /// same-roll re-open wrongly forget the buys.
    bought: Vec<CatalogId>,
    /// The roll `bought` is scoped to; a different identity is fresh stock and
    /// empties the set. Shares the `Arc` with `acted_fingerprint` rather than
    /// deep-copying every slot's strings.
    bought_fingerprint: Option<Fingerprint>,
    /// Watchdog armed. Only ever true for live actuation: Off is player-paced
    /// advice and `DryRun` never yields wire feedback, so deadlines would
    /// self-halt both.
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

    /// Arms or disarms the recovery watchdog: while armed, every issued
    /// refresh/buy gets a deadline, escalating nudge → re-issue → honest halt.
    ///
    /// Armed exactly when the actuator really clicks. That was once a wiring-
    /// time fact, which is why this used to be a one-way `enable_recovery`; the
    /// rehearsal switch is now live, so the same session can cross the line in
    /// both directions.
    ///
    /// **Disarming clears [`Self::expectation`], and that is the whole reason
    /// this is not a bare field write.** A deadline set while armed would
    /// otherwise outlive the watchdog that owns it: the next [`Event::Tick`]
    /// finds a pending expectation, and `watchdog` has no notion of "armed" —
    /// `recovery` gates *setting* deadlines, not honouring them. The session
    /// would climb the ladder and re-issue clicks for a rehearsal nobody is
    /// watching.
    ///
    /// Re-arming deliberately does **not** restore it. The proof that
    /// expectation waited on belongs to a job that ran in the other mode; the
    /// next issued refresh opens a fresh one.
    pub fn set_recovery(&mut self, enabled: bool) {
        self.recovery = enabled;
        if !enabled {
            self.expectation = None;
        }
    }

    /// Whether the recovery watchdog is armed right now. Not a constant for the
    /// session: see [`Self::set_recovery`].
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

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    /// The refresh meta the stop logic enforces: server-sent, locally debited,
    /// discarded on `Start`. A view must display this, not the raw snapshot.
    pub fn refresh_meta(&self) -> Option<RefreshMeta> {
        self.refresh_meta
    }

    /// Matched-but-unbought catalog ids; untrackable matches (id 0, sold
    /// out) never enter it.
    pub fn checklist(&self) -> &[CatalogId] {
        &self.checklist
    }

    /// Last gold balance echoed by a purchase this run; `None` before the
    /// first buy and again after `Start` clears it.
    pub fn gold_balance(&self) -> Option<Gold> {
        self.gold_balance
    }

    /// The returned actions **are** the decision; the state mutation alone is
    /// not the output. `clippy::must_use_candidate` cannot flag a `&mut self`
    /// method, so this attribute is the only thing that catches a dropped
    /// refresh or buy.
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
                // An unrestricted filter would match every slot of every shop.
                // Accepted swaps apply from the next *new* snapshot: neither
                // the stored one nor a duplicate re-send is re-evaluated.
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
    /// snapshot. Ignored mid-session so a stray `Start` cannot reset counters,
    /// and refused while the filter is unrestricted — the loop must never hunt
    /// without a target, whoever sent the event.
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

    /// Idempotent once stopped (the original reason is not relabelled) and a
    /// no-op while Idle: a session that never ran did not stop. The invariant
    /// lives here so every Stop producer gets it for free.
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
        // Decide against a borrow, then store once: no per-path store to drift.
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
        // A duplicate must never bill a second refresh or re-buy.
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
        // The awaited proof arrived: disarm. Duplicates and degraded arrivals
        // returned above — a re-open is not a re-roll. Pause-entry and
        // emit_refresh re-arm below.
        self.expectation = None;

        let (targets, buyable) = self.plan_targets(snapshot);
        // Aligned by construction: the checklist is exactly the clickable
        // targets, so an actuator buying `id: Some` targets clears the pause.
        self.checklist = targets.iter().filter_map(|target| target.id).collect();

        if targets.is_empty() {
            // A new no-match shop also unpauses (the hourly rotation replaced
            // the matches).
            self.status = Status::Watching;
            return self.refresh_or_halt(now_ms);
        }
        self.progress.matches_found = self
            .progress
            .matches_found
            .saturating_add(targets.len() as u32);
        if !buyable {
            // Every match is dead stock (sold out or beyond the known gold);
            // pausing would park the loop until the hourly rotation. Show the
            // match and keep hunting.
            self.status = Status::Watching;
            let mut actions = vec![Action::Buy { targets }];
            actions.extend(self.refresh_or_halt(now_ms));
            return actions;
        }
        // Never refresh over a match, and never halt over it: a reached
        // `max_matches` does not fire here, because the matched items are the
        // hunt's goal. The pause resolves first, then the limit lands at the
        // next gate.
        self.status = Status::Paused;
        if self.recovery && !self.checklist.is_empty() {
            // Only echo-clearable pauses get a deadline: an empty-checklist
            // pause waits on the player, not the game.
            self.expectation = Some(Expectation::purchase(now_ms));
        }
        vec![Action::Buy { targets }]
    }

    /// The matched slots, clickable or display-only, planned against the last
    /// echoed gold balance debited in click order (the second 184k bookmark of
    /// a 200k purse is out of reach). An unknown balance or price fails open.
    /// The flag says whether anything at all is in reach of a buy.
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
            let already_bought = item.id.is_some_and(|id| self.bought.contains(&id));
            let in_reach = !item.is_sold_out() && affordable && !already_bought;
            if in_reach && let (Some(balance), Some(price)) = (gold, item.price) {
                // Saturating: `price <= balance` holds only through
                // `affordable` above, which an added `in_reach` term could
                // break. At zero the next item reads unaffordable, as intended.
                gold = Some(balance.saturating_sub(price));
            }
            buyable |= in_reach;
            targets.push(BuyTarget {
                slot: item.effective_slot(index),
                // Only ids a buy can land on: a sold-out, unaffordable or
                // already-bought slot may not hold the checklist open.
                id: item.id.filter(|_| in_reach),
            });
        }
        (targets, buyable)
    }

    /// A server-confirmed buy: records the echoed gold balance, then — only
    /// while `Paused` — checks the item off the checklist. The buy clearing
    /// the last entry resumes the hunt through the limits gate.
    fn on_purchase(
        &mut self,
        item: Option<CatalogId>,
        gold: Option<Gold>,
        now_ms: u64,
    ) -> Vec<Action> {
        if gold.is_some() {
            // Truth whatever the status: the next buys are planned against it.
            self.gold_balance = gold;
        }
        // Also truth whatever the status — the slot is spent for the rest of
        // the roll. The `bought` guard doubles as replay dedup, so a buy counts
        // at most once per roll; a re-buy in a fresh roll counts again.
        if let Some(item) = item
            && !self.bought.contains(&item)
        {
            self.bought.push(item);
            // The haul is narrower than `bought`: the *run's* take. Hence only
            // while a run is live (a post-stop buy is the player's own), and
            // only when the id still sits in the roll — a stale echo whose roll
            // rotated out would otherwise bucket as a phantom Other.
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
        // consumed purchase, or an echo the server gave no id.
        let Some(position) = item.and_then(|item| self.checklist.iter().position(|&id| id == item))
        else {
            return Vec::new();
        };
        self.checklist.swap_remove(position);
        if !self.checklist.is_empty() {
            if self.recovery {
                // Proof of life: the remaining buys restart the ladder at rung
                // zero with a full deadline.
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
    /// Quietly `Watching`, every stop reason applies. With a refresh in flight
    /// the count/spend gates move into `refresh_or_halt` — the awaited roll is
    /// paid for and may hold a match, so only time may halt over it, as in
    /// `Paused`. Time first, watchdog second.
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
            // Named, not `_`: the tick is the only time-driven check-point, so
            // a new `Status` landing here would silently get no limits and no
            // watchdog. Keep it a compile error.
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

    /// Single emission point: every refresh, the auto-resume one included, is
    /// counted and debited before it goes out — and, with recovery on,
    /// expected to produce a snapshot in time.
    fn emit_refresh(&mut self, now_ms: u64) -> Action {
        self.progress.refreshes = self.progress.refreshes.saturating_add(1);
        let cost = self.refresh_cost();
        self.progress.spent = self.progress.spent.saturating_add(cost);
        if let Some(meta) = self.refresh_meta.as_mut() {
            // Keeps the estimate fresh across snapshots that omit meta; a
            // server-sent meta overwrites it with truth.
            meta.crystal_balance = meta.crystal_balance.saturating_sub(cost);
        }
        if self.recovery {
            self.expectation = Some(Expectation::snapshot(now_ms));
        }
        Action::Refresh
    }

    /// Spend tracking never waits for a snapshot that carries meta.
    ///
    /// A wire-sent **zero** is refused and the constant used instead: zero
    /// switches both money gates off at once — out-of-funds becomes
    /// `balance < 0`, never true, and `spent` stops accumulating, so
    /// `max_spend` is unreachable too. Any other cost is believed. If free
    /// refreshes ever become real, this costs a spurious `MaxSpend`: it stops
    /// rather than spends, the direction to be wrong in.
    fn refresh_cost(&self) -> Crystals {
        match self.refresh_meta {
            Some(meta) if meta.cost > Crystals::new(0) => meta.cost,
            _ => REFRESH_COST_CRYSTALS,
        }
    }

    fn halt(&mut self, reason: StopReason) -> Vec<Action> {
        self.status = Status::Stopped(reason);
        // Ids are stable per item and snapshots stored while Stopped are never
        // evaluated, so a surviving checklist would keep painting yesterday's
        // matches as "wanted" in the view.
        self.checklist.clear();
        // Every stop path (Unresponsive included) disarms the watchdog.
        self.expectation = None;
        vec![Action::Halt(reason)]
    }

    /// Checked before every refresh; the order below is the priority order.
    fn stop_reason(&self, now_ms: u64) -> Option<StopReason> {
        // `refresh_cost()`, not `meta.cost`: debiting 3 per refresh while
        // weighing the balance against a wire-sent 0 here would leave
        // out-of-funds unreachable on the very message the floor exists for.
        if let Some(meta) = self.refresh_meta
            && meta.crystal_balance < self.refresh_cost()
        {
            return Some(StopReason::OutOfFunds);
        }
        if let Some(max) = self.limits.max_refreshes
            && self.progress.refreshes >= max
        {
            return Some(StopReason::MaxRefreshes);
        }
        // Hard ceiling: also stop when the *next* refresh would cross it.
        // `checked_add` rather than `saturating_add` — see its doc.
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
