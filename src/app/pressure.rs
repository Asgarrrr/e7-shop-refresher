//! The vocabulary the two pumps share, and nothing else.
//!
//! [`CaptureEvent`] crosses the metadata channel from the capture thread to
//! reassembly; [`PressureResync`] keeps that crossing lossless when the queue
//! or byte budget says no. They are the only state the two pumps share.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use tokio::sync::mpsc;
use tracing::error;

use crate::stream::{BudgetedSegment, PipelineBudget, ResyncCause};

/// `stream.rs` reasons about "a 512-slot channel" when it justifies its size
/// canaries, so this number and those canaries move together.
pub(super) const CAPTURE_EVENT_QUEUE: usize = 512;

/// Event flowing from the capture thread to reassembly.
pub(super) enum CaptureEvent {
    /// A byte-admitted TCP segment to reassemble.
    Budgeted(BudgetedSegment),
    /// Shop Watch was just re-enabled after a pause: the reassembler must
    /// re-anchor a fresh origin (the bytes during the pause are lost).
    Resync,
    /// Byte pressure invalidated continuity. Unlike a player resync, this is
    /// counted and deduplicated by [`PressureResync`].
    PressureResync,
}

/// Stored **by value** in a [`CAPTURE_EVENT_QUEUE`]-slot channel, so one extra
/// field on the largest variant silently inflates tens of KiB of resident
/// memory. `stream.rs`'s canaries pin the *fields*; this pins the queued enum.
/// If it fires, re-measure — do not box a variant without saying why here.
///
/// Re-measured 2026-08-20 after the `#[cfg(test)]`-only `Segment` variant was
/// deleted: still 120. `Budgeted(BudgetedSegment)` was already the largest
/// variant (it carries the `PayloadLease`'s `PipelineBudget` handle on top of
/// the same flow/seq/syn/payload shape `Segment` had), so removing `Segment`
/// dropped a variant without shrinking the enum.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(
    size_of::<CaptureEvent>() == 120,
    "CaptureEvent grew: it is queued by value, so this is per-slot queue memory"
);

/// The pressure-marker protocol, in the order it cycles. Explicit discriminants
/// in an `AtomicU8`, as [`crate::watch::HaltSource`] does it, so readers get a
/// `match` rather than a chain of `!=`.
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
    /// The fallback cannot be reached today, but this runs once per captured
    /// packet and a panic there costs a player the session — so, as with
    /// `stream.rs`'s `pending_after_release`, a conservative value plus a report
    /// with the fail-fast left in `debug_assert!`. `Pending` is the conservative
    /// one: `Ack` would drop the marker and leave reassembly anchored on bytes
    /// that never arrived.
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

/// Kept out of [`Resync::from_u8`], which runs once per captured packet.
#[cold]
#[inline(never)]
fn report_unknown_resync(value: u8) {
    error!(
        value,
        "unknown pressure-resync discriminant; holding the marker pending"
    );
    debug_assert!(false, "PressureResync holds only its own discriminants");
}

/// Lossless single-producer pressure marker. A full metadata queue leaves the
/// request Pending; capture retries before admitting later bytes.
///
/// `Relaxed` throughout, deliberately: this atomic is a state machine, not a
/// publication channel — the marker itself rides the `mpsc`, which supplies the
/// happens-before edge. What must hold (never enqueued twice, never lost on a
/// `Full`) rests on RMW atomicity and single-location modification order, both
/// of which `Relaxed` gives. Do not "strengthen" these back.
#[derive(Clone, Default)]
pub(super) struct PressureResync(Arc<AtomicU8>);

impl PressureResync {
    /// Opens a re-anchor episode, attributed to `cause`, and reports whether
    /// this call is the one that opened it.
    ///
    /// `cause` is recorded only on the `true` branch, which is what keeps the
    /// counters honest under a cascade: a ring overflow that then floods the
    /// funnel is one hole in one byte stream, and counting the follow-on
    /// symptoms as separate causes would let the loudest consequence outvote the
    /// origin in `PipelineStats::dominant_resync`. One episode, one cause, the
    /// first one to see it.
    pub(super) fn request(&self, budget: &PipelineBudget, cause: ResyncCause) -> bool {
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
            budget.record_resync(cause);
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
        assert!(pressure.request(&budget, ResyncCause::ByteQuota));
        assert!(!pressure.try_enqueue(&tx));
        assert!(pressure.is_blocking_segments());
        assert_eq!(budget.snapshot().dropped_segments, 1);
        assert_eq!(budget.snapshot().dropped_bytes, 16);
        assert_eq!(budget.snapshot().resyncs, 1);
        assert_eq!(
            budget.snapshot().dominant_resync(),
            Some(ResyncCause::ByteQuota)
        );

        assert!(matches!(rx.try_recv(), Ok(CaptureEvent::Resync)));
        assert!(pressure.try_enqueue(&tx));
        // A second cause arriving inside the same episode is refused, and does
        // not get counted: `dominant_resync` still names what started it.
        assert!(!pressure.request(&budget, ResyncCause::DriverRing));
        assert_eq!(
            budget.snapshot().dominant_resync(),
            Some(ResyncCause::ByteQuota)
        );
        for _ in 0..511 {
            assert!(matches!(rx.try_recv(), Ok(CaptureEvent::Resync)));
        }
        assert!(matches!(rx.try_recv(), Ok(CaptureEvent::PressureResync)));
        pressure.acknowledge();
        assert!(!pressure.is_blocking_segments());
        assert_eq!(budget.snapshot().resyncs, 1);
    }

    /// A closed metadata channel is the shutdown path, and it must clear the
    /// marker rather than leave it standing. Without the `store(Resync::Ack)` on
    /// `Closed`, the state sticks at `Enqueued` for an acknowledgement that can
    /// never come — the consumer is gone — so `is_blocking_segments` answers
    /// `true` for the rest of the process and capture refuses every later
    /// segment while retrying a marker no queue will take.
    #[test]
    fn a_closed_queue_clears_the_marker_instead_of_blocking_segments_forever() {
        let budget = PipelineBudget::new();
        let pressure = PressureResync::default();
        let (tx, rx) = mpsc::channel(4);
        drop(rx);

        assert!(pressure.request(&budget, ResyncCause::ByteQuota));
        assert!(pressure.is_blocking_segments());

        assert!(!pressure.try_enqueue(&tx), "a closed queue took no marker");

        assert!(
            !pressure.is_blocking_segments(),
            "a marker owed to a consumer that no longer exists must not hold capture shut"
        );
        // And the episode is genuinely over, not merely unreported: a later
        // request can open a fresh one.
        assert!(pressure.request(&budget, ResyncCause::DriverRing));
        assert_eq!(budget.snapshot().resyncs, 2);
    }

    /// `try_enqueue` runs once per captured packet while a marker is owed, so
    /// its two no-op paths carry the "never enqueued twice, never lost" promise:
    /// with a marker already in the queue it reports success without queueing a
    /// second one, and with nothing owed it reports failure without queueing at
    /// all.
    #[test]
    fn try_enqueue_queues_one_marker_per_episode_and_none_outside_one() {
        let budget = PipelineBudget::new();
        let pressure = PressureResync::default();
        let (tx, mut rx) = mpsc::channel(4);

        // Nothing owed: the CAS off `Pending` fails and no marker is queued.
        assert!(
            !pressure.try_enqueue(&tx),
            "no episode is open, so there is nothing to enqueue"
        );
        assert!(rx.try_recv().is_err());
        assert!(!pressure.is_blocking_segments());

        assert!(pressure.request(&budget, ResyncCause::ByteQuota));
        assert!(pressure.try_enqueue(&tx));
        // Already queued: the same failing CAS, and this time the answer is
        // "done", still without a second marker.
        assert!(
            pressure.try_enqueue(&tx),
            "the marker is already in the queue; this call has nothing left to do"
        );
        assert!(pressure.try_enqueue(&tx));
        assert!(
            pressure.is_blocking_segments(),
            "a queued marker still holds later segments behind it"
        );

        assert!(matches!(rx.try_recv(), Ok(CaptureEvent::PressureResync)));
        assert!(
            rx.try_recv().is_err(),
            "three try_enqueue calls, one marker"
        );
    }
}
