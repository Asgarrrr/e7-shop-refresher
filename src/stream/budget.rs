//! Byte accounting for every owned payload after packet parsing.
//!
//! Nothing here knows what TCP is. The move-only
//! [`BudgetedChunk`]/[`PayloadLease`] pair exists so a `Vec<u8>` and its lease
//! are never separated except through [`BudgetedChunk::into_parts`].
//!
//! [`PipelineBudget::release`] runs from a `Drop` that may execute while a
//! worker unwinds, so it saturates and reports; [`PipelineBudget::try_retag`]
//! keeps a hard assert in every profile because no `Drop` reaches it.

use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use tracing::error;

use super::{
    BudgetLimits, CAPTURE_STAGE_BYTES, MAX_PENDING_BYTES, OUTBOUND_STAGE_BYTES,
    PIPELINE_GLOBAL_BYTES, REASSEMBLY_STAGE_BYTES,
};
use crate::capture::{FlowKey, Segment};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Capture,
    Reassembly,
    Outbound,
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

    /// # Panics
    ///
    /// Panics if any stage quota exceeds the global one: a stage that can never
    /// fill is a silent hole in the memory guard. Only reachable through
    /// `with_test_limits`; the parent module proves the shipped constants.
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

    /// Test-only escape from the production constants. Takes the named struct,
    /// not four positional `usize`s, where a silently swapped pair would read
    /// as a passing test of a budget nobody meant to describe.
    #[cfg(test)]
    pub(crate) fn with_test_limits(limits: BudgetLimits) -> Self {
        Self::with_limits(limits)
    }

    /// Takes ownership of `segment`'s payload, pairing the buffer with the
    /// lease that will give its bytes back.
    ///
    /// Charges `capacity()`, not `len()`: `parse_segment` trims the frame in
    /// place, so that capacity is the whole frame's (~40–60 bytes of headers
    /// and padding on top) and is the memory actually retained. The lease
    /// records what was charged and [`BudgetedChunk::capacity`] replays it,
    /// never a fresh `Vec::capacity()`, so a later `truncate` cannot make a
    /// release disagree with its reservation. [`CAPTURE_STAGE_BYTES`] was
    /// deliberately not re-baselined for that overhead: it bounds memory, not
    /// packet count, and nothing here is profiled.
    ///
    /// # Errors
    ///
    /// The `Err` payload is `segment` itself, unmodified, because a quota is
    /// full; the caller drops it and records the drop, or retries after a
    /// release elsewhere.
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
    /// Runs from [`PayloadLease::drop`], which workers execute while unwinding,
    /// and a panic from a `Drop` during an unwind aborts the process with no
    /// `crash.log` — the failure `crash.rs` exists to prevent. So an accounting
    /// bug saturates and is reported here; `debug_assert!` keeps the fail-fast
    /// where the cost is a developer's stack trace, not a player's session.
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

    // Not swallowed errors: the closures always return `Some`, so only `Ok`.
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

/// The rare branch of [`PipelineBudget::release`], out of a body that runs once
/// per released payload; see `release` on why it saturates instead of asserting.
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

/// Whether `bytes` more pending bytes still fit `held`, and the new total if so.
/// `None` folds overflow into over-quota: both mean "do not buffer this", and
/// neither may be a wrapping add on a counter fed by wire-supplied lengths.
pub(super) fn fits_pending(held: usize, bytes: usize) -> Option<usize> {
    held.checked_add(bytes)
        .filter(|total| *total <= MAX_PENDING_BYTES)
}

/// Takes a displaced chunk's charge back off a stream's pending total.
///
/// The per-stream twin of [`PipelineBudget::release`], defensive for the same
/// reason: a bare subtraction wrapped to ~1.8e19 makes [`fits_pending`] refuse
/// every out-of-order segment forever — silent resync churn a player sees only
/// as a shop that stops updating — and with overflow checks on it panics in the
/// reassembly task, which `catch_unwind` turns into a dead session.
pub(super) fn pending_after_release(pending_bytes: usize, released: usize) -> usize {
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

    /// Discards the first `n` bytes of the buffer. Only the buffer shrinks: the
    /// lease keeps charging what it reserved, so [`Self::capacity`] is unchanged
    /// and a later release still matches its reservation.
    pub(super) fn drain_front(&mut self, n: usize) {
        self.bytes.drain(..n);
    }

    pub(super) fn try_retag_pending(&mut self) -> bool {
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
    /// Returns this chunk untouched only when it is larger than the entire
    /// outbound quota — a wait that could never succeed. Transient shortages
    /// are awaited, so `Err` means "never", not "not yet": drop it (recording
    /// the drop) rather than retry.
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

    /// The budget this payload is charged against, cloned (an `Arc` bump)
    /// because the reassembler records the drop and resync after the payload
    /// has moved on into a half-stream.
    pub(super) fn budget(&self) -> PipelineBudget {
        self.payload.lease.budget.clone()
    }

    /// Drops the TCP framing the reassembler has already read off the segment.
    /// A `HalfStream` is below the level that knows about flows, so the segment
    /// ends here and the chunk carries its lease onward.
    pub(super) fn into_payload(self) -> BudgetedChunk {
        self.payload
    }
}

// No `Deref<Target = [u8]>` here, unlike `BudgetedChunk`: it was declined
// because `capacity()` reports the lease size while `len()` reports the buffer
// length, and `HalfStream::absorb` shrinks the latter without the former.

// Size canaries: a `CaptureEvent` holding a `BudgetedSegment` is stored by value
// in a 512-slot channel, so one extra field silently inflates tens of KiB of
// queue. Not ABI contracts — `repr(Rust)` layout is unspecified — so a failure
// means re-measure, not work around.
#[cfg(target_pointer_width = "64")]
const _: () = {
    // 24 (Vec) + 24 (PayloadLease: Arc + usize + Stage) = 48.
    assert!(size_of::<BudgetedChunk>() == 48);
    // 64 (FlowKey) + 48 (BudgetedChunk) + 4 (seq) + 1 (syn), padded to 120.
    assert!(size_of::<BudgetedSegment>() == 120);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{flow, sized_seg, test_budget};

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

    /// In a shipped build an accounting bug must degrade to a saturating
    /// subtraction plus a log, never a panic that turns a `Drop` during an
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

    /// The same bug in a debug or test build still fails fast.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "pipeline total accounting underflow")]
    fn release_underflow_fails_fast_in_debug_builds() {
        test_budget(128, 128, 128, 128).release(Stage::Capture, 64);
    }

    /// `try_retag` is never reached from a `Drop`, so its assert is hard.
    #[test]
    #[should_panic(expected = "pipeline stage accounting underflow")]
    fn retag_underflow_panics_in_every_profile() {
        let budget = test_budget(128, 128, 128, 128);
        budget.try_retag(Stage::Capture, Stage::Outbound, 64);
    }
}
