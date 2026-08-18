//! The "Shop Watch" capture and action gate.
//!
//! While off, the captured stream is not forwarded and the actuator rejects
//! work. The controller normally projects its status into the gate, while a
//! shared halt-cause latch lets safety-critical producers force it off
//! synchronously without relying on a bounded command queue.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use tokio::sync::Notify;

const NO_HALT: u8 = 0;

/// Origin of a safety halt request.
///
/// This is a transport-layer cause. The session loop owns the mapping to
/// user-visible domain events and stop reasons.
///
/// Each discriminant is a distinct bit: causes accumulate in one mask, so a
/// request arriving while another is being dispatched is retained rather than
/// swallowed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum HaltSource {
    PlayerStopped = 1 << 0,
    ActuatorFailed = 1 << 1,
}

impl HaltSource {
    /// Lowest set bit first. The mask carries no arrival order, and taking the
    /// lower discriminant keeps the historical first-cause order for the usual
    /// "player stops, then something fails" sequence.
    fn lowest_in(mask: u8) -> Option<Self> {
        [Self::PlayerStopped, Self::ActuatorFailed]
            .into_iter()
            .find(|source| mask & *source as u8 != 0)
    }
}

struct Inner {
    enabled: AtomicBool,
    pending_halt: AtomicU8,
    halt_notify: Notify,
}

#[derive(Clone)]
pub struct WatchGate {
    inner: Arc<Inner>,
}

impl WatchGate {
    pub fn new(enabled: bool) -> Self {
        Self {
            inner: Arc::new(Inner {
                enabled: AtomicBool::new(enabled),
                pending_halt: AtomicU8::new(NO_HALT),
                halt_notify: Notify::new(),
            }),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::Acquire)
    }

    /// Projects ordinary controller state into the gate.
    ///
    /// A pending safety halt always wins over an attempt to re-arm. The
    /// second check closes the race with a request that starts between the
    /// first check and the enabled store.
    pub fn set(&self, on: bool) {
        if !on {
            self.inner.enabled.store(false, Ordering::Release);
            return;
        }

        if self.inner.pending_halt.load(Ordering::SeqCst) != NO_HALT {
            self.inner.enabled.store(false, Ordering::Release);
            return;
        }

        self.inner.enabled.store(true, Ordering::Release);
        if self.inner.pending_halt.load(Ordering::SeqCst) != NO_HALT {
            self.inner.enabled.store(false, Ordering::Release);
        }
    }

    /// Forces the gate off and durably latches the cause alongside any other
    /// already pending.
    ///
    /// Every distinct cause survives until it is acknowledged: a fatal actuator
    /// failure raised while the loop is still dispatching a player Stop reaches
    /// the controller instead of being dropped behind it. A repeat of a cause
    /// already latched is idempotent — the domain event it maps to is too.
    ///
    /// This operation does not allocate, block, or depend on queue capacity.
    pub fn request_halt(&self, source: HaltSource) {
        self.inner.enabled.store(false, Ordering::Release);
        self.inner
            .pending_halt
            .fetch_or(source as u8, Ordering::SeqCst);
        // Close the race with `set(true)` if it observed an empty mask before
        // this cause was published.
        self.inner.enabled.store(false, Ordering::Release);
        self.inner.halt_notify.notify_one();
    }

    /// Waits for a pending cause without consuming it.
    pub async fn halt_requested(&self) -> HaltSource {
        loop {
            if let Some(source) =
                HaltSource::lowest_in(self.inner.pending_halt.load(Ordering::SeqCst))
            {
                return source;
            }
            // Notify retains a permit when the request wins before this wait,
            // and the loop rechecks the durable atomic mask after wake-up.
            self.inner.halt_notify.notified().await;
        }
    }

    /// Clears exactly the cause that the session has already dispatched,
    /// leaving every other latched cause pending.
    ///
    /// Only the session loop may acknowledge halts. Keeping acknowledgement
    /// separate from waiting makes cancellation of a `tokio::select!` branch
    /// harmless and preserves the cause through controller dispatch.
    pub fn acknowledge_halt(&self, dispatched: HaltSource) {
        self.inner
            .pending_halt
            .fetch_and(!(dispatched as u8), Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::{HaltSource, WatchGate};

    #[test]
    fn halt_disables_an_enabled_gate_synchronously() {
        let gate = WatchGate::new(true);

        gate.request_halt(HaltSource::PlayerStopped);

        assert!(!gate.is_enabled());
    }

    #[tokio::test]
    async fn request_before_waiter_is_observed() {
        let gate = WatchGate::new(true);
        gate.request_halt(HaltSource::PlayerStopped);

        assert_eq!(gate.halt_requested().await, HaltSource::PlayerStopped);
    }

    #[tokio::test]
    async fn first_cause_remains_latched_until_acknowledged() {
        let gate = WatchGate::new(true);
        gate.request_halt(HaltSource::PlayerStopped);
        gate.request_halt(HaltSource::ActuatorFailed);

        assert_eq!(gate.halt_requested().await, HaltSource::PlayerStopped);
        gate.acknowledge_halt(HaltSource::PlayerStopped);

        gate.request_halt(HaltSource::ActuatorFailed);
        assert_eq!(gate.halt_requested().await, HaltSource::ActuatorFailed);
    }

    #[tokio::test]
    async fn a_cause_raised_during_dispatch_is_not_lost() {
        let gate = WatchGate::new(true);

        // The player stops; the actuator fails fatally while the session loop
        // is still dispatching that stop.
        gate.request_halt(HaltSource::PlayerStopped);
        assert_eq!(gate.halt_requested().await, HaltSource::PlayerStopped);
        gate.request_halt(HaltSource::ActuatorFailed);
        gate.acknowledge_halt(HaltSource::PlayerStopped);

        // The failure must still reach the controller, and must keep the gate
        // from re-arming until it does.
        assert_eq!(gate.halt_requested().await, HaltSource::ActuatorFailed);
        gate.set(true);
        assert!(!gate.is_enabled());

        gate.acknowledge_halt(HaltSource::ActuatorFailed);
        gate.set(true);
        assert!(gate.is_enabled());
    }

    #[test]
    fn pending_halt_prevents_rearming_until_acknowledged() {
        let gate = WatchGate::new(true);
        gate.request_halt(HaltSource::PlayerStopped);

        gate.set(true);
        assert!(!gate.is_enabled());

        gate.acknowledge_halt(HaltSource::PlayerStopped);
        gate.set(true);
        assert!(gate.is_enabled());
    }

    #[tokio::test]
    async fn wrong_or_stale_acknowledgement_cannot_clear_a_cause() {
        let gate = WatchGate::new(true);
        gate.request_halt(HaltSource::PlayerStopped);

        gate.acknowledge_halt(HaltSource::ActuatorFailed);
        assert_eq!(gate.halt_requested().await, HaltSource::PlayerStopped);

        gate.acknowledge_halt(HaltSource::PlayerStopped);
        gate.request_halt(HaltSource::ActuatorFailed);
        gate.acknowledge_halt(HaltSource::PlayerStopped);
        assert_eq!(gate.halt_requested().await, HaltSource::ActuatorFailed);
    }
}
