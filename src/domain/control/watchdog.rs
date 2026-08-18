//! The recovery watchdog: an expectation deadline behind every issued
//! refresh or buy, escalated on ticks — blind confirm re-click, full
//! re-issue, honest halt. Armed only for live actuation.

use super::{Action, BuyTarget, Controller, Recovery, StopReason};

// The worst honest first response observed is ≈ 4 s; every accepted echo
// re-grants the full purchase window, so the value must NOT scale with
// checklist length.
const EXPECT_SNAPSHOT_MS: u64 = 10_000;
const EXPECT_PURCHASE_MS: u64 = 10_000;

/// One millisecond past the ladder's rung-`rung` deadline; `rung` counts from 1
/// (first timeout), through 2 (re-issue), to 3 (honest halt).
///
/// Test-only, and it lives *here* rather than in either test module because two
/// suites need it: `control::tests` and `app::session::tests` both used to spell
/// these deadlines `10_001` / `20_001` / `30_001` — 30-odd bare literals whose
/// only tie to the window above was arithmetic in the reader's head, in a file
/// where every sibling tuning value is named. A change to the window now moves
/// every tick with it.
///
/// Every escalation re-grants a *full* window rather than shortening it, which is
/// what makes the rungs exact multiples.
#[cfg(test)]
pub(crate) const fn past_rung(rung: u64) -> u64 {
    rung * EXPECT_SNAPSHOT_MS + 1
}

// `past_rung` collapses the two windows into one number, which is honest only
// while they are equal. They are separate constants because they answer to
// different evidence (a shop response vs. a purchase echo), so this is the check
// that the collapse stays true rather than an assumption.
#[cfg(test)]
const _: () = assert!(
    EXPECT_SNAPSHOT_MS == EXPECT_PURCHASE_MS,
    "past_rung() assumes one window length — give the tests a second helper if these ever differ"
);

/// The wire proof owed after an issued action: a snapshot for a refresh,
/// purchase echoes for buys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Proof {
    Snapshot,
    Purchase,
}

impl Proof {
    const fn window_ms(self) -> u64 {
        match self {
            Proof::Snapshot => EXPECT_SNAPSHOT_MS,
            Proof::Purchase => EXPECT_PURCHASE_MS,
        }
    }
}

/// What the watchdog waits on, carrying the recovery-ladder rung already
/// climbed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Expectation {
    proof: Proof,
    deadline_ms: u64,
    attempt: u8,
}

impl Expectation {
    /// A rung-zero snapshot expectation with a full deadline.
    pub(super) fn snapshot(now_ms: u64) -> Self {
        Self::armed(Proof::Snapshot, now_ms)
    }

    /// A rung-zero purchase expectation with a full deadline.
    pub(super) fn purchase(now_ms: u64) -> Self {
        Self::armed(Proof::Purchase, now_ms)
    }

    fn armed(proof: Proof, now_ms: u64) -> Self {
        Self {
            proof,
            deadline_ms: now_ms + proof.window_ms(),
            attempt: 0,
        }
    }

    /// Same proof and rung, fresh full deadline.
    fn regrant(self, now_ms: u64) -> Self {
        Self {
            deadline_ms: now_ms + self.proof.window_ms(),
            ..self
        }
    }

    /// One rung higher, fresh full deadline. Saturating rather than bare `+`:
    /// the cap is the ladder's `match` arms in [`Controller::watchdog`] (rung ≥ 2
    /// halts and clears the expectation), which is non-local reasoning the
    /// operator should not depend on.
    fn escalate(self, now_ms: u64) -> Self {
        Self {
            attempt: self.attempt.saturating_add(1),
            ..self.regrant(now_ms)
        }
    }
}

impl Controller {
    /// The recovery ladder, run from ticks once a deadline lapses: miss #1 →
    /// blind confirm re-click (free — nothing clickable sits under a closed
    /// modal), miss #2 → full re-issue, miss #3 → honest halt. Suspended
    /// while the link is down: no proof can arrive over a dead wire, and the
    /// reconnect backoff alone outlasts the whole ladder.
    pub(super) fn watchdog(&mut self, now_ms: u64) -> Vec<Action> {
        if !self.link_up {
            return Vec::new();
        }
        let Some(expectation) = self.expectation.filter(|e| now_ms >= e.deadline_ms) else {
            return Vec::new();
        };
        match (expectation.proof, expectation.attempt) {
            (Proof::Snapshot, 0) => {
                self.expectation = Some(expectation.escalate(now_ms));
                vec![Action::Recover(Recovery::ConfirmRefresh)]
            }
            (Proof::Snapshot, 1) => {
                // Through the gate on purpose: the re-issue re-counts and
                // re-debits (`max_spend` is a ceiling promise, so
                // overcounting fails safe) and may halt honestly on a limit
                // instead of double-rolling.
                let actions: Vec<Action> = self
                    .refresh_or_halt(now_ms)
                    .into_iter()
                    // Pass-through by design: only `Refresh` is relabelled, and
                    // leaving every other action alone stays right for any
                    // variant `Action` gains. Not a swallowed case — do not
                    // expand this arm into a variant list.
                    .map(|action| match action {
                        Action::Refresh => Action::Recover(Recovery::Refresh),
                        other => other,
                    })
                    .collect();
                // Some ⇔ emit_refresh ran and armed a fresh rung zero (a
                // halt cleared it instead): the ladder must not reset itself.
                if self.expectation.is_some() {
                    self.expectation = Some(expectation.escalate(now_ms));
                }
                actions
            }
            (Proof::Purchase, 0) => {
                self.expectation = Some(expectation.escalate(now_ms));
                vec![Action::Recover(Recovery::ConfirmBuy)]
            }
            (Proof::Purchase, 1) => {
                self.expectation = Some(expectation.escalate(now_ms));
                vec![Action::Recover(Recovery::Buy {
                    targets: self.recovery_buy_targets(),
                })]
            }
            // The wildcard is for the *rung* dimension only (rung ≥ 2 is the
            // halt); the proof dimension is spelled out so a new `Proof` variant
            // is a compile error here instead of an immediate halt that blames
            // the game. Clippy cannot see this — the scrutinee is a tuple.
            (Proof::Snapshot | Proof::Purchase, _) => self.halt(StopReason::Unresponsive),
        }
    }

    /// The outstanding buys, rebuilt by identity from the checklist against
    /// the stored snapshot — never re-filtered: a mid-pause filter swap must
    /// not redraw what the pause is waiting on.
    fn recovery_buy_targets(&self) -> Vec<BuyTarget> {
        let Some(snapshot) = self.last_snapshot.as_ref() else {
            return Vec::new();
        };
        snapshot
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let id = item.id?;
                self.checklist.contains(&id).then(|| BuyTarget {
                    slot: item.effective_slot(index),
                    id: Some(id),
                })
            })
            .collect()
    }

    /// The outage may have swallowed the awaited proof mid-flight: re-grant
    /// a full deadline. The rung already climbed is kept — a retry that
    /// never got its answer must still escalate, not restart the ladder.
    pub(super) fn on_link_up(&mut self, now_ms: u64) -> Vec<Action> {
        self.link_up = true;
        if let Some(expectation) = self.expectation {
            self.expectation = Some(expectation.regrant(now_ms));
        }
        Vec::new()
    }
}
