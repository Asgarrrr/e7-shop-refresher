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
    PIPELINE_GLOBAL_BYTES, REASSEMBLY_STAGE_BYTES, ResyncCause, ResyncTier,
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
    /// One counter per [`ResyncCause`], indexed by [`ResyncCause::index`].
    /// There is deliberately no separate total: [`PipelineBudget::snapshot`]
    /// sums these, so the total and its breakdown cannot drift apart.
    resyncs: [AtomicU64; ResyncCause::COUNT],
}

#[derive(Clone)]
pub(crate) struct PipelineBudget(Arc<BudgetInner>);

/// One read of the budget: five live gauges and four counters that only grow.
///
/// The counters cover the whole process — [`PipelineBudget`] is built once and
/// never reset — unless this value came out of [`Self::since`], which rebases
/// them onto one run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PipelineStats {
    pub current_total: usize,
    pub current_capture: usize,
    pub current_reassembly: usize,
    pub current_outbound: usize,
    pub high_water_total: usize,
    pub dropped_segments: u64,
    pub dropped_bytes: u64,
    /// Every re-anchor these counters cover, whatever caused it: the sum of
    /// [`Self::resyncs_by_cause`], never counted separately from it — which is
    /// why [`Self::since`] re-sums it rather than rebasing it.
    ///
    /// Faults and housekeeping alike, because this is a count of re-anchors and
    /// not of things that went wrong. Only [`Self::dominant_resync`] separates
    /// the two, and only it feeds a player; this number's readers are `warn!`
    /// fields, where "how many times did a stream re-anchor" is the question.
    pub resyncs: u64,
    pub resyncs_by_cause: [u64; ResyncCause::COUNT],
}

impl PipelineStats {
    /// The cause behind most of the re-anchors that *cost* something, or `None`
    /// if there were none. Ask it of a [`Self::since`] result and it answers for
    /// one run instead of for the whole process, which is what the window does.
    ///
    /// This is the sole input to the player-facing verdict and its amber, so it
    /// is where the "only a [`ResyncTier::Degradation`] is a fault" rule is
    /// stated — once, above the counts rather than beside each caller. A
    /// [`ResyncTier::Housekeeping`] cause is still counted, and still readable in
    /// [`Self::resyncs_by_cause`] and the logs; it simply has no vote here. It
    /// used to have one, and it used it: 46 lossless stream evictions in ~90 s
    /// outvoted everything and painted a healthy run amber.
    ///
    /// Strict `>`, so the first [`ResyncCause::ALL`] entry wins a tie — see that
    /// constant for why its order is the answer to "which of these started it".
    ///
    /// # The two families are counted in different units
    ///
    /// The comparison below is by raw magnitude, and the numbers it compares do
    /// not measure the same thing. `DriverRing`, `CaptureFunnel`, `ByteQuota` and
    /// `MetadataQueue` reach these counters through `app::pressure::PressureResync::request`,
    /// which records a cause only when it opens an episode — a sustained fault
    /// counts once per marker round-trip, deliberately, so a cascade cannot let
    /// its own consequences outvote its origin. Every cause `stream` records for
    /// itself — `ReassemblyStream`, `ReassemblyShared`, `StreamEvicted` and
    /// `ConnectionReset` — is recorded once per event instead. So a long backend
    /// fault and a rare reassembly overflow are weighed as if their counts meant
    /// the same, and the more compressed family can lose a vote it should win.
    ///
    /// Not currently reachable in volume, as far as the code bounds it: every
    /// one of those per-event records costs real bytes, so none of them can
    /// repeat cheaply. The two reassembly causes fire only when a pending-byte
    /// quota overflows — `MAX_PENDING_BYTES` (8 MiB) for one stream,
    /// `REASSEMBLY_STAGE_BYTES` (16 MiB) shared — and the stream is discarded on
    /// the way out, so each costs another whole cap of gap-buffered bytes.
    /// `StreamEvicted` needs a flow buffering behind a gap to be the stalest
    /// entry in a full table, and `ConnectionReset` needs the server to abort a
    /// connection that has a gap open. Reaching the 46 records that made this
    /// visible would take hundreds of MiB buffered behind gaps, and the kernel
    /// filter admits one game server's own traffic. What made it reachable was
    /// the lossless eviction, which cost nothing per record and has been taken
    /// out of this comparison — and, since `Reassembler` began retiring closed
    /// flows, largely out of existence.
    ///
    /// The fix, if a future cause makes it reachable again, is to give the
    /// reassembly causes the same episode semantics rather than to weight them
    /// here: an episode is the unit a *verdict* wants, since it asks what went
    /// wrong once, not how many times a consequence repeated. That means a
    /// marker `stream` does not own, so it belongs with `app::pressure`.
    // Same reachability gap as `capture::CaptureHealth`'s methods: the only
    // shipped caller is `ui::capture_health`, and two of the four `just clippy`
    // lanes build without `gui`, where this reads as dead despite `just verify`'s
    // `gui,actuator` lane proving it live.
    #[allow(
        dead_code,
        reason = "read by ui::capture_health (gui) and by app's tests; the two no-gui clippy lanes see neither"
    )]
    pub(crate) fn dominant_resync(&self) -> Option<ResyncCause> {
        let mut dominant: Option<ResyncCause> = None;
        for cause in ResyncCause::ALL {
            match cause.tier() {
                ResyncTier::Degradation => {}
                ResyncTier::Housekeeping => continue,
            }
            let count = self.resyncs_by_cause[cause.index()];
            if count == 0 {
                continue;
            }
            if dominant.is_none_or(|held| count > self.resyncs_by_cause[held.index()]) {
                dominant = Some(cause);
            }
        }
        dominant
    }

    /// The same snapshot with its cumulative counters counted from `baseline`
    /// instead of from process start — one run's share of what the budget has
    /// been adding up since `app::setup` built it.
    ///
    /// The five gauges ([`Self::current_total`] and the three stages,
    /// [`Self::high_water_total`]) are passed through untouched: they say what
    /// the pipeline holds *now*, and a high-water mark is a maximum, not a sum,
    /// so neither has a per-run reading a subtraction could produce.
    ///
    /// The tie-break is deliberately not re-done here. This returns a
    /// [`PipelineStats`], so [`Self::dominant_resync`] answers on the rebased
    /// counts with the one implementation there has ever been — and that matters
    /// more after a subtraction than before one, because the subtraction is what
    /// *creates* most ties: it takes a long-standing count down to what this run
    /// added, and [`ResyncCause::ALL`]'s order is then what decides.
    ///
    /// `saturating_sub`, never a bare one: a baseline is an earlier read of
    /// counters that only ever grow, so it cannot lead them — but this crate's
    /// rule is that an accounting bug degrades to a saturating op rather than a
    /// panic (`Cargo.toml`'s `overflow-checks = true` makes a bare subtraction
    /// panic in a shipped build), and what calls this is an egui frame. The same
    /// saturation is what makes a torn read of [`RunBaselineCell`] harmless.
    #[allow(
        dead_code,
        reason = "read by ui::capture_health (gui) and by app::session's tests; the plain lib \
                  target the two no-gui clippy lanes also build sees neither"
    )]
    pub(crate) fn since(&self, baseline: RunBaseline) -> Self {
        let mut run = *self;
        // Re-summed from the rebased breakdown rather than rebased itself, so
        // the "total is exactly the sum of its causes" this type promises holds
        // for a run exactly as `PipelineBudget::snapshot` makes it hold for the
        // process.
        let mut resyncs = 0_u64;
        let at_arming = baseline.resyncs_by_cause;
        for (count, before) in run.resyncs_by_cause.iter_mut().zip(at_arming) {
            *count = count.saturating_sub(before);
            resyncs = resyncs.saturating_add(*count);
        }
        run.resyncs = resyncs;
        run.dropped_segments = self
            .dropped_segments
            .saturating_sub(baseline.dropped_segments);
        run.dropped_bytes = self.dropped_bytes.saturating_sub(baseline.dropped_bytes);
        run
    }
}

/// The cumulative counters as they stood when a run armed, so a reader can
/// report what *this* run did out of a budget that counts what the process did.
///
/// [`PipelineBudget`] is built once, in `app::setup`, and a clone rides in
/// `SessionHandles` for the window's whole life; [`PipelineBudget::record_resync`]
/// only ever adds, and nothing in this crate resets it. That is deliberate — the
/// same structure guards the live memory leases, whose byte accounting must not
/// be zeroed under a running pipeline — but it left `ui::capture_health`
/// reporting one transient re-anchor as a standing fault for the rest of the
/// process, a `Command::Stop` and a fresh Start included, because a stop only
/// disarms the `WatchGate`.
///
/// So a run is bounded by subtraction and the budget is left alone. *When* the
/// subtrahend is taken is the whole of the guarantee, and it is not the reader's
/// to decide: see [`RunBaselineCell`], which publishes it on the edge that opens
/// the gate.
///
/// A type of its own rather than a second [`PipelineStats`], for the reason
/// `PipelineBudget::with_test_limits` takes a named struct rather than four
/// positional `usize`s: `a.since(b)` and `b.since(a)` are both spellings of a
/// plausible mistake, and only one of them compiles.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RunBaseline {
    dropped_segments: u64,
    dropped_bytes: u64,
    resyncs_by_cause: [u64; ResyncCause::COUNT],
}

impl RunBaseline {
    /// The zero a run arming now counts from.
    ///
    /// [`Default`] — every count zero — is what [`RunBaselineCell`] holds before
    /// the first arming, where it is also the truth: a shut gate forwards no
    /// segment, and `app::ingest` discards a backend loss taken with the gate off
    /// rather than counting it, so nothing behind these counters can have moved
    /// yet.
    pub(crate) fn armed_at(stats: &PipelineStats) -> Self {
        Self {
            dropped_segments: stats.dropped_segments,
            dropped_bytes: stats.dropped_bytes,
            resyncs_by_cause: stats.resyncs_by_cause,
        }
    }
}

/// The [`RunBaseline`] the run on screen counts from: written by the thread that
/// opens the `WatchGate`, read by the egui frame that renders the verdict.
///
/// The baseline used to be taken by the window itself, on the first frame that
/// *sampled* the session as armed. But the counters are incremented on the
/// capture thread and the gate is opened by the session loop, both ahead of a
/// frame that repaints at 4 Hz — and the capture thread outranks the egui thread
/// (`THREAD_PRIORITY_HIGHEST`, see `app::workers::raise_capture_thread_priority`),
/// so the window falls furthest behind on exactly the machine busy enough to
/// overflow the capture funnel. A re-anchor in the first fraction of a second of
/// a run — precisely what this readout exists to report — therefore landed
/// *before* the frame that took the baseline, and `since` subtracted it out of
/// the run that suffered it, for the rest of that run.
///
/// Carries the [`PipelineBudget`] it baselines against, so the arming site holds
/// one handle and cannot pair a baseline with another budget's counters.
#[derive(Clone)]
pub(crate) struct RunBaselineCell {
    budget: PipelineBudget,
    published: Arc<PublishedBaseline>,
}

/// [`RunBaseline`]'s fields, one `AtomicU64` each.
///
/// `Relaxed` on all ten, and nothing claimed beyond each cell's own modification
/// order: these cells publish no memory besides themselves, and a frame that
/// catches the ten stores mid-flight reads a mix of this arming's values and the
/// previous arming's — every one of which is *lower* than the counter it will be
/// subtracted from, so a torn read can only over-report a fault for the 250 ms
/// until the next repaint, never hide one. Hiding one is the defect this type
/// exists to close.
///
/// Ten cells rather than one lock the egui frame has to take: the frame reads
/// this on every repaint, and there is nothing here a mutex would buy that the
/// direction of the tear does not already give.
#[derive(Default)]
struct PublishedBaseline {
    dropped_segments: AtomicU64,
    dropped_bytes: AtomicU64,
    resyncs_by_cause: [AtomicU64; ResyncCause::COUNT],
}

impl RunBaselineCell {
    pub(crate) fn new(budget: PipelineBudget) -> Self {
        Self {
            budget,
            published: Arc::new(PublishedBaseline::default()),
        }
    }

    /// The counters as they stand right now — the zero an arming that is about
    /// to happen would count from.
    ///
    /// Read *before* the store that opens the gate and published only if that
    /// store opened it (see `app::session::SessionGate::arm`). No packet crosses
    /// a gate that is still shut — `app::ingest` skips the iteration before it
    /// can charge anything — so nothing *this* run does can land between the read
    /// and the arming, where a read taken after the arming could already carry
    /// the re-anchor the capture thread records on the first packet past it.
    ///
    /// What can still land in that window is a re-anchor from the previous run's
    /// backlog draining through reassembly, and it is then charged to the new
    /// run. That is the direction this readout has to fail in — reporting a fault
    /// that has already been reported, rather than hiding a fresh one — and the
    /// window is the handful of instructions between two adjacent statements.
    ///
    /// Goes through [`PipelineBudget::snapshot`], five gauges and a short lock it
    /// has no use for, rather than reading the four counters again here: this
    /// runs once per session dispatch — a tick a second, a server message, a
    /// command — and one uncontended lock there is worth less than a second
    /// definition of what the counters read.
    pub(crate) fn counters_now(&self) -> RunBaseline {
        RunBaseline::armed_at(&self.budget.snapshot())
    }

    /// Makes `baseline` the zero every later frame counts this run from.
    pub(crate) fn publish(&self, baseline: RunBaseline) {
        self.published
            .dropped_segments
            .store(baseline.dropped_segments, Ordering::Relaxed);
        self.published
            .dropped_bytes
            .store(baseline.dropped_bytes, Ordering::Relaxed);
        for (cell, count) in self
            .published
            .resyncs_by_cause
            .iter()
            .zip(baseline.resyncs_by_cause)
        {
            cell.store(count, Ordering::Relaxed);
        }
    }

    /// The zero the run on screen counts from, for [`PipelineStats::since`].
    #[allow(
        dead_code,
        reason = "read by ui::capture_health (gui) and by app's tests; the plain lib target \
                  the two no-gui clippy lanes also build sees neither"
    )]
    pub(crate) fn baseline(&self) -> RunBaseline {
        let mut resyncs_by_cause = [0_u64; ResyncCause::COUNT];
        for (slot, cell) in resyncs_by_cause
            .iter_mut()
            .zip(&self.published.resyncs_by_cause)
        {
            *slot = cell.load(Ordering::Relaxed);
        }
        RunBaseline {
            dropped_segments: self.published.dropped_segments.load(Ordering::Relaxed),
            dropped_bytes: self.published.dropped_bytes.load(Ordering::Relaxed),
            resyncs_by_cause,
        }
    }
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
            resyncs: [const { AtomicU64::new(0) }; ResyncCause::COUNT],
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
            fin: segment.fin,
            rst: segment.rst,
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
        // The region that has to be atomic is the read-check-write above, and it
        // ends on this line; the guard ends with it rather than wherever the
        // block happens to close. Buys nothing today — only `true` follows — so
        // this is not a contention fix. It is here so a statement appended after
        // the write does not silently join an atomic region it was never meant
        // to be part of.
        drop(usage);
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

    /// Counts one re-anchor against the cause that forced it.
    ///
    /// The caller always knows that cause — it is the branch it is standing in
    /// — so nothing downstream has to infer it from the totals, which is what
    /// [`ResyncCause`] exists to stop.
    pub(crate) fn record_resync(&self, cause: ResyncCause) {
        let _ = self.0.resyncs[cause.index()].fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |current| Some(current.saturating_add(1)),
        );
    }

    pub(crate) fn snapshot(&self) -> PipelineStats {
        let usage = self.0.usage.lock().unwrap_or_else(|err| err.into_inner());
        // Not one atomic group, for the reason `CaptureHealth::snapshot` gives
        // about its own three: every counter is monotonic and read in the
        // dozens, so a torn read is a snapshot one event older than it could
        // have been, never a wrong diagnosis.
        let mut resyncs_by_cause = [0_u64; ResyncCause::COUNT];
        let mut resyncs = 0_u64;
        for (slot, counter) in resyncs_by_cause.iter_mut().zip(&self.0.resyncs) {
            *slot = counter.load(Ordering::Relaxed);
            resyncs = resyncs.saturating_add(*slot);
        }
        PipelineStats {
            current_total: usage.total,
            current_capture: usage.capture,
            current_reassembly: usage.reassembly,
            current_outbound: usage.outbound,
            high_water_total: usage.high_water,
            dropped_segments: self.0.dropped_segments.load(Ordering::Relaxed),
            dropped_bytes: self.0.dropped_bytes.load(Ordering::Relaxed),
            resyncs,
            resyncs_by_cause,
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
    /// Carried through unread by this module, exactly as `syn` is: byte
    /// accounting knows nothing of TCP, and [`super::reassembly`] is the only
    /// layer that can act on a connection ending. See [`Segment::fin`].
    pub fin: bool,
    /// See [`Segment::rst`].
    pub rst: bool,
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
    // 64 (FlowKey) + 48 (BudgetedChunk) + 4 (seq) + 3 (syn, fin, rst), padded
    // to 120.
    //
    // Re-measured 2026-08-22, when `fin` and `rst` were added so a closed
    // connection could be retired instead of held until the stream table needed
    // its slot: still 120, and `app::pressure`'s `CaptureEvent` canary — which
    // wraps this variant — is still 120 too. The two bools landed in padding
    // `syn` already occupied.
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

    /// The total and its breakdown are one number read two ways, so no sequence
    /// of records can make them disagree.
    #[test]
    fn the_resync_total_is_exactly_the_sum_of_its_causes() {
        let budget = test_budget(64, 64, 64, 64);
        assert_eq!(budget.snapshot().resyncs, 0);
        assert_eq!(budget.snapshot().dominant_resync(), None);

        for (index, cause) in ResyncCause::ALL.into_iter().enumerate() {
            // A different count per cause, so a slot swapped against
            // `ResyncCause::index` shows up as a wrong dominant cause rather
            // than as a total that still adds up.
            for _ in 0..=index {
                budget.record_resync(cause);
            }
        }
        let stats = budget.snapshot();
        assert_eq!(stats.resyncs, stats.resyncs_by_cause.iter().sum::<u64>());
        for (index, cause) in ResyncCause::ALL.into_iter().enumerate() {
            assert_eq!(
                stats.resyncs_by_cause[cause.index()],
                index as u64 + 1,
                "{cause:?} landed in the wrong slot"
            );
        }
        // The answer is the highest-counted *degradation* cause, which is the
        // last one in `ALL`. `StreamReclaimed` sits one slot before it and so
        // outnumbers every other cause here except that one — and still does not
        // win, because it is housekeeping and never reaches the comparison.
        assert!(
            stats.resyncs_by_cause[ResyncCause::StreamReclaimed.index()]
                > stats.resyncs_by_cause[ResyncCause::StreamEvicted.index()],
            "the housekeeping cause must outnumber a fault, or it wins by default"
        );
        assert_eq!(stats.dominant_resync(), Some(ResyncCause::ConnectionReset));
    }

    /// The defect: a housekeeping cause is counted and never wins the verdict,
    /// however many of them there are. The field reading is 46 lossless stream
    /// evictions in ~90 s, which used to outvote everything and paint a healthy
    /// run amber.
    #[test]
    fn a_housekeeping_cause_is_counted_but_never_wins_the_verdict() {
        let budget = test_budget(64, 64, 64, 64);
        for _ in 0..46 {
            budget.record_resync(ResyncCause::StreamReclaimed);
        }

        let stats = budget.snapshot();
        assert_eq!(stats.dominant_resync(), None);
        // Still a number a maintainer can read, in the total and by cause.
        assert_eq!(stats.resyncs, 46);
        assert_eq!(
            stats.resyncs_by_cause[ResyncCause::StreamReclaimed.index()],
            46
        );

        // And one genuine fault outvotes all forty-six of them.
        budget.record_resync(ResyncCause::StreamEvicted);
        assert_eq!(
            budget.snapshot().dominant_resync(),
            Some(ResyncCause::StreamEvicted)
        );
    }

    /// A tie names the *earlier* cause, which is the point of
    /// `ResyncCause::ALL`'s order: a driver ring overflow is what then floods
    /// the funnel, so naming the funnel would be naming the consequence.
    #[test]
    fn a_tie_between_causes_names_the_one_that_would_have_started_it() {
        let budget = test_budget(64, 64, 64, 64);
        budget.record_resync(ResyncCause::CaptureFunnel);
        budget.record_resync(ResyncCause::DriverRing);
        assert_eq!(
            budget.snapshot().dominant_resync(),
            Some(ResyncCause::DriverRing)
        );
        // A clear majority still wins over the ordering.
        budget.record_resync(ResyncCause::CaptureFunnel);
        assert_eq!(
            budget.snapshot().dominant_resync(),
            Some(ResyncCause::CaptureFunnel)
        );
    }

    /// The defect [`RunBaseline`] exists for: the budget is built once in
    /// `app::setup` and never reset, so one re-anchor from an earlier run used
    /// to word and colour the verdict for every run after it.
    #[test]
    fn what_an_earlier_run_did_is_not_this_runs_verdict() {
        let budget = test_budget(64, 64, 64, 64);
        budget.record_resync(ResyncCause::CaptureFunnel);
        budget.record_drop(512);

        let baseline = RunBaseline::armed_at(&budget.snapshot());

        let run = budget.snapshot().since(baseline);
        assert_eq!(run.dominant_resync(), None);
        assert_eq!((run.dropped_segments, run.resyncs), (0, 0));
        // Subtraction, not a reset: the budget still counts everything it did.
        let stats = budget.snapshot();
        assert_eq!(stats.dominant_resync(), Some(ResyncCause::CaptureFunnel));
        assert_eq!((stats.dropped_segments, stats.resyncs), (1, 1));

        // What this run does is still its own.
        budget.record_resync(ResyncCause::ByteQuota);
        budget.record_drop(512);
        let run = budget.snapshot().since(baseline);
        assert_eq!(run.dominant_resync(), Some(ResyncCause::ByteQuota));
        assert_eq!((run.dropped_segments, run.resyncs), (1, 1));
        // The rebased total is the sum of the rebased breakdown, exactly as the
        // process-wide pair is.
        assert_eq!(run.resyncs, run.resyncs_by_cause.iter().sum::<u64>());
    }

    /// The subtraction is what *creates* most ties — a long-standing funnel
    /// count comes down to what this run added — so `ResyncCause::ALL`'s order
    /// has to break them on the rebased numbers, not on the process's.
    #[test]
    fn a_tie_the_baseline_creates_still_names_the_one_that_would_have_started_it() {
        let budget = test_budget(64, 64, 64, 64);
        for _ in 0..5 {
            budget.record_resync(ResyncCause::CaptureFunnel);
        }
        let baseline = RunBaseline::armed_at(&budget.snapshot());
        budget.record_resync(ResyncCause::CaptureFunnel);
        budget.record_resync(ResyncCause::DriverRing);

        // Five to one for the process, one to one for this run.
        assert_eq!(
            budget.snapshot().dominant_resync(),
            Some(ResyncCause::CaptureFunnel)
        );
        assert_eq!(
            budget.snapshot().since(baseline).dominant_resync(),
            Some(ResyncCause::DriverRing)
        );
    }

    /// Every one of the ten published cells is written and read back. A field
    /// left out of `publish` keeps reading zero, which reports the whole
    /// process's count as this run's — silently, and for that one cause only.
    #[test]
    fn the_published_baseline_round_trips_every_counter() {
        let budget = test_budget(64, 64, 64, 64);
        let run = RunBaselineCell::new(budget.clone());
        // Before the first arming the cell is the zero it should be.
        assert_eq!(run.baseline(), RunBaseline::default());

        // A different count per cause, so a slot written into the wrong cell
        // shows up rather than cancelling out.
        for (index, cause) in ResyncCause::ALL.into_iter().enumerate() {
            for _ in 0..=index {
                budget.record_resync(cause);
            }
        }
        budget.record_drop(512);
        run.publish(run.counters_now());

        assert_eq!(run.baseline(), RunBaseline::armed_at(&budget.snapshot()));
        let this_run = budget.snapshot().since(run.baseline());
        assert_eq!(this_run.resyncs, 0);
        assert_eq!((this_run.dropped_segments, this_run.dropped_bytes), (0, 0));
        assert_eq!(this_run.dominant_resync(), None);
    }

    /// A baseline is an earlier read of counters that never shrink, so it can
    /// never lead them. If the accounting ever broke, an egui frame must read
    /// "healthy" — not panic under `overflow-checks`, which is the rule
    /// `release_underflow_saturates_instead_of_panicking` states for the byte
    /// side of the same structure.
    #[test]
    fn a_baseline_ahead_of_the_counters_saturates_instead_of_wrapping() {
        let budget = test_budget(64, 64, 64, 64);
        let empty = budget.snapshot();
        budget.record_resync(ResyncCause::DriverRing);
        budget.record_drop(512);

        let run = empty.since(RunBaseline::armed_at(&budget.snapshot()));

        assert_eq!((run.dropped_segments, run.dropped_bytes), (0, 0));
        assert_eq!(run.dominant_resync(), None);
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
