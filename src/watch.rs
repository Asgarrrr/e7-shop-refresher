//! The "Shop Watch" capture and action gate.
//!
//! While off, no further captured bytes are admitted and the actuator rejects
//! work. This gate is read on the data path in exactly one place — `app::ingest`,
//! at the top of its per-packet loop and again immediately before it forwards —
//! so "off" means "nothing more is taken in", not "everything stops": work
//! already admitted runs to completion, which is up to `CAPTURE_EVENT_QUEUE`
//! events (512, `app::pressure`), whatever `stream::reassembly` is buffering, and
//! up to `PIPELINE_QUEUE` chunks (256, `app::mod`). Draining them is the
//! behaviour this gate wants: cutting the backlog off would hand the analysis
//! server a message truncated halfway through.
//!
//! The controller normally projects its status into the gate, while a shared
//! halt-cause latch lets safety-critical producers force it off synchronously
//! without relying on a bounded command queue.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use tokio::sync::Notify;

const NO_HALT: u8 = 0;

/// Origin of a safety halt request — a transport-layer cause; the session loop
/// maps it to user-visible domain events.
///
/// Each discriminant is a distinct bit, so causes accumulate in one mask and a
/// request arriving mid-dispatch is retained rather than swallowed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum HaltSource {
    PlayerStopped = 1 << 0,
    ActuatorFailed = 1 << 1,
}

impl HaltSource {
    /// Lowest set bit first: the mask carries no arrival order, so the lower
    /// discriminant stands in for the usual "player stops, then something
    /// fails". Reads cause bits out of a whole gate state, ignoring [`OFF`].
    fn lowest_in(mask: u8) -> Option<Self> {
        [Self::PlayerStopped, Self::ActuatorFailed]
            .into_iter()
            .find(|source| mask & *source as u8 != 0)
    }
}

/// Every cause bit [`HaltSource`] can latch.
const HALT_MASK: u8 = HaltSource::PlayerStopped as u8 | HaltSource::ActuatorFailed as u8;

/// The gate's own position, stored **inverted**: set means shut. Inverted so
/// `request_halt` is one unconditional `fetch_or` — closing the gate and
/// latching the cause in one indivisible update, with no retry loop on the
/// safety path.
const OFF: u8 = 1 << 7;

// A cause bit overlapping `OFF` would make a latched halt read as an armed gate.
// Six cause bits are free; this fails the build at the seventh.
const _: () = assert!(HALT_MASK & OFF == 0);

struct Inner {
    /// The pending halt mask (low bits, see [`HaltSource`]) and the gate's
    /// position ([`OFF`]) in **one** atomic. Two locations cannot hold this: a
    /// re-arming store and a halting store can both be in flight, the last one
    /// wins, and the gate ends up armed with a cause latched. In one location "a
    /// latched cause implies a shut gate" holds at every instant.
    ///
    /// That also makes `Relaxed` sufficient throughout: modification order on one
    /// location is total, a read-modify-write always sees the latest value, and
    /// the gate publishes no memory besides itself.
    state: AtomicU8,
    halt_notify: Notify,
}

/// The capture and action gate: while it is shut, no captured bytes are admitted
/// — see the module doc for what already-admitted work still does — and the
/// actuator refuses jobs. One `Arc` behind a cheap clone,
/// shared by the GUI thread, the session loop, the capture thread and the
/// actuator task. The crate's only safety cutoff, so a halt latches (see
/// [`WatchGate::request_halt`]) and the controller's status projection cannot
/// undo it.
#[derive(Clone)]
pub struct WatchGate {
    inner: Arc<Inner>,
}

impl WatchGate {
    pub fn new(enabled: bool) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: AtomicU8::new(if enabled { NO_HALT } else { OFF }),
                halt_notify: Notify::new(),
            }),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.state.load(Ordering::Relaxed) & OFF == 0
    }

    /// Projects ordinary controller state into the gate, reporting whether
    /// **this** call is the one that opened it.
    ///
    /// A pending safety halt always wins over an attempt to re-arm: reading the
    /// mask and re-arming are one atomic update of one location, so no request
    /// can land between them.
    ///
    /// The `bool` is the arming edge, and it is resolved *inside* that same
    /// read-modify-write rather than by a caller comparing two polls of
    /// [`WatchGate::is_enabled`]. `app::session::SessionGate` hangs the capture
    /// readout's per-run baseline on it: an edge derived from a separate poll
    /// leaves a window in which the capture thread has already seen the gate
    /// open and recorded a re-anchor, and a baseline taken after that window
    /// subtracts the run's own first fault away. `set(false)` never opens
    /// anything and always reports `false`.
    pub fn set(&self, on: bool) -> bool {
        if !on {
            self.inner.state.fetch_or(OFF, Ordering::Relaxed);
            return false;
        }

        // `Err` is the ordinary "a cause is latched, stay shut" outcome, not a
        // failure: the closure declines the update rather than reporting one.
        // On `Ok` the payload is the state this call replaced, so `OFF` in it is
        // exactly "the gate was shut until now".
        self.inner
            .state
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |state| {
                (state & HALT_MASK == NO_HALT).then_some(state & !OFF)
            })
            .is_ok_and(|replaced| replaced & OFF != 0)
    }

    /// Forces the gate off and durably latches the cause alongside any other
    /// already pending.
    ///
    /// Every distinct cause survives until acknowledged, so an actuator failure
    /// raised while the loop is still dispatching a player Stop reaches the
    /// controller instead of being dropped behind it.
    ///
    /// One `fetch_or`: no allocation, no blocking, no queue capacity, and no
    /// spinning against a concurrent re-arm. Once it returns the gate is shut
    /// and stays shut until [`WatchGate::acknowledge_halt`] clears this cause.
    pub fn request_halt(&self, source: HaltSource) {
        self.inner
            .state
            .fetch_or(source as u8 | OFF, Ordering::Relaxed);
        self.inner.halt_notify.notify_one();
    }

    /// Waits — indefinitely — for a pending cause and returns it *without*
    /// consuming it; [`WatchGate::acknowledge_halt`] clears it.
    ///
    /// `next_`, not `is_`: this is not a predicate but the first `biased` arm of
    /// the session `select!`, and it parks the loop until a halt exists. Reading
    /// it as a cheap `bool` is how a caller ends up awaiting a halt it meant to
    /// test for.
    pub async fn next_halt(&self) -> HaltSource {
        loop {
            if let Some(source) = HaltSource::lowest_in(self.inner.state.load(Ordering::Relaxed)) {
                return source;
            }
            // Notify retains a permit when the request wins before this wait,
            // and the loop rechecks the durable atomic mask after wake-up.
            self.inner.halt_notify.notified().await;
        }
    }

    /// Clears exactly the cause the session has already dispatched, leaving every
    /// other latched cause pending. Only the session loop may acknowledge;
    /// keeping that separate from waiting makes cancelling a `tokio::select!`
    /// branch harmless.
    ///
    /// Acknowledging does not re-arm: `OFF` survives the clear, so the gate
    /// reopens only once the controller projects `Watching` through `set(true)`.
    pub fn acknowledge_halt(&self, dispatched: HaltSource) {
        self.inner
            .state
            .fetch_and(!(dispatched as u8), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

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

        assert_eq!(gate.next_halt().await, HaltSource::PlayerStopped);
    }

    #[tokio::test]
    async fn first_cause_remains_latched_until_acknowledged() {
        let gate = WatchGate::new(true);
        gate.request_halt(HaltSource::PlayerStopped);
        gate.request_halt(HaltSource::ActuatorFailed);

        assert_eq!(gate.next_halt().await, HaltSource::PlayerStopped);
        gate.acknowledge_halt(HaltSource::PlayerStopped);

        gate.request_halt(HaltSource::ActuatorFailed);
        assert_eq!(gate.next_halt().await, HaltSource::ActuatorFailed);
    }

    #[tokio::test]
    async fn a_cause_raised_during_dispatch_is_not_lost() {
        let gate = WatchGate::new(true);

        // The player stops; the actuator fails fatally while the session loop
        // is still dispatching that stop.
        gate.request_halt(HaltSource::PlayerStopped);
        assert_eq!(gate.next_halt().await, HaltSource::PlayerStopped);
        gate.request_halt(HaltSource::ActuatorFailed);
        gate.acknowledge_halt(HaltSource::PlayerStopped);

        // The failure must still reach the controller, and must keep the gate
        // from re-arming until it does.
        assert_eq!(gate.next_halt().await, HaltSource::ActuatorFailed);
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
        assert_eq!(gate.next_halt().await, HaltSource::PlayerStopped);

        gate.acknowledge_halt(HaltSource::PlayerStopped);
        gate.request_halt(HaltSource::ActuatorFailed);
        gate.acknowledge_halt(HaltSource::PlayerStopped);
        assert_eq!(gate.next_halt().await, HaltSource::ActuatorFailed);
    }

    /// The arming edge is the gate's own transition, not the caller's intent:
    /// re-projecting `Watching` on every dispatch must report it once, and a
    /// projection the latched halt refuses must not report it at all. The
    /// capture readout's per-run baseline is published on this `bool`, so a
    /// spurious `true` wipes a running run's verdict and a missing one leaves it
    /// counting the previous run's.
    #[test]
    fn only_the_call_that_opens_the_gate_reports_the_arming_edge() {
        let gate = WatchGate::new(false);

        assert!(gate.set(true), "shut -> armed is the edge");
        assert!(!gate.set(true), "already armed is not");
        assert!(!gate.set(false), "closing is never an arming edge");

        gate.request_halt(HaltSource::PlayerStopped);
        assert!(!gate.set(true), "a latched cause opens nothing");

        gate.acknowledge_halt(HaltSource::PlayerStopped);
        assert!(gate.set(true));
    }

    /// The only shape that can observe the halt/re-arm race, with the three roles
    /// on three threads as they are in the shipped build.
    ///
    /// This fails within milliseconds on a two-atomic version of this file, and
    /// still fails if that version is promoted to `SeqCst` throughout: an
    /// all-`SeqCst` Dekker handshake only makes `false` the last store in the
    /// total order, so a reader can still catch the gate armed while a cause is
    /// latched. Only one atomic makes the state unrepresentable.
    #[test]
    fn a_concurrent_rearm_can_never_reopen_a_latched_halt() {
        // Enough rounds to lose the race many times over on a loaded box, and
        // still a few milliseconds of runtime.
        const ROUNDS: usize = 20_000;
        // The window is a handful of instructions wide, so each round polls
        // rather than sampling once.
        const POLLS: usize = 16;

        let gate = WatchGate::new(true);
        let stop = AtomicBool::new(false);

        let halt_loop = |source: HaltSource| {
            for _ in 0..ROUNDS {
                gate.request_halt(source);
                for _ in 0..POLLS {
                    assert!(
                        !gate.is_enabled(),
                        "{source:?} was latched and the gate re-armed anyway"
                    );
                }
                gate.acknowledge_halt(source);
            }
        };

        // The flag has to be set on the unwind path too: a bare `store` after
        // the assertions would leave `thread::scope` joining a thread that never
        // exits, turning a failure into a hang.
        struct StopOnDrop<'a>(&'a AtomicBool);
        impl Drop for StopOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        std::thread::scope(|scope| {
            // The session loop projects the controller status on every snapshot,
            // purchase, tick and command.
            scope.spawn(|| {
                while !stop.load(Ordering::Relaxed) {
                    gate.set(true);
                }
            });
            scope.spawn(|| halt_loop(HaltSource::PlayerStopped));

            let _stop_spinner = StopOnDrop(&stop);
            halt_loop(HaltSource::ActuatorFailed);
        });
    }
}
