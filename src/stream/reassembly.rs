//! Sequence-space reassembly: ordering, deduplication, gaps, SYN incarnations,
//! and the one-shot initial anchor burst.
//!
//! This is the half the parent module's doc comment describes — the relative
//! offset rule and why the distance is measured to the currently expected byte
//! rather than to the origin is stated there, once. Everything here treats a
//! payload as an opaque [`BudgetedChunk`] and asks [`super::budget`] whether it
//! may be held: this half decides *what* to buffer, that half decides *if there
//! is room*.

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

/// Cap on the number of tracked streams. One armed game connection needs one;
/// reconnections and — since capture is port-wide — any host sending from the
/// game port would otherwise mint keys without bound, each able to buffer up to
/// `MAX_PENDING_BYTES`. Well above legitimate need; the stalest entry is
/// evicted past it.
const MAX_STREAMS: usize = 64;

/// Segments held during the one-shot initial anchor window.
///
/// Ordering is isolated per flow. Replacing each flow's original slots with its
/// sequence-ordered segments preserves the observed inter-flow cadence while
/// letting [`Reassembler`] remain the sole authority for overlap,
/// deduplication, gaps, and SYN incarnations.
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
    /// Panics if the segment would exceed either burst cap. That is a caller
    /// contract, not a runtime condition: check [`Self::would_exceed`] first and
    /// flush the burst instead. The assert is deliberate — silently accepting
    /// the segment would let one post-resync burst grow past the 256 KiB /
    /// 128-segment bound the whole anchor decision is predicated on, and the
    /// caller that skipped the check is the bug.
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

    /// Whether the burst has reached either cap. `>=` on both terms: the
    /// segment count lands on its cap exactly, but a byte counter almost never
    /// hits 262 144 on the nose, so an equality test there would leave the byte
    /// bound resting entirely on `would_exceed` catching the next segment.
    pub(crate) fn is_at_limit(&self) -> bool {
        self.segments.len() >= INITIAL_ANCHOR_MAX_SEGMENTS
            || self.payload_bytes >= INITIAL_ANCHOR_MAX_BYTES
    }

    pub(crate) fn into_ordered(self) -> Vec<BudgetedSegment> {
        // `collect` over a slice iterator is already exact-size (TrustedLen):
        // one allocation, no growth. Only the map needs a hint — a nominal
        // burst is the single armed game connection.
        let slots: Vec<_> = self.segments.iter().map(|segment| segment.flow).collect();
        let mut flows: HashMap<_, Vec<BudgetedSegment>> = HashMap::with_capacity(1);
        for segment in self.segments {
            flows.entry(segment.flow).or_default().push(segment);
        }
        for segments in flows.values_mut() {
            // A valid TCP receive window spans less than the signed sequence
            // half-space; the byte cap bounds memory, not sequence gaps. Select
            // an origin first so wrap sorting has a transitive key under that
            // TCP invariant.
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
            // Sorted in place and reversed so the replay below can `pop` from the
            // back: a second, differently-typed map purely to gain `pop_front`
            // would re-hash every key for a container change.
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
    /// [`ReassemblyOutcome::Chunks`] may be empty: the segment was a duplicate,
    /// partially filled a gap, or still waits on a missing predecessor. FIN is
    /// not modelled — a stream is never torn down, so a segment reordered ahead
    /// of a gap (a FIN-flagged one included) keeps its buffered payload until the
    /// gap fills.
    ///
    /// [`ReassemblyOutcome::Pressure`] is *not* "nothing yet": the pending-byte
    /// quota was exhausted, **every** tracked flow's anchor and buffer have
    /// already been cleared, and this segment was dropped and counted. The caller
    /// must re-anchor (`AnchorState::AwaitingFirst`) rather than wait for a gap
    /// fill that can never arrive — waiting freezes the half-stream for good.
    pub(crate) fn push_budgeted(&mut self, segment: BudgetedSegment) -> ReassemblyOutcome {
        let key = segment.flow;
        let dropped_capacity = segment.capacity();
        let budget = segment.budget();
        self.clock += 1;
        if segment.syn && self.syn_starts_new_incarnation(&segment) {
            self.streams.remove(&key);
        }
        // A genuinely new flow past the cap evicts the stalest one first, so a
        // reconnect churn or a flood of forged source ports cannot grow the
        // map without bound. An existing flow never triggers eviction.
        if self.streams.len() >= MAX_STREAMS && !self.streams.contains_key(&key) {
            self.evict_stalest();
        }
        let clock = self.clock;
        let half = self.streams.entry(key).or_default();
        half.last_active = clock;
        let outcome = half.push(segment.seq, segment.syn, segment.into_payload());
        // Exhaustive by construction: a variant added to `HalfOutcome` becomes
        // a compile error here rather than a runtime panic that would kill the
        // reassembly task and the whole session.
        match outcome {
            HalfOutcome::Chunks(chunks) => ReassemblyOutcome::Chunks(chunks),
            HalfOutcome::Pressure => {
                // Never jump across a known gap. All anchors are invalid after
                // a shared pending-quota failure; the next segment starts
                // cleanly.
                self.clear();
                // Drop metrics identify the packet that caused recovery;
                // pending chunks discarded by the global clear are collateral
                // state, not additional captured packets.
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

    /// Returns whether this SYN starts a new incarnation of an already tracked
    /// connection, in which case the caller drops the stale sequence space.
    ///
    /// Only two of this connection's SYNs can ever reach here — the server's
    /// handshake SYN-ACK and its retransmissions — because the client's own SYN
    /// travels the direction that is never captured. So the question is purely
    /// "is this the same incarnation as the one already tracked": a SYN on a
    /// flow nothing has been seen on yet starts nothing, it simply anchors.
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

    /// Drops the least-recently-active stream. Called only when a new key
    /// would exceed `MAX_STREAMS`; the scan is over a small, capped map.
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

    /// Resets all state so the next segment of each flow re-anchors a new
    /// origin. Used after a Shop Watch pause to restart from a clean resync
    /// point rather than a stale `next_off`.
    pub fn clear(&mut self) {
        self.streams.clear();
    }
}

/// Reassembly state of the captured half of a connection — the server-to-client
/// one, the only half that reaches this layer — in relative offsets.
#[derive(Default)]
struct HalfStream {
    /// Last `Reassembler::clock` value at which this stream saw a segment;
    /// the eviction key.
    last_active: u64,
    /// Stream origin (sequence number of the first byte); `None` until first seen.
    baseline: Option<u32>,
    /// Initial SYN sequence number for this connection incarnation, if seen.
    syn_seq: Option<u32>,
    /// Offset (from `baseline`) of the next expected byte.
    next_off: i64,
    /// Buffered future segments, keyed by offset (monotonic order, no wrap).
    pending: BTreeMap<i64, BudgetedChunk>,
    pending_bytes: usize,
}

impl HalfStream {
    fn push(&mut self, seq: u32, syn: bool, payload: BudgetedChunk) -> HalfOutcome {
        // Recorded before the baseline below, so `syn_starts_new_incarnation`
        // can tell a retransmitted SYN from a fresh incarnation on the next
        // segment.
        if syn {
            self.syn_seq.get_or_insert(seq);
        }
        // SYN consumes a sequence number: data starts at seq + 1.
        let data_seq = if syn { seq.wrapping_add(1) } else { seq };
        self.baseline.get_or_insert(data_seq);
        // Measure from the currently expected byte, then shift back to an
        // absolute offset. The distance stays within the TCP window (small),
        // so the i32 span in `seq_diff` never overflows however far the stream
        // has advanced.
        let expected_seq = self.expected_seq();
        let offset = self.next_off + seq_diff(data_seq, expected_seq);

        // One exact slot rather than `Vec::new()`: the in-order case — the whole
        // point of the path — carries a single chunk, and for a 48-byte element
        // the first `push` on an empty `Vec` jumps straight to capacity 4, so it
        // was allocating 192 bytes per packet to hold 48. More than one chunk
        // only happens when `drain` flushes a filled gap, which grows from here.
        // The trade is stated plainly: the cases that deliver *nothing* (a
        // retransmission, a segment buffered behind a gap, a bare SYN) now
        // allocate one slot where `Vec::new()` allocated none. They are the rare
        // ones — `capture::ip` already drops empty non-SYN payloads upstream.
        //
        // A `SmallVec<[BudgetedChunk; 1]>` would remove even that one allocation,
        // and it was weighed and declined rather than deferred. It costs a
        // dependency, and it would inline 48 bytes into `HalfOutcome` and
        // `ReassemblyOutcome`, both of which are returned by value up two call
        // frames per packet — trading a malloc for a memcpy of the same order,
        // with no measurement either way. Nothing in this crate has been profiled
        // (`Cargo.toml`'s `[profile.release]` says so, at length, about `lto`), and
        // `mem-smallvec` itself asks for a profile first. The exact `Vec` keeps the
        // whole win that is provable from the numbers above.
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

        // offset <= next_off: the segment starts at or before the expected byte,
        // so the distance is non-negative and bounded by the sequence window.
        // Let the conversion carry that invariant instead of an `as` cast: a
        // negative difference would silently become ~1.8e19, make the length
        // test below false, and drop the segment — freezing this half-stream
        // for good with no panic, no log and no metric.
        let Ok(already) = usize::try_from(self.next_off - offset) else {
            report_absorb_invariant(self.next_off, offset);
            // Same observable behaviour as the retransmission case below:
            // deliver nothing, but do not claim pressure — a spurious `false`
            // here would clear every anchor of every flow.
            return true;
        };
        if already < payload.as_slice().len() {
            if already != 0 {
                payload.drain_front(already);
            }
            self.next_off += payload.as_slice().len() as i64;
            out.push(payload);
        }
        // else: fully delivered already (retransmission) — ignored.
        true
    }

    fn buffer_future(&mut self, offset: i64, mut payload: BudgetedChunk) -> bool {
        let capacity = payload.capacity();
        // One probe of the key instead of `get` + `remove` + `insert`. Beyond the
        // two saved `O(log n)` walks, the `entry` form is what makes the ordering
        // below safe: a displaced chunk is only uncounted *and* removed once the
        // new one has cleared the quota, so a rejection can never leave the map
        // and `pending_bytes` disagreeing. The old shape relied on `false`
        // propagating to `HalfOutcome::Pressure`, which wipes every stream anyway.
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
    /// in the wrapping `u32` space. `baseline` is always set by the time this
    /// runs (`push` inserts it first).
    fn expected_seq(&self) -> u32 {
        // `next_off` is non-negative and the sequence space is mod 2^32:
        // keeping the low 32 bits of the offset IS the intended conversion, the
        // same modular arithmetic the explicit `wrapping_*` calls do elsewhere.
        // The detour through `u64` spells out that the truncation happens in an
        // unsigned space, so no sign extension is involved.
        let offset = (self.next_off as u64) as u32;
        self.baseline.unwrap_or(0).wrapping_add(offset)
    }
}

/// The rare branch of [`HalfStream::absorb`]'s offset invariant, kept out of a
/// body that runs once per captured segment.
#[cold]
#[inline(never)]
fn report_absorb_invariant(next_off: i64, offset: i64) {
    error!(next_off, offset, "reassembly invariant violated");
    debug_assert!(offset <= next_off, "absorb offset exceeds next_off");
}

/// The rare branch of [`Reassembler::push_budgeted`]'s pressure arm: taking the
/// budget mutex for a snapshot and building seven fields belongs off the
/// per-packet path.
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
    /// The bytes that became contiguous, in order. Empty is normal: a duplicate,
    /// a partial gap fill, or a segment still waiting on a predecessor.
    Chunks(Vec<BudgetedChunk>),
    /// The pending-byte quota was exhausted: every flow's state has been cleared
    /// and the caller must re-anchor. Not a "nothing yet".
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

    /// A plain data segment on a given flow (no SYN): for multi-flow tests.
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

    /// Sorting is per flow, but the *slots* are global: a burst interleaving
    /// two connections must come back with each connection ordered and the
    /// observed alternation between them untouched.
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
    /// This is the generalization `20-test.md`'s `test-007` asked `proptest` for,
    /// and it is done without the dependency because the space is *small enough to
    /// exhaust*: 6! = 720 orders × 4 origins = 2 880 cases, in a few
    /// milliseconds. Exhaustion is strictly stronger than sampling — there is no
    /// order left for a random generator to have missed, and no
    /// `proptest-regressions/` file to commit for a suite that cannot find a case
    /// the next run would not also find. The two properties are the documented
    /// rule from `docs/initial-stream-anchor.md`, which the hand-written
    /// six-permutation table above proves for n = 3 only.
    #[test]
    fn every_arrival_order_yields_the_immediate_suffix_of_the_stream() {
        // Origins: an ordinary one, the last sequence before the wrap, one
        // straddling it exactly, and zero.
        for origin in [1_000_u32, u32::MAX - 5, u32::MAX - 11, 0] {
            let payloads: [&[u8]; 6] = [b"AB", b"CD", b"EF", b"GH", b"IJ", b"KL"];
            let whole: Vec<u8> = payloads.concat();
            let segments: Vec<Segment> = payloads
                .iter()
                .enumerate()
                .map(|(index, bytes)| {
                    // `wrapping_add`: the point of the near-`u32::MAX` origins is
                    // that the sequence space wraps under the segments.
                    seg(origin.wrapping_add(index as u32 * 2), false, bytes)
                })
                .collect();

            for order in permutations(payloads.len()) {
                let mut reassembler = Reassembler::new();
                let mut delivered = Vec::new();
                for index in order.iter().copied() {
                    delivered.extend(reassembler.push(&segments[index]));
                }
                // 1. Whatever the order, what comes out is a *suffix* of the
                //    original byte stream — never a permutation of it, never a
                //    gap in the middle. That is the guarantee the analysis server
                //    decodes against: it can resync from any point, but it cannot
                //    survive reordered or hole-punched bytes.
                assert!(
                    whole.ends_with(&delivered),
                    "origin {origin}, order {order:?} delivered {delivered:?}, not a suffix"
                );
                // 2. And the suffix starts exactly at the first segment to
                //    arrive: that arrival is what anchors the stream, and
                //    everything before it is already history.
                let anchor = order[0];
                assert_eq!(
                    delivered.len(),
                    whole.len() - anchor * 2,
                    "origin {origin}, order {order:?} did not anchor on segment {anchor}"
                );
            }
        }
    }

    /// The three algebraic properties `seq_diff` is defined by, over a lattice of
    /// bases that includes every wrap boundary.
    ///
    /// `test-007`'s third named target. Dependency-free for the same reason as the
    /// permutation sweep: the interesting inputs are *exactly* the boundaries —
    /// `0`, `u32::MAX`, `i32::MAX` (where the signed reading flips), and their
    /// neighbours — which a lattice names and a uniform generator reaches with
    /// probability ~0. `seq_diff` is `const fn`, so this could in principle be a
    /// `const _: () = assert!(…)`; it is a test instead because the loop covers
    /// 1 000+ pairs and a const block would have to spell each one out.
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
            // 1. Reflexive: a sequence number is zero from itself, at every base
            //    including both sides of the wrap.
            assert_eq!(seq_diff(base, base), 0, "base {base}");
            for delta in deltas {
                let other = base.wrapping_add(delta as u32);
                // 2. It reads the *relative* distance, so a wrap between the two
                //    is invisible. This is the property `HalfStream::push` relies
                //    on: the offset is derived from the distance to the currently
                //    expected byte, never to the fixed origin, so the signed
                //    window tracks a stream that has advanced past 2 GiB.
                assert_eq!(seq_diff(other, base), delta, "base {base}, delta {delta}");
                // 3. Antisymmetric. `i32::MIN` is the one value that cannot be
                //    negated, and no `delta` here reaches it — deliberately: at
                //    exactly half the space apart, "ahead" and "behind" are the
                //    same answer, which is a property of the circle and not of
                //    this function.
                assert_eq!(
                    seq_diff(base, other),
                    -delta,
                    "base {base}, delta {delta} is not antisymmetric"
                );
            }
        }
        // The half-space edge, stated rather than left implicit: 2^31 apart reads
        // as `i32::MIN` in both directions, which is why `MAX_PENDING_BYTES` and
        // the anchor logic bound how far out of order a segment may be.
        assert_eq!(seq_diff(0, 0x8000_0000), i64::from(i32::MIN));
        assert_eq!(seq_diff(0x8000_0000, 0), i64::from(i32::MIN));
    }

    /// Every permutation of `0..n`, in lexicographic order. Ten lines, no
    /// dependency, and deterministic — the alternative was a random generator
    /// that would sample a space this test exhausts.
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

    /// Each flow anchors on its own first segment: a mid-stream start on one
    /// connection must neither hold back nor re-anchor another. The `1000`
    /// segment arriving after `1002` on `first` is already-delivered history for
    /// *that* flow only, while the identical sequence on `second` is its origin.
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
        // Baseline is set by the first observed segment.
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
        // Two future segments arrive out of order: nothing deliverable yet.
        assert!(r.push(&seg(1006, false, b"GH")).is_empty());
        assert!(r.push(&seg(1004, false, b"EF")).is_empty());
        // Filling the gap flushes everything buffered, in order.
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
        // Overlaps "CD" (already seen) and brings "EF".
        assert_eq!(r.push(&seg(1002, false, b"CDEF")), b"EF");
    }

    #[test]
    fn syn_sets_the_baseline() {
        let mut r = Reassembler::new();
        // The SYN (seq 999, no data) anchors the origin at 1000.
        assert!(r.push(&seg(999, true, b"")).is_empty());
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
    }

    #[test]
    fn gap_filled_out_of_order() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
        assert!(r.push(&seg(1004, false, b"EF")).is_empty()); // gap.
        assert_eq!(r.push(&seg(1002, false, b"CD")), b"CDEF");
    }

    #[test]
    fn reassembles_across_sequence_wrap() {
        let mut r = Reassembler::new();
        // Baseline just before the u32 sequence space wraps.
        assert_eq!(r.push(&seg(0xFFFF_FFFE, false, b"AB")), b"AB");
        // The next segment is at 0x0000_0000 (wrap): still contiguous.
        assert_eq!(r.push(&seg(0x0000_0000, false, b"CD")), b"CD");
    }

    #[test]
    fn reordering_across_wrap_is_ordered_correctly() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(0xFFFF_FFFE, false, b"AB")), b"AB");
        // A post-wrap future segment is buffered, then the gap is filled.
        assert!(r.push(&seg(0x0000_0002, false, b"EF")).is_empty());
        assert_eq!(r.push(&seg(0x0000_0000, false, b"CD")), b"CDEF");
    }

    #[test]
    fn delivers_far_past_two_gigabytes() {
        // A half-stream that has already delivered 2^31 bytes: the next
        // in-order segment must still be recognised, not dropped as a phantom
        // retransmission (the old origin-anchored offset overflowed i32 here).
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
        // And the following contiguous segment keeps flowing.
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
        // Far more distinct flows than the cap (e.g. a forged-source-port
        // flood on the game port): the map must not grow without bound.
        for port in 0..(MAX_STREAMS as u32 * 3) {
            r.push(&seg_on(flow_from(port as u16), 1000, b"AB"));
        }
        assert_eq!(r.streams.len(), MAX_STREAMS);
    }

    #[test]
    fn eviction_keeps_the_active_flow() {
        let mut r = Reassembler::new();
        let hot = flow_from(1);
        // Fill to the cap, keeping `hot` continuously active as newcomers
        // arrive, so it is never the stalest and survives eviction.
        r.push(&seg_on(hot, 1000, b"AB"));
        for port in 100..(100 + MAX_STREAMS as u32 * 2) {
            r.push(&seg_on(flow_from(port as u16), 1000, b"XY"));
            r.push(&seg_on(hot, 1002, b"CD")); // keep hot fresh
        }
        assert_eq!(r.streams.len(), MAX_STREAMS);
        assert!(r.streams.contains_key(&hot));
    }

    #[test]
    fn clear_resets_baseline_for_resync() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
        // After a pause the state is wiped: a far-ahead segment becomes a new
        // origin instead of being buffered forever.
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
        // Buffered behind a gap: bytes of the incarnation about to be replaced.
        assert!(r.push(&seg(1004, false, b"EF")).is_empty());
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
