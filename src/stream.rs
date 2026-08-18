//! TCP reassembly.
//!
//! Capture observes traffic below TCP, so segments may arrive out of order,
//! duplicated (retransmissions), or overlapping. This layer reconstructs, per
//! connection, the ordered byte stream the TCP stack would deliver — which is
//! what the analysis server expects.
//!
//! Only the server-to-client half of a connection is ever captured, so "the
//! stream of a flow" is unambiguous: there is no second half to disambiguate
//! against, and a `FlowKey` is the whole reassembly identity.
//!
//! All work is done in *relative offsets* from the stream origin (the first
//! observed segment). TCP sequence numbers are `u32` and wrap; a segment's
//! offset is derived from its distance to the *currently expected* byte, not
//! to the fixed origin, so the signed `i32` sequence window tracks the stream
//! as it advances. Anchoring the distance to the origin instead would break
//! once a half-stream delivered 2 GiB: the distance would exceed `i32` range
//! and every later segment would look like an already-delivered retransmission.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use tracing::{error, warn};

use crate::capture::{FlowKey, Segment};

/// Cap on out-of-order bytes buffered per tracked stream (memory guard).
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;

pub(crate) const PIPELINE_GLOBAL_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const CAPTURE_STAGE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const REASSEMBLY_STAGE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const OUTBOUND_STAGE_BYTES: usize = 8 * 1024 * 1024;

// These four numbers are the only defence against unbounded memory on a capture
// path that runs for hours, and they are what a later tuning pass edits by hand.
// On the production path their relation is pure arithmetic over constants, so it
// is checked here rather than on the player's machine — `with_limits` keeps the
// runtime asserts because `with_test_limits` passes arbitrary values. The two
// caps declared further down are part of the same relation and are named here
// deliberately; item order is irrelevant inside a `const` block.
const _: () = {
    assert!(CAPTURE_STAGE_BYTES <= PIPELINE_GLOBAL_BYTES);
    assert!(REASSEMBLY_STAGE_BYTES <= PIPELINE_GLOBAL_BYTES);
    assert!(OUTBOUND_STAGE_BYTES <= PIPELINE_GLOBAL_BYTES);
    // The per-stream pending cap must fit the global reassembly quota, or it is
    // dead code: the stage limit trips first, every time.
    assert!(MAX_PENDING_BYTES <= REASSEMBLY_STAGE_BYTES);
    // A burst is held in the capture stage while it buffers, so a burst cap
    // above that quota could never fill.
    assert!(INITIAL_ANCHOR_MAX_BYTES <= CAPTURE_STAGE_BYTES);
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Capture,
    Reassembly,
    Outbound,
}

#[derive(Clone, Copy)]
struct BudgetLimits {
    global: usize,
    capture: usize,
    reassembly: usize,
    outbound: usize,
}

#[derive(Default)]
struct Usage {
    total: usize,
    capture: usize,
    reassembly: usize,
    outbound: usize,
    high_water: usize,
}

struct BudgetInner {
    limits: BudgetLimits,
    usage: Mutex<Usage>,
    released: Notify,
    dropped_segments: AtomicU64,
    dropped_bytes: AtomicU64,
    resyncs: AtomicU64,
}

/// Shared accounting for every owned payload after packet parsing.
#[derive(Clone)]
pub(crate) struct PipelineBudget(Arc<BudgetInner>);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PipelineStats {
    pub current_total: usize,
    pub current_capture: usize,
    pub current_reassembly: usize,
    pub current_outbound: usize,
    pub high_water_total: usize,
    pub dropped_segments: u64,
    pub dropped_bytes: u64,
    pub resyncs: u64,
}

impl PipelineBudget {
    pub(crate) fn new() -> Self {
        Self::with_limits(BudgetLimits {
            global: PIPELINE_GLOBAL_BYTES,
            capture: CAPTURE_STAGE_BYTES,
            reassembly: REASSEMBLY_STAGE_BYTES,
            outbound: OUTBOUND_STAGE_BYTES,
        })
    }

    /// Builds a budget from explicit per-stage quotas.
    ///
    /// # Panics
    ///
    /// Panics if any stage quota exceeds the global one: a stage that can never
    /// fill is a silent hole in the memory guard, not a conservative setting.
    /// The production constants are proved at compile time above; this catches
    /// the `with_test_limits` path, which passes arbitrary values.
    fn with_limits(limits: BudgetLimits) -> Self {
        assert!(limits.capture <= limits.global);
        assert!(limits.reassembly <= limits.global);
        assert!(limits.outbound <= limits.global);
        Self(Arc::new(BudgetInner {
            limits,
            usage: Mutex::new(Usage::default()),
            released: Notify::new(),
            dropped_segments: AtomicU64::new(0),
            dropped_bytes: AtomicU64::new(0),
            resyncs: AtomicU64::new(0),
        }))
    }

    #[cfg(test)]
    pub(crate) fn with_test_limits(
        global: usize,
        capture: usize,
        reassembly: usize,
        outbound: usize,
    ) -> Self {
        Self::with_limits(BudgetLimits {
            global,
            capture,
            reassembly,
            outbound,
        })
    }

    /// Reserves capture-stage bytes for `segment`'s payload and takes ownership
    /// of it, pairing the buffer with the lease that will give the bytes back.
    ///
    /// # Errors
    ///
    /// The `Err` payload is not a description of a failure: it is `segment`
    /// itself, handed back unmodified, because the global or capture-stage quota
    /// is full. The caller owns it again and decides what happens next — drop it
    /// and record the drop, or retry once a lease elsewhere releases.
    pub(crate) fn admit_capture(&self, segment: Segment) -> Result<BudgetedSegment, Segment> {
        let bytes = segment.payload.capacity();
        if !self.reserve_new(Stage::Capture, bytes) {
            return Err(segment);
        }
        Ok(BudgetedSegment {
            flow: segment.flow,
            seq: segment.seq,
            syn: segment.syn,
            payload: BudgetedChunk {
                bytes: segment.payload,
                lease: PayloadLease {
                    budget: self.clone(),
                    bytes,
                    stage: Stage::Capture,
                },
            },
        })
    }

    fn reserve_new(&self, stage: Stage, bytes: usize) -> bool {
        let mut usage = self.0.usage.lock().unwrap_or_else(|err| err.into_inner());
        let Some(total) = usage.total.checked_add(bytes) else {
            return false;
        };
        let current = stage_bytes(&usage, stage);
        let Some(stage_total) = current.checked_add(bytes) else {
            return false;
        };
        if total > self.0.limits.global || stage_total > self.stage_limit(stage) {
            return false;
        }
        usage.total = total;
        *stage_bytes_mut(&mut usage, stage) = stage_total;
        usage.high_water = usage.high_water.max(total);
        true
    }

    fn try_retag(&self, from: Stage, to: Stage, bytes: usize) -> bool {
        if from == to {
            return true;
        }
        let mut usage = self.0.usage.lock().unwrap_or_else(|err| err.into_inner());
        let Some(target) = stage_bytes(&usage, to).checked_add(bytes) else {
            return false;
        };
        if target > self.stage_limit(to) {
            return false;
        }
        let source = stage_bytes(&usage, from);
        assert!(source >= bytes, "pipeline stage accounting underflow");
        *stage_bytes_mut(&mut usage, from) = source - bytes;
        *stage_bytes_mut(&mut usage, to) = target;
        true
    }

    /// Gives a lease's bytes back to the pool.
    ///
    /// Runs from [`PayloadLease::drop`], which the workers execute *while
    /// unwinding* whenever a `catch_unwind` boundary tears down live
    /// `BudgetedChunk`/`BudgetedSegment` values. A panic raised from a `Drop`
    /// during an unwind aborts the process immediately — no banner, and no
    /// `crash.log`, which is exactly the failure mode `crash.rs` exists to
    /// prevent. So an accounting bug saturates and is reported here instead of
    /// asserting; `debug_assert!` keeps the fail-fast in debug and test builds,
    /// where the abort costs a developer a stack trace, not a player a session.
    fn release(&self, stage: Stage, bytes: usize) {
        let mut usage = self.0.usage.lock().unwrap_or_else(|err| err.into_inner());
        let current = stage_bytes(&usage, stage);
        if usage.total < bytes || current < bytes {
            report_release_underflow(stage, bytes, usage.total, current);
        }
        usage.total = usage.total.saturating_sub(bytes);
        *stage_bytes_mut(&mut usage, stage) = current.saturating_sub(bytes);
        drop(usage);
        self.0.released.notify_waiters();
    }

    fn stage_limit(&self, stage: Stage) -> usize {
        match stage {
            Stage::Capture => self.0.limits.capture,
            Stage::Reassembly => self.0.limits.reassembly,
            Stage::Outbound => self.0.limits.outbound,
        }
    }

    // The discarded `fetch_update` results below are not swallowed errors: the
    // closures always return `Some`, so the call can only report `Ok`.
    pub(crate) fn record_drop(&self, bytes: usize) {
        let _ =
            self.0
                .dropped_segments
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    Some(current.saturating_add(1))
                });
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        let _ =
            self.0
                .dropped_bytes
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    Some(current.saturating_add(bytes))
                });
    }

    pub(crate) fn record_resync(&self) {
        let _ = self
            .0
            .resyncs
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_add(1))
            });
    }

    pub(crate) fn snapshot(&self) -> PipelineStats {
        let usage = self.0.usage.lock().unwrap_or_else(|err| err.into_inner());
        PipelineStats {
            current_total: usage.total,
            current_capture: usage.capture,
            current_reassembly: usage.reassembly,
            current_outbound: usage.outbound,
            high_water_total: usage.high_water,
            dropped_segments: self.0.dropped_segments.load(Ordering::Relaxed),
            dropped_bytes: self.0.dropped_bytes.load(Ordering::Relaxed),
            resyncs: self.0.resyncs.load(Ordering::Relaxed),
        }
    }

    #[cfg(test)]
    pub(crate) fn admit_outbound_for_test(&self, bytes: Vec<u8>) -> BudgetedChunk {
        let capacity = bytes.capacity();
        assert!(self.reserve_new(Stage::Capture, capacity));
        let mut chunk = BudgetedChunk {
            bytes,
            lease: PayloadLease {
                budget: self.clone(),
                bytes: capacity,
                stage: Stage::Capture,
            },
        };
        assert!(chunk.lease.try_retag(Stage::Outbound));
        chunk
    }
}

/// The rare branch of [`PipelineBudget::release`], kept out of a body that runs
/// once per released payload. See `release`'s comment for why this reports and
/// saturates instead of asserting in shipped builds.
#[cold]
#[inline(never)]
fn report_release_underflow(stage: Stage, bytes: usize, total: usize, current: usize) {
    error!(
        stage = ?stage,
        released_bytes = bytes,
        total_bytes = total,
        stage_bytes = current,
        "pipeline accounting underflow; saturating the release"
    );
    debug_assert!(total >= bytes, "pipeline total accounting underflow");
    debug_assert!(current >= bytes, "pipeline stage accounting underflow");
}

fn stage_bytes(usage: &Usage, stage: Stage) -> usize {
    match stage {
        Stage::Capture => usage.capture,
        Stage::Reassembly => usage.reassembly,
        Stage::Outbound => usage.outbound,
    }
}

fn stage_bytes_mut(usage: &mut Usage, stage: Stage) -> &mut usize {
    match stage {
        Stage::Capture => &mut usage.capture,
        Stage::Reassembly => &mut usage.reassembly,
        Stage::Outbound => &mut usage.outbound,
    }
}

pub(crate) struct PayloadLease {
    budget: PipelineBudget,
    bytes: usize,
    stage: Stage,
}

impl PayloadLease {
    fn try_retag(&mut self, target: Stage) -> bool {
        if self.budget.try_retag(self.stage, target, self.bytes) {
            self.stage = target;
            true
        } else {
            false
        }
    }
}

impl Drop for PayloadLease {
    fn drop(&mut self) {
        self.budget.release(self.stage, self.bytes);
    }
}

/// Move-only payload and its unique byte-accounting lease.
pub struct BudgetedChunk {
    bytes: Vec<u8>,
    lease: PayloadLease,
}

impl BudgetedChunk {
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn capacity(&self) -> usize {
        self.lease.bytes
    }

    fn try_retag_pending(&mut self) -> bool {
        self.lease.try_retag(Stage::Reassembly)
    }

    /// Accounts for a chunk the pipeline could not forward. Consuming it is the
    /// point: the lease releases the reserved bytes as it drops.
    pub(crate) fn record_drop(self) {
        self.lease.budget.record_drop(self.lease.bytes);
    }

    /// Moves this chunk's lease from the reassembly stage to the outbound one,
    /// waiting until a release makes room.
    ///
    /// # Errors
    ///
    /// The `Err` payload is this chunk, handed back untouched, and only ever
    /// because it is larger than the entire outbound quota — a wait that could
    /// never succeed. Every transient shortage is awaited instead, so `Err`
    /// means "never", not "not yet"; the caller owns the chunk again and must
    /// drop it (recording the drop) rather than retry.
    pub(crate) async fn retag_outbound(mut self) -> Result<Self, Self> {
        if self.capacity() > self.lease.budget.stage_limit(Stage::Outbound) {
            return Err(self);
        }
        let budget = self.lease.budget.clone();
        loop {
            let notified = budget.0.released.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.lease.try_retag(Stage::Outbound) {
                return Ok(self);
            }
            notified.await;
        }
    }

    pub(crate) fn into_parts(self) -> (Vec<u8>, PayloadLease) {
        let Self { bytes, lease } = self;
        (bytes, lease)
    }
}

#[cfg(test)]
impl std::fmt::Debug for BudgetedChunk {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.bytes.fmt(formatter)
    }
}

#[cfg(test)]
impl PartialEq<Vec<u8>> for BudgetedChunk {
    fn eq(&self, other: &Vec<u8>) -> bool {
        self.bytes == *other
    }
}

impl Deref for BudgetedChunk {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

pub(crate) struct BudgetedSegment {
    pub flow: FlowKey,
    pub seq: u32,
    pub syn: bool,
    payload: BudgetedChunk,
}

impl BudgetedSegment {
    pub(crate) fn payload(&self) -> &[u8] {
        self.payload.as_slice()
    }

    pub(crate) fn capacity(&self) -> usize {
        self.payload.capacity()
    }
}

// No `Deref<Target = [u8]>` here, unlike `BudgetedChunk` above: a segment is a
// captured TCP record (flow, seq, syn, payload), not a transparent wrapper around
// bytes, and surfacing `len()`/`is_empty()`/`first()` on it would read as
// properties of the segment. Payload reads go through `payload()`. The pair is
// also a trap worth not building: `BudgetedChunk::capacity()` reports the *lease*
// size while `len()` reports the current buffer length, and `HalfStream::absorb`
// shrinks the latter without the former.

// Size canaries for the per-packet types. Every captured packet becomes one of
// these, and a
// `CaptureEvent` holding a `BudgetedSegment` is stored *by value* in a 512-slot
// channel: one extra field in `FlowKey` or `PayloadLease` silently inflates
// tens of KiB of queue. These are not ABI contracts — the types are `repr(Rust)`
// and their layout is unspecified — so a failure here means "re-measure and
// update the number, deliberately", never "work around it".
#[cfg(target_pointer_width = "64")]
const _: () = {
    // 24 (Vec) + 24 (PayloadLease: Arc + usize + Stage) = 48.
    assert!(size_of::<BudgetedChunk>() == 48);
    // 64 (FlowKey) + 48 (BudgetedChunk) + 4 (seq) + 1 (syn), padded to 120.
    assert!(size_of::<BudgetedSegment>() == 120);
};

/// Cap on the number of tracked streams. One armed game connection needs one;
/// reconnections and — since capture is port-wide — any host sending from the
/// game port would otherwise mint keys without bound, each able to buffer up to
/// `MAX_PENDING_BYTES`. Well above legitimate need; the stalest entry is
/// evicted past it.
const MAX_STREAMS: usize = 64;

/// One post-resync burst is deliberately small: it only gives reordered
/// predecessors a chance to establish the initial sequence anchor.
pub(crate) const INITIAL_ANCHOR_MAX_BYTES: usize = 256 * 1024;
pub(crate) const INITIAL_ANCHOR_MAX_SEGMENTS: usize = 128;

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
        let budget = segment.payload.lease.budget.clone();
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
        let outcome = half.push(segment.seq, segment.syn, segment.payload);
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
                payload.bytes.drain(..already);
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

/// Whether `bytes` more pending bytes still fit `held`, and the new total if so.
///
/// The `None` arm folds overflow and over-quota together: both mean "do not
/// buffer this", and neither may be expressed as a wrapping add on a counter fed
/// by wire-supplied payload lengths.
fn fits_pending(held: usize, bytes: usize) -> Option<usize> {
    held.checked_add(bytes)
        .filter(|total| *total <= MAX_PENDING_BYTES)
}

/// Takes a displaced chunk's charge back off a stream's pending total.
///
/// The per-stream twin of [`PipelineBudget::release`], and defensive for the same
/// reason spelled out there: every caller holds the invariant today (a chunk's
/// charge is `lease.bytes`, which never drifts when `absorb` trims the buffer,
/// and both call sites have just taken the entry out of `pending`), but a bare
/// subtraction fails badly in *both* profiles if that ever stops being true. It
/// wraps to ~1.8e19 wherever overflow checks are off, which makes
/// [`fits_pending`] refuse every out-of-order segment forever — permanent silent
/// resync churn, visible to the player only as a shop that stops updating — and
/// panics inside the reassembly task wherever they are on, which `catch_unwind`
/// turns into a dead session. Saturating plus a named log is diagnosable; a
/// `debug_assert!` keeps the fail-fast where an abort costs a developer a stack
/// trace rather than a player a session.
fn pending_after_release(pending_bytes: usize, released: usize) -> usize {
    if pending_bytes < released {
        report_pending_underflow(pending_bytes, released);
    }
    pending_bytes.saturating_sub(released)
}

#[cold]
#[inline(never)]
fn report_pending_underflow(pending_bytes: usize, released: usize) {
    error!(
        pending_bytes,
        released, "pending accounting underflow; saturating the release"
    );
    debug_assert!(
        pending_bytes >= released,
        "pending stream accounting underflow"
    );
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
        ReassemblyOutcome::Chunks(chunks) => {
            chunks.into_iter().flat_map(|chunk| chunk.bytes).collect()
        }
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
    use std::net::{Ipv4Addr, SocketAddr};

    use super::*;

    fn flow() -> FlowKey {
        flow_from(51000)
    }

    fn flow_from(client_port: u16) -> FlowKey {
        FlowKey {
            client: SocketAddr::from((Ipv4Addr::new(192, 168, 1, 10), client_port)),
            server: SocketAddr::from((Ipv4Addr::new(104, 116, 20, 111), 3333)),
        }
    }

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

    fn test_budget(
        global: usize,
        capture: usize,
        reassembly: usize,
        outbound: usize,
    ) -> PipelineBudget {
        PipelineBudget::with_limits(BudgetLimits {
            global,
            capture,
            reassembly,
            outbound,
        })
    }

    fn sized_seg(flow: FlowKey, seq: u32, len: usize, capacity: usize) -> Segment {
        let mut payload = Vec::with_capacity(capacity);
        payload.resize(len, b'X');
        Segment {
            flow,
            seq,
            syn: false,
            payload,
        }
    }

    fn flatten_half(outcome: HalfOutcome) -> Vec<u8> {
        match outcome {
            HalfOutcome::Chunks(chunks) => {
                chunks.into_iter().flat_map(|chunk| chunk.bytes).collect()
            }
            HalfOutcome::Pressure => Vec::new(),
        }
    }

    #[test]
    fn byte_budget_never_exceeds_global_limit() {
        let budget = test_budget(96, 96, 64, 64);
        let first = budget.admit_capture(sized_seg(flow(), 0, 8, 64)).unwrap();
        assert_eq!(budget.snapshot().current_total, 64);
        let rejected = budget.admit_capture(sized_seg(flow(), 8, 8, 64));
        assert!(rejected.is_err());
        assert_eq!(budget.snapshot().current_total, 64);
        drop(first);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    #[tokio::test]
    async fn byte_budget_leases_release_and_retag_stage_bytes() {
        let budget = test_budget(128, 128, 128, 128);
        let mut admitted = budget.admit_capture(sized_seg(flow(), 0, 8, 64)).unwrap();
        assert!(admitted.payload.try_retag_pending());
        let stats = budget.snapshot();
        assert_eq!((stats.current_capture, stats.current_reassembly), (0, 64));
        let chunk = admitted.payload.retag_outbound().await.unwrap();
        let stats = budget.snapshot();
        assert_eq!((stats.current_reassembly, stats.current_outbound), (0, 64));
        drop(chunk);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    #[test]
    fn byte_budget_high_water_is_monotonic_under_repeated_pressure() {
        let budget = test_budget(64, 64, 64, 64);
        for _ in 0..8 {
            let admitted = budget.admit_capture(sized_seg(flow(), 0, 8, 64)).unwrap();
            assert!(budget.admit_capture(sized_seg(flow(), 0, 1, 1)).is_err());
            assert_eq!(budget.snapshot().high_water_total, 64);
            drop(admitted);
        }
        assert_eq!(budget.snapshot().current_total, 0);
        assert_eq!(budget.snapshot().high_water_total, 64);
    }

    /// `release` runs from `PayloadLease::drop`, possibly while a worker
    /// unwinds: in a shipped build an accounting bug must degrade to a
    /// saturating subtraction plus a log, never to a panic that would turn the
    /// unwind into an `abort()` with no crash log.
    #[test]
    #[cfg(not(debug_assertions))]
    fn release_underflow_saturates_instead_of_panicking() {
        let budget = test_budget(128, 128, 128, 128);
        budget.release(Stage::Capture, 64);
        let stats = budget.snapshot();
        assert_eq!((stats.current_total, stats.current_capture), (0, 0));
        // Still usable afterwards: the counters are floored, not corrupted.
        let admitted = budget.admit_capture(sized_seg(flow(), 0, 8, 64)).unwrap();
        assert_eq!(budget.snapshot().current_total, 64);
        drop(admitted);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    /// The same bug in a debug or test build still fails fast, where an abort
    /// costs a developer a stack trace rather than a player a session.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "pipeline total accounting underflow")]
    fn release_underflow_fails_fast_in_debug_builds() {
        test_budget(128, 128, 128, 128).release(Stage::Capture, 64);
    }

    /// `try_retag` is never reached from a `Drop`, so it keeps a hard assert in
    /// every profile.
    #[test]
    #[should_panic(expected = "pipeline stage accounting underflow")]
    fn retag_underflow_panics_in_every_profile() {
        let budget = test_budget(128, 128, 128, 128);
        budget.try_retag(Stage::Capture, Stage::Outbound, 64);
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
            .payload;
        assert_eq!(flatten_half(half.push(expected, false, first)), b"AB");
        // And the following contiguous segment keeps flowing.
        let second = budget
            .admit_capture(seg(expected.wrapping_add(2), false, b"CD"))
            .unwrap()
            .payload;
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
