//! Passive traffic capture and the packet-source abstraction.

mod ip;

#[cfg(all(windows, feature = "windivert-backend"))]
mod windivert;

use std::net::SocketAddr;

use crate::error::Result;

pub use ip::parse_segment;

#[cfg(all(windows, feature = "windivert-backend"))]
pub use windivert::WinDivertSource;

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
    pub fin: bool,
    pub payload: Vec<u8>,
}

/// Blocking source of TCP segments. Implementations observe traffic without
/// ever modifying it.
pub trait PacketSource: Send {
    /// Blocks until the next TCP segment matching the filter is captured.
    fn next_segment(&mut self) -> Result<Segment>;
}
