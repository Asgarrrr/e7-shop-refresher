//! The vocabulary the two pumps share, and nothing else.
//!
//! [`CaptureEvent`] is what crosses the metadata channel from the capture
//! thread to reassembly; [`PressureResync`] is the marker protocol that keeps
//! that crossing lossless when the queue or the byte budget says no. They are
//! the *only* two items both `super::ingest` and `super::reassembly` need,
//! which is why they live here rather than in the root: neither pump has to
//! import half of the other's module to speak to it.
//!
//! This is also why the seam is safe. The two pumps run on different threads
//! and share exactly these two types — one moved through an `mpsc` channel, one
//! an `Arc<AtomicU8>`. There is no shared `&mut` state to split.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use tokio::sync::mpsc;
use tracing::error;

#[cfg(test)]
use crate::capture::Segment;
use crate::stream::{BudgetedSegment, PipelineBudget};

/// Metadata queue depth between the capture thread and reassembly. `stream.rs`
/// reasons about "a 512-slot channel" when it justifies its size canaries, so
/// this number and those canaries move together — see the `CaptureEvent` assert
/// below.
pub(super) const CAPTURE_EVENT_QUEUE: usize = 512;

/// Event flowing from the capture thread to reassembly.
pub(super) enum CaptureEvent {
    /// A byte-admitted TCP segment to reassemble.
    Budgeted(BudgetedSegment),
    /// Test-only compatibility path; production admits before enqueueing.
    #[cfg(test)]
    Segment(Segment),
    /// Shop Watch was just re-enabled after a pause: the reassembler must
    /// re-anchor a fresh origin (the bytes during the pause are lost).
    Resync,
    /// Byte pressure invalidated continuity. Unlike a player resync, this is
    /// counted and deduplicated by [`PressureResync`].
    PressureResync,
}

/// `CaptureEvent` is stored **by value** in a [`CAPTURE_EVENT_QUEUE`]-slot
/// channel, so its size is that queue's footprint: one extra field on the
/// largest variant silently inflates tens of KiB of always-resident memory.
/// `stream.rs`'s canaries pin the *fields* (`BudgetedSegment`, `Segment`); this
/// one pins the enum that is actually queued.
///
/// If this fires, re-measure and update the number deliberately — never work
/// around it by boxing a variant without saying why here.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(
    size_of::<CaptureEvent>() == 120,
    "CaptureEvent grew: it is queued by value, so this is per-slot queue memory"
);

/// The three states of the pressure-marker protocol, in the order they cycle:
/// `Ack -> Pending -> Enqueued -> Ack`. Stored in an `AtomicU8` with explicit
/// discriminants, the way [`crate::watch::HaltSource`] already does it — the two
/// named atomics sit in the same pipeline and both deserve a `match` rather than
/// a chain of `!=`.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Resync {
    /// Nothing outstanding: segments flow.
    Ack = 0,
    /// A re-anchor is owed but the marker has not reached the queue yet.
    Pending = 1,
    /// The marker is in the queue; the consumer will acknowledge it.
    Enqueued = 2,
}

impl Resync {
    /// Decodes a value this type itself wrote. [`PressureResync`] is the only
    /// writer and every store goes through `Self as u8`, so the fallback cannot
    /// be reached today — but this runs once per captured packet on the capture
    /// thread, which `spawn_capture_with_budget` wraps in `catch_unwind`, so a
    /// panic here would cost a player the whole session for a developer's
    /// mistake. It therefore follows the same policy `stream.rs` writes down for
    /// `pending_after_release`: a conservative value plus a named report, with
    /// the fail-fast kept in `debug_assert!` where an abort costs a stack trace
    /// rather than a session.
    ///
    /// `Pending` is the conservative choice, not `Ack`: it keeps segments
    /// blocked and a marker owed, where `Ack` would *drop* a resync marker and
    /// leave reassembly anchored on bytes that never arrived.
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Ack,
            1 => Self::Pending,
            2 => Self::Enqueued,
            other => {
                report_unknown_resync(other);
                Self::Pending
            }
        }
    }
}

/// The impossible branch of [`Resync::from_u8`], kept out of a body that runs
/// once per captured packet.
#[cold]
#[inline(never)]
fn report_unknown_resync(value: u8) {
    error!(
        value,
        "unknown pressure-resync discriminant; holding the marker pending"
    );
    debug_assert!(false, "PressureResync holds only its own discriminants");
}

/// Lossless single-producer pressure marker protocol. A full metadata queue
/// leaves the request Pending; capture retries before admitting later bytes.
///
/// `Relaxed` throughout, deliberately: this atomic is a state machine, not a
/// publication channel. The only thing that crosses the thread boundary is the
/// [`CaptureEvent::PressureResync`] marker, and it rides the `mpsc` channel,
/// which supplies the happens-before edge. What must hold here — the marker is
/// never enqueued twice, and never lost when `try_send` reports `Full` — rests
/// on RMW atomicity and on the modification order over a single location, both
/// of which `Relaxed` already gives. Do not "strengthen" these back.
#[derive(Clone, Default)]
pub(super) struct PressureResync(Arc<AtomicU8>);

impl PressureResync {
    pub(super) fn request(&self, budget: &PipelineBudget) -> bool {
        if self
            .0
            .compare_exchange(
                Resync::Ack as u8,
                Resync::Pending as u8,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            budget.record_resync();
            true
        } else {
            false
        }
    }

    pub(super) fn try_enqueue(&self, tx: &mpsc::Sender<CaptureEvent>) -> bool {
        if self
            .0
            .compare_exchange(
                Resync::Pending as u8,
                Resync::Enqueued as u8,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return self.state() == Resync::Enqueued;
        }
        match tx.try_send(CaptureEvent::PressureResync) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                let changed = self.0.compare_exchange(
                    Resync::Enqueued as u8,
                    Resync::Pending as u8,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
                debug_assert!(changed.is_ok());
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.0.store(Resync::Ack as u8, Ordering::Relaxed);
                false
            }
        }
    }

    fn state(&self) -> Resync {
        Resync::from_u8(self.0.load(Ordering::Relaxed))
    }

    pub(super) fn is_blocking_segments(&self) -> bool {
        match self.state() {
            Resync::Ack => false,
            Resync::Pending | Resync::Enqueued => true,
        }
    }

    pub(super) fn acknowledge(&self) {
        let previous = Resync::from_u8(self.0.swap(Resync::Ack as u8, Ordering::Relaxed));
        debug_assert_eq!(previous, Resync::Enqueued);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::fixtures::segment_with_capacity;
    use crate::stream::BudgetLimits;

    #[test]
    fn capture_pressure_counts_bytes_and_queues_one_resync() {
        let budget = PipelineBudget::with_test_limits(BudgetLimits {
            global: 8,
            capture: 8,
            reassembly: 8,
            outbound: 8,
        });
        let pressure = PressureResync::default();
        let (tx, mut rx) = mpsc::channel(512);
        for _ in 0..512 {
            tx.try_send(CaptureEvent::Resync).unwrap();
        }

        let Err(rejected) = budget.admit_capture(segment_with_capacity(1000, 1, 16)) else {
            panic!("oversized segment unexpectedly admitted")
        };
        budget.record_drop(rejected.payload.capacity());
        assert!(pressure.request(&budget));
        assert!(!pressure.try_enqueue(&tx));
        assert!(pressure.is_blocking_segments());
        assert_eq!(budget.snapshot().dropped_segments, 1);
        assert_eq!(budget.snapshot().dropped_bytes, 16);
        assert_eq!(budget.snapshot().resyncs, 1);

        assert!(matches!(rx.try_recv(), Ok(CaptureEvent::Resync)));
        assert!(pressure.try_enqueue(&tx));
        assert!(!pressure.request(&budget));
        for _ in 0..511 {
            assert!(matches!(rx.try_recv(), Ok(CaptureEvent::Resync)));
        }
        assert!(matches!(rx.try_recv(), Ok(CaptureEvent::PressureResync)));
        pressure.acknowledge();
        assert!(!pressure.is_blocking_segments());
        assert_eq!(budget.snapshot().resyncs, 1);
    }
}
