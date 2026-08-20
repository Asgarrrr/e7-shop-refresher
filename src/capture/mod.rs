//! Passive traffic capture and the packet-source abstraction.

mod ip;

// The one capture backend. Without `pcap-backend` the crate has no way to
// capture at all, and says so (see `app::build_source`).
#[cfg(all(windows, feature = "pcap-backend"))]
mod pcap;

use std::net::SocketAddr;

use crate::error::Result;

pub use ip::parse_segment;

#[cfg(all(windows, feature = "pcap-backend"))]
pub use pcap::PcapSource;

/// A blocking capture source paired with the capability that wakes it at
/// teardown, so the app cannot start a receive thread without retaining its
/// stop handle.
pub(crate) struct CaptureSource {
    pub(crate) packets: Box<dyn PacketSource>,
    pub(crate) stop: Box<dyn CaptureStop>,
}

#[cfg(any(test, all(windows, feature = "pcap-backend")))]
impl CaptureSource {
    pub(crate) fn new(
        packets: impl PacketSource + 'static,
        stop: impl CaptureStop + 'static,
    ) -> Self {
        Self {
            packets: Box::new(packets),
            stop: Box::new(stop),
        }
    }
}

/// One-shot, idempotent wake capability for a blocking [`PacketSource`].
///
/// Implementations must not close a raw OS handle concurrently with receive.
///
/// Infallible because every implementor's wake is a flag store, which left
/// `stop_and_join`'s error branch dead; widen back to `Result` if a future
/// implementor's wake can genuinely fail.
pub(crate) trait CaptureStop: Send {
    fn stop(&mut self);
}

/// Identifies the TCP connection a segment belongs to.
///
/// Endpoints are stored by role (`server` owns `game_port`), not direction of
/// travel. Only one direction is ever captured (see [`parse_segment`]), so a
/// flow and its server-to-client byte stream are the same thing here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FlowKey {
    pub client: SocketAddr,
    pub server: SocketAddr,
}

/// A captured server-to-client TCP segment, normalized for reassembly.
#[derive(Debug, Clone)]
pub struct Segment {
    pub flow: FlowKey,
    /// TCP sequence number of the first byte of `payload`.
    pub seq: u32,
    pub syn: bool,
    /// The server-to-client bytes: the captured frame's own allocation trimmed
    /// in place (see [`parse_segment`]), so `capacity()` stays the whole
    /// frame's for the segment's life — the memory actually retained, and what
    /// `PipelineBudget::admit_capture` charges.
    pub payload: Vec<u8>,
}

// Size canaries: one of these exists per captured packet and is queued by
// value, so a field added here is paid for on every packet. `repr(Rust)`
// layout is unspecified — a failure means re-measure, not work around.
#[cfg(target_pointer_width = "64")]
const _: () = {
    // Two `SocketAddr` (32 each: the IPv6 variant is 28 bytes plus a tag).
    assert!(size_of::<FlowKey>() == 64);
    // 64 (FlowKey) + 24 (Vec) + 4 (seq) + 1 (syn), padded to 96.
    assert!(size_of::<Segment>() == 96);
};

/// Blocking source of TCP segments. Implementations observe traffic without
/// ever modifying it.
pub trait PacketSource: Send {
    /// Blocks until the next TCP segment matching the filter is captured.
    fn next_segment(&mut self) -> Result<Segment>;

    /// Reports, and clears, whether the backend lost captured packets since
    /// the previous call.
    ///
    /// A passive tap never sees already-ACKed bytes again, so a hole from
    /// backend-side loss can never be filled by retransmission — the capture
    /// loop resyncs instead of waiting for a gap fill that will not arrive.
    /// Backends that cannot lose packets keep the default.
    fn take_capture_loss(&mut self) -> bool {
        false
    }
}
