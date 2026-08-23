//! TCP reassembly.
//!
//! Capture observes traffic below TCP, so segments may arrive out of order,
//! duplicated, or overlapping; this layer reconstructs, per connection, the
//! ordered byte stream the TCP stack would deliver. Only the server-to-client
//! half is ever captured, so `FlowKey` is the whole reassembly identity.
//!
//! Offsets are relative and measured from the currently expected byte, not
//! from the fixed origin: anchoring to the origin breaks once a half-stream
//! has delivered 2 GiB, where the distance exceeds `i32` range and every
//! later segment reads as an already-delivered retransmission.
//!
//! [`budget`] owns byte accounting and knows nothing of TCP; [`reassembly`]
//! treats a budgeted payload as an opaque carrier. The quota numbers and their
//! compile-time relation live here, the only module that sees both halves.

mod budget;
mod reassembly;

pub use budget::BudgetedChunk;
pub(crate) use budget::{BudgetedSegment, PipelineBudget, RunBaselineCell};
/// The per-run reading of the counters, and the zero it is read from — both
/// named only by `ui::capture_health`. [`budget::RunBaselineCell`], which
/// publishes that zero, is exported unconditionally above: `app::session` writes
/// it in every build.
///
/// Gated, like [`BudgetLimits`]'s own declaration below and for the same reason:
/// a `pub(crate) use` nothing in the crate names is an unused import, which
/// `-D warnings` makes fatal — and two of the four `just clippy` lanes build
/// without `gui`.
#[cfg(feature = "gui")]
pub(crate) use budget::{PipelineStats, RunBaseline};
pub use reassembly::Reassembler;
pub(crate) use reassembly::{InitialBurst, ReassemblyOutcome};

/// Whether a re-anchor made the run worse, or merely happened.
///
/// The distinction is not a shade of severity; it is a question with a yes or a
/// no: were bytes that this pipeline had already received discarded without
/// being delivered? Only a `Degradation` reaches the player's verdict — the rule
/// is stated once, in [`budget::PipelineStats::dominant_resync`] — and because
/// [`ResyncCause::tier`] matches exhaustively, a cause added later cannot reach
/// that verdict by default; it has to say which of the two it is.
///
/// It exists because one cause was reaching it that never lost a byte. The
/// stream table used to fill with the game's own closed connections, re-anchoring
/// flows that had nothing left to deliver — a patched build logged 46 of them in
/// ~90 s — and counting those alongside genuine losses made `dominant_resync`
/// name them and paint a healthy run amber.
///
/// That flood has since been cut off at its source: a connection that ends is
/// now retired instead of waiting to be evicted (see
/// [`ResyncCause::StreamReclaimed`]). The distinction stays because it is the
/// right one, not because of the volume that exposed it — a lossless re-anchor
/// was never a fault at any rate, and the remaining ones are exactly the signal
/// that some flow is *not* being retired.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResyncTier {
    /// Bytes the pipeline had were thrown away undelivered, or a gap in the
    /// delivered stream is now permanent. The run is worse for it, and the
    /// player is told.
    Degradation,
    /// A flow re-anchored and no byte was lost doing it. Counted, because a
    /// number is what tells a maintainer how often this happens; never a
    /// verdict, because there is nothing for a player to act on.
    Housekeeping,
}

/// Why a byte stream lost continuity and had to re-anchor.
///
/// Every re-anchor here is counted, and they used to be counted *together*: one
/// `resyncs` total, from which `ui::capture_health` re-derived a cause it had
/// never been told. It guessed "a slow connection or a driver hiccup" for causes
/// that are neither — a full frame funnel and an exhausted byte quota are this
/// process falling behind, not the wire — and a player reading that went looking
/// at their router. The cause now travels with the signal, from the thread that
/// observed it to the sentence the window renders.
///
/// A cause also carries a [`ResyncTier`]: counting a re-anchor and blaming the
/// run for it are two decisions, and only the first is true of every variant
/// here.
///
/// Declared here rather than in [`budget`], which owns the counters: three of
/// these are named by `capture` and `app::ingest`, so the enum belongs to the
/// module that already exists to see more than one half.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResyncCause {
    /// The capture driver's own drop counter moved: packets matched the filter
    /// and the kernel ring had no room. The one cause genuinely outside this
    /// process, and the only one the old sentence described correctly.
    DriverRing,
    /// `capture::pcap`'s frame funnel overflowed: the frames were captured, and
    /// this process did not drain them in time.
    CaptureFunnel,
    /// A pipeline stage's byte quota refused a segment.
    ByteQuota,
    /// The capture → reassembly channel had no free slot.
    MetadataQueue,
    /// One flow filled its own out-of-order buffer behind a gap that a passive
    /// tap can never see filled.
    ReassemblyStream,
    /// The shared reassembly quota was exhausted, whoever was holding it.
    ReassemblyShared,
    /// The stream table was full, the stalest flow was evicted, and that flow
    /// was still holding a segment behind a gap: bytes this pipeline received
    /// and charged for are discarded here, undelivered.
    ///
    /// Records no dropped segment — nothing was *refused*, `Reassembler::push_budgeted`
    /// states the rule — so this counter is the only trace the loss leaves.
    StreamEvicted,
    /// The same eviction with nothing buffered behind a gap: the flow's place in
    /// its own sequence space went, not a byte of it.
    ///
    /// The reason [`ResyncTier`] exists, and once the common case by a wide
    /// margin. `capture::parse_segment` used to discard FIN and RST, so a closed
    /// connection was never retired and lingered until the table needed its
    /// slot; the game opens a short connection every ~1.7 s, so all 64 slots
    /// held its own dead clones within about two minutes. A patched build logged
    /// 46 of these in ~90 s, every one with `buffered_bytes=0`.
    ///
    /// That source is gone: `Reassembler::push_budgeted` now drops a flow when
    /// its connection ends. What is left is the residue this counter is worth
    /// watching for — flows whose end was never sent, or never captured — and a
    /// run in which it climbs again means the retirement path is not firing.
    StreamReclaimed,
    /// The server aborted a connection (RST) while that flow was still holding a
    /// segment behind a gap: bytes this pipeline received and charged for are
    /// discarded, and no retransmission is coming for the hole in front of them.
    ///
    /// [`Self::StreamEvicted`]'s loss, from the opposite direction — that one is
    /// the table reclaiming a slot from a flow that was still working, this one
    /// is the connection ending under a flow that was. Kept apart because the
    /// sentence a player is shown differs: nothing about an abort implicates the
    /// stream table, and pointing at it would send them after the wrong thing.
    ///
    /// An orderly close records nothing at all, here or anywhere: a flow that
    /// delivered every byte it received and then ended did not re-anchor, and
    /// counting one re-anchor per connection would have replaced the 46 noise
    /// events this fix removes with about 35 a minute.
    ConnectionReset,
}

impl ResyncCause {
    /// Every variant, in declaration order.
    ///
    /// That order is load-bearing twice: it indexes the counter array, and it
    /// breaks ties in `PipelineStats::dominant_resync`, where the *earlier*
    /// cause wins. So the causes this process cannot control come first — a tie
    /// between "the driver dropped packets" and "we then fell behind draining
    /// them" names the one that started it.
    ///
    /// [`Self::StreamReclaimed`] and [`Self::ConnectionReset`] are appended
    /// rather than slotted beside the evictions they are variants of: both
    /// properties are positional, and appending is the one placement that leaves
    /// every existing cause's counter slot and tie-break rank exactly where it
    /// was. `StreamReclaimed`'s own rank decides nothing anyway —
    /// `dominant_resync` skips [`ResyncTier::Housekeeping`] before it ever
    /// compares counts — and `ConnectionReset` last means it loses every tie it
    /// is in, which is the right way round: a tie between it and an eviction
    /// names the table, the standing condition, over the one aborted connection.
    pub(crate) const ALL: [Self; 9] = [
        Self::DriverRing,
        Self::CaptureFunnel,
        Self::ByteQuota,
        Self::MetadataQueue,
        Self::ReassemblyStream,
        Self::ReassemblyShared,
        Self::StreamEvicted,
        Self::StreamReclaimed,
        Self::ConnectionReset,
    ];

    pub(crate) const COUNT: usize = Self::ALL.len();

    /// This cause's slot in the per-cause counters. `as usize` on a fieldless
    /// enum, pinned to [`Self::ALL`]'s order by the assertion below.
    pub(crate) const fn index(self) -> usize {
        self as usize
    }

    /// Whether this cause cost the run bytes, or only a position.
    ///
    /// Written out variant by variant, with no `_` arm, so that adding a cause
    /// is a compile error until its author has answered the question. That is
    /// the whole mechanism: the answer is consumed in exactly one place
    /// (`PipelineStats::dominant_resync`), and every cause that reaches a player
    /// reaches them through it.
    pub(crate) const fn tier(self) -> ResyncTier {
        match self {
            Self::DriverRing
            | Self::CaptureFunnel
            | Self::ByteQuota
            | Self::MetadataQueue
            | Self::ReassemblyStream
            | Self::ReassemblyShared
            | Self::StreamEvicted
            | Self::ConnectionReset => ResyncTier::Degradation,
            Self::StreamReclaimed => ResyncTier::Housekeeping,
        }
    }

    /// The short field value for a `warn!` line, where the log is grepped and a
    /// sentence is not. The player-facing prose lives in `ui::capture_health`:
    /// this module owns the vocabulary, the window owns the wording.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::DriverRing => "driver_ring",
            Self::CaptureFunnel => "capture_funnel",
            Self::ByteQuota => "byte_quota",
            Self::MetadataQueue => "metadata_queue",
            Self::ReassemblyStream => "reassembly_stream",
            Self::ReassemblyShared => "reassembly_shared",
            Self::StreamEvicted => "stream_evicted",
            Self::StreamReclaimed => "stream_reclaimed",
            Self::ConnectionReset => "connection_reset",
        }
    }
}

// `index` casts the discriminant, so a variant reordered without moving its
// entry in `ALL` would silently swap two causes' counters — the diagnosis would
// name the wrong one, and nothing else would fail.
const _: () = {
    let mut slot = 0;
    while slot < ResyncCause::COUNT {
        assert!(ResyncCause::ALL[slot].index() == slot);
        slot += 1;
    }
};

/// Cap on out-of-order bytes buffered per tracked stream (memory guard).
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;

pub(crate) const PIPELINE_GLOBAL_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const CAPTURE_STAGE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const REASSEMBLY_STAGE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const OUTBOUND_STAGE_BYTES: usize = 8 * 1024 * 1024;

/// Deliberately small: one post-resync burst only gives reordered
/// predecessors a chance to establish the initial sequence anchor.
pub(crate) const INITIAL_ANCHOR_MAX_BYTES: usize = 256 * 1024;
pub(crate) const INITIAL_ANCHOR_MAX_SEGMENTS: usize = 128;

/// Per-stage byte quotas, `pub(crate)` only for the test-only
/// `PipelineBudget::with_test_limits`. Declared here rather than in [`budget`]
/// because it is named from outside `stream` only under `cfg(test)`, so a
/// re-export would be an unused import in a shipped build — fatal under
/// `-D warnings`.
#[derive(Clone, Copy)]
pub(crate) struct BudgetLimits {
    pub(crate) global: usize,
    pub(crate) capture: usize,
    pub(crate) reassembly: usize,
    pub(crate) outbound: usize,
}

// The pipeline's memory bound, and what a tuning pass edits by hand. Not the
// whole bound: a frame is charged only at `admit_capture`, so the queue between
// the capture threads and `PcapSource::next_segment` is bounded separately by
// `capture::pcap::FRAME_QUEUE_BYTES` (4 MiB), with `FRAME_QUEUE_SLOTS` as its
// backstop. `with_limits` still asserts at runtime
// because `with_test_limits` passes arbitrary values.
const _: () = {
    assert!(CAPTURE_STAGE_BYTES <= PIPELINE_GLOBAL_BYTES);
    assert!(REASSEMBLY_STAGE_BYTES <= PIPELINE_GLOBAL_BYTES);
    assert!(OUTBOUND_STAGE_BYTES <= PIPELINE_GLOBAL_BYTES);
    // Or the per-stream cap is dead code: the stage limit would trip first.
    assert!(MAX_PENDING_BYTES <= REASSEMBLY_STAGE_BYTES);
    // A burst buffers inside the capture stage, so a larger cap could never fill.
    assert!(INITIAL_ANCHOR_MAX_BYTES <= CAPTURE_STAGE_BYTES);
};

// Fixtures kept here rather than duplicated across both submodules' suites.
#[cfg(test)]
use std::net::{Ipv4Addr, SocketAddr};

#[cfg(test)]
use crate::capture::{FlowKey, Segment};

#[cfg(test)]
fn flow() -> FlowKey {
    flow_from(51000)
}

#[cfg(test)]
fn flow_from(client_port: u16) -> FlowKey {
    FlowKey {
        client: SocketAddr::from((Ipv4Addr::new(192, 168, 1, 10), client_port)),
        server: SocketAddr::from((Ipv4Addr::new(104, 116, 20, 111), 3333)),
    }
}

#[cfg(test)]
fn sized_seg(flow: FlowKey, seq: u32, len: usize, capacity: usize) -> Segment {
    let mut payload = Vec::with_capacity(capacity);
    payload.resize(len, b'X');
    Segment {
        flow,
        seq,
        syn: false,
        fin: false,
        rst: false,
        payload,
    }
}

#[cfg(test)]
fn test_budget(
    global: usize,
    capture: usize,
    reassembly: usize,
    outbound: usize,
) -> PipelineBudget {
    PipelineBudget::with_test_limits(BudgetLimits {
        global,
        capture,
        reassembly,
        outbound,
    })
}
