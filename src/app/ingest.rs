//! The capture pump: the blocking receive loop, and the three cold reports it
//! keeps out of its own body.
//!
//! Runs on a dedicated OS thread owned by `super::workers`, and talks to
//! reassembly only through [`CaptureEvent`] and the [`PressureResync`] marker.

use tokio::sync::{mpsc, watch};
use tracing::{debug, error, warn};

use crate::capture::{CaptureLoss, PacketSource};
use crate::stream::{PipelineBudget, ResyncCause};
use crate::watch::WatchGate;

use super::pressure::{CaptureEvent, PressureResync};

/// The *absence* of a "capture progress" line in a long log is the diagnostic:
/// the backend is reporting nothing on the configured port.
const CAPTURE_PROGRESS_EVERY: u64 = 1000;

/// The three rare reports of the per-packet loop, out of line: `#[cold]` +
/// `#[inline(never)]` stops LLVM hoisting the tracing machinery and the
/// mutex-taking `PipelineStats` construction into the hot path for branches
/// that fire once a session or never. At this crate's scale that is about
/// hot-body layout, not throughput.
#[cold]
#[inline(never)]
fn report_backend_loss(budget: &PipelineBudget, cause: ResyncCause) {
    let stats = budget.snapshot();
    warn!(
        cause = cause.label(),
        dropped_segments = stats.dropped_segments,
        dropped_bytes = stats.dropped_bytes,
        resyncs = stats.resyncs,
        "capture backend lost packets; dropping until resync acknowledgement"
    );
}

/// Out of line, as [`report_backend_loss`]. Reports every stage because a
/// byte-pressure event is diagnosed by *which* stage holds the bytes.
#[cold]
#[inline(never)]
fn report_byte_pressure(budget: &PipelineBudget) {
    let stats = budget.snapshot();
    warn!(
        current_total = stats.current_total,
        capture_bytes = stats.current_capture,
        pending_bytes = stats.current_reassembly,
        outbound_bytes = stats.current_outbound,
        dropped_segments = stats.dropped_segments,
        dropped_bytes = stats.dropped_bytes,
        resyncs = stats.resyncs,
        "capture pipeline byte pressure; dropping until resync acknowledgement"
    );
}

/// Out of line, as [`report_backend_loss`]. Slots ran out here, not bytes, so
/// only the capture stage's own numbers are relevant.
#[cold]
#[inline(never)]
fn report_metadata_queue_full(budget: &PipelineBudget) {
    let stats = budget.snapshot();
    warn!(
        current_total = stats.current_total,
        capture_bytes = stats.current_capture,
        dropped_segments = stats.dropped_segments,
        dropped_bytes = stats.dropped_bytes,
        resyncs = stats.resyncs,
        "capture metadata queue full; dropping until resync acknowledgement"
    );
}

/// Which re-anchor a backend-side loss is, in the pipeline's own vocabulary.
///
/// The two become one hole in one byte stream from here on, and this is the
/// last line that can still tell them apart — so it is the line that has to, or
/// the window is back to guessing which it was.
const fn resync_cause(loss: CaptureLoss) -> ResyncCause {
    match loss {
        CaptureLoss::DriverRing => ResyncCause::DriverRing,
        CaptureLoss::Funnel => ResyncCause::CaptureFunnel,
    }
}

/// A recv error ends the loop AND is reported through `fatal`: tracing is inert
/// in the windowed build, so only the session loop can show the player.
pub(super) fn capture_loop_budgeted(
    mut source: Box<dyn PacketSource>,
    tx: mpsc::Sender<CaptureEvent>,
    gate: WatchGate,
    shutdown: watch::Receiver<bool>,
    fatal: mpsc::Sender<String>,
    budget: PipelineBudget,
    pressure_resync: PressureResync,
) {
    let mut was_enabled = gate.is_enabled();
    let mut pending_player_resync = false;
    let mut admitted_segments: u64 = 0;
    loop {
        let segment = match source.next_segment() {
            Ok(segment) => segment,
            Err(err) => {
                if !*shutdown.borrow() {
                    // `Error::Capture`'s `Display` already opens with
                    // "network capture: "; a prefix would double the kind.
                    error!(error = ?err, "capture interrupted");
                    let _ = fatal.blocking_send(err.to_string());
                }
                break;
            }
        };

        // A packet already queued in the driver may arrive after the signal;
        // discard it rather than forward after teardown.
        if *shutdown.borrow() {
            break;
        }

        let enabled = gate.is_enabled();
        // Off -> on: the reassembler must re-anchor before the next byte or it
        // treats the jump as an unfillable gap. The marker retries rather than
        // block for space; parking here backs up the backend's callbacks.
        let arming = enabled && !was_enabled;
        if arming {
            pending_player_resync = true;
        }
        was_enabled = enabled;

        // Whatever the flag holds on this iteration predates the session being
        // armed, so it is discarded rather than charged to it.
        //
        // The tap captures from launch, not from Start: `Session::run` builds
        // the source — which opens every adapter and starts its threads —
        // before the gate is ever armed, so the funnel behind this loop can
        // overflow for as long as the session sits shut, and `LossFlag` keeps
        // the first cause it was handed until someone takes it. Left unread
        // here, that flag met the first segment after arming and billed a
        // brand-new session for a hole that predated it — which is what put
        // "this app fell behind its own capture queue" on screen at the very
        // moment a player pressed Start.
        //
        // The arming iteration is the only place this has to happen, and
        // draining while shut as well would be redundant: the gate starts shut
        // (`app::setup`), `was_enabled` is seeded from it, and so every path
        // from shut to armed passes through exactly one `arming` iteration.
        // Clearing the flag earlier would change when it is cleared, never
        // whether the session is charged.
        //
        // Discarding costs nothing: a shut gate forwards no bytes, so there is
        // no continuity for the hole to break, and the transition above owes a
        // re-anchor before any byte is forwarded — charging the loss would buy
        // a second re-anchor for the hole the first already covers.
        if arming {
            let _ = source.take_capture_loss();
        }

        if !enabled {
            continue;
        }
        if tx.is_closed() {
            break;
        }

        let capacity = segment.payload.capacity();
        if pending_player_resync {
            match tx.try_send(CaptureEvent::Resync) {
                Ok(()) => pending_player_resync = false,
                // These bytes belong to the epoch the resync discards anyway.
                Err(mpsc::error::TrySendError::Full(_)) => {
                    budget.record_drop(capacity);
                    continue;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => break,
            }
        }

        // A backend-side loss breaks continuity like byte pressure, so it
        // reuses the same counted, lossless resync protocol — carrying which
        // loss it was, which is the one thing byte pressure cannot tell it.
        //
        // On an arming iteration this is the second take, and it still charges:
        // what it finds was raised after the arming was seen, so it belongs to
        // the session that just started, not to the idle stretch before it.
        if let Some(loss) = source.take_capture_loss() {
            let cause = resync_cause(loss);
            if pressure_resync.request(&budget, cause) {
                report_backend_loss(&budget, cause);
            }
        }
        if pressure_resync.is_blocking_segments() {
            budget.record_drop(capacity);
            pressure_resync.try_enqueue(&tx);
            continue;
        }

        let segment = match budget.admit_capture(segment) {
            Ok(segment) => segment,
            Err(segment) => {
                budget.record_drop(segment.payload.capacity());
                if pressure_resync.request(&budget, ResyncCause::ByteQuota) {
                    report_byte_pressure(&budget);
                }
                pressure_resync.try_enqueue(&tx);
                continue;
            }
        };
        // Re-read, because the one at the top of this iteration is stale by now:
        // the gate is shut from other threads (`app::workers::shutdown`'s
        // `gate.set(false)`, the GUI's Stop, `WatchGate::request_halt` from the
        // actuator), and between that read and this send sit
        // `source.take_capture_loss` above and `budget.admit_capture`, which takes
        // the usage `Mutex`. A gate shutting in that window forwarded this segment
        // anyway; `a_gate_shut_mid_iteration_forwards_nothing` below is that
        // segment, and it fails without these four lines.
        //
        // Nothing is counted, matching the `!enabled` skip above, which counts
        // nothing either: no byte reaches reassembly while the gate is shut, so
        // there is no continuity for this hole to break, and the next arming
        // already owes a re-anchor before any byte goes out. `continue` drops
        // `segment`, and its lease hands the reserved bytes back
        // (`stream::budget::PayloadLease::drop`).
        //
        // `was_enabled` has to record the shut: this iteration is the only
        // observer of it, so without the store a gate that re-opens before the
        // next `next_segment` returns would show `enabled == was_enabled` and
        // raise no arming — leaving the segment dropped here as a silent gap.
        // `a_gate_shut_and_reopened_between_two_segments_still_re_anchors` is
        // that shape.
        if !gate.is_enabled() {
            was_enabled = false;
            continue;
        }
        match tx.try_send(CaptureEvent::Budgeted(segment)) {
            Ok(()) => {
                admitted_segments += 1;
                if admitted_segments.is_multiple_of(CAPTURE_PROGRESS_EVERY) {
                    let stats = budget.snapshot();
                    debug!(
                        segments = admitted_segments,
                        dropped_segments = stats.dropped_segments,
                        dropped_bytes = stats.dropped_bytes,
                        resyncs = stats.resyncs,
                        "capture progress"
                    );
                }
            }
            // Matched on the error, not the variant: destructuring
            // `Full(CaptureEvent::Budgeted(_))` would leave a second arm that
            // could only be `unreachable!` — a panic on the per-packet path for
            // an impossibility. `capacity` is what `admit_capture` charged.
            Err(mpsc::error::TrySendError::Full(event)) => {
                budget.record_drop(capacity);
                if pressure_resync.request(&budget, ResyncCause::MetadataQueue) {
                    report_metadata_queue_full(&budget);
                }
                // The lease inside releases its bytes on drop, and must do so
                // before the retry below asks for space.
                drop(event);
                pressure_resync.try_enqueue(&tx);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => break,
        }
    }
}

#[cfg(test)]
pub(super) fn capture_loop(
    source: Box<dyn PacketSource>,
    tx: mpsc::Sender<CaptureEvent>,
    gate: WatchGate,
    shutdown: watch::Receiver<bool>,
    fatal: mpsc::Sender<String>,
) {
    capture_loop_budgeted(
        source,
        tx,
        gate,
        shutdown,
        fatal,
        PipelineBudget::new(),
        PressureResync::default(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Result;
    use crate::app::fixtures::initial_anchor_segment;
    use crate::capture::Segment;

    struct EnableOnFirstSegment {
        gate: WatchGate,
        segment: Option<Segment>,
    }

    impl PacketSource for EnableOnFirstSegment {
        fn next_segment(&mut self) -> Result<Segment> {
            if let Some(segment) = self.segment.take() {
                self.gate.set(true);
                return Ok(segment);
            }
            Err(crate::Error::Capture(
                "characterization complete".to_owned(),
            ))
        }
    }

    /// Reports one backend-side packet loss, then delivers `segments` copies of
    /// one segment before running dry.
    ///
    /// `arms` is the gate it opens as the last of those goes out, which leaves
    /// every segment before it crossing a shut gate — the tap `Session::run`
    /// opens at launch, losing packets while the player has not pressed Start
    /// yet. Left `None`, the gate stays as the caller set it.
    struct LosingSource {
        segment: Segment,
        segments: usize,
        lost: Option<CaptureLoss>,
        arms: Option<WatchGate>,
    }

    impl PacketSource for LosingSource {
        fn next_segment(&mut self) -> Result<Segment> {
            let Some(remaining) = self.segments.checked_sub(1) else {
                return Err(crate::Error::Capture(
                    "characterization complete".to_owned(),
                ));
            };
            self.segments = remaining;
            if remaining == 0
                && let Some(gate) = self.arms.take()
            {
                gate.set(true);
            }
            Ok(self.segment.clone())
        }

        fn take_capture_loss(&mut self) -> Option<CaptureLoss> {
            self.lost.take()
        }
    }

    /// Shuts the gate from *inside* an iteration, at the one point the loop
    /// hands control back to the source after it has already read the gate:
    /// `take_capture_loss`. Stands in for the threads that really shut it — the
    /// GUI's Stop, `app::workers::shutdown`, `WatchGate::request_halt` — landing
    /// in the window between that read and the send at the bottom of the loop.
    ///
    /// `None`, so the shut is the only thing this source does: a reported loss
    /// would enqueue a resync and confuse what the assertions read.
    struct ShutsGateMidIteration {
        gate: WatchGate,
        segment: Option<Segment>,
    }

    impl PacketSource for ShutsGateMidIteration {
        fn next_segment(&mut self) -> Result<Segment> {
            self.segment
                .take()
                .ok_or_else(|| crate::Error::Capture("characterization complete".to_owned()))
        }

        fn take_capture_loss(&mut self) -> Option<CaptureLoss> {
            self.gate.set(false);
            None
        }
    }

    /// The cutoff is the gate, not the iteration that read it.
    ///
    /// The loop reads the gate once per segment and then does real work —
    /// `take_capture_loss`, the pressure checks, `admit_capture` and its mutex —
    /// before it sends. A Stop landing in there was answered one segment late.
    #[test]
    fn a_gate_shut_mid_iteration_forwards_nothing() {
        let gate = WatchGate::new(true);
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let (fatal_tx, _fatal_rx) = mpsc::channel(1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let budget = PipelineBudget::new();

        capture_loop_budgeted(
            Box::new(ShutsGateMidIteration {
                gate: gate.clone(),
                segment: Some(initial_anchor_segment(1000, b"AB")),
            }),
            event_tx,
            gate,
            shutdown_rx,
            fatal_tx,
            budget.clone(),
            PressureResync::default(),
        );

        assert!(
            event_rx.try_recv().is_err(),
            "the gate was shut before the send; nothing may cross it"
        );
        let stats = budget.snapshot();
        // The segment was admitted before the gate shut, so its lease is the
        // thing this skip can leak. Zero here is the release.
        assert_eq!(stats.current_capture, 0, "the admitted lease is given back");
        assert_eq!(stats.current_total, 0);
        // Matching the `!enabled` skip at the top of the loop, which counts
        // nothing: a shut gate is a stop, not a pipeline fault.
        assert_eq!(stats.dropped_segments, 0);
        assert_eq!(stats.resyncs, 0);
    }

    /// Shuts the gate mid-iteration on the first segment and has re-opened it by
    /// the time the second is handed over — so no `is_enabled` read at the top of
    /// an iteration ever sees it shut. `set(true)` on every segment is the
    /// shipped shape: `session::apply` re-projects the status on every dispatch.
    struct ShutsThenReopens {
        gate: WatchGate,
        segment: Segment,
        remaining: usize,
        /// One shut only: on the second iteration `take_capture_loss` is called
        /// twice (once for the arming drain at the top), and a second shut there
        /// would close the gate the test just re-opened.
        shuts: bool,
    }

    impl PacketSource for ShutsThenReopens {
        fn next_segment(&mut self) -> Result<Segment> {
            let Some(remaining) = self.remaining.checked_sub(1) else {
                return Err(crate::Error::Capture(
                    "characterization complete".to_owned(),
                ));
            };
            self.remaining = remaining;
            self.gate.set(true);
            Ok(self.segment.clone())
        }

        fn take_capture_loss(&mut self) -> Option<CaptureLoss> {
            if std::mem::take(&mut self.shuts) {
                self.gate.set(false);
            }
            None
        }
    }

    /// The gap the re-read opens, and the re-anchor that closes it.
    ///
    /// Skipping the send leaves a hole in the byte stream, so the reassembler
    /// owes a re-anchor before the next forwarded byte. It gets one only if the
    /// loop remembers that it saw the gate shut — a gate that shuts and re-opens
    /// between two `next_segment` calls is invisible to every other read.
    #[test]
    fn a_gate_shut_and_reopened_between_two_segments_still_re_anchors() {
        let gate = WatchGate::new(true);
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let (fatal_tx, _fatal_rx) = mpsc::channel(1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let second = initial_anchor_segment(1000, b"AB");

        capture_loop_budgeted(
            Box::new(ShutsThenReopens {
                gate: gate.clone(),
                segment: second.clone(),
                remaining: 2,
                shuts: true,
            }),
            event_tx,
            gate,
            shutdown_rx,
            fatal_tx,
            PipelineBudget::new(),
            PressureResync::default(),
        );

        assert!(
            matches!(event_rx.try_recv(), Ok(CaptureEvent::Resync)),
            "the skipped segment is a hole; the next byte needs a fresh origin"
        );
        match event_rx.try_recv().expect("the second segment follows") {
            CaptureEvent::Budgeted(segment) => assert_eq!(segment.seq, second.seq),
            CaptureEvent::Resync | CaptureEvent::PressureResync => {
                panic!("expected the second segment after the resync")
            }
        }
        assert!(
            event_rx.try_recv().is_err(),
            "the first segment never went out"
        );
    }

    #[test]
    fn initial_anchor_off_to_on_enqueues_resync_before_triggering_segment() {
        let gate = WatchGate::new(false);
        let triggering = initial_anchor_segment(1000, b"AB");
        let source = EnableOnFirstSegment {
            gate: gate.clone(),
            segment: Some(triggering.clone()),
        };
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let (fatal_tx, _fatal_rx) = mpsc::channel(1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        capture_loop(Box::new(source), event_tx, gate, shutdown_rx, fatal_tx);

        assert!(matches!(event_rx.try_recv(), Ok(CaptureEvent::Resync)));
        let emitted = event_rx.try_recv().expect("triggering segment follows");
        match emitted {
            CaptureEvent::Budgeted(segment) => {
                assert_eq!(segment.seq, triggering.seq);
                assert_eq!(segment.payload(), triggering.payload);
            }
            CaptureEvent::Resync | CaptureEvent::PressureResync => {
                panic!("expected triggering segment after resync")
            }
        }
    }

    #[test]
    fn off_to_on_resync_drops_bytes_rather_than_parking_the_capture_thread() {
        let gate = WatchGate::new(false);
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let (fatal_tx, _fatal_rx) = mpsc::channel(1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let budget = PipelineBudget::new();
        // Occupy the only slot the resync marker could take: a blocking send
        // here used to park the capture thread and back up the backend's
        // callback queue until it lost packets.
        event_tx.try_send(CaptureEvent::PressureResync).unwrap();
        let source = EnableOnFirstSegment {
            gate: gate.clone(),
            segment: Some(initial_anchor_segment(1000, b"AB")),
        };

        capture_loop_budgeted(
            Box::new(source),
            event_tx,
            gate,
            shutdown_rx,
            fatal_tx,
            budget.clone(),
            PressureResync::default(),
        );

        // Returning at all is the point: the segment ahead of the un-enqueued
        // marker is dropped and counted, never forwarded past a stale anchor.
        assert_eq!(budget.snapshot().dropped_segments, 1);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(CaptureEvent::PressureResync)
        ));
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn a_backend_packet_loss_resyncs_instead_of_ending_the_session() {
        let gate = WatchGate::new(true);
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let (fatal_tx, mut fatal_rx) = mpsc::channel(1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let budget = PipelineBudget::new();
        let source = LosingSource {
            segment: initial_anchor_segment(1000, b"AB"),
            segments: 1,
            lost: Some(CaptureLoss::DriverRing),
            arms: None,
        };

        capture_loop_budgeted(
            Box::new(source),
            event_tx,
            gate,
            shutdown_rx,
            fatal_tx,
            budget.clone(),
            PressureResync::default(),
        );

        // The lost bytes cost one segment and a re-anchor, not the session.
        assert!(matches!(
            event_rx.try_recv(),
            Ok(CaptureEvent::PressureResync)
        ));
        let stats = budget.snapshot();
        assert_eq!(stats.resyncs, 1);
        assert_eq!(stats.dropped_segments, 1);
        assert_eq!(stats.dominant_resync(), Some(ResyncCause::DriverRing));
        // The only fatal is the source running out of characterization data.
        assert_eq!(
            fatal_rx.try_recv().unwrap(),
            "network capture: characterization complete"
        );
    }

    /// Runs one session whose backend reports `loss` before it delivers
    /// anything, and hands back the budget it counted against.
    ///
    /// `arms_after` is how many segments cross the gate while it is still shut,
    /// the gate opening as the one after them goes out. The three shapes are the
    /// three moments a loss can reach the loop:
    ///
    /// - `None` — the gate is open from the first segment and never transitions,
    ///   which is a loss reaching a session already running.
    /// - `Some(0)` — the gate opens as the very first segment is delivered, so
    ///   no shut iteration ever runs and the loss is first seen on the arming
    ///   iteration itself.
    /// - `Some(n)`, `n >= 1` — `n` segments cross the shut gate before it opens,
    ///   so the loss is seen and discarded while the session is still idle.
    ///
    /// The budget rather than its snapshot: `PipelineStats` is not nameable
    /// outside `stream` (see `BudgetLimits`' note there for why it is not
    /// re-exported), and each caller wants different fields off it anyway.
    fn budget_after_backend_loss(loss: CaptureLoss, arms_after: Option<usize>) -> PipelineBudget {
        let (event_tx, _event_rx) = mpsc::channel(4);
        let (fatal_tx, _fatal_rx) = mpsc::channel(1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let budget = PipelineBudget::new();
        let gate = WatchGate::new(arms_after.is_none());
        capture_loop_budgeted(
            Box::new(LosingSource {
                segment: initial_anchor_segment(1000, b"AB"),
                segments: arms_after.unwrap_or(0) + 1,
                lost: Some(loss),
                arms: arms_after.is_some().then(|| gate.clone()),
            }),
            event_tx,
            gate,
            shutdown_rx,
            fatal_tx,
            budget.clone(),
            PressureResync::default(),
        );
        budget
    }

    /// The attribution the window's sentence rests on.
    ///
    /// A ring overflow and a full funnel are the same hole in the same byte
    /// stream by the time reassembly sees them, and they must still arrive here
    /// as different causes. While both landed in one `resyncs` total, the window
    /// read that total as "a slow connection or a driver hiccup" — and told a
    /// player whose machine was merely busy to go and look at their network.
    #[test]
    fn a_funnel_loss_and_a_driver_loss_are_counted_as_different_causes() {
        for (loss, expected) in [
            (CaptureLoss::DriverRing, ResyncCause::DriverRing),
            (CaptureLoss::Funnel, ResyncCause::CaptureFunnel),
        ] {
            let stats = budget_after_backend_loss(loss, None).snapshot();
            assert_eq!(stats.resyncs, 1, "one loss is one re-anchor, either way");
            assert_eq!(stats.dominant_resync(), Some(expected));
        }
    }

    /// The re-anchor a session was billed for before it existed.
    ///
    /// The tap runs from launch and `LossFlag` keeps its first cause until it is
    /// taken, so a funnel overflow during the wait for Start stayed latched for
    /// the whole of it. Taken for the first time past the gate, it landed on the
    /// first segment after arming, and the window told a player who had just
    /// pressed Start that this app had fallen behind its own capture queue.
    #[test]
    fn a_loss_from_before_the_gate_opened_is_not_charged_to_the_session() {
        let stats = budget_after_backend_loss(CaptureLoss::Funnel, Some(1)).snapshot();
        assert_eq!(stats.resyncs, 0, "the loss predates the armed session");
        assert_eq!(stats.dominant_resync(), None);
        // Nor is the armed session's own first segment collateral: it is
        // admitted, not dropped behind a resync it never needed.
        assert_eq!(stats.dropped_segments, 0);
    }

    /// The same loss, with no shut iteration in front of it.
    ///
    /// The flag is raised by the capture threads and only read here, so a
    /// shut-era loss need not be visible on any shut iteration: it can first
    /// appear on the one that detects the arming, which is why that iteration
    /// is where the drain has to sit. `Some(0)` is exactly that shape — the
    /// gate opens with the first segment this loop ever sees — and it is the
    /// case a drain placed on the shut iterations alone would let through, on
    /// the single packet the player's Start produces.
    #[test]
    fn a_loss_first_seen_on_the_arming_iteration_is_not_charged_either() {
        let stats = budget_after_backend_loss(CaptureLoss::Funnel, Some(0)).snapshot();
        assert_eq!(
            stats.resyncs, 0,
            "the loss still predates the armed session"
        );
        assert_eq!(stats.dominant_resync(), None);
        assert_eq!(stats.dropped_segments, 0);
    }
}
