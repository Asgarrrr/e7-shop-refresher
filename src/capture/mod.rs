//! Passive traffic capture and the packet-source abstraction.

mod ip;

// The one capture backend: an Npcap tap over every adapter, in this process.
// `pcap-backend` is either on, or the crate has no way to capture and says so
// (see `app::build_source`).
#[cfg(all(windows, feature = "pcap-backend"))]
mod pcap;

use std::net::SocketAddr;

use crate::error::Result;

pub use ip::parse_segment;

#[cfg(all(windows, feature = "pcap-backend"))]
pub use pcap::PcapSource;

/// A blocking capture source paired with the capability that wakes it during
/// session teardown, so the app cannot start a receive thread without
/// retaining its stop handle.
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
/// Calling `stop` more than once has the same effect as calling it once.
///
/// Infallible: all three implementors returned `Ok(())` unconditionally,
/// leaving `stop_and_join`'s error branch dead. Widen back to `Result` if a
/// future implementor's wake can genuinely fail.
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
    /// The server-to-client bytes. In the capture path this is the captured
    /// frame's own allocation trimmed down in place (see [`parse_segment`]),
    /// so `payload.capacity()` stays the whole frame's, headers included, for
    /// the segment's life. That's the memory actually retained, and what
    /// `PipelineBudget::admit_capture` charges.
    pub payload: Vec<u8>,
}

// Size canaries: one of these exists per captured packet and the budgeted
// form derived from it is queued by value, so a field added here is paid for
// on every packet. `repr(Rust)` layout is unspecified, so a failure means
// re-measure and update the number, not work around it.
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
    /// loop turns this into a resync instead of waiting for a gap fill that
    /// will not arrive. Backends that cannot lose packets keep the default.
    fn take_capture_loss(&mut self) -> bool {
        false
    }
}
