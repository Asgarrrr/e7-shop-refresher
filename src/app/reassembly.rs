//! The reassembly pump: the post-resync anchor window, and the forwarding
//! ladder that keeps a byte-pressure event from being reported as a closed
//! pipeline.
//!
//! Runs as a Tokio task. Its whole input is the [`CaptureEvent`] channel and its
//! whole output is the outbound `BudgetedChunk` channel, so the only state it
//! shares with the capture pump is the [`PressureResync`] marker it
//! acknowledges — the seam is the channel, not a buffer.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::warn;

#[cfg(test)]
use crate::stream::PipelineBudget;
use crate::stream::{BudgetedChunk, BudgetedSegment, InitialBurst, Reassembler, ReassemblyOutcome};

use super::pressure::{CaptureEvent, PressureResync};

/// Conservative one-shot allowance for reordered predecessors immediately
/// after capture resumes. Ten milliseconds is the documented hard cap: it
/// bounds latency even though no server-side timing evidence is available.
const INITIAL_ANCHOR_WINDOW: Duration = Duration::from_millis(10);

enum AnchorState {
    /// Normal forwarding, including process startup before the first Resync.
    Steady,
    /// A Resync occurred and no segment has arrived since.
    AwaitingFirst,
    /// The one bounded post-resync burst is waiting for predecessors.
    Buffering {
        burst: InitialBurst,
        deadline: Instant,
    },
}

/// Consumes capture events, reassembles, forwards the ordered stream.
#[cfg(test)]
async fn reassemble_loop(
    events: mpsc::Receiver<CaptureEvent>,
    raw_tx: mpsc::Sender<BudgetedChunk>,
) {
    reassemble_loop_with_pressure(events, raw_tx, PressureResync::default()).await;
}

pub(super) async fn reassemble_loop_with_pressure(
    mut events: mpsc::Receiver<CaptureEvent>,
    raw_tx: mpsc::Sender<BudgetedChunk>,
    pressure_resync: PressureResync,
) {
    let mut reassembler = Reassembler::new();
    let mut anchor = AnchorState::Steady;
    loop {
        let event = if let AnchorState::Buffering { deadline, .. } = &anchor {
            let deadline = *deadline;
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => {
                    if !flush_anchor(&mut anchor, &mut reassembler, &raw_tx).await {
                        break;
                    }
                    continue;
                }
                _ = raw_tx.closed() => {
                    let _ = flush_anchor(&mut anchor, &mut reassembler, &raw_tx).await;
                    break;
                }
                event = events.recv() => event,
            }
        } else {
            events.recv().await
        };

        let Some(event) = event else {
            let _ = flush_anchor(&mut anchor, &mut reassembler, &raw_tx).await;
            break;
        };

        let segment = match event {
            CaptureEvent::Resync => {
                // A newer epoch invalidates bytes that were never committed.
                // The next allowed segment starts a fresh one-shot window.
                reassembler.clear();
                anchor = AnchorState::AwaitingFirst;
                continue;
            }
            CaptureEvent::PressureResync => {
                reassembler.clear();
                anchor = AnchorState::AwaitingFirst;
                pressure_resync.acknowledge();
                continue;
            }
            CaptureEvent::Budgeted(segment) => segment,
            #[cfg(test)]
            CaptureEvent::Segment(segment) => PipelineBudget::new()
                .admit_capture(segment)
                .expect("test segment fits capture quota"),
        };

        // A SYN is never held behind the anchor deadline: it re-anchors the
        // sequence space, so buffering it would make the burst's own ordering
        // meaningless. Commit any older burst first, then let `Reassembler`
        // classify/reset the connection incarnation immediately
        // (`Reassembler::syn_starts_new_incarnation`).
        if segment.syn {
            if !flush_anchor(&mut anchor, &mut reassembler, &raw_tx).await {
                break;
            }
            anchor = AnchorState::Steady;
            if !forward_or_rearm(&mut reassembler, segment, &raw_tx, &mut anchor).await {
                break;
            }
            continue;
        }

        match &mut anchor {
            AnchorState::Steady => {
                if !forward_or_rearm(&mut reassembler, segment, &raw_tx, &mut anchor).await {
                    break;
                }
            }
            AnchorState::AwaitingFirst => {
                let mut burst = InitialBurst::new();
                if burst.would_exceed(&segment) {
                    anchor = AnchorState::Steady;
                    if !forward_or_rearm(&mut reassembler, segment, &raw_tx, &mut anchor).await {
                        break;
                    }
                    continue;
                }
                burst.push(segment);
                if burst.is_at_limit() {
                    anchor = AnchorState::Buffering {
                        burst,
                        deadline: Instant::now(),
                    };
                    if !flush_anchor(&mut anchor, &mut reassembler, &raw_tx).await {
                        break;
                    }
                } else {
                    anchor = AnchorState::Buffering {
                        burst,
                        deadline: Instant::now() + INITIAL_ANCHOR_WINDOW,
                    };
                }
            }
            AnchorState::Buffering { burst, .. } => {
                if burst.would_exceed(&segment) {
                    if !flush_anchor(&mut anchor, &mut reassembler, &raw_tx).await {
                        break;
                    }
                    if !forward_or_rearm(&mut reassembler, segment, &raw_tx, &mut anchor).await {
                        break;
                    }
                } else {
                    burst.push(segment);
                    if burst.is_at_limit()
                        && !flush_anchor(&mut anchor, &mut reassembler, &raw_tx).await
                    {
                        break;
                    }
                }
            }
        }
    }
}

async fn flush_anchor(
    anchor: &mut AnchorState,
    reassembler: &mut Reassembler,
    raw_tx: &mpsc::Sender<BudgetedChunk>,
) -> bool {
    let AnchorState::Buffering { burst, .. } = std::mem::replace(anchor, AnchorState::Steady)
    else {
        return true;
    };
    for segment in burst.into_ordered() {
        let status = match reassembler.push_budgeted(segment) {
            ReassemblyOutcome::Chunks(chunks) => forward_chunks(chunks, reassembler, raw_tx).await,
            ReassemblyOutcome::Pressure => ForwardStatus::Pressure,
        };
        match status {
            ForwardStatus::Open => {}
            // Both forms of pressure abandon the rest of the burst: its bytes
            // belong to an origin that no longer exists.
            ForwardStatus::Pressure => {
                *anchor = AnchorState::AwaitingFirst;
                return true;
            }
            ForwardStatus::Closed => return false,
        }
    }
    true
}

/// Forwards `segment`, re-arming the anchor if either form of byte pressure
/// invalidated the origin those bytes belonged to. `false` means the downstream
/// closed and the caller must `break`.
///
/// A plain function rather than a macro on purpose: the four call sites in
/// `reassemble_loop_with_pressure` are the crate's most correctness-critical
/// transitions (a wrong `anchor` here stalls reassembly forever), and a
/// `macro_rules!` would hide them from rust-analyzer, the debugger and the error
/// messages while buying nothing but the same line count. Only the *post*-forward
/// transition lives here — two call sites set `AnchorState::Steady` immediately
/// before calling, and that ordering is theirs to keep.
async fn forward_or_rearm(
    reassembler: &mut Reassembler,
    segment: BudgetedSegment,
    raw_tx: &mpsc::Sender<BudgetedChunk>,
    anchor: &mut AnchorState,
) -> bool {
    match forward_segment(reassembler, segment, raw_tx).await {
        ForwardStatus::Open => true,
        ForwardStatus::Pressure => {
            *anchor = AnchorState::AwaitingFirst;
            true
        }
        ForwardStatus::Closed => false,
    }
}

async fn forward_segment(
    reassembler: &mut Reassembler,
    segment: BudgetedSegment,
    raw_tx: &mpsc::Sender<BudgetedChunk>,
) -> ForwardStatus {
    match reassembler.push_budgeted(segment) {
        ReassemblyOutcome::Chunks(chunks) => forward_chunks(chunks, reassembler, raw_tx).await,
        ReassemblyOutcome::Pressure => ForwardStatus::Pressure,
    }
}

enum ForwardStatus {
    Open,
    Pressure,
    Closed,
}

/// Moves reassembled chunks into the outbound stage.
///
/// A chunk larger than the whole outbound quota can never be retagged, so it
/// is dropped. That is a hole in the forwarded byte stream, not a closed
/// pipeline: reassembly state is cleared so the next segment re-anchors,
/// exactly as under pending-byte pressure. Reporting it as a closed downstream
/// used to tear the session down and call it a clean end.
async fn forward_chunks(
    chunks: Vec<BudgetedChunk>,
    reassembler: &mut Reassembler,
    raw_tx: &mpsc::Sender<BudgetedChunk>,
) -> ForwardStatus {
    // WebSocket frame boundaries are deliberately non-semantic: reassembly
    // has always produced different batches for in-order arrivals and gap
    // fills. Only the concatenated byte order is the protocol contract.
    for chunk in chunks {
        let retag = chunk.retag_outbound();
        tokio::pin!(retag);
        let chunk = tokio::select! {
            _ = raw_tx.closed() => return ForwardStatus::Closed,
            chunk = &mut retag => match chunk {
                Ok(chunk) => chunk,
                Err(oversized) => {
                    let bytes = oversized.capacity();
                    reassembler.clear();
                    oversized.record_drop();
                    warn!(
                        bytes,
                        "reassembled chunk exceeds the outbound quota; dropped, re-anchoring"
                    );
                    return ForwardStatus::Pressure;
                }
            },
        };
        if raw_tx.send(chunk).await.is_err() {
            return ForwardStatus::Closed;
        }
    }
    ForwardStatus::Open
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};

    use super::*;
    use crate::app::fixtures::{
        initial_anchor_segment, initial_anchor_segment_in, segment_with_capacity,
    };
    use crate::capture::FlowKey;
    use crate::stream::BudgetLimits;

    async fn recv_exact(rx: &mut mpsc::Receiver<BudgetedChunk>, expected_len: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        while bytes.len() < expected_len {
            let chunk = rx.recv().await.expect("outbound channel closed early");
            bytes.extend_from_slice(chunk.as_slice());
        }
        bytes
    }

    #[tokio::test]
    async fn stalled_outbound_never_exceeds_pipeline_budget() {
        let budget = PipelineBudget::with_test_limits(BudgetLimits {
            global: 128,
            capture: 128,
            reassembly: 128,
            outbound: 64,
        });
        let mut reassembler = Reassembler::new();
        let mut chunks = match reassembler.push_budgeted(
            budget
                .admit_capture(segment_with_capacity(1000, 1, 64))
                .unwrap(),
        ) {
            ReassemblyOutcome::Chunks(chunks) => chunks,
            ReassemblyOutcome::Pressure => panic!("unexpected pressure"),
        };
        chunks.extend(
            match reassembler.push_budgeted(
                budget
                    .admit_capture(segment_with_capacity(1001, 1, 64))
                    .unwrap(),
            ) {
                ReassemblyOutcome::Chunks(chunks) => chunks,
                ReassemblyOutcome::Pressure => panic!("unexpected pressure"),
            },
        );
        let (tx, mut rx) = mpsc::channel(1);
        // The reassembler travels with the task: `forward_chunks` may have to
        // clear it, and the test still needs it afterwards to release leases.
        let task = tokio::spawn(async move {
            let status = forward_chunks(chunks, &mut reassembler, &tx).await;
            (status, reassembler)
        });
        tokio::task::yield_now().await;
        let stats = budget.snapshot();
        assert_eq!(stats.current_total, 128);
        assert_eq!(stats.current_outbound, 64);
        assert!(stats.current_total <= 128);

        let first = rx.recv().await.unwrap();
        drop(first);
        let (status, reassembler) = task.await.unwrap();
        assert!(matches!(status, ForwardStatus::Open));
        let second = rx.recv().await.unwrap();
        assert!(budget.snapshot().current_total <= 128);
        drop(second);
        drop(reassembler);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn steady_pending_pressure_rearms_the_initial_anchor_window() {
        let budget = PipelineBudget::with_test_limits(BudgetLimits {
            global: 256,
            capture: 256,
            reassembly: 8,
            outbound: 256,
        });
        let (event_tx, event_rx) = mpsc::channel(8);
        let (raw_tx, mut raw_rx) = mpsc::channel(8);
        let task = tokio::spawn(reassemble_loop_with_pressure(
            event_rx,
            raw_tx,
            PressureResync::default(),
        ));

        event_tx
            .send(CaptureEvent::Budgeted(
                budget
                    .admit_capture(segment_with_capacity(1000, 1, 8))
                    .unwrap(),
            ))
            .await
            .unwrap();
        drop(raw_rx.recv().await.unwrap());

        event_tx
            .send(CaptureEvent::Budgeted(
                budget
                    .admit_capture(segment_with_capacity(2000, 1, 16))
                    .unwrap(),
            ))
            .await
            .unwrap();
        event_tx
            .send(CaptureEvent::Budgeted(
                budget
                    .admit_capture(segment_with_capacity(9000, 1, 8))
                    .unwrap(),
            ))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert!(matches!(
            raw_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(budget.snapshot().current_capture, 8);

        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;
        assert_eq!(recv_exact(&mut raw_rx, 1).await, b"X");
        drop(event_tx);
        task.await.unwrap();
        drop(raw_rx);
        assert_eq!(budget.snapshot().current_total, 0);
        assert_eq!(budget.snapshot().resyncs, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_first_post_resync_segment_waits_once_then_forwards() {
        let (event_tx, event_rx) = mpsc::channel(4);
        let (raw_tx, mut raw_rx) = mpsc::channel(1);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx));

        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(1000, b"AB")))
            .await
            .unwrap();

        tokio::task::yield_now().await;
        assert!(matches!(
            raw_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;
        assert_eq!(recv_exact(&mut raw_rx, 2).await, b"AB");
        drop(event_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_all_six_permutations_preserve_order_across_chunks() {
        let segments = [
            initial_anchor_segment(1000, b"AB"),
            initial_anchor_segment(1002, b"CD"),
            initial_anchor_segment(1004, b"EF"),
        ];
        for permutation in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let (event_tx, event_rx) = mpsc::channel(4);
            let (raw_tx, mut raw_rx) = mpsc::channel(1);
            let task = tokio::spawn(reassemble_loop(event_rx, raw_tx));
            event_tx.send(CaptureEvent::Resync).await.unwrap();
            for index in permutation {
                event_tx
                    .send(CaptureEvent::Segment(segments[index].clone()))
                    .await
                    .unwrap();
            }

            tokio::task::yield_now().await;
            assert!(matches!(
                raw_rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
            tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;
            assert_eq!(
                recv_exact(&mut raw_rx, 6).await,
                b"ABCDEF",
                "arrival permutation {permutation:?}"
            );
            drop(event_tx);
            task.await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_exact_segment_limit_flushes_immediately() {
        let (event_tx, event_rx) = mpsc::channel(256);
        let (raw_tx, mut raw_rx) = mpsc::channel(1);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx));
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        for index in 0..128u32 {
            event_tx
                .send(CaptureEvent::Segment(initial_anchor_segment(
                    1000 + index,
                    b"X",
                )))
                .await
                .unwrap();
        }

        assert_eq!(recv_exact(&mut raw_rx, 128).await, vec![b'X'; 128]);
        drop(event_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_exact_byte_limit_flushes_immediately() {
        let (event_tx, event_rx) = mpsc::channel(2);
        let (raw_tx, mut raw_rx) = mpsc::channel(1);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx));
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(
                1000,
                &vec![b'X'; crate::stream::INITIAL_ANCHOR_MAX_BYTES],
            )))
            .await
            .unwrap();

        assert_eq!(
            raw_rx.recv().await.map(|bytes| bytes.len()),
            Some(crate::stream::INITIAL_ANCHOR_MAX_BYTES)
        );
        drop(event_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_byte_overflow_flushes_before_processing_next_segment() {
        let (event_tx, event_rx) = mpsc::channel(3);
        let (raw_tx, mut raw_rx) = mpsc::channel(2);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx));
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(
                1000,
                &vec![b'A'; crate::stream::INITIAL_ANCHOR_MAX_BYTES - 1],
            )))
            .await
            .unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(
                1000u32.wrapping_add((crate::stream::INITIAL_ANCHOR_MAX_BYTES - 1) as u32),
                b"BC",
            )))
            .await
            .unwrap();

        assert_eq!(
            raw_rx.recv().await.map(|bytes| bytes.len()),
            Some(crate::stream::INITIAL_ANCHOR_MAX_BYTES - 1)
        );
        assert_eq!(recv_exact(&mut raw_rx, 2).await, b"BC");
        drop(event_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_channel_close_flushes_pending_burst() {
        let (event_tx, event_rx) = mpsc::channel(2);
        let (raw_tx, mut raw_rx) = mpsc::channel(1);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx));
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(1000, b"AB")))
            .await
            .unwrap();
        drop(event_tx);

        assert_eq!(recv_exact(&mut raw_rx, 2).await, b"AB");
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_downstream_close_does_not_wait_for_deadline() {
        let (event_tx, event_rx) = mpsc::channel(2);
        let (raw_tx, raw_rx) = mpsc::channel(1);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx));
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(1000, b"AB")))
            .await
            .unwrap();
        tokio::task::yield_now().await;

        // The paused clock advances whenever nothing is runnable, so
        // `task.await` resolves with or without the `raw_tx.closed()` branch:
        // the deadline arm would reach the same `break` 10 ms later. The elapsed
        // time is the only thing that tells the two apart.
        let before = Instant::now();
        drop(raw_rx);
        task.await.unwrap();

        assert_eq!(
            Instant::now().duration_since(before),
            Duration::ZERO,
            "a closed downstream must not wait out the anchor window"
        );
        drop(event_tx);
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_new_resync_discards_and_rearms_pending_epoch() {
        let (event_tx, event_rx) = mpsc::channel(4);
        let (raw_tx, mut raw_rx) = mpsc::channel(1);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx));
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(1000, b"AB")))
            .await
            .unwrap();
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(9000, b"XY")))
            .await
            .unwrap();

        tokio::task::yield_now().await;
        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;
        assert_eq!(recv_exact(&mut raw_rx, 2).await, b"XY");
        drop(event_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_returns_to_immediate_steady_state_after_one_flush() {
        let (event_tx, event_rx) = mpsc::channel(3);
        let (raw_tx, mut raw_rx) = mpsc::channel(2);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx));
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(1000, b"AB")))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;
        assert_eq!(recv_exact(&mut raw_rx, 2).await, b"AB");

        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(1002, b"CD")))
            .await
            .unwrap();
        assert_eq!(recv_exact(&mut raw_rx, 2).await, b"CD");
        drop(event_tx);
        task.await.unwrap();
    }

    /// Two game connections captured at once: the burst sorts each one on its
    /// own sequence space, but replays them into the slots they were observed
    /// in, so the alternation between the connections survives the reordering.
    #[tokio::test(start_paused = true)]
    async fn initial_anchor_isolates_flows_while_preserving_slots() {
        let (event_tx, event_rx) = mpsc::channel(6);
        let (raw_tx, mut raw_rx) = mpsc::channel(1);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx));
        let first = initial_anchor_segment(1000, b"AB").flow;
        let second = FlowKey {
            client: SocketAddr::from((Ipv4Addr::new(192, 168, 1, 10), 52000)),
            server: first.server,
        };
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        for segment in [
            initial_anchor_segment_in(first, 1002, false, b"CD"),
            initial_anchor_segment_in(second, 2002, false, b"WX"),
            initial_anchor_segment_in(first, 1000, false, b"AB"),
            initial_anchor_segment_in(second, 2000, false, b"UV"),
        ] {
            event_tx.send(CaptureEvent::Segment(segment)).await.unwrap();
        }
        tokio::task::yield_now().await;
        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;

        assert_eq!(recv_exact(&mut raw_rx, 8).await, b"ABUVCDWX");
        drop(event_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_wrap_and_overlap_are_reassembled_after_burst_ordering() {
        let (event_tx, event_rx) = mpsc::channel(5);
        let (raw_tx, mut raw_rx) = mpsc::channel(1);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx));
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        for segment in [
            initial_anchor_segment(0, b"CDEF"),
            initial_anchor_segment(2, b"EFGH"),
            initial_anchor_segment(u32::MAX - 1, b"ABCD"),
        ] {
            event_tx.send(CaptureEvent::Segment(segment)).await.unwrap();
        }
        tokio::task::yield_now().await;
        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;

        assert_eq!(recv_exact(&mut raw_rx, 8).await, b"ABCDEFGH");
        drop(event_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_syn_is_never_delayed_and_new_flows_need_no_global_window() {
        let (event_tx, event_rx) = mpsc::channel(6);
        let (raw_tx, mut raw_rx) = mpsc::channel(2);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx));
        let first = initial_anchor_segment(1000, b"AB").flow;
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment_in(
                first, 999, true, b"",
            )))
            .await
            .unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(1000, b"AB")))
            .await
            .unwrap();
        assert_eq!(recv_exact(&mut raw_rx, 2).await, b"AB");

        let second = FlowKey {
            client: SocketAddr::from((Ipv4Addr::new(192, 168, 1, 10), 52000)),
            server: first.server,
        };
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment_in(
                second, 4999, true, b"",
            )))
            .await
            .unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment_in(
                second, 5000, false, b"XY",
            )))
            .await
            .unwrap();
        assert_eq!(recv_exact(&mut raw_rx, 2).await, b"XY");
        drop(event_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_new_syn_flushes_pending_burst_then_resets_immediately() {
        let (event_tx, event_rx) = mpsc::channel(5);
        let (raw_tx, mut raw_rx) = mpsc::channel(2);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx));
        let flow = initial_anchor_segment(1000, b"old").flow;
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment_in(
                flow, 1000, false, b"old",
            )))
            .await
            .unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment_in(
                flow, 8999, true, b"",
            )))
            .await
            .unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment_in(
                flow, 9000, false, b"new",
            )))
            .await
            .unwrap();

        assert_eq!(recv_exact(&mut raw_rx, 3).await, b"old");
        assert_eq!(recv_exact(&mut raw_rx, 3).await, b"new");
        drop(event_tx);
        task.await.unwrap();
    }
}
