//! The reassembly pump: the post-resync anchor window, and the forwarding
//! ladder that keeps a byte-pressure event from being reported as a closed
//! pipeline. The only state shared with the capture pump is the
//! [`PressureResync`] marker this task acknowledges.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::warn;

#[cfg(test)]
use crate::stream::PipelineBudget;
use crate::stream::{BudgetedChunk, BudgetedSegment, InitialBurst, Reassembler, ReassemblyOutcome};

use super::pressure::{CaptureEvent, PressureResync};

/// One-shot allowance for reordered predecessors right after capture resumes.
/// A hard cap on latency, chosen without server-side timing evidence.
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
        };

        // A SYN re-anchors the sequence space, so holding it behind the
        // deadline would make the burst's ordering meaningless: commit any
        // older burst first, then let `Reassembler` reset the incarnation.
        if segment.syn {
            if !flush_anchor(&mut anchor, &mut reassembler, &raw_tx).await {
                break;
            }
            // Nothing assigned here: `flush_anchor` already leaves `anchor`
            // `Steady` on every exit path except one — a byte-pressure event
            // during the flush, which re-arms `AwaitingFirst` for the flows
            // that burst's abandonment orphaned. That re-arm belongs to those
            // other flows, not to this SYN, which re-anchors itself the
            // moment `forward_or_rearm` below runs; overwriting it here was
            // this SYN cancelling a promise made to someone else.
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
            // Either form of pressure abandons the rest of the burst: the
            // failing flow's bytes belong to an origin that no longer exists,
            // and dropping the others is conservatism — one anchor window is
            // re-armed here and the survivors carry on through it.
            ForwardStatus::Pressure => {
                *anchor = AnchorState::AwaitingFirst;
                return true;
            }
            ForwardStatus::Closed => return false,
        }
    }
    true
}

/// Forwards `segment`, re-arming the anchor if byte pressure invalidated the
/// origin those bytes belonged to. `false` means the downstream closed and the
/// caller must `break`.
///
/// Only the *post*-forward transition lives here — one call site (the
/// `would_exceed` path in the main loop) sets `AnchorState::Steady`
/// immediately before calling; that ordering is its own, because no
/// `flush_anchor` precedes it there. The SYN path's call, by contrast, is
/// preceded by `flush_anchor`, which already owns `anchor` on every exit it
/// takes — including the pressure re-arm a caller must not overwrite — so it
/// assigns nothing first.
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

/// A chunk larger than the whole outbound quota can never be retagged, so it is
/// dropped — a hole in the byte stream, not a closed pipeline — and reassembly
/// is cleared so the next segment re-anchors. This used to tear the session
/// down and report it as a clean end.
async fn forward_chunks(
    chunks: Vec<BudgetedChunk>,
    reassembler: &mut Reassembler,
    raw_tx: &mpsc::Sender<BudgetedChunk>,
) -> ForwardStatus {
    // WebSocket frame boundaries are deliberately non-semantic: only the
    // concatenated byte order is the protocol contract.
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
        budgeted, initial_anchor_segment, initial_anchor_segment_in, segment_with_capacity,
    };
    use crate::capture::{FlowKey, Segment};
    use crate::stream::BudgetLimits;

    async fn recv_exact(rx: &mut mpsc::Receiver<BudgetedChunk>, expected_len: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        while bytes.len() < expected_len {
            let chunk = rx.recv().await.expect("outbound channel closed early");
            bytes.extend_from_slice(chunk.as_slice());
        }
        bytes
    }

    /// A running [`reassemble_loop`] and the three things a test drives it
    /// through: the capture side it feeds, the outbound side it reads, and the
    /// budget both are accounted against.
    ///
    /// The budget is a field rather than a local because a test must admit its
    /// segments against the *same* accounting the pump releases them from —
    /// see [`budgeted`]'s note on the fixture that made that impossible.
    ///
    /// Deliberately not hidden: both channel depths, because several tests here
    /// size one so that a `send` cannot outrun the loop and one sizes the
    /// outbound side to hold a chunk back; and the shutdown, which stays two
    /// lines at the call site (`drop(pump.event_tx)` then awaiting `pump.task`)
    /// because three tests read `raw_rx` or `budget` *after* the loop has ended.
    struct Pump {
        event_tx: mpsc::Sender<CaptureEvent>,
        raw_rx: mpsc::Receiver<BudgetedChunk>,
        budget: PipelineBudget,
        task: tokio::task::JoinHandle<()>,
    }

    impl Pump {
        /// Spawns the loop against a full-size budget: nothing sheds, so what a
        /// test observes is the anchor window's own behaviour.
        fn spawn(events: usize, raw: usize) -> Self {
            Self::with_budget(PipelineBudget::new(), events, raw)
        }

        fn with_budget(budget: PipelineBudget, events: usize, raw: usize) -> Self {
            let (event_tx, event_rx) = mpsc::channel(events);
            let (raw_tx, raw_rx) = mpsc::channel(raw);
            Self {
                event_tx,
                raw_rx,
                budget,
                task: tokio::spawn(reassemble_loop(event_rx, raw_tx)),
            }
        }

        /// Admits `segment` against this pump's budget and hands it to the loop.
        async fn send(&self, segment: Segment) {
            self.event_tx
                .send(CaptureEvent::Budgeted(budgeted(&self.budget, segment)))
                .await
                .unwrap();
        }

        async fn resync(&self) {
            self.event_tx.send(CaptureEvent::Resync).await.unwrap();
        }
    }

    /// A budget whose only tight stage is reassembly: eight bytes is less than
    /// one buffered gap, so a flow that has to hold a predecessor reaches
    /// [`ReassemblyOutcome::Pressure`] instead of buffering it. Every other
    /// stage is generous, so the pressure a test using this sees is always the
    /// reassembly one and never an incidental capture or outbound refusal.
    fn tight_reassembly_budget() -> PipelineBudget {
        PipelineBudget::with_test_limits(BudgetLimits {
            global: 256,
            capture: 256,
            reassembly: 8,
            outbound: 256,
        })
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
        // Travels with the task: `forward_chunks` may clear it, and the test
        // still needs it afterwards to release leases.
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
        let mut pump = Pump::with_budget(tight_reassembly_budget(), 8, 8);

        pump.send(segment_with_capacity(1000, 1, 8)).await;
        drop(pump.raw_rx.recv().await.unwrap());

        pump.send(segment_with_capacity(2000, 1, 16)).await;
        pump.send(segment_with_capacity(9000, 1, 8)).await;
        tokio::task::yield_now().await;
        assert!(matches!(
            pump.raw_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(pump.budget.snapshot().current_capture, 8);

        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;
        assert_eq!(recv_exact(&mut pump.raw_rx, 1).await, b"X");
        drop(pump.event_tx);
        pump.task.await.unwrap();
        drop(pump.raw_rx);
        assert_eq!(pump.budget.snapshot().current_total, 0);
        assert_eq!(pump.budget.snapshot().resyncs, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_first_post_resync_segment_waits_once_then_forwards() {
        let mut pump = Pump::spawn(4, 1);

        pump.resync().await;
        pump.send(initial_anchor_segment(1000, b"AB")).await;

        tokio::task::yield_now().await;
        assert!(matches!(
            pump.raw_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;
        assert_eq!(recv_exact(&mut pump.raw_rx, 2).await, b"AB");
        drop(pump.event_tx);
        pump.task.await.unwrap();
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
            let mut pump = Pump::spawn(4, 1);
            pump.resync().await;
            for index in permutation {
                pump.send(segments[index].clone()).await;
            }

            tokio::task::yield_now().await;
            assert!(matches!(
                pump.raw_rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
            tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;
            assert_eq!(
                recv_exact(&mut pump.raw_rx, 6).await,
                b"ABCDEF",
                "arrival permutation {permutation:?}"
            );
            drop(pump.event_tx);
            pump.task.await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_exact_segment_limit_flushes_immediately() {
        let mut pump = Pump::spawn(256, 1);
        pump.resync().await;
        for index in 0..128u32 {
            pump.send(initial_anchor_segment(1000 + index, b"X")).await;
        }

        assert_eq!(recv_exact(&mut pump.raw_rx, 128).await, vec![b'X'; 128]);
        drop(pump.event_tx);
        pump.task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_exact_byte_limit_flushes_immediately() {
        let mut pump = Pump::spawn(2, 1);
        pump.resync().await;
        pump.send(initial_anchor_segment(
            1000,
            &vec![b'X'; crate::stream::INITIAL_ANCHOR_MAX_BYTES],
        ))
        .await;

        assert_eq!(
            pump.raw_rx.recv().await.map(|bytes| bytes.len()),
            Some(crate::stream::INITIAL_ANCHOR_MAX_BYTES)
        );
        drop(pump.event_tx);
        pump.task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_byte_overflow_flushes_before_processing_next_segment() {
        let mut pump = Pump::spawn(3, 2);
        pump.resync().await;
        pump.send(initial_anchor_segment(
            1000,
            &vec![b'A'; crate::stream::INITIAL_ANCHOR_MAX_BYTES - 1],
        ))
        .await;
        pump.send(initial_anchor_segment(
            1000u32.wrapping_add((crate::stream::INITIAL_ANCHOR_MAX_BYTES - 1) as u32),
            b"BC",
        ))
        .await;

        assert_eq!(
            pump.raw_rx.recv().await.map(|bytes| bytes.len()),
            Some(crate::stream::INITIAL_ANCHOR_MAX_BYTES - 1)
        );
        assert_eq!(recv_exact(&mut pump.raw_rx, 2).await, b"BC");
        drop(pump.event_tx);
        pump.task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_channel_close_flushes_pending_burst() {
        let mut pump = Pump::spawn(2, 1);
        pump.resync().await;
        pump.send(initial_anchor_segment(1000, b"AB")).await;
        drop(pump.event_tx);

        assert_eq!(recv_exact(&mut pump.raw_rx, 2).await, b"AB");
        pump.task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_downstream_close_does_not_wait_for_deadline() {
        let pump = Pump::spawn(2, 1);
        pump.resync().await;
        pump.send(initial_anchor_segment(1000, b"AB")).await;
        tokio::task::yield_now().await;

        // The paused clock advances whenever nothing is runnable, so both
        // branches reach the same `break`; elapsed time is the only tell.
        let before = Instant::now();
        drop(pump.raw_rx);
        pump.task.await.unwrap();

        assert_eq!(
            Instant::now().duration_since(before),
            Duration::ZERO,
            "a closed downstream must not wait out the anchor window"
        );
        drop(pump.event_tx);
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_new_resync_discards_and_rearms_pending_epoch() {
        let mut pump = Pump::spawn(4, 1);
        pump.resync().await;
        pump.send(initial_anchor_segment(1000, b"AB")).await;
        pump.resync().await;
        pump.send(initial_anchor_segment(9000, b"XY")).await;

        tokio::task::yield_now().await;
        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;
        assert_eq!(recv_exact(&mut pump.raw_rx, 2).await, b"XY");
        drop(pump.event_tx);
        pump.task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_returns_to_immediate_steady_state_after_one_flush() {
        let mut pump = Pump::spawn(3, 2);
        pump.resync().await;
        pump.send(initial_anchor_segment(1000, b"AB")).await;
        tokio::task::yield_now().await;
        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;
        assert_eq!(recv_exact(&mut pump.raw_rx, 2).await, b"AB");

        pump.send(initial_anchor_segment(1002, b"CD")).await;
        assert_eq!(recv_exact(&mut pump.raw_rx, 2).await, b"CD");
        drop(pump.event_tx);
        pump.task.await.unwrap();
    }

    /// Two connections at once: the burst sorts each on its own sequence space
    /// but replays into the observed slots, so the alternation survives.
    #[tokio::test(start_paused = true)]
    async fn initial_anchor_isolates_flows_while_preserving_slots() {
        let mut pump = Pump::spawn(6, 1);
        let first = initial_anchor_segment(1000, b"AB").flow;
        let second = FlowKey {
            client: SocketAddr::from((Ipv4Addr::new(192, 168, 1, 10), 52000)),
            server: first.server,
        };
        pump.resync().await;
        for segment in [
            initial_anchor_segment_in(first, 1002, false, b"CD"),
            initial_anchor_segment_in(second, 2002, false, b"WX"),
            initial_anchor_segment_in(first, 1000, false, b"AB"),
            initial_anchor_segment_in(second, 2000, false, b"UV"),
        ] {
            pump.send(segment).await;
        }
        tokio::task::yield_now().await;
        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;

        assert_eq!(recv_exact(&mut pump.raw_rx, 8).await, b"ABUVCDWX");
        drop(pump.event_tx);
        pump.task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_wrap_and_overlap_are_reassembled_after_burst_ordering() {
        let mut pump = Pump::spawn(5, 1);
        pump.resync().await;
        for segment in [
            initial_anchor_segment(0, b"CDEF"),
            initial_anchor_segment(2, b"EFGH"),
            initial_anchor_segment(u32::MAX - 1, b"ABCD"),
        ] {
            pump.send(segment).await;
        }
        tokio::task::yield_now().await;
        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;

        assert_eq!(recv_exact(&mut pump.raw_rx, 8).await, b"ABCDEFGH");
        drop(pump.event_tx);
        pump.task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_syn_is_never_delayed_and_new_flows_need_no_global_window() {
        let mut pump = Pump::spawn(6, 2);
        let first = initial_anchor_segment(1000, b"AB").flow;
        pump.resync().await;
        pump.send(initial_anchor_segment_in(first, 999, true, b""))
            .await;
        pump.send(initial_anchor_segment(1000, b"AB")).await;
        assert_eq!(recv_exact(&mut pump.raw_rx, 2).await, b"AB");

        let second = FlowKey {
            client: SocketAddr::from((Ipv4Addr::new(192, 168, 1, 10), 52000)),
            server: first.server,
        };
        pump.send(initial_anchor_segment_in(second, 4999, true, b""))
            .await;
        pump.send(initial_anchor_segment_in(second, 5000, false, b"XY"))
            .await;
        assert_eq!(recv_exact(&mut pump.raw_rx, 2).await, b"XY");
        drop(pump.event_tx);
        pump.task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_new_syn_flushes_pending_burst_then_resets_immediately() {
        let mut pump = Pump::spawn(5, 2);
        let flow = initial_anchor_segment(1000, b"old").flow;
        pump.resync().await;
        pump.send(initial_anchor_segment_in(flow, 1000, false, b"old"))
            .await;
        pump.send(initial_anchor_segment_in(flow, 8999, true, b""))
            .await;
        pump.send(initial_anchor_segment_in(flow, 9000, false, b"new"))
            .await;

        assert_eq!(recv_exact(&mut pump.raw_rx, 3).await, b"old");
        assert_eq!(recv_exact(&mut pump.raw_rx, 3).await, b"new");
        drop(pump.event_tx);
        pump.task.await.unwrap();
    }

    /// `flush_anchor`'s pressure arm re-arms one anchor window for the flows
    /// the abandoned burst dropped. A SYN that triggers the flush must not
    /// cancel that re-arm for a *different* flow: flow B's gapped segment
    /// below pushes the flush into `ReassemblyOutcome::Pressure`, and the
    /// following reversed pair on flow B must still land in order through
    /// the re-armed window, not go straight through as if nothing had
    /// happened.
    #[tokio::test]
    async fn a_syn_does_not_cancel_the_rearm_a_pressured_flush_asked_for() {
        // Tight enough on the reassembly stage that a real gap cannot be
        // buffered: the second flow-B segment below is what turns the flush
        // into pressure.
        let mut pump = Pump::with_budget(tight_reassembly_budget(), 8, 8);

        let flow_a = initial_anchor_segment(1, b"").flow;
        let flow_b = FlowKey {
            client: SocketAddr::from((Ipv4Addr::new(192, 168, 1, 10), 52000)),
            server: flow_a.server,
        };

        pump.resync().await;
        // Opens the initial-anchor burst and anchors flow B's baseline.
        pump.send(initial_anchor_segment_in(flow_b, 5000, false, b"X"))
            .await;
        // A real gap past that baseline: buffering its 9 bytes exceeds the
        // 8-byte reassembly quota, so replaying this burst hits
        // `ReassemblyOutcome::Pressure`.
        pump.send(initial_anchor_segment_in(flow_b, 5010, false, &[b'Y'; 9]))
            .await;
        // A SYN on an unrelated flow A, still inside the 10 ms window: this
        // is what triggers `flush_anchor` and, with it, the pressure re-arm.
        pump.send(initial_anchor_segment_in(flow_a, 9000, true, b""))
            .await;
        // Two more flow-B segments, arriving in reversed sequence order, meant
        // to land in the re-armed window and come out correctly ordered.
        pump.send(initial_anchor_segment_in(flow_b, 7002, false, b"CD"))
            .await;
        pump.send(initial_anchor_segment_in(flow_b, 7000, false, b"AB"))
            .await;

        drop(pump.event_tx);
        pump.task.await.unwrap();

        // Evidence the pressure arm actually ran, not just that no gap ever
        // formed.
        assert_eq!(pump.budget.snapshot().resyncs, 1);

        assert_eq!(recv_exact(&mut pump.raw_rx, 1).await, b"X");
        assert_eq!(recv_exact(&mut pump.raw_rx, 4).await, b"ABCD");
    }

    /// The old `#[cfg(test)]` fixture re-admitted every segment against its own
    /// throwaway `PipelineBudget::new()`, so no test could ever see a burst's
    /// leases outlive the burst — each segment's lease belonged to a budget
    /// nobody else held a reference to. With one budget shared for the whole
    /// burst, this pins the thing that fixture made impossible to assert: every
    /// byte the anchor window held is released once the burst is flushed and
    /// the forwarded chunks are dropped.
    #[tokio::test(start_paused = true)]
    async fn the_anchor_burst_releases_every_lease_it_held() {
        let mut pump = Pump::spawn(4, 4);

        pump.resync().await;
        for segment in [
            initial_anchor_segment(1000, b"AB"),
            initial_anchor_segment(1002, b"CD"),
            initial_anchor_segment(1004, b"EF"),
        ] {
            pump.send(segment).await;
        }
        tokio::task::yield_now().await;
        assert!(
            pump.budget.snapshot().current_total > 0,
            "the buffered burst must still be holding its leases"
        );

        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;
        assert_eq!(recv_exact(&mut pump.raw_rx, 6).await, b"ABCDEF");

        drop(pump.event_tx);
        pump.task.await.unwrap();
        drop(pump.raw_rx);
        assert_eq!(pump.budget.snapshot().current_total, 0);
    }
}
