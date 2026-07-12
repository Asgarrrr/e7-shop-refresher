//! The recovery watchdog: an expectation deadline behind every issued
//! refresh or buy, escalated on ticks — blind confirm re-click, full
//! re-issue, honest halt. Armed only for live actuation.

use super::{Action, BuyTarget, Controller, Recovery, StopReason};

// The worst honest first response observed is ≈ 4 s; every accepted echo
// re-grants the full purchase window, so the value must NOT scale with
// checklist length.
const EXPECT_SNAPSHOT_MS: u64 = 10_000;
const EXPECT_PURCHASE_MS: u64 = 10_000;

/// The wire proof owed after an issued action: a snapshot for a refresh,
/// purchase echoes for buys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Proof {
    Snapshot,
    Purchase,
}

impl Proof {
    fn window_ms(self) -> u64 {
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

    /// One rung higher, fresh full deadline.
    fn escalate(self, now_ms: u64) -> Self {
        Self {
            attempt: self.attempt + 1,
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
            _ => self.halt(StopReason::Unresponsive),
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
                let id = item.catalog_id()?;
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
