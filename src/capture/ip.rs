//! Decodes a captured IP packet into a [`Segment`].

use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU16;
use std::ops::Range;

use etherparse::{NetSlice, SlicedPacket, TransportSlice};

use super::{FlowKey, Segment};

/// Extracts a server-to-client TCP segment from an IP packet.
///
/// Trims the frame's allocation in place rather than copying a subslice out:
/// the capture thread already copied it out of the driver's ring, and a second
/// copy is not free (a measured RSC/LRO frame was 48 870 bytes). The trade is
/// that `payload.capacity()` stays the whole frame's; see [`Segment::payload`].
///
/// `None` when malformed or not TCP sent by `game_port`: only the side owning
/// that port can be the server. The kernel filter should already exclude the
/// other direction; this guards against a laxer one.
pub fn parse_segment(mut frame: Vec<u8>, game_port: NonZeroU16) -> Option<Segment> {
    let span = segment_span(&frame, game_port)?;
    // Neither call reallocates nor shrinks the buffer: one memmove down to
    // offset 0 replaces an allocation plus a memcpy.
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
/// from, so the frame can be borrowed for the decode and then trimmed by value
/// with no copy in between.
struct SegmentSpan {
    flow: FlowKey,
    seq: u32,
    syn: bool,
    payload: Range<usize>,
}

fn segment_span(bytes: &[u8], game_port: NonZeroU16) -> Option<SegmentSpan> {
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
        // ARP carries no shop stream. Named rather than wildcarded so a new
        // `NetSlice` variant fails to compile here instead of being silently
        // dropped; the enum is not `#[non_exhaustive]`, so that is possible.
        NetSlice::Arp(_) => return None,
    };

    let TransportSlice::Tcp(tcp) = sliced.transport? else {
        return None;
    };

    // Control packets carry no stream bytes, and reassembly never tears a
    // half-stream down on FIN. Only SYN, which anchors the baseline, and
    // data-bearing segments matter.
    if tcp.payload().is_empty() && !tcp.syn() {
        return None;
    }

    let src = SocketAddr::new(src_ip, tcp.source_port());
    let dst = SocketAddr::new(dst_ip, tcp.destination_port());

    if src.port() != game_port.get() {
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
/// subslice.
///
/// `etherparse` hands back subslices of the buffer it was given, so this
/// practically cannot fail; if it ever did, refusing the packet beats trimming
/// the frame to the wrong bytes.
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
    const GAME_PORT_NZ: NonZeroU16 = NonZeroU16::new(GAME_PORT).expect("3333 is not zero");

    /// Build an IPv4 TCP packet (IP layer down, no Ethernet) as raw bytes.
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
        let seg = parse_segment(bytes, GAME_PORT_NZ).expect("should parse");
        assert_eq!(seg.flow.server, SocketAddr::from((server.0, server.1)));
        assert_eq!(seg.flow.client, SocketAddr::from((client.0, client.1)));
        assert_eq!(seg.seq, 1000);
        assert_eq!(seg.payload, b"AB");
        assert!(!seg.syn);
    }

    #[test]
    fn the_payload_reuses_the_frame_allocation_instead_of_copying_out_of_it() {
        // `Vec` keeps its capacity across `truncate`/`drain`, so "same buffer"
        // is observable as "same capacity".
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
        let seg = parse_segment(bytes, GAME_PORT_NZ).expect("should parse");
        assert_eq!(seg.payload, b"AB");
        assert_eq!(seg.payload.capacity(), frame_capacity);
    }

    #[test]
    fn a_segment_sent_to_the_game_port_is_not_a_segment_at_all() {
        // Client -> server half of the connection above.
        let bytes = ipv4_tcp(
            ([192, 168, 1, 10], 51000),
            ([104, 116, 20, 111], GAME_PORT), // dst port == game_port
            2000,
            false,
            b"XY",
        );
        assert!(parse_segment(bytes, GAME_PORT_NZ).is_none());
    }

    #[test]
    fn pure_ack_is_dropped() {
        let bytes = ipv4_tcp(
            ([104, 116, 20, 111], GAME_PORT),
            ([192, 168, 1, 10], 51000),
            3000,
            false,
            b"",
        );
        assert!(parse_segment(bytes, GAME_PORT_NZ).is_none());
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
        let seg = parse_segment(bytes, GAME_PORT_NZ).expect("SYN should be kept");
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
        assert!(parse_segment(bytes, GAME_PORT_NZ).is_none());
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
        assert!(parse_segment(bytes[..bytes.len() / 2].to_vec(), GAME_PORT_NZ).is_none());
        assert!(parse_segment(b"not a packet at all".to_vec(), GAME_PORT_NZ).is_none());
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
        let seg = parse_segment(bytes, GAME_PORT_NZ).expect("should parse");
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
        let seg = parse_segment(bytes, GAME_PORT_NZ).expect("parse");
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg), b"AB");
    }

    /// A deterministic 64-bit xorshift, seeded so a failure found by the sweep
    /// below reproduces from the test source alone: no regressions file, no
    /// run-to-run CI variance.
    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn parse_segment_is_total_on_hostile_bytes() {
        // Capture is port-wide, so any host sending from `game_port` reaches
        // this function: its input is adversary-adjacent and must be total.
        // Risk points are `subslice_range`'s address arithmetic and the
        // `truncate`/`drain(..start)` pair, which panics if the range isn't
        // inside the frame.
        //
        // `proptest` was declined here too: the contract is "`None` or a
        // valid `Segment`, never panics", so there is no counterexample to
        // minimise, only a seed to re-run.
        let mut state = 0x5EED_1234_ABCD_0001_u64;

        // (a) Unstructured: pure garbage, every length from empty to past an
        //     IPv6 header.
        for len in 0..=80usize {
            for _ in 0..24 {
                let bytes: Vec<u8> = (0..len)
                    .map(|_| (xorshift(&mut state) >> 24) as u8)
                    .collect();
                if let Some(segment) = parse_segment(bytes, GAME_PORT_NZ) {
                    assert!(
                        segment.syn || !segment.payload.is_empty(),
                        "a data segment with no payload got through"
                    );
                }
            }
        }

        // (b) Structured: one byte corrupted, which is what walks the decoder
        //     deep enough to reach the trim. A corrupted length field could
        //     make `etherparse` hand back a slice the frame doesn't contain.
        let valid = ipv4_tcp(
            ([104, 116, 20, 111], GAME_PORT),
            ([192, 168, 1, 10], 51_000),
            1_000,
            false,
            b"PAYLOAD-BYTES",
        );
        let mut parsed = 0_u32;
        for index in 0..valid.len() {
            for _ in 0..32 {
                let mut mutated = valid.clone();
                mutated[index] = (xorshift(&mut state) >> 24) as u8;
                if let Some(segment) = parse_segment(mutated, GAME_PORT_NZ) {
                    parsed += 1;
                    assert!(segment.payload.len() <= valid.len());
                    assert_eq!(segment.flow.server.port(), GAME_PORT);
                }
            }
        }
        // A sweep that parses nothing proves nothing; tripwire on the
        // fixture, not the function.
        assert!(
            parsed > 100,
            "only {parsed} mutations parsed — sweep is inert"
        );

        // Truncation at every length: a real capture yields these on a snaplen cut.
        for cut in 0..valid.len() {
            let _ = parse_segment(valid[..cut].to_vec(), GAME_PORT_NZ);
        }
    }
}
