//! TCP reassembly.
//!
//! Capture observes traffic below TCP, so segments may arrive out of order,
//! duplicated (retransmissions), or overlapping. This layer reconstructs,
//! per connection, the ordered byte stream the TCP stack would deliver.
//! Only the server-to-client half is ever captured, so "the stream of a
//! flow" is unambiguous and `FlowKey` is the whole reassembly identity.
//!
//! Work is done in relative offsets from the stream origin (the first
//! observed segment). TCP sequence numbers are `u32` and wrap; a segment's
//! offset is derived from its distance to the currently expected byte, not
//! the fixed origin, so the signed `i32` sequence window tracks the stream
//! as it advances. Anchoring to the origin instead would break once a
//! half-stream delivered 2 GiB: the distance would exceed `i32` range and
//! every later segment would look like an already-delivered retransmission.
//!
//! Two submodules, one seam. [`budget`] owns byte accounting — the shared
//! `Mutex<Usage>` pool, per-stage quotas, and the move-only
//! `BudgetedChunk`/`PayloadLease` pair that makes a double release
//! unrepresentable; nothing in it knows what TCP is. [`reassembly`] owns
//! everything above and treats a budgeted payload as an opaque carrier. This
//! file keeps the quota numbers and their compile-time relation, being the
//! only module that sees both halves.

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

/// One post-resync burst is deliberately small: it only gives reordered
/// predecessors a chance to establish the initial sequence anchor.
pub(crate) const INITIAL_ANCHOR_MAX_BYTES: usize = 256 * 1024;
pub(crate) const INITIAL_ANCHOR_MAX_SEGMENTS: usize = 128;

/// Per-stage byte quotas. `pub(crate)` only so the test-only
/// `PipelineBudget::with_test_limits` can be named by the two sibling test
/// suites that override the production constants; nothing outside this
/// module can build a budget from it on a production path.
///
/// Declared here, not in [`budget`]: it's named from outside `stream` only
/// under `cfg(test)`, so a `pub(crate) use` re-export would be an unused
/// import in a shipped build — a broken build under `-D warnings`.
#[derive(Clone, Copy)]
pub(crate) struct BudgetLimits {
    pub(crate) global: usize,
    pub(crate) capture: usize,
    pub(crate) reassembly: usize,
    pub(crate) outbound: usize,
}

// These four numbers are the only defence against unbounded memory on a
// capture path that runs for hours, and what a later tuning pass edits by
// hand. Their relation is pure arithmetic over constants, checked here
// rather than on the player's machine — `with_limits` still keeps runtime
// asserts because `with_test_limits` passes arbitrary values.
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

// Fixtures shared by both submodules' test suites, not duplicated: a flow
// key and a capacity-vs-length segment are the vocabulary of both halves —
// `budget` charges the capacity, `reassembly` orders by sequence number.
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
