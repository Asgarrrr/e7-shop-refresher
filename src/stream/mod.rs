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
pub(crate) use budget::{BudgetedSegment, PipelineBudget};
pub use reassembly::Reassembler;
pub(crate) use reassembly::{InitialBurst, ReassemblyOutcome};

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
// `capture::pcap::FRAME_QUEUE_DEPTH`. `with_limits` still asserts at runtime
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
