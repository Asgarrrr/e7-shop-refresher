//! Passive traffic capture and the packet-source abstraction.

mod ip;

// The unelevated end of the split: launching the broker and connecting to it.
// Gated with the backend rather than with `windows` alone, because without the
// backend there is no broker to launch — and `windows-sys` itself is an optional
// dependency this feature turns on.
#[cfg(all(windows, feature = "windivert-backend"))]
mod elevate;

// Unconditional on purpose. The broker protocol is pure `std::io` with no Win32
// in it, and gating it on `windivert-backend` would keep its tests out of both
// portable lanes of `just verify` — the only lanes that ever exercise it, since
// the elevated side is untestable by construction.
mod pipe;

#[cfg(all(windows, feature = "windivert-backend"))]
mod windivert;

use std::net::SocketAddr;

use crate::error::Result;

pub use ip::parse_segment;
// Re-exported rather than left `pub` inside a private module: this crate is a
// lib plus a bin, so only an item reachable from the crate root escapes
// `dead_code`, and every lane builds with `-D warnings`. The broker (the sole
// caller of `write_frame`) does not exist yet.
pub use pipe::{
    FRAME_FLAG_CAPTURE_LOSS, FRAME_KIND_DIAGNOSTIC, FRAME_KIND_FATAL, FRAME_KIND_PACKET,
    PipeSource, write_frame,
};

#[cfg(all(windows, feature = "windivert-backend"))]
pub(crate) use elevate::spawn_elevated_broker;
#[cfg(all(windows, feature = "windivert-backend"))]
pub use windivert::WinDivertSource;

/// Largest packet the capture driver can deliver (`WINDIVERT_MTU_MAX`), and by
/// extension the largest frame payload the broker may put on the pipe.
/// Coalesced receives (RSC/LSO) routinely exceed the wire MTU, so anything
/// smaller as a buffer makes `recv` fail on the first bulk transfer.
///
/// Lives here rather than in the backend because both ends of the pipe have to
/// agree on it: the elevated side sizes its receive buffer from it, the
/// unelevated side refuses any frame claiming more.
pub const MAX_PACKET_BYTES: usize = 65_575;

/// A blocking capture source paired with the capability that wakes it during
/// session teardown. Keeping the pair together makes it impossible for the
/// app to start a receive thread without retaining its stop handle.
pub(crate) struct CaptureSource {
    pub(crate) packets: Box<dyn PacketSource>,
    pub(crate) stop: Box<dyn CaptureStop>,
}

#[cfg(any(test, all(windows, feature = "windivert-backend")))]
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
pub(crate) trait CaptureStop: Send {
    fn stop(&mut self) -> Result<()>;
}

/// Direction of a segment relative to the game server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    ClientToServer,
    /// Server response — carries the shop contents.
    ServerToClient,
}

/// Identifies a TCP connection independently of the observed direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FlowKey {
    pub client: SocketAddr,
    pub server: SocketAddr,
}

/// A captured TCP segment, normalized for reassembly.
#[derive(Debug, Clone)]
pub struct Segment {
    pub flow: FlowKey,
    pub direction: Direction,
    /// TCP sequence number of the first byte of `payload`.
    pub seq: u32,
    pub syn: bool,
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
    assert!(std::mem::size_of::<FlowKey>() == 64);
    // 64 (FlowKey) + 24 (Vec) + 4 (seq) + 1 (direction) + 1 (syn), padded to 96.
    assert!(std::mem::size_of::<Segment>() == 96);
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
