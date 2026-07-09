//! Decodes a raw IP packet (WinDivert network layer) into a [`Segment`].

use std::net::{IpAddr, SocketAddr};

use etherparse::{NetSlice, SlicedPacket, TransportSlice};

use super::{Direction, FlowKey, Segment};

/// Extracts a TCP segment from an IP packet.
///
/// Returns `None` when the packet is not TCP concerning `game_port`, or is
/// malformed. Direction is inferred from which side owns `game_port`.
pub fn parse_segment(bytes: &[u8], game_port: u16) -> Option<Segment> {
    let sliced = SlicedPacket::from_ip(bytes).ok()?;

    let (src_ip, dst_ip) = match sliced.net? {
        NetSlice::Ipv4(ip) => {
            let header = ip.header();
            (
                IpAddr::V4(header.source_addr()),
                IpAddr::V4(header.destination_addr()),
            )
        }
        NetSlice::Ipv6(ip) => {
            let header = ip.header();
            (
                IpAddr::V6(header.source_addr()),
                IpAddr::V6(header.destination_addr()),
            )
        }
        _ => return None,
    };

    let TransportSlice::Tcp(tcp) = sliced.transport? else {
        return None;
    };

    // Skip pure ACKs (no data, no SYN/FIN): they carry no stream bytes and are
    // ~half the packets on an active connection. SYN/FIN are kept (baseline /
    // teardown signal).
    if tcp.payload().is_empty() && !tcp.syn() && !tcp.fin() {
        return None;
    }

    let src = SocketAddr::new(src_ip, tcp.source_port());
    let dst = SocketAddr::new(dst_ip, tcp.destination_port());

    let (direction, flow) = if dst.port() == game_port {
        (
            Direction::ClientToServer,
            FlowKey {
                client: src,
                server: dst,
            },
        )
    } else if src.port() == game_port {
        (
            Direction::ServerToClient,
            FlowKey {
                client: dst,
                server: src,
            },
        )
    } else {
        return None;
    };

    Some(Segment {
        flow,
        direction,
        seq: tcp.sequence_number(),
        syn: tcp.syn(),
        fin: tcp.fin(),
        payload: tcp.payload().to_vec(),
    })
}
