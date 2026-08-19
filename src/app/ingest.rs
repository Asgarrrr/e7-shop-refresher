//! The capture pump: the blocking receive loop, and the three cold reports it
//! keeps out of its own body.
//!
//! Runs on a dedicated OS thread (`super::workers` owns the thread and the
//! `catch_unwind` around this loop) and talks to reassembly through exactly
//! two things from `super::pressure`: the [`CaptureEvent`] channel and the
//! [`PressureResync`] marker.

use tokio::sync::{mpsc, watch};
use tracing::{debug, error, warn};

use crate::capture::PacketSource;
use crate::stream::PipelineBudget;
use crate::watch::WatchGate;

use super::pressure::{CaptureEvent, PressureResync};

/// How many admitted segments between two "capture progress" lines. The
/// *absence* of that line in a long log is the diagnostic: the backend is
/// reporting nothing on the configured port.
const CAPTURE_PROGRESS_EVERY: u64 = 1000;

/// The three rare reports of the per-packet loop, out of line.
///
/// `#[cold]` + `#[inline(never)]` keeps only the branch test in
/// [`capture_loop_budgeted`]'s body: with `codegen-units = 1`, LLVM would
/// otherwise inline the tracing callsite machinery and the `PipelineStats`
/// construction (which takes the budget mutex) into the per-packet hot path,
/// for branches that fire once a session or never. Scale is small — one port,
/// 82 matched packets in the feasibility probe — so this is about hot-body
/// layout and readability, not throughput.
#[cold]
#[inline(never)]
fn report_backend_loss(budget: &PipelineBudget) {
    let stats = budget.snapshot();
    warn!(
        dropped_segments = stats.dropped_segments,
        dropped_bytes = stats.dropped_bytes,
        resyncs = stats.resyncs,
        "capture backend lost packets; dropping until resync acknowledgement"
    );
}

/// See [`report_backend_loss`] for why this is out of line. Reports every stage
/// because a byte-pressure event is diagnosed by *which* stage is holding bytes.
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

/// See [`report_backend_loss`] for why this is out of line. Slots, not bytes, ran
/// out here, so only the capture stage's own numbers are relevant.
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

/// Capture loop (synchronous context). Stops when the pipeline closes.
///
/// A recv error ends the loop AND is reported through `fatal`: tracing is
/// inert in the windowed build, so the session loop must journal the failure
/// and turn it into an error outcome the player can see.
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
                    // `Error::Capture`'s `Display` already opens with "network
                    // capture: "; a prefix here would double the kind.
                    error!(error = ?err, "capture interrupted");
                    let _ = fatal.blocking_send(err.to_string());
                }
                break;
            }
        };

        // shutdown_recv may first yield a packet already queued in the driver.
        // Discard it and drop the source rather than forwarding after teardown.
        if *shutdown.borrow() {
            break;
        }

        let enabled = gate.is_enabled();
        // Off -> on: the reassembler must re-anchor before the next byte, or
        // it treats the jump as an unfillable gap and stops delivering. The
        // marker retries later instead of blocking for space — parking this
        // thread would back up the backend's callback queue.
        if enabled && !was_enabled {
            pending_player_resync = true;
        }
        was_enabled = enabled;

        if !enabled {
            continue; // Shop Watch off: emit nothing.
        }
        if tx.is_closed() {
            break;
        }

        let capacity = segment.payload.capacity();
        if pending_player_resync {
            match tx.try_send(CaptureEvent::Resync) {
                Ok(()) => pending_player_resync = false,
                // Every byte ahead of the marker belongs to the epoch the
                // resync discards anyway, so dropping it loses nothing.
                Err(mpsc::error::TrySendError::Full(_)) => {
                    budget.record_drop(capacity);
                    continue;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => break,
            }
        }

        // A backend-side loss breaks continuity like byte pressure, so it
        // reuses the counted, lossless resync protocol instead of stalling.
        if source.take_capture_loss() && pressure_resync.request(&budget) {
            report_backend_loss(&budget);
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
                if pressure_resync.request(&budget) {
                    report_byte_pressure(&budget);
                }
                pressure_resync.try_enqueue(&tx);
                continue;
            }
        };
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
            // `Full(CaptureEvent::Budgeted(segment))` to read `segment.capacity()`
            // left a second `Full(_)` arm that could only be `unreachable!` — a
            // panic on the per-packet path of a `catch_unwind`-wrapped thread, for
            // an impossibility. `capacity`, taken above, is the same number
            // `admit_capture` charges the lease (`BudgetedSegment::capacity` ==
            // `lease.bytes`).
            Err(mpsc::error::TrySendError::Full(event)) => {
                budget.record_drop(capacity);
                if pressure_resync.request(&budget) {
                    report_metadata_queue_full(&budget);
                }
                // Explicit: the lease inside releases the reserved bytes as it
                // goes, and it must go before the retry below asks for space.
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

    /// Reports one backend-side packet loss, then delivers its only segment.
    struct LosingSource {
        segment: Option<Segment>,
        lost: bool,
    }

    impl PacketSource for LosingSource {
        fn next_segment(&mut self) -> Result<Segment> {
            self.segment
                .take()
                .ok_or_else(|| crate::Error::Capture("characterization complete".to_owned()))
        }

        fn take_capture_loss(&mut self) -> bool {
            std::mem::take(&mut self.lost)
        }
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
            CaptureEvent::Segment(_) | CaptureEvent::Resync | CaptureEvent::PressureResync => {
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
            segment: Some(initial_anchor_segment(1000, b"AB")),
            lost: true,
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
        // The only fatal is the source running out of characterization data.
        assert_eq!(
            fatal_rx.try_recv().unwrap(),
            "network capture: characterization complete"
        );
    }
}
