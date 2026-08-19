//! Sequence-space reassembly: ordering, deduplication, gaps, SYN incarnations,
//! and the one-shot initial anchor burst. The relative-offset rule is
//! documented on the parent module. This half treats a payload as an opaque
//! [`BudgetedChunk`] and decides *what* to buffer; [`super::budget`] decides
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
/// sequence-ordered segments, preserving inter-flow cadence while leaving
/// [`Reassembler`] the sole authority for overlap, dedup, gaps, and SYN
/// incarnations.
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
    /// Panics if the segment would exceed either cap — check
    /// [`Self::would_exceed`] first. Guards the 256 KiB / 128-segment bound.
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

    /// Whether the burst has reached either cap. `>=` on both terms: segment
    /// count lands on its cap exactly, but bytes rarely hit 262 144 exactly,
    /// so equality would leave the bound resting on `would_exceed` alone.
    pub(crate) fn is_at_limit(&self) -> bool {
        self.segments.len() >= INITIAL_ANCHOR_MAX_SEGMENTS
            || self.payload_bytes >= INITIAL_ANCHOR_MAX_BYTES
    }

    pub(crate) fn into_ordered(self) -> Vec<BudgetedSegment> {
        // Slice-iterator `collect` is exact-size (TrustedLen): one allocation; the map just needs a capacity hint.
        let slots: Vec<_> = self.segments.iter().map(|segment| segment.flow).collect();
        let mut flows: HashMap<_, Vec<BudgetedSegment>> = HashMap::with_capacity(1);
        for segment in self.segments {
            flows.entry(segment.flow).or_default().push(segment);
        }
        for segments in flows.values_mut() {
            // A valid TCP window is smaller than the signed half-space — select an origin first for a transitive sort key.
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
            // Reversed so the replay below can `pop` from the back, instead of a second map just for `pop_front`.
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

    /// Integrates a segment, returning the bytes that became contiguous. See
    /// [`ReassemblyOutcome`] for what `Chunks` vs `Pressure` means.
    ///
    /// FIN is not modelled: a segment reordered ahead of a gap keeps its
    /// buffered payload until the gap fills. On `Pressure`, the caller must
    /// re-anchor (`AnchorState::AwaitingFirst`) instead of waiting on a gap
    /// fill that can never arrive.
    pub(crate) fn push_budgeted(&mut self, segment: BudgetedSegment) -> ReassemblyOutcome {
        let key = segment.flow;
        let dropped_capacity = segment.capacity();
        let budget = segment.budget();
        self.clock += 1;
        if segment.syn && self.syn_starts_new_incarnation(&segment) {
            self.streams.remove(&key);
        }
        // A new flow past the cap evicts the stalest one first, so reconnect
        // churn or a forged-source-port flood cannot grow the map unbounded.
        if self.streams.len() >= MAX_STREAMS && !self.streams.contains_key(&key) {
            self.evict_stalest();
        }
        let clock = self.clock;
        let half = self.streams.entry(key).or_default();
        half.last_active = clock;
        let outcome = half.push(segment.seq, segment.syn, segment.into_payload());
        // Exhaustive by construction: a new `HalfOutcome` variant becomes a
        // compile error here, not a runtime panic that kills the session.
        match outcome {
            HalfOutcome::Chunks(chunks) => ReassemblyOutcome::Chunks(chunks),
            HalfOutcome::Pressure => {
                // All anchors are invalid after a shared pending-quota
                // failure; clearing lets the next segment start cleanly.
                self.clear();
                // Drop metrics identify only the packet that caused recovery;
                // chunks discarded by the clear are collateral, not extra captures.
                budget.record_drop(dropped_capacity);
                budget.record_resync();
                warn_reassembly_pressure(&budget);
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

    /// Returns whether this SYN starts a new incarnation of an already
    /// tracked connection (the caller then drops the stale sequence space).
    ///
    /// Only two SYNs of a connection reach here — the handshake SYN-ACK and
    /// its retransmissions, since the client's own SYN travels the uncaptured
    /// direction. A SYN on an untracked flow simply anchors.
    fn syn_starts_new_incarnation(&self, segment: &BudgetedSegment) -> bool {
        debug_assert!(segment.syn);

        let Some(half) = self.streams.get(&segment.flow) else {
            return false;
        };
        // A retransmitted SYN carries the sequence number already anchored on.
        if half.syn_seq == Some(segment.seq) {
            return false;
        }
        // The handshake SYN arriving late, after data already anchored this
        // stream mid-flight at exactly the byte that SYN would have produced.
        if half.syn_seq.is_none() && half.baseline == Some(segment.seq.wrapping_add(1)) {
            return false;
        }
        true
    }

    /// Drops the least-recently-active stream; called only when a new key
    /// would exceed `MAX_STREAMS`.
    fn evict_stalest(&mut self) {
        if let Some(&key) = self
            .streams
            .iter()
            .min_by_key(|(_, half)| half.last_active)
            .map(|(key, _)| key)
        {
            self.streams.remove(&key);
        }
    }

    /// Resets all state so each flow re-anchors on its next segment. Used
    /// after a Shop Watch pause to avoid resyncing from a stale `next_off`.
    pub fn clear(&mut self) {
        self.streams.clear();
    }
}

/// Reassembly state of the captured (server-to-client) half of a connection, in relative offsets.
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
        // Recorded before `baseline` so `syn_starts_new_incarnation` can tell retransmission from a fresh incarnation.
        if syn {
            self.syn_seq.get_or_insert(seq);
        }
        // SYN consumes a sequence number: data starts at seq + 1.
        let data_seq = if syn { seq.wrapping_add(1) } else { seq };
        self.baseline.get_or_insert(data_seq);
        // Offset is measured from the currently expected byte, then shifted
        // back to absolute. The distance stays within the TCP window, so the
        // i32 span in `seq_diff` never overflows.
        let expected_seq = self.expected_seq();
        let offset = self.next_off + seq_diff(data_seq, expected_seq);

        // `Vec::with_capacity(1)`, not `Vec::new()`, whose first `push` on a
        // 48-byte element jumps to capacity 4 (192 bytes to hold 48). The
        // common in-order case needs one chunk; only `drain` flushing a gap
        // needs more. Nothing-delivering cases (retransmission, gap-buffered,
        // bare SYN) now allocate one slot instead of none, but are rare —
        // `capture::ip` drops empty non-SYN payloads upstream.
        //
        // `SmallVec<[BudgetedChunk; 1]>` was declined: it costs a dependency
        // and inlines 48 bytes into `HalfOutcome`/`ReassemblyOutcome`, both
        // returned by value twice per packet — an unmeasured malloc-for-memcpy
        // trade. Nothing here is profiled, and `mem-smallvec` asks for one first.
        let mut out = Vec::with_capacity(1);
        if !self.absorb(offset, payload, &mut out) {
            return HalfOutcome::Pressure;
        }
        if !self.drain(&mut out) {
            return HalfOutcome::Pressure;
        }
        HalfOutcome::Chunks(out)
    }

    /// Integrates one segment: in order (append), future (buffer), or old (trim).
    fn absorb(
        &mut self,
        offset: i64,
        mut payload: BudgetedChunk,
        out: &mut Vec<BudgetedChunk>,
    ) -> bool {
        if payload.as_slice().is_empty() {
            return true;
        }
        if offset > self.next_off {
            return self.buffer_future(offset, payload);
        }

        // offset <= next_off: the distance is non-negative, bounded by the
        // sequence window. `try_from`, not `as`: a negative difference would
        // silently become ~1.8e19, fail the length check, and drop the
        // segment — freezing this half-stream forever, silently.
        let Ok(already) = usize::try_from(self.next_off - offset) else {
            report_absorb_invariant(self.next_off, offset);
            // Deliver nothing, but don't report pressure — a spurious `false` here would clear every anchor of every flow.
            return true;
        };
        if already < payload.as_slice().len() {
            if already != 0 {
                payload.drain_front(already);
            }
            self.next_off += payload.as_slice().len() as i64;
            out.push(payload);
        }
        true
    }

    fn buffer_future(&mut self, offset: i64, mut payload: BudgetedChunk) -> bool {
        let capacity = payload.capacity();
        // One `entry` probe, not `get`+`remove`+`insert`, saves two `O(log n)`
        // walks and displaces a chunk only once the new one clears the quota.
        match self.pending.entry(offset) {
            Entry::Occupied(mut slot) => {
                // Keep only the largest segment seen at a given offset.
                if slot.get().as_slice().len() >= payload.as_slice().len() {
                    return true;
                }
                let held = pending_after_release(self.pending_bytes, slot.get().capacity());
                let Some(total) = fits_pending(held, capacity) else {
                    return false;
                };
                if !payload.try_retag_pending() {
                    return false;
                }
                self.pending_bytes = total;
                // Returns the displaced chunk, whose lease releases as it drops.
                drop(slot.insert(payload));
            }
            Entry::Vacant(slot) => {
                let Some(total) = fits_pending(self.pending_bytes, capacity) else {
                    return false;
                };
                if !payload.try_retag_pending() {
                    return false;
                }
                self.pending_bytes = total;
                slot.insert(payload);
            }
        }
        true
    }

    /// Flushes buffered segments that became contiguous once `next_off` advanced.
    fn drain(&mut self, out: &mut Vec<BudgetedChunk>) -> bool {
        while let Some((&offset, _)) = self.pending.first_key_value() {
            if offset > self.next_off {
                break; // gap still present.
            }
            let (offset, payload) = self.pending.pop_first().expect("peeked above");
            self.pending_bytes = pending_after_release(self.pending_bytes, payload.capacity());
            if !self.absorb(offset, payload, out) {
                return false;
            }
        }
        true
    }

    /// Sequence number of the next expected byte: `baseline + next_off`, back
    /// in the wrapping `u32` space (`baseline` is always set by the time this
    /// runs — `push` inserts it first).
    fn expected_seq(&self) -> u32 {
        // `next_off` is non-negative, mod 2^32: keeping the low 32 bits is the
        // intended conversion, same as `wrapping_*` elsewhere. `u64` detour keeps truncation unsigned, avoiding sign extension.
        let offset = (self.next_off as u64) as u32;
        self.baseline.unwrap_or(0).wrapping_add(offset)
    }
}

/// The rare branch of [`HalfStream::absorb`]'s offset invariant, kept off the hot path.
#[cold]
#[inline(never)]
fn report_absorb_invariant(next_off: i64, offset: i64) {
    error!(next_off, offset, "reassembly invariant violated");
    debug_assert!(offset <= next_off, "absorb offset exceeds next_off");
}

/// The rare branch of [`Reassembler::push_budgeted`]'s pressure arm: the budget mutex and seven fields belong off the per-packet path.
#[cold]
#[inline(never)]
fn warn_reassembly_pressure(budget: &PipelineBudget) {
    let stats = budget.snapshot();
    warn!(
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

enum HalfOutcome {
    Chunks(Vec<BudgetedChunk>),
    Pressure,
}

/// What [`Reassembler::push_budgeted`] did with a segment.
pub(crate) enum ReassemblyOutcome {
    /// The bytes that became contiguous, in order. Empty is normal: a
    /// duplicate, a partial gap fill, or a segment still waiting on a predecessor.
    Chunks(Vec<BudgetedChunk>),
    /// The pending-byte quota was exhausted: every flow's state has been
    /// cleared and the caller must re-anchor. Not a "nothing yet".
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
    use crate::stream::{flow, flow_from, sized_seg, test_budget};

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
            HalfOutcome::Pressure => Vec::new(),
        }
    }

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

    /// Sorting is per flow, but the *slots* are global: interleaved connections must come back each ordered, alternation intact.
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
    /// origins — including two that straddle the `u32` wrap.
    ///
    /// Exhaustive (6! × 4 origins = 2 880 cases), not `proptest`-sampled: the
    /// space is small enough to exhaust in milliseconds. Proves the two
    /// properties from `docs/initial-stream-anchor.md` that the
    /// six-permutation table above only covers for n = 3.
    #[test]
    fn every_arrival_order_yields_the_immediate_suffix_of_the_stream() {
        for origin in [1_000_u32, u32::MAX - 5, u32::MAX - 11, 0] {
            let payloads: [&[u8]; 6] = [b"AB", b"CD", b"EF", b"GH", b"IJ", b"KL"];
            let whole: Vec<u8> = payloads.concat();
            let segments: Vec<Segment> = payloads
                .iter()
                .enumerate()
                .map(|(index, bytes)| {
                    // `wrapping_add`: near-`u32::MAX` origins wrap the sequence space under the segments.
                    seg(origin.wrapping_add(index as u32 * 2), false, bytes)
                })
                .collect();

            for order in permutations(payloads.len()) {
                let mut reassembler = Reassembler::new();
                let mut delivered = Vec::new();
                for index in order.iter().copied() {
                    delivered.extend(reassembler.push(&segments[index]));
                }
                // 1. What comes out is a *suffix* of the byte stream — never a
                //    permutation, never a gap. The analysis server decodes
                //    against this: it can resync from any point but not
                //    survive reordered or hole-punched bytes.
                assert!(
                    whole.ends_with(&delivered),
                    "origin {origin}, order {order:?} delivered {delivered:?}, not a suffix"
                );
                // 2. The suffix starts at the first segment to arrive: that
                //    arrival anchors the stream.
                let anchor = order[0];
                assert_eq!(
                    delivered.len(),
                    whole.len() - anchor * 2,
                    "origin {origin}, order {order:?} did not anchor on segment {anchor}"
                );
            }
        }
    }

    /// The three algebraic properties `seq_diff` is defined by, over a
    /// lattice of bases including every wrap boundary.
    ///
    /// Dependency-free like the permutation sweep: interesting inputs are
    /// exactly the boundaries — `0`, `u32::MAX`, `i32::MAX` (where signed
    /// reading flips) — which a uniform generator reaches with probability
    /// ~0. `seq_diff` is `const fn`, so this could be a `const _: () =
    /// assert!(…)`; it's a test instead because the loop covers 1 000+ pairs.
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
                // 2. Relative distance: a wrap is invisible — `HalfStream::push`
                //    tracks a stream past 2 GiB by measuring from the expected byte.
                assert_eq!(seq_diff(other, base), delta, "base {base}, delta {delta}");
                // 3. Antisymmetric. No `delta` reaches `i32::MIN` (it can't
                //    be negated): at exactly half the space apart, "ahead"
                //    and "behind" are the same answer, a circle property.
                assert_eq!(
                    seq_diff(base, other),
                    -delta,
                    "base {base}, delta {delta} is not antisymmetric"
                );
            }
        }
        // 2^31 apart reads as `i32::MIN` both ways — why `MAX_PENDING_BYTES`
        // and the anchor logic bound how far out of order a segment may be.
        assert_eq!(seq_diff(0, 0x8000_0000), i64::from(i32::MIN));
        assert_eq!(seq_diff(0x8000_0000, 0), i64::from(i32::MIN));
    }

    /// Every permutation of `0..n`, lexicographic, dependency-free — the
    /// alternative was a random generator sampling a space this exhausts.
    fn permutations(n: usize) -> Vec<Vec<usize>> {
        if n == 0 {
            return vec![Vec::new()];
        }
        let mut out = Vec::new();
        for head in 0..n {
            for mut rest in permutations(n - 1) {
                // Shift the tail indices that sit at or above `head` up by one,
                // so `rest` becomes a permutation of `0..n` minus `head`.
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

    /// Each flow anchors on its own first segment: the `1000` segment after
    /// `1002` on `first` is history for that flow, while the same seq on
    /// `second` is its origin.
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
        assert_eq!(r.push(&seg(0x0000_0000, false, b"CD")), b"CD"); // wraps, still contiguous.
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
        // A half-stream that already delivered 2^31 bytes: the next in-order
        // segment must still be recognised — the old offset overflowed i32.
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
        // `hot` stays active as newcomers fill the cap, so it survives eviction.
        r.push(&seg_on(hot, 1000, b"AB"));
        for port in 100..(100 + MAX_STREAMS as u32 * 2) {
            r.push(&seg_on(flow_from(port as u16), 1000, b"XY"));
            r.push(&seg_on(hot, 1002, b"CD"));
        }
        assert_eq!(r.streams.len(), MAX_STREAMS);
        assert!(r.streams.contains_key(&hot));
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
