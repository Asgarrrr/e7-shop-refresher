//! Decodes a captured IP packet into a [`Segment`].

use std::net::{IpAddr, SocketAddr};
use std::ops::Range;

use etherparse::{NetSlice, SlicedPacket, TransportSlice};

use super::{FlowKey, Segment};

/// Extracts a server-to-client TCP segment from an IP packet.
///
/// Takes the frame **by value and reuses its allocation**: the payload is that
/// same buffer trimmed in place, never a fresh copy of a subslice of it. The
/// capture thread already had to copy the frame out of the driver's ring
/// (`pcap::capture_loop`, where the slice dies on the next receive), so that
/// copy is unavoidable — this one was not, and it was not small either: the
/// snaplen is 262 144 and a measured RSC/LRO frame was 48 870 bytes. The price
/// of reusing the buffer is that `payload.capacity()` stays the whole frame's;
/// [`Segment::payload`] says what that means for the byte budget.
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
pub fn parse_segment(mut frame: Vec<u8>, game_port: u16) -> Option<Segment> {
    let span = segment_span(&frame, game_port)?;
    // Neither call reallocates and neither shrinks the buffer: `truncate` drops
    // the trailing headers-and-padding bookkeeping, `drain` memmoves the payload
    // down to offset 0. One memmove replaces one allocation plus one memcpy.
    frame.truncate(span.payload.end);
    frame.drain(..span.payload.start);
    Some(Segment {
        flow: span.flow,
        seq: span.seq,
        syn: span.syn,
        payload: frame,
    })
}

/// A decoded segment header plus the payload's *range* inside the frame it came
/// from. Split out of [`parse_segment`] so the frame can be borrowed for the
/// decode and then trimmed by value, with no copy in between.
struct SegmentSpan {
    flow: FlowKey,
    seq: u32,
    syn: bool,
    payload: Range<usize>,
}

fn segment_span(bytes: &[u8], game_port: u16) -> Option<SegmentSpan> {
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
        // ARP is not a shop stream. Named rather than wildcarded: `NetSlice` is
        // not `#[non_exhaustive]`, so a variant can only appear in a major
        // etherparse bump, and when it does this should stop compiling here
        // rather than silently drop whatever it is.
        NetSlice::Arp(_) => return None,
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

    Some(SegmentSpan {
        flow,
        seq: tcp.sequence_number(),
        syn: tcp.syn(),
        payload: subslice_range(bytes, tcp.payload())?,
    })
}

/// The range `inner` occupies inside `outer`, or `None` when it is not a
/// subslice of it.
///
/// Addresses only, no `unsafe`. `etherparse` hands back subslices of the very
/// buffer it was given, so neither the subtraction nor the bound can fail in
/// practice; if a future version ever returned something else, this refuses the
/// packet instead of trimming the frame down to the wrong bytes.
fn subslice_range(outer: &[u8], inner: &[u8]) -> Option<Range<usize>> {
    let start = inner.as_ptr().addr().checked_sub(outer.as_ptr().addr())?;
    let end = start.checked_add(inner.len())?;
    (end <= outer.len()).then_some(start..end)
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
        let seg = parse_segment(bytes, GAME_PORT).expect("should parse");
        // The sender owns the game port, so it is the server; the peer is the
        // client. Roles, not direction of travel — the flow key is symmetric.
        assert_eq!(seg.flow.server, SocketAddr::from((server.0, server.1)));
        assert_eq!(seg.flow.client, SocketAddr::from((client.0, client.1)));
        assert_eq!(seg.seq, 1000);
        assert_eq!(seg.payload, b"AB");
        assert!(!seg.syn);
    }

    #[test]
    fn the_payload_reuses_the_frame_allocation_instead_of_copying_out_of_it() {
        // The whole point of taking the frame by value: one copy on the packet
        // path, not two. A `Vec` keeps its capacity across `truncate`/`drain`, so
        // "the payload is still the frame's own buffer" is observable as "the
        // capacity is still the frame's" — and that is also the number
        // `PipelineBudget::admit_capture` charges.
        let bytes = ipv4_tcp(
            ([104, 116, 20, 111], GAME_PORT),
            ([192, 168, 1, 10], 51000),
            8000,
            false,
            b"AB",
        );
        let frame_capacity = bytes.capacity();
        assert!(
            frame_capacity > 2,
            "the headers make the frame the larger one"
        );
        let seg = parse_segment(bytes, GAME_PORT).expect("should parse");
        assert_eq!(seg.payload, b"AB");
        assert_eq!(seg.payload.capacity(), frame_capacity);
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
        assert!(parse_segment(bytes, GAME_PORT).is_none());
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
        assert!(parse_segment(bytes, GAME_PORT).is_none());
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
        let seg = parse_segment(bytes, GAME_PORT).expect("SYN should be kept");
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
        assert!(parse_segment(bytes, GAME_PORT).is_none());
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
        assert!(parse_segment(bytes[..bytes.len() / 2].to_vec(), GAME_PORT).is_none());
        // Arbitrary garbage is not parseable.
        assert!(parse_segment(b"not a packet at all".to_vec(), GAME_PORT).is_none());
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
        let seg = parse_segment(bytes, GAME_PORT).expect("should parse");
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
        let seg = parse_segment(bytes, GAME_PORT).expect("parse");
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg), b"AB");
    }
}
