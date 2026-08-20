//! Sequence-space reassembly: ordering, deduplication, gaps, SYN incarnations,
//! and the one-shot initial anchor burst. The relative-offset rule is on the
//! parent module. This half decides *what* to buffer; [`super::budget`] decides
//! *if there is room*.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap};

use tracing::{error, warn};

use super::budget::{
    BudgetedChunk, BudgetedSegment, PipelineBudget, fits_pending, pending_after_release,
};
use super::{INITIAL_ANCHOR_MAX_BYTES, INITIAL_ANCHOR_MAX_SEGMENTS};
use crate::capture::FlowKey;
#[cfg(test)]
use crate::capture::Segment;

/// Cap on tracked streams: reconnections or a port-wide flood could mint keys
/// without bound, each buffering up to `MAX_PENDING_BYTES`. Stalest entry evicted once reached.
const MAX_STREAMS: usize = 64;

/// Segments held during the one-shot initial anchor window.
///
/// Ordering is isolated per flow: each flow's slots are replaced with its
/// sequence-ordered segments, which preserves inter-flow cadence and leaves
/// [`Reassembler`] the sole authority for overlap, dedup, gaps and SYNs.
pub(crate) struct InitialBurst {
    segments: Vec<BudgetedSegment>,
    payload_bytes: usize,
}

impl InitialBurst {
    pub(crate) fn new() -> Self {
        Self {
            segments: Vec::new(),
            payload_bytes: 0,
        }
    }

    pub(crate) fn would_exceed(&self, segment: &BudgetedSegment) -> bool {
        self.segments.len() >= INITIAL_ANCHOR_MAX_SEGMENTS
            || self
                .payload_bytes
                .checked_add(segment.payload().len())
                .is_none_or(|bytes| bytes > INITIAL_ANCHOR_MAX_BYTES)
    }

    /// Admits `segment` into the burst.
    ///
    /// # Panics
    ///
    /// Panics unless [`Self::would_exceed`] was checked first.
    pub(crate) fn push(&mut self, segment: BudgetedSegment) {
        assert!(
            !self.would_exceed(&segment),
            "initial anchor burst limits must be checked before insertion"
        );
        self.payload_bytes += segment.payload().len();
        self.segments.push(segment);
    }

    #[cfg(test)]
    fn push_test(&mut self, segment: Segment) {
        self.push(
            PipelineBudget::new()
                .admit_capture(segment)
                .expect("test segment fits the production capture quota"),
        );
    }

    /// Whether the burst has reached either cap. `>=` on both terms: the byte
    /// total rarely lands on 262 144 exactly, and equality alone would leave
    /// the bound resting on `would_exceed`.
    pub(crate) fn is_at_limit(&self) -> bool {
        self.segments.len() >= INITIAL_ANCHOR_MAX_SEGMENTS
            || self.payload_bytes >= INITIAL_ANCHOR_MAX_BYTES
    }

    pub(crate) fn into_ordered(self) -> Vec<BudgetedSegment> {
        let slots: Vec<_> = self.segments.iter().map(|segment| segment.flow).collect();
        // One flow is the ordinary case; several is the port-wide accident.
        let mut flows: HashMap<_, Vec<BudgetedSegment>> = HashMap::with_capacity(1);
        for segment in self.segments {
            flows.entry(segment.flow).or_default().push(segment);
        }
        for segments in flows.values_mut() {
            // `seq_diff` is not a transitive order on its own, so sort against
            // an origin: a valid TCP window is smaller than the signed
            // half-space, so every segment measures from it unambiguously.
            let origin = segments
                .iter()
                .map(segment_data_seq)
                .reduce(|earliest, candidate| {
                    if seq_diff(candidate, earliest) < 0 {
                        candidate
                    } else {
                        earliest
                    }
                })
                .expect("a burst flow is never empty");
            segments.sort_by_key(|segment| seq_diff(segment_data_seq(segment), origin));
            // Reversed so the replay below can `pop` from the back.
            segments.reverse();
        }

        slots
            .into_iter()
            .map(|key| {
                flows
                    .get_mut(&key)
                    .and_then(Vec::pop)
                    .expect("every burst slot has one segment")
            })
            .collect()
    }
}

/// Reassembles traffic from several connections, keyed by flow.
#[derive(Default)]
pub struct Reassembler {
    streams: HashMap<FlowKey, HalfStream>,
    /// Monotonic activity stamp, bumped per segment; the eviction clock.
    clock: u64,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Integrates a segment, returning the bytes that became contiguous.
    ///
    /// FIN is not modelled: a segment reordered ahead of a gap keeps its
    /// buffered payload until the gap fills, which is why `Pressure` obliges
    /// the caller to re-anchor (`AnchorState::AwaitingFirst`) rather than wait
    /// on a fill that can never arrive.
    pub(crate) fn push_budgeted(&mut self, segment: BudgetedSegment) -> ReassemblyOutcome {
        let key = segment.flow;
        let dropped_capacity = segment.capacity();
        let budget = segment.budget();
        self.clock += 1;
        if segment.syn && self.syn_starts_new_incarnation(&segment) {
            self.streams.remove(&key);
        }
        if self.streams.len() >= MAX_STREAMS && !self.streams.contains_key(&key) {
            self.evict_stalest(&budget);
        }
        let clock = self.clock;
        let half = self.streams.entry(key).or_default();
        half.last_active = clock;
        let outcome = half.push(segment.seq, segment.syn, segment.into_payload());
        match outcome {
            HalfOutcome::Chunks(chunks) => ReassemblyOutcome::Chunks(chunks),
            HalfOutcome::Pressure(cause) => {
                // Sized to the failure. `MAX_PENDING_BYTES` is a *per-stream*
                // cap and half of `REASSEMBLY_STAGE_BYTES`, so one stream fills
                // it with the shared pool still slack; capture is port-wide
                // (`tcp and src port …`, no host filter), so a stray flow can
                // pile 8 MiB behind one lost segment. Clearing every stream for
                // that cost the game's own flow its `baseline` and `next_off`
                // mid-message — a shop refresh — over a quota it never touched.
                // Shared-stage exhaustion is the opposite: the bytes that must
                // come back before *anyone* can buffer again are the other
                // streams' pending buffers, so the wide reset is the recovery.
                match cause {
                    PressureCause::Stream => {
                        self.streams.remove(&key);
                    }
                    PressureCause::Shared => self.clear(),
                }
                // Drop metrics identify only the packet that caused recovery;
                // chunks discarded by the reset above — one stream's or every
                // stream's — are collateral, not extra captures.
                budget.record_drop(dropped_capacity);
                budget.record_resync();
                warn_reassembly_pressure(&budget, cause);
                ReassemblyOutcome::Pressure
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn push(&mut self, segment: &Segment) -> Vec<u8> {
        let admitted = PipelineBudget::new()
            .admit_capture(segment.clone())
            .expect("test segment fits the production capture quota");
        flatten_chunks(self.push_budgeted(admitted))
    }

    /// Whether this SYN starts a new incarnation of an already tracked
    /// connection, so the caller can drop the stale sequence space. Only the
    /// handshake SYN-ACK and its retransmissions reach here — the client's own
    /// SYN travels the uncaptured direction — and a SYN on an untracked flow
    /// simply anchors.
    fn syn_starts_new_incarnation(&self, segment: &BudgetedSegment) -> bool {
        debug_assert!(segment.syn);

        let Some(half) = self.streams.get(&segment.flow) else {
            return false;
        };
        // A retransmitted SYN carries the sequence number already anchored on.
        if half.syn_seq == Some(segment.seq) {
            return false;
        }
        // The handshake SYN arriving late, after data anchored this stream
        // mid-flight at exactly the byte that SYN would have produced.
        if half.syn_seq.is_none() && half.baseline == Some(segment.seq.wrapping_add(1)) {
            return false;
        }
        true
    }

    /// Drops the least-recently-active stream, only when a new key would exceed
    /// `MAX_STREAMS`.
    ///
    /// An eviction *is* an anchor loss, hence the budget: capture is port-wide,
    /// so foreign flows past 64 keys evict by staleness, and between two shop
    /// refreshes the quietest flow on that port is the game's own. It re-anchors
    /// silently, so without `record_resync` the missing snapshot appears in no
    /// counter. Same call as [`Self::push_budgeted`]'s pressure arm, and no
    /// `record_drop`, for the reason stated there.
    fn evict_stalest(&mut self, budget: &PipelineBudget) {
        let Some(&key) = self
            .streams
            .iter()
            .min_by_key(|(_, half)| half.last_active)
            .map(|(key, _)| key)
        else {
            return;
        };
        let evicted = self
            .streams
            .remove(&key)
            .expect("the key was just read out of this map");
        // `clock` counts segments across *all* flows, so this is how many other
        // packets went by while the stream said nothing: the one number that
        // separates "crowded out" from "went quiet".
        let segments_since_active = self.clock.saturating_sub(evicted.last_active);
        budget.record_resync();
        warn_stream_evicted(budget, evicted, segments_since_active);
    }

    /// Resets all state so each flow re-anchors on its next segment. Used
    /// after a Shop Watch pause to avoid resyncing from a stale `next_off`.
    pub fn clear(&mut self) {
        self.streams.clear();
    }
}

/// The captured (server-to-client) half of a connection, in relative offsets.
#[derive(Default)]
struct HalfStream {
    /// Last `Reassembler::clock` value this stream was active; eviction key.
    last_active: u64,
    /// Stream origin (sequence number of the first byte); `None` until first seen.
    baseline: Option<u32>,
    syn_seq: Option<u32>,
    /// Offset (from `baseline`) of the next expected byte.
    next_off: i64,
    /// Buffered future segments, keyed by offset (monotonic order, no wrap).
    pending: BTreeMap<i64, BudgetedChunk>,
    pending_bytes: usize,
}

impl HalfStream {
    fn push(&mut self, seq: u32, syn: bool, payload: BudgetedChunk) -> HalfOutcome {
        // Before `baseline`: `syn_starts_new_incarnation` reads both to tell a
        // retransmitted SYN from a fresh incarnation.
        if syn {
            self.syn_seq.get_or_insert(seq);
        }
        // SYN consumes a sequence number: data starts at seq + 1.
        let data_seq = if syn { seq.wrapping_add(1) } else { seq };
        self.baseline.get_or_insert(data_seq);
        // Measured from the currently expected byte, then shifted back to
        // absolute: the distance stays inside the TCP window, so `seq_diff`'s
        // i32 span never overflows.
        let expected_seq = self.expected_seq();
        let offset = self.next_off + seq_diff(data_seq, expected_seq);

        // `with_capacity(1)`: `Vec::new()`'s first `push` of a 48-byte element
        // jumps to capacity 4. `SmallVec` was declined — a dependency, plus 48
        // bytes inlined into two by-value returns per packet, for a malloc
        // nothing here has profiled.
        let mut out = Vec::with_capacity(1);
        if let Err(cause) = self.absorb(offset, payload, &mut out) {
            return HalfOutcome::Pressure(cause);
        }
        if let Err(cause) = self.drain(&mut out) {
            return HalfOutcome::Pressure(cause);
        }
        HalfOutcome::Chunks(out)
    }

    /// Integrates one segment: in order (append), future (buffer), or old (trim).
    fn absorb(
        &mut self,
        offset: i64,
        mut payload: BudgetedChunk,
        out: &mut Vec<BudgetedChunk>,
    ) -> Result<(), PressureCause> {
        if payload.as_slice().is_empty() {
            return Ok(());
        }
        if offset > self.next_off {
            return self.buffer_future(offset, payload);
        }

        // `try_from`, not `as`: a negative distance would silently become
        // ~1.8e19, fail the length check below, and freeze this half-stream.
        let Ok(already) = usize::try_from(self.next_off - offset) else {
            report_absorb_invariant(self.next_off, offset);
            // Deliver nothing, but report no pressure: no quota refused
            // anything, and inventing one would throw away an anchor over an
            // arithmetic bug in this function.
            return Ok(());
        };
        if already < payload.as_slice().len() {
            if already != 0 {
                payload.drain_front(already);
            }
            self.next_off += payload.as_slice().len() as i64;
            out.push(payload);
        }
        Ok(())
    }

    /// Buffers a segment that sits past the gap, or reports which quota refused
    /// it. The per-stream cap is checked first, being the tighter of the two by
    /// construction: it attributes an overflow to the stream that caused it even
    /// when the shared pool happens to be nearly full as well.
    fn buffer_future(
        &mut self,
        offset: i64,
        mut payload: BudgetedChunk,
    ) -> Result<(), PressureCause> {
        let capacity = payload.capacity();
        // One `entry` probe: a held chunk is displaced only once the new one
        // has cleared both quotas.
        match self.pending.entry(offset) {
            Entry::Occupied(mut slot) => {
                // Keep only the largest segment seen at a given offset.
                if slot.get().as_slice().len() >= payload.as_slice().len() {
                    return Ok(());
                }
                let held = pending_after_release(self.pending_bytes, slot.get().capacity());
                let Some(total) = fits_pending(held, capacity) else {
                    return Err(PressureCause::Stream);
                };
                if !payload.try_retag_pending() {
                    return Err(PressureCause::Shared);
                }
                self.pending_bytes = total;
                // Returns the displaced chunk, whose lease releases as it drops.
                drop(slot.insert(payload));
            }
            Entry::Vacant(slot) => {
                let Some(total) = fits_pending(self.pending_bytes, capacity) else {
                    return Err(PressureCause::Stream);
                };
                if !payload.try_retag_pending() {
                    return Err(PressureCause::Shared);
                }
                self.pending_bytes = total;
                slot.insert(payload);
            }
        }
        Ok(())
    }

    /// Flushes buffered segments that became contiguous once `next_off` advanced.
    fn drain(&mut self, out: &mut Vec<BudgetedChunk>) -> Result<(), PressureCause> {
        while let Some((&offset, _)) = self.pending.first_key_value() {
            if offset > self.next_off {
                break; // gap still present.
            }
            let (offset, payload) = self.pending.pop_first().expect("peeked above");
            self.pending_bytes = pending_after_release(self.pending_bytes, payload.capacity());
            self.absorb(offset, payload, out)?;
        }
        Ok(())
    }

    /// Sequence number of the next expected byte: `baseline + next_off`, back in
    /// the wrapping `u32` space (`push` always sets `baseline` before this runs).
    fn expected_seq(&self) -> u32 {
        // `next_off` is non-negative and read mod 2^32, so keeping the low 32
        // bits is the intended conversion; the `u64` detour keeps that
        // truncation unsigned, avoiding sign extension.
        let offset = (self.next_off as u64) as u32;
        self.baseline.unwrap_or(0).wrapping_add(offset)
    }
}

/// The rare branch of [`HalfStream::absorb`], kept off the per-packet path.
#[cold]
#[inline(never)]
fn report_absorb_invariant(next_off: i64, offset: i64) {
    error!(next_off, offset, "reassembly invariant violated");
    debug_assert!(offset <= next_off, "absorb offset exceeds next_off");
}

/// The rare branch of the pressure arm: the budget mutex and seven fields
/// belong off the per-packet path.
#[cold]
#[inline(never)]
fn warn_reassembly_pressure(budget: &PipelineBudget, cause: PressureCause) {
    let stats = budget.snapshot();
    // The same counters accompany a flood on an unrelated port and a genuine
    // pool exhaustion; only the second is a reason to revisit the quotas.
    let scope = match cause {
        PressureCause::Stream => "one stream's own pending cap; that flow re-anchors",
        PressureCause::Shared => "the shared reassembly quota; every flow re-anchors",
    };
    warn!(
        scope,
        current_total = stats.current_total,
        capture_bytes = stats.current_capture,
        pending_bytes = stats.current_reassembly,
        outbound_bytes = stats.current_outbound,
        dropped_segments = stats.dropped_segments,
        dropped_bytes = stats.dropped_bytes,
        resyncs = stats.resyncs,
        "reassembly pending-byte pressure; state cleared for a fresh anchor"
    );
}

/// The rare branch of [`Reassembler::evict_stalest`], out of line like
/// [`warn_reassembly_pressure`].
///
/// Owns the evicted stream rather than borrowing it: its leases release as it
/// drops, and the pool total is only worth printing once those bytes are back.
/// The absent `record_drop` is not an omission — nothing was *refused* here,
/// and [`Reassembler::push_budgeted`] states the rule; counting the discarded
/// gap buffer would make `dropped_segments` mean one thing under quota pressure
/// and another under table pressure, so its bytes go in the warning instead.
#[cold]
#[inline(never)]
fn warn_stream_evicted(budget: &PipelineBudget, evicted: HalfStream, segments_since_active: u64) {
    // In the counters, an eviction that cost a shop refresh and one that
    // discarded a flow which never delivered a byte are the same event.
    let loss = if evicted.pending_bytes > 0 {
        "a flow buffering behind a gap; its half-received message is gone"
    } else if evicted.next_off > 0 {
        "a flow mid-stream; its decoder resyncs on the next segment"
    } else {
        "a flow that had only anchored; nothing had been delivered from it"
    };
    let delivered_bytes = evicted.next_off;
    let buffered_bytes = evicted.pending_bytes;
    drop(evicted);
    let stats = budget.snapshot();
    warn!(
        loss,
        stream_cap = MAX_STREAMS,
        segments_since_active,
        delivered_bytes,
        buffered_bytes,
        pending_bytes = stats.current_reassembly,
        resyncs = stats.resyncs,
        "stream table full: other flows on the capture port crowded out the stalest one, \
         which must re-anchor"
    );
}

/// Which of the two independent quotas refused to buffer a segment.
///
/// Separate limits, not two readings of one: [`MAX_PENDING_BYTES`] bounds a
/// single stream's gap buffer and is half the reassembly stage's share, so
/// either can be reached with the other still slack. The distinction exists so
/// [`Reassembler::push_budgeted`] can size the recovery to the failure.
///
/// [`MAX_PENDING_BYTES`]: super::MAX_PENDING_BYTES
#[derive(Clone, Copy)]
enum PressureCause {
    /// This one stream filled its own pending-byte cap.
    Stream,
    /// The reassembly stage's shared quota is full, whoever holds it.
    Shared,
}

enum HalfOutcome {
    Chunks(Vec<BudgetedChunk>),
    Pressure(PressureCause),
}

/// What [`Reassembler::push_budgeted`] did with a segment.
pub(crate) enum ReassemblyOutcome {
    /// The bytes that became contiguous, in order. Empty is normal: a duplicate,
    /// a partial gap fill, or a segment still waiting on a predecessor.
    Chunks(Vec<BudgetedChunk>),
    /// A pending-byte quota was exhausted and the segment's flow lost its place:
    /// its state is gone and the caller must re-anchor, not wait. Whether the
    /// other flows were cleared too is deliberately not reported — the caller
    /// re-arms one anchor window either way.
    Pressure,
}

#[cfg(test)]
fn flatten_chunks(outcome: ReassemblyOutcome) -> Vec<u8> {
    match outcome {
        ReassemblyOutcome::Chunks(chunks) => chunks
            .into_iter()
            .flat_map(|chunk| chunk.into_parts().0)
            .collect(),
        ReassemblyOutcome::Pressure => Vec::new(),
    }
}

fn segment_data_seq(segment: &BudgetedSegment) -> u32 {
    if segment.syn {
        segment.seq.wrapping_add(1)
    } else {
        segment.seq
    }
}

/// Signed distance `a - b` over the circular sequence-number space.
const fn seq_diff(a: u32, b: u32) -> i64 {
    (a.wrapping_sub(b) as i32) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{MAX_PENDING_BYTES, flow, flow_from, sized_seg, test_budget};

    fn seg_in(flow: FlowKey, seq: u32, syn: bool, payload: &[u8]) -> Segment {
        Segment {
            flow,
            seq,
            syn,
            payload: Vec::from(payload),
        }
    }

    fn seg(seq: u32, syn: bool, payload: &[u8]) -> Segment {
        seg_in(flow(), seq, syn, payload)
    }

    fn seg_on(flow: FlowKey, seq: u32, payload: &[u8]) -> Segment {
        seg_in(flow, seq, false, payload)
    }

    fn flatten_half(outcome: HalfOutcome) -> Vec<u8> {
        match outcome {
            HalfOutcome::Chunks(chunks) => chunks
                .into_iter()
                .flat_map(|chunk| chunk.into_parts().0)
                .collect(),
            HalfOutcome::Pressure(_) => Vec::new(),
        }
    }

    /// The shared arm: 64 streams, none near its own cap, exhaust the stage
    /// between them, so every anchor deserves to go. The wide `clear()` shows up
    /// as a reassembly total of zero, not the 63 buffers an eviction would leave.
    #[test]
    fn pending_bytes_are_global_across_sixty_four_streams() {
        let budget = test_budget(4096, 4096, 1024, 4096);
        let mut reassembler = Reassembler::new();
        for port in 0..64u16 {
            let flow = flow_from(port + 1);
            drop(flatten_chunks(reassembler.push_budgeted(
                budget.admit_capture(sized_seg(flow, 1000, 1, 16)).unwrap(),
            )));
            let outcome = reassembler
                .push_budgeted(budget.admit_capture(sized_seg(flow, 2000, 1, 16)).unwrap());
            assert!(matches!(outcome, ReassemblyOutcome::Chunks(ref chunks) if chunks.is_empty()));
            assert!(budget.snapshot().current_total <= 4096);
        }
        assert_eq!(budget.snapshot().current_reassembly, 1024);
        // Exactly the check the failing stream is about to run: its own cap has
        // room, so this stays a shared-arm test even if the constants move.
        assert!(
            fits_pending(16, 16).is_some(),
            "the per-stream cap must not be the limit under test here"
        );
        assert!(matches!(
            reassembler.push_budgeted(
                budget
                    .admit_capture(sized_seg(flow_from(1), 3000, 1, 16))
                    .unwrap()
            ),
            ReassemblyOutcome::Pressure
        ));
        assert_eq!(budget.snapshot().current_reassembly, 0);
        drop(reassembler);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    /// The per-stream arm, and the reason the two are told apart at all: a
    /// stranger on the port fills its own 8 MiB cap while the flow that matters
    /// is mid-message, and losing the game flow's baseline there costs a shop
    /// refresh, so the recovery must stop at the stream that overflowed.
    ///
    /// Every shared quota is four times the per-stream cap, so `try_retag_pending`
    /// cannot fail and this stays a test of `fits_pending`.
    #[test]
    fn a_stream_that_fills_its_own_pending_cap_leaves_every_other_anchor_alone() {
        const CHUNK: usize = 64 * 1024;
        const SHARED: usize = MAX_PENDING_BYTES * 4;
        let budget = test_budget(SHARED, SHARED, SHARED, SHARED);
        let mut reassembler = Reassembler::new();
        let push = |reassembler: &mut Reassembler, segment: Segment| {
            flatten_chunks(reassembler.push_budgeted(budget.admit_capture(segment).unwrap()))
        };
        let game = flow_from(51000);
        let stranger = flow_from(51001);

        assert_eq!(push(&mut reassembler, seg_on(game, 1000, b"AB")), b"AB");
        // The stranger anchors, then loses the byte at 1001 forever.
        assert_eq!(push(&mut reassembler, seg_on(stranger, 1000, b"A")), b"A");

        let capacity_chunks = MAX_PENDING_BYTES / CHUNK;
        for index in 0..capacity_chunks {
            let seq = 2000 + (index * CHUNK) as u32;
            assert!(
                push(&mut reassembler, sized_seg(stranger, seq, CHUNK, CHUNK)).is_empty(),
                "chunk {index} sits behind the gap and delivers nothing"
            );
        }
        let before = budget.snapshot();
        assert_eq!(before.current_reassembly, MAX_PENDING_BYTES);
        assert!(
            before.current_reassembly + CHUNK <= SHARED,
            "the shared quota must still have room, or this proves nothing"
        );

        let overflow = 2000 + (capacity_chunks * CHUNK) as u32;
        assert!(matches!(
            reassembler.push_budgeted(
                budget
                    .admit_capture(sized_seg(stranger, overflow, CHUNK, CHUNK))
                    .unwrap()
            ),
            ReassemblyOutcome::Pressure
        ));

        assert!(
            !reassembler.streams.contains_key(&stranger),
            "the stream that overflowed must lose its state and re-anchor"
        );
        assert!(
            reassembler.streams.contains_key(&game),
            "another flow's private cap is not this flow's problem: its anchor must survive"
        );
        let survivor = &reassembler.streams[&game];
        assert_eq!(survivor.baseline, Some(1000));
        assert_eq!(survivor.next_off, 2);
        // A retransmission is recognisable as history only while the anchor
        // holds: a cleared flow re-anchors on it and delivers "AB" twice, a
        // duplicate a decoder can resync from but cannot un-see.
        assert!(
            push(&mut reassembler, seg_on(game, 1000, b"AB")).is_empty(),
            "the surviving flow must still know 1000 is behind it"
        );
        assert_eq!(push(&mut reassembler, seg_on(game, 1002, b"CD")), b"CD");

        assert_eq!(budget.snapshot().current_reassembly, 0);
        drop(reassembler);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    #[test]
    fn pressure_clears_state_and_next_segment_reanchors() {
        let budget = test_budget(256, 256, 8, 256);
        let mut reassembler = Reassembler::new();
        drop(flatten_chunks(reassembler.push_budgeted(
            budget.admit_capture(sized_seg(flow(), 1000, 2, 8)).unwrap(),
        )));
        assert!(matches!(
            reassembler.push_budgeted(
                budget
                    .admit_capture(sized_seg(flow(), 2000, 2, 16))
                    .unwrap()
            ),
            ReassemblyOutcome::Pressure
        ));
        let output = flatten_chunks(
            reassembler.push_budgeted(budget.admit_capture(sized_seg(flow(), 9000, 2, 8)).unwrap()),
        );
        assert_eq!(output, b"XX");
        assert_eq!(budget.snapshot().resyncs, 1);
        drop(reassembler);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    #[test]
    fn gap_fill_moves_chunks_without_exceeding_budget() {
        let budget = test_budget(128, 128, 64, 128);
        let mut reassembler = Reassembler::new();
        drop(flatten_chunks(reassembler.push_budgeted(
            budget.admit_capture(seg(1000, false, b"AB")).unwrap(),
        )));
        assert!(matches!(
            reassembler.push_budgeted(
                budget.admit_capture(seg(1004, false, b"EF")).unwrap()
            ),
            ReassemblyOutcome::Chunks(ref chunks) if chunks.is_empty()
        ));
        let chunks = match reassembler
            .push_budgeted(budget.admit_capture(seg(1002, false, b"CD")).unwrap())
        {
            ReassemblyOutcome::Chunks(chunks) => chunks,
            ReassemblyOutcome::Pressure => panic!("unexpected pressure"),
        };
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].as_slice(), b"CD");
        assert_eq!(chunks[1].as_slice(), b"EF");
        assert!(budget.snapshot().current_total <= 128);
        drop(chunks);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    fn collect_arrivals(segments: &[Segment; 3], permutation: [usize; 3]) -> Vec<u8> {
        let mut reassembler = Reassembler::new();
        let mut output = Vec::new();
        for index in permutation {
            output.extend(reassembler.push(&segments[index]));
        }
        output
    }

    fn collect_anchored(segments: &[Segment; 3], permutation: [usize; 3]) -> Vec<u8> {
        let mut burst = InitialBurst::new();
        for index in permutation {
            burst.push_test(segments[index].clone());
        }
        let mut reassembler = Reassembler::new();
        burst
            .into_ordered()
            .into_iter()
            .flat_map(|segment| flatten_chunks(reassembler.push_budgeted(segment)))
            .collect()
    }

    #[test]
    fn initial_anchor_burst_orders_all_six_permutations() {
        let segments = [
            seg(1000, false, b"AB"),
            seg(1002, false, b"CD"),
            seg(1004, false, b"EF"),
        ];

        for permutation in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            assert_eq!(
                collect_anchored(&segments, permutation),
                b"ABCDEF",
                "arrival permutation {permutation:?}"
            );
        }
    }

    #[test]
    fn initial_anchor_burst_order_is_wrap_safe_and_overlap_stays_centralized() {
        let wrapped = [
            seg(u32::MAX - 1, false, b"AB"),
            seg(0, false, b"CD"),
            seg(2, false, b"EF"),
        ];
        assert_eq!(collect_anchored(&wrapped, [2, 0, 1]), b"ABCDEF");

        let overlap = [
            seg(1000, false, b"ABCD"),
            seg(1002, false, b"CDEF"),
            seg(1006, false, b"GH"),
        ];
        assert_eq!(collect_anchored(&overlap, [1, 2, 0]), b"ABCDEFGH");
    }

    /// Sorting is per flow but the *slots* are global: interleaved connections
    /// come back each ordered, with the alternation intact.
    #[test]
    fn initial_anchor_burst_preserves_inter_flow_slots() {
        let first = flow();
        let second = flow_from(52000);
        let mut burst = InitialBurst::new();
        burst.push_test(seg_on(first, 1002, b"CD"));
        burst.push_test(seg_on(second, 2002, b"WX"));
        burst.push_test(seg_on(first, 1000, b"AB"));
        burst.push_test(seg_on(second, 2000, b"UV"));

        let ordered = burst.into_ordered();
        let observed: Vec<_> = ordered
            .iter()
            .map(|segment| (segment.flow, segment.seq))
            .collect();
        assert_eq!(
            observed,
            vec![(first, 1000), (second, 2000), (first, 1002), (second, 2002)]
        );
    }

    #[test]
    fn initial_anchor_all_six_permutations_keep_the_immediate_suffix() {
        let segments = [
            seg(1000, false, b"AB"),
            seg(1002, false, b"CD"),
            seg(1004, false, b"EF"),
        ];
        let cases: [([usize; 3], &[u8]); 6] = [
            ([0, 1, 2], b"ABCDEF"),
            ([0, 2, 1], b"ABCDEF"),
            ([1, 0, 2], b"CDEF"),
            ([1, 2, 0], b"CDEF"),
            ([2, 0, 1], b"EF"),
            ([2, 1, 0], b"EF"),
        ];

        for (permutation, expected) in cases {
            assert_eq!(
                collect_arrivals(&segments, permutation),
                expected,
                "arrival permutation {permutation:?}"
            );
        }
    }

    /// The suffix rule for **every** arrival order of six segments, at four
    /// origins, two of which straddle the `u32` wrap. Exhaustive (2 880 cases)
    /// rather than sampled: the space is small enough, and the table above
    /// covers the two properties below only for n = 3.
    #[test]
    fn every_arrival_order_yields_the_immediate_suffix_of_the_stream() {
        for origin in [1_000_u32, u32::MAX - 5, u32::MAX - 11, 0] {
            let payloads: [&[u8]; 6] = [b"AB", b"CD", b"EF", b"GH", b"IJ", b"KL"];
            let whole: Vec<u8> = payloads.concat();
            let segments: Vec<Segment> = payloads
                .iter()
                .enumerate()
                .map(|(index, bytes)| {
                    // Origins near `u32::MAX` wrap under the segments.
                    seg(origin.wrapping_add(index as u32 * 2), false, bytes)
                })
                .collect();

            for order in permutations(payloads.len()) {
                let mut reassembler = Reassembler::new();
                let mut delivered = Vec::new();
                for index in order.iter().copied() {
                    delivered.extend(reassembler.push(&segments[index]));
                }
                // 1. A *suffix*, never a permutation and never a gap: the
                //    analysis server resyncs from any point, but cannot
                //    survive reordered or hole-punched bytes.
                assert!(
                    whole.ends_with(&delivered),
                    "origin {origin}, order {order:?} delivered {delivered:?}, not a suffix"
                );
                // 2. The suffix starts at the first segment to arrive, which
                //    is the arrival that anchors the stream.
                let anchor = order[0];
                assert_eq!(
                    delivered.len(),
                    whole.len() - anchor * 2,
                    "origin {origin}, order {order:?} did not anchor on segment {anchor}"
                );
            }
        }
    }

    /// The three algebraic properties `seq_diff` is defined by, over a lattice
    /// of bases including every wrap boundary. The interesting inputs are
    /// exactly those boundaries — `0`, `u32::MAX`, `i32::MAX`, where the signed
    /// reading flips — which a uniform generator reaches with probability ~0.
    #[test]
    fn seq_diff_is_antisymmetric_and_wrap_relative() {
        let bases = [
            0_u32,
            1,
            1_000,
            0x7FFF_FFFF,
            0x8000_0000,
            0x8000_0001,
            u32::MAX - 1,
            u32::MAX,
        ];
        let deltas: [i64; 9] = [-1_000, -2, -1, 0, 1, 2, 1_000, 65_535, 1_048_576];
        for base in bases {
            // 1. Reflexive: zero distance from itself, at every base.
            assert_eq!(seq_diff(base, base), 0, "base {base}");
            for delta in deltas {
                let other = base.wrapping_add(delta as u32);
                // 2. Relative distance: a wrap is invisible, which is how
                //    `HalfStream::push` tracks a stream past 2 GiB.
                assert_eq!(seq_diff(other, base), delta, "base {base}, delta {delta}");
                // 3. Antisymmetric. No `delta` reaches `i32::MIN`, which
                //    cannot be negated: half the space apart, "ahead" and
                //    "behind" are the same answer.
                assert_eq!(
                    seq_diff(base, other),
                    -delta,
                    "base {base}, delta {delta} is not antisymmetric"
                );
            }
        }
        // 2^31 apart reads as `i32::MIN` both ways: why `MAX_PENDING_BYTES` and
        // the anchor logic bound how far out of order a segment may be.
        assert_eq!(seq_diff(0, 0x8000_0000), i64::from(i32::MIN));
        assert_eq!(seq_diff(0x8000_0000, 0), i64::from(i32::MIN));
    }

    /// Every permutation of `0..n`, lexicographic and dependency-free.
    fn permutations(n: usize) -> Vec<Vec<usize>> {
        if n == 0 {
            return vec![Vec::new()];
        }
        let mut out = Vec::new();
        for head in 0..n {
            for mut rest in permutations(n - 1) {
                // Shift tail indices at or above `head` up by one, so `rest`
                // becomes a permutation of `0..n` minus `head`.
                for index in &mut rest {
                    if *index >= head {
                        *index += 1;
                    }
                }
                rest.insert(0, head);
                out.push(rest);
            }
        }
        out
    }

    #[test]
    fn initial_anchor_suffix_characterization_is_wrap_safe() {
        let segments = [
            seg(u32::MAX - 1, false, b"AB"),
            seg(0, false, b"CD"),
            seg(2, false, b"EF"),
        ];

        assert_eq!(collect_arrivals(&segments, [0, 2, 1]), b"ABCDEF");
        assert_eq!(collect_arrivals(&segments, [2, 0, 1]), b"EF");
    }

    #[test]
    fn initial_anchor_overlap_keeps_only_bytes_after_the_immediate_suffix() {
        let mut reassembler = Reassembler::new();
        let arrivals = [
            seg(1002, false, b"CDEF"),
            seg(1000, false, b"ABCD"),
            seg(1002, false, b"CDEF"),
            seg(1006, false, b"GH"),
        ];
        let mut output = Vec::new();
        for segment in arrivals {
            output.extend(reassembler.push(&segment));
        }

        assert_eq!(output, b"CDEFGH");
    }

    /// Each flow anchors on its own first segment: `1000` after `1002` is
    /// history on `first`, while the same seq is the origin on `second`.
    #[test]
    fn initial_anchor_is_isolated_by_flow() {
        let mut reassembler = Reassembler::new();
        let first = flow();
        let second = flow_from(52000);

        assert_eq!(reassembler.push(&seg_on(first, 1002, b"CD")), b"CD");
        assert_eq!(reassembler.push(&seg_on(second, 1000, b"XY")), b"XY");

        assert!(reassembler.push(&seg_on(first, 1000, b"AB")).is_empty());
        assert_eq!(reassembler.push(&seg_on(second, 1002, b"Z!")), b"Z!");
    }

    #[test]
    fn in_order() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
        assert_eq!(r.push(&seg(1002, false, b"CD")), b"CD");
    }

    #[test]
    fn reordering_flushes_multiple_buffered_segments() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
        assert!(r.push(&seg(1006, false, b"GH")).is_empty());
        assert!(r.push(&seg(1004, false, b"EF")).is_empty());
        assert_eq!(r.push(&seg(1002, false, b"CD")), b"CDEFGH");
    }

    #[test]
    fn retransmission_is_ignored() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
        assert!(r.push(&seg(1000, false, b"AB")).is_empty());
    }

    #[test]
    fn overlapping_segment_keeps_only_fresh_tail() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, b"ABCD")), b"ABCD");
        assert_eq!(r.push(&seg(1002, false, b"CDEF")), b"EF"); // "CD" already seen.
    }

    #[test]
    fn syn_sets_the_baseline() {
        let mut r = Reassembler::new();
        assert!(r.push(&seg(999, true, b"")).is_empty()); // SYN anchors origin at 1000.
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
    }

    #[test]
    fn gap_filled_out_of_order() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
        assert!(r.push(&seg(1004, false, b"EF")).is_empty());
        assert_eq!(r.push(&seg(1002, false, b"CD")), b"CDEF");
    }

    #[test]
    fn reassembles_across_sequence_wrap() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(0xFFFF_FFFE, false, b"AB")), b"AB");
        assert_eq!(r.push(&seg(0x0000_0000, false, b"CD")), b"CD");
    }

    #[test]
    fn reordering_across_wrap_is_ordered_correctly() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(0xFFFF_FFFE, false, b"AB")), b"AB");
        assert!(r.push(&seg(0x0000_0002, false, b"EF")).is_empty());
        assert_eq!(r.push(&seg(0x0000_0000, false, b"CD")), b"CDEF");
    }

    #[test]
    fn delivers_far_past_two_gigabytes() {
        // A half-stream past 2^31 delivered bytes: the next in-order segment
        // must still be recognised, where an origin-relative offset overflowed.
        let mut half = HalfStream {
            baseline: Some(0),
            next_off: (1i64 << 31) + 1000,
            ..Default::default()
        };
        let expected = half.expected_seq();
        let budget = PipelineBudget::new();
        let first = budget
            .admit_capture(seg(expected, false, b"AB"))
            .unwrap()
            .into_payload();
        assert_eq!(flatten_half(half.push(expected, false, first)), b"AB");
        let second = budget
            .admit_capture(seg(expected.wrapping_add(2), false, b"CD"))
            .unwrap()
            .into_payload();
        assert_eq!(
            flatten_half(half.push(expected.wrapping_add(2), false, second)),
            b"CD"
        );
    }

    #[test]
    fn tracked_stream_count_is_bounded() {
        let mut r = Reassembler::new();
        // A forged-source-port flood on the game port must not grow the map.
        for port in 0..(MAX_STREAMS as u32 * 3) {
            r.push(&seg_on(flow_from(port as u16), 1000, b"AB"));
        }
        assert_eq!(r.streams.len(), MAX_STREAMS);
    }

    #[test]
    fn eviction_keeps_the_active_flow() {
        let mut r = Reassembler::new();
        let hot = flow_from(1);
        r.push(&seg_on(hot, 1000, b"AB"));
        for port in 100..(100 + MAX_STREAMS as u32 * 2) {
            r.push(&seg_on(flow_from(port as u16), 1000, b"XY"));
            r.push(&seg_on(hot, 1002, b"CD"));
        }
        assert_eq!(r.streams.len(), MAX_STREAMS);
        assert!(r.streams.contains_key(&hot));
    }

    /// The eviction path end to end: the game's flow goes quiet mid-message, as
    /// it does between shop refreshes, while foreign flows on the same port fill
    /// the table, so staleness picks the game's own stream. It re-anchors either
    /// way; the point is that the loss is *counted*, on the same `resyncs`
    /// number the pressure arm uses.
    ///
    /// Uses `push_budgeted` against one shared budget: the `push` helper mints a
    /// throwaway `PipelineBudget` per call and would read zero counters.
    #[test]
    fn evicting_a_crowded_out_stream_counts_the_lost_anchor() {
        let budget = PipelineBudget::new();
        let mut reassembler = Reassembler::new();
        let push = |reassembler: &mut Reassembler, segment: Segment| {
            flatten_chunks(reassembler.push_budgeted(budget.admit_capture(segment).unwrap()))
        };
        let game = flow();

        assert_eq!(push(&mut reassembler, seg_on(game, 1000, b"AB")), b"AB");
        // Mid-message: a segment past a gap, buffered and waiting.
        assert!(push(&mut reassembler, seg_on(game, 1004, b"EF")).is_empty());
        assert_eq!(budget.snapshot().resyncs, 0);

        // 63 newcomers fill the table; the 64th is the first key with nowhere
        // to go, and the game's stream is by then the stalest.
        for port in 1..=(MAX_STREAMS as u16) {
            push(&mut reassembler, seg_on(flow_from(port), 1000, b"XY"));
        }

        assert_eq!(reassembler.streams.len(), MAX_STREAMS);
        assert!(
            !reassembler.streams.contains_key(&game),
            "the quiet flow is the stalest one, so this test must be evicting it"
        );
        assert_eq!(
            budget.snapshot().resyncs,
            1,
            "an evicted stream lost its anchor and must be counted like every other anchor loss"
        );
        // Pinned so it stays declined: nothing was *refused* here — the segment
        // that triggered the eviction was admitted — and `push_budgeted` rules
        // that chunks a recovery throws away are collateral, not extra
        // captures. The discarded gap buffer is in the warning's bytes instead.
        assert_eq!(budget.snapshot().dropped_segments, 0);
        // The evicted stream's buffered segment gave its lease back on the way out.
        assert_eq!(budget.snapshot().current_reassembly, 0);

        assert_eq!(push(&mut reassembler, seg_on(game, 9000, b"ZZ")), b"ZZ");
        drop(reassembler);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    #[test]
    fn clear_resets_baseline_for_resync() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
        r.clear();
        assert_eq!(r.push(&seg(9000, false, b"XY")), b"XY");
    }

    #[test]
    fn a_new_syn_resets_the_reused_flow_and_leaves_every_other_one_alone() {
        let mut r = Reassembler::new();
        let reused = flow();
        let unrelated = flow_from(52000);

        assert!(r.push(&seg(999, true, b"")).is_empty());
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
        assert!(r.push(&seg(1004, false, b"EF")).is_empty()); // buffered, gap-bound.
        assert_eq!(r.push(&seg_on(unrelated, 3000, b"UV")), b"UV");

        assert!(r.push(&seg(8999, true, b"")).is_empty());

        let fresh = &r.streams[&reused];
        assert_eq!(fresh.baseline, Some(9000));
        assert_eq!(fresh.syn_seq, Some(8999));
        assert_eq!(fresh.next_off, 0);
        assert!(
            fresh.pending.is_empty(),
            "the previous incarnation's gap buffer must not survive the reset"
        );
        let untouched = &r.streams[&unrelated];
        assert_eq!(untouched.baseline, Some(3000));
        assert_eq!(untouched.next_off, 2);
    }

    #[test]
    fn same_syn_retransmission_preserves_pending_data() {
        let mut r = Reassembler::new();
        assert!(r.push(&seg(999, true, b"")).is_empty());
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
        assert!(r.push(&seg(1004, false, b"EF")).is_empty());

        assert!(r.push(&seg(999, true, b"")).is_empty());
        assert_eq!(r.push(&seg(1002, false, b"CD")), b"CDEF");
        assert!(r.push(&seg(1004, false, b"EF")).is_empty());
    }

    #[test]
    fn late_matching_syn_does_not_reset_midstream_anchor() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");

        assert!(r.push(&seg(999, true, b"")).is_empty());
        assert_eq!(r.push(&seg(1002, false, b"CD")), b"CD");

        let half = &r.streams[&flow()];
        assert_eq!(half.baseline, Some(1000));
        assert_eq!(half.syn_seq, Some(999));
        assert_eq!(half.next_off, 4);
    }

    #[test]
    fn data_bearing_syn_is_delivered_once() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(999, true, b"AB")), b"AB");
        assert!(r.push(&seg(999, true, b"AB")).is_empty());
    }

    #[test]
    fn new_syn_handles_wrapped_data_sequence() {
        let mut r = Reassembler::new();
        assert!(r.push(&seg(u32::MAX, true, b"")).is_empty());
        assert_eq!(r.streams[&flow()].baseline, Some(0));
        assert_eq!(r.push(&seg(0, false, b"AB")), b"AB");
    }
}
