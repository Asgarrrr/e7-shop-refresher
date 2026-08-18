//! Passive traffic capture and the packet-source abstraction.

mod ip;

// The one capture backend: an Npcap tap over every adapter, in this very
// process. There is no second backend to arbitrate against — `pcap-backend` is
// either on, or the crate has no way to capture and says so (see
// `app::build_source`).
#[cfg(all(windows, feature = "pcap-backend"))]
mod pcap;

use std::net::SocketAddr;

use crate::error::Result;

pub use ip::parse_segment;

#[cfg(all(windows, feature = "pcap-backend"))]
pub use pcap::PcapSource;

/// A blocking capture source paired with the capability that wakes it during
/// session teardown. Keeping the pair together makes it impossible for the
/// app to start a receive thread without retaining its stop handle.
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
/// Infallible, and narrowed to say so. It returned `Result<()>` and all three
/// implementors — `PcapStop` (one relaxed atomic store), and the two test doubles
/// — returned `Ok(())` unconditionally, so `CaptureWorker::stop_and_join`'s
/// `if let Err(err) = self.stop.stop()` was a branch that could not be taken and
/// an error line that could not be logged. A `Result` a trait cannot produce is
/// worse than no `Result`: it tells the caller to handle a case that does not
/// exist, and the handling is then untested by construction. A future
/// implementor whose wake genuinely can fail should widen this back, with the
/// call site's recovery written at the same time.
pub(crate) trait CaptureStop: Send {
    fn stop(&mut self);
}

/// Identifies the TCP connection a segment belongs to.
///
/// The two endpoints are stored under the roles they play, not under the
/// direction of travel: `server` is whichever side owns `game_port`. Only one
/// direction of a connection is ever captured (see [`parse_segment`]), so a
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
    /// The server-to-client bytes — and, in the capture path, the captured
    /// frame's own allocation trimmed down to them in place rather than a copy
    /// carved out of it (see [`parse_segment`]).
    ///
    /// So `payload.capacity()` is the whole frame's, headers included, and stays
    /// that way for the segment's life. That is deliberate: it is the memory
    /// actually retained, and it is what `PipelineBudget::admit_capture` charges,
    /// which makes the one per-packet buffer the byte budget used to be blind to
    /// visible to it.
    pub payload: Vec<u8>,
}

// Size canaries for the per-packet types. One of these exists per captured
// packet, and the budgeted form derived from it is queued by value; a field
// added here is paid for on every packet. These are `repr(Rust)` and their
// layout is unspecified, so a failure means "re-measure and update the number
// deliberately", not "work around it".
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
    /// A passive tap never sees already-ACKed bytes again, so a hole left by a
    /// backend-side loss can never be filled by a retransmission. The capture
    /// loop turns this into a resync instead of letting reassembly wait for a
    /// gap fill that will not arrive. Backends that cannot lose packets keep
    /// the default.
    fn take_capture_loss(&mut self) -> bool {
        false
    }
}
