//! Decodes a captured IP packet into a [`Segment`].

use std::net::{IpAddr, SocketAddr};

use etherparse::{NetSlice, SlicedPacket, TransportSlice};

use super::{FlowKey, Segment};

/// Extracts a server-to-client TCP segment from an IP packet.
///
/// Returns `None` when the packet is malformed, or is not TCP *sent by*
/// `game_port`. The port test is what identifies the server: only the side that
/// owns `game_port` can be it, and only what that side sends carries the shop
/// response this product decodes. A client-to-server segment is therefore not
/// a segment with another label — it is not a segment at all, and stops here.
/// The kernel filter (`tcp and src port {game_port}`, see `pcap`) already makes
/// that the only traffic delivered; this is the same rule restated where the
/// bytes are actually interpreted, so a backend with a laxer filter cannot
/// smuggle the wrong half of a connection into reassembly.
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

    // Skip control packets that carry no stream bytes (pure ACKs, and pure
    // FINs — reassembly never tears a half-stream down on FIN). Only SYN, which
    // anchors the baseline, and data-bearing segments matter.
    if tcp.payload().is_empty() && !tcp.syn() {
        return None;
    }

    let src = SocketAddr::new(src_ip, tcp.source_port());
    let dst = SocketAddr::new(dst_ip, tcp.destination_port());

    if src.port() != game_port {
        return None;
    }
    let flow = FlowKey {
        client: dst,
        server: src,
    };

    Some(Segment {
        flow,
        seq: tcp.sequence_number(),
        syn: tcp.syn(),
        payload: tcp.payload().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use etherparse::PacketBuilder;

    const GAME_PORT: u16 = 3333;

    /// Build an IPv4 TCP packet (IP layer down, no Ethernet) as raw bytes.
    /// `syn`/`payload` shape the flags `parse_segment` inspects.
    fn ipv4_tcp(
        src: ([u8; 4], u16),
        dst: ([u8; 4], u16),
        seq: u32,
        syn: bool,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut b = PacketBuilder::ipv4(src.0, dst.0, 64).tcp(src.1, dst.1, seq, 64_240);
        if syn {
            b = b.syn();
        }
        let mut out = Vec::with_capacity(b.size(payload.len()));
        b.write(&mut out, payload).expect("write packet");
        out
    }

    /// Build an IPv6 TCP packet (IP layer down, no Ethernet) as raw bytes.
    fn ipv6_tcp(
        src: ([u8; 16], u16),
        dst: ([u8; 16], u16),
        seq: u32,
        syn: bool,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut b = PacketBuilder::ipv6(src.0, dst.0, 64).tcp(src.1, dst.1, seq, 64_240);
        if syn {
            b = b.syn();
        }
        let mut out = Vec::with_capacity(b.size(payload.len()));
        b.write(&mut out, payload).expect("write packet");
        out
    }

    #[test]
    fn a_segment_sent_by_the_game_port_is_parsed_and_names_both_endpoints() {
        let server = ([104, 116, 20, 111], GAME_PORT); // src port == game_port
        let client = ([192, 168, 1, 10], 51000);
        let bytes = ipv4_tcp(server, client, 1000, false, b"AB");
        let seg = parse_segment(&bytes, GAME_PORT).expect("should parse");
        // The sender owns the game port, so it is the server; the peer is the
        // client. Roles, not direction of travel — the flow key is symmetric.
        assert_eq!(seg.flow.server, SocketAddr::from((server.0, server.1)));
        assert_eq!(seg.flow.client, SocketAddr::from((client.0, client.1)));
        assert_eq!(seg.seq, 1000);
        assert_eq!(seg.payload, b"AB");
        assert!(!seg.syn);
    }

    #[test]
    fn a_segment_sent_to_the_game_port_is_not_a_segment_at_all() {
        // The client -> server half of the very connection the test above
        // parses. It used to be decoded and labelled, then discarded further
        // down; nothing has ever decoded it, so it is refused here instead.
        let bytes = ipv4_tcp(
            ([192, 168, 1, 10], 51000),
            ([104, 116, 20, 111], GAME_PORT), // dst port == game_port
            2000,
            false,
            b"XY",
        );
        assert!(parse_segment(&bytes, GAME_PORT).is_none());
    }

    #[test]
    fn pure_ack_is_dropped() {
        // Empty payload, not a SYN: a pure control packet carrying no bytes.
        let bytes = ipv4_tcp(
            ([104, 116, 20, 111], GAME_PORT),
            ([192, 168, 1, 10], 51000),
            3000,
            false,
            b"",
        );
        assert!(parse_segment(&bytes, GAME_PORT).is_none());
    }

    #[test]
    fn data_bearing_syn_is_kept() {
        // A SYN with no payload still anchors the baseline and must be kept.
        let bytes = ipv4_tcp(
            ([104, 116, 20, 111], GAME_PORT),
            ([192, 168, 1, 10], 51000),
            4000,
            true,
            b"",
        );
        let seg = parse_segment(&bytes, GAME_PORT).expect("SYN should be kept");
        assert!(seg.syn);
        assert!(seg.payload.is_empty());
    }

    #[test]
    fn wrong_port_is_ignored() {
        // Neither endpoint owns the game port.
        let bytes = ipv4_tcp(
            ([104, 116, 20, 111], 4444),
            ([192, 168, 1, 10], 51000),
            5000,
            false,
            b"AB",
        );
        assert!(parse_segment(&bytes, GAME_PORT).is_none());
    }

    #[test]
    fn truncated_bytes_are_rejected() {
        let bytes = ipv4_tcp(
            ([104, 116, 20, 111], GAME_PORT),
            ([192, 168, 1, 10], 51000),
            6000,
            false,
            b"AB",
        );
        // Half of a valid packet is not parseable.
        assert!(parse_segment(&bytes[..bytes.len() / 2], GAME_PORT).is_none());
        // Arbitrary garbage is not parseable.
        assert!(parse_segment(b"not a packet at all", GAME_PORT).is_none());
    }

    #[test]
    fn ipv6_data_is_parsed() {
        let server = [
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
        ];
        let client = [
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02,
        ];
        let bytes = ipv6_tcp((server, GAME_PORT), (client, 51000), 7000, false, b"AB");
        let seg = parse_segment(&bytes, GAME_PORT).expect("should parse");
        assert_eq!(seg.seq, 7000);
        assert_eq!(seg.payload, b"AB");
    }

    #[test]
    fn parsed_segment_flows_through_reassembler() {
        use crate::stream::Reassembler;
        let bytes = ipv4_tcp(
            ([104, 116, 20, 111], GAME_PORT),
            ([192, 168, 1, 10], 51000),
            1000,
            false,
            b"AB",
        );
        let seg = parse_segment(&bytes, GAME_PORT).expect("parse");
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg), b"AB");
    }
}
