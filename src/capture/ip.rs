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
        fin: span.fin,
        rst: span.rst,
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
    fin: bool,
    rst: bool,
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
            // A header that declares no payload length hands length authority
            // to the *captured slice*: `Ipv6Slice::from_slice` takes the
            // `0 == header.payload_length() && slice.len() > Ipv6Header::LEN`
            // branch at etherparse-0.20.3/src/net/ipv6_slice.rs:35 and records
            // `LenSource::Slice`, so every byte after the 40-byte header comes
            // back as TCP payload — an Ethernet trailer, or the next segment's
            // headers inside a coalesced RSC/LRO superframe. IPv4's equivalent
            // (a lying `total_length`) fails closed and at worst leaves a
            // detectable gap; this one splices foreign bytes into the stream
            // silently. Reverting this `return` makes
            // `an_ipv6_packet_declaring_no_payload_length_is_refused` below
            // report a payload of `[65, 66, 84, 82, 65, 73, 76, 69, 82]`
            // ("ABTRAILER") for a segment whose header carried two bytes.
            //
            // Refusing costs this capture nothing observable. With
            // `payload_length == 0` and nothing past the header, that same
            // branch is not taken, the payload is empty, and `sliced.transport?`
            // below already refused the packet for having no TCP header. The
            // only other legitimate sender of a zero here is an RFC 2675
            // jumbogram, which by definition carries more than 65 535 payload
            // bytes and therefore needs a path MTU above 65 575 end to end.
            // The filter is `tcp and src port <game_port>` on a game connection
            // from a residential client to a remote server; that path's MTU is
            // 1500. The one thing that does hand this function a buffer larger
            // than an MTU is receive-side coalescing, and there the NIC rewrites
            // the header of a frame it built itself — the largest measured here
            // was 48 870 bytes (see `parse_segment` above), well inside a `u16`.
            if header.payload_length() == 0 {
                return None;
            }
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

    // Control packets carry no stream bytes, but three of them still say
    // something reassembly acts on: SYN anchors the baseline, and FIN and RST
    // end the connection, which is what lets `stream::Reassembler` give the
    // flow's slot back instead of holding it until the table needs it. FIN and
    // RST used to be refused here, and that refusal was the whole reason a
    // closed connection was never retired: one short connection every ~1.7 s
    // filled all 64 slots with the game's own dead clones inside two minutes,
    // and a patched build then logged 46 evictions in ~90 s.
    //
    // A pure ACK is the one control packet that says nothing either way, and it
    // is still dropped here. It is now *all* of what the field measured as 224
    // unparsed frames per 1000 delivered: that ratio was this ACK plus the RST
    // below it, two of the nine frames a server sends per shop request, and the
    // RST has since moved to the admitted side. One frame per request is the
    // whole remaining cost, which is why the kernel filter was left alone rather
    // than taught to test for a payload — see `super::CaptureCounters`.
    if tcp.payload().is_empty() && !tcp.syn() && !tcp.fin() && !tcp.rst() {
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
        fin: tcp.fin(),
        rst: tcp.rst(),
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

    /// Documentation-range addresses (RFC 3849), one per side.
    const IPV6_SERVER: [u8; 16] = [
        0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
    ];
    const IPV6_CLIENT: [u8; 16] = [
        0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02,
    ];

    /// The three control bits [`parse_segment`] reads, so a test names the one
    /// it means rather than passing three positional booleans in the right
    /// order.
    #[derive(Clone, Copy)]
    struct Flags {
        syn: bool,
        fin: bool,
        rst: bool,
    }

    impl Flags {
        const DATA: Self = Self {
            syn: false,
            fin: false,
            rst: false,
        };
        const SYN: Self = Self {
            syn: true,
            fin: false,
            rst: false,
        };
        const FIN: Self = Self {
            syn: false,
            fin: true,
            rst: false,
        };
        const RST: Self = Self {
            syn: false,
            fin: false,
            rst: true,
        };
    }

    /// Build an IPv4 TCP packet (IP layer down, no Ethernet) as raw bytes.
    fn ipv4_tcp(
        src: ([u8; 4], u16),
        dst: ([u8; 4], u16),
        seq: u32,
        flags: Flags,
        payload: &[u8],
    ) -> Vec<u8> {
        let b = PacketBuilder::ipv4(src.0, dst.0, 64).tcp(src.1, dst.1, seq, 64_240);
        write_tcp(b, flags, payload)
    }

    /// Build an IPv6 TCP packet (IP layer down, no Ethernet) as raw bytes.
    fn ipv6_tcp(
        src: ([u8; 16], u16),
        dst: ([u8; 16], u16),
        seq: u32,
        flags: Flags,
        payload: &[u8],
    ) -> Vec<u8> {
        let b = PacketBuilder::ipv6(src.0, dst.0, 64).tcp(src.1, dst.1, seq, 64_240);
        write_tcp(b, flags, payload)
    }

    fn write_tcp(
        mut b: etherparse::PacketBuilderStep<etherparse::TcpHeader>,
        flags: Flags,
        payload: &[u8],
    ) -> Vec<u8> {
        if flags.syn {
            b = b.syn();
        }
        if flags.fin {
            b = b.fin();
        }
        if flags.rst {
            b = b.rst();
        }
        let mut out = Vec::with_capacity(b.size(payload.len()));
        b.write(&mut out, payload).expect("write packet");
        out
    }

    #[test]
    fn a_segment_sent_by_the_game_port_is_parsed_and_names_both_endpoints() {
        let server = ([104, 116, 20, 111], GAME_PORT); // src port == game_port
        let client = ([192, 168, 1, 10], 51000);
        let bytes = ipv4_tcp(server, client, 1000, Flags::DATA, b"AB");
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
            Flags::DATA,
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
            Flags::DATA,
            b"XY",
        );
        assert!(parse_segment(bytes, GAME_PORT_NZ).is_none());
    }

    /// The one zero-payload control packet still refused here, and now the only
    /// thing `unparsed` counts besides an undecodable frame.
    #[test]
    fn pure_ack_is_dropped() {
        let bytes = ipv4_tcp(
            ([104, 116, 20, 111], GAME_PORT),
            ([192, 168, 1, 10], 51000),
            3000,
            Flags::DATA,
            b"",
        );
        assert!(parse_segment(bytes, GAME_PORT_NZ).is_none());
    }

    /// A bare FIN and a bare RST reach reassembly, which is the whole of the
    /// root-cause fix: with them refused here, a closed connection kept its slot
    /// in the stream table until something else needed it, and the game opening
    /// one short connection every ~1.7 s filled all 64 slots with its own dead
    /// clones inside two minutes.
    #[test]
    fn a_close_reaches_reassembly_even_carrying_no_bytes() {
        for (flags, expected_fin, expected_rst) in
            [(Flags::FIN, true, false), (Flags::RST, false, true)]
        {
            let bytes = ipv4_tcp(
                ([104, 116, 20, 111], GAME_PORT),
                ([192, 168, 1, 10], 51000),
                3100,
                flags,
                b"",
            );
            let seg = parse_segment(bytes, GAME_PORT_NZ).expect("a close must be kept");
            assert_eq!(seg.fin, expected_fin);
            assert_eq!(seg.rst, expected_rst);
            assert!(!seg.syn);
            assert!(seg.payload.is_empty());
        }
    }

    /// A FIN riding the last data segment — the ordinary shape of an orderly
    /// close — keeps both its bytes and its flag.
    #[test]
    fn a_data_bearing_fin_keeps_its_payload_and_its_flag() {
        let bytes = ipv4_tcp(
            ([104, 116, 20, 111], GAME_PORT),
            ([192, 168, 1, 10], 51000),
            3200,
            Flags::FIN,
            b"AB",
        );
        let seg = parse_segment(bytes, GAME_PORT_NZ).expect("should parse");
        assert!(seg.fin);
        assert_eq!(seg.payload, b"AB");
    }

    /// The direction guard is upstream of the flag test, so the client's own
    /// close is still not a segment: only the server's half is reassembled.
    #[test]
    fn a_close_sent_to_the_game_port_is_still_not_a_segment() {
        let bytes = ipv4_tcp(
            ([192, 168, 1, 10], 51000),
            ([104, 116, 20, 111], GAME_PORT),
            3300,
            Flags::FIN,
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
            Flags::SYN,
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
            Flags::DATA,
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
            Flags::DATA,
            b"AB",
        );
        assert!(parse_segment(bytes[..bytes.len() / 2].to_vec(), GAME_PORT_NZ).is_none());
        assert!(parse_segment(b"not a packet at all".to_vec(), GAME_PORT_NZ).is_none());
    }

    #[test]
    fn ipv6_data_is_parsed() {
        let bytes = ipv6_tcp(
            (IPV6_SERVER, GAME_PORT),
            (IPV6_CLIENT, 51000),
            7000,
            Flags::DATA,
            b"AB",
        );
        let seg = parse_segment(bytes, GAME_PORT_NZ).expect("should parse");
        assert_eq!(seg.seq, 7000);
        assert_eq!(seg.payload, b"AB");
    }

    /// Fixed IPv6 header size, and the offset of its `payload_length` field,
    /// per RFC 8200 §3 — spelled out because these tests edit the field by
    /// hand, which is the only way to build a packet `PacketBuilder` refuses
    /// to emit.
    const IPV6_HEADER_LEN: usize = 40;
    const IPV6_PAYLOAD_LENGTH_AT: Range<usize> = 4..6;

    /// The length authority for an IPv6 payload is the header, not the bytes
    /// that happened to be captured. A trailer past the declared length — an
    /// Ethernet pad, or the next segment of a coalesced RSC frame — must not
    /// reach reassembly.
    ///
    /// Asserts the exact payload rather than a bound on its length: the
    /// failure this guards is bytes being *added*, and "no longer than the
    /// frame" cannot see that.
    #[test]
    fn ipv6_stops_the_payload_where_the_header_says_it_ends() {
        let mut bytes = ipv6_tcp(
            (IPV6_SERVER, GAME_PORT),
            (IPV6_CLIENT, 51000),
            7100,
            Flags::DATA,
            b"AB",
        );
        bytes.extend_from_slice(b"TRAILER");
        let seg = parse_segment(bytes, GAME_PORT_NZ).expect("should parse");
        assert_eq!(seg.payload, b"AB");
    }

    /// The escape hatch at `etherparse-0.20.3/src/net/ipv6_slice.rs:35`: with
    /// `payload_length == 0` and anything at all behind the header, etherparse
    /// stops believing the header and takes the captured slice as the packet.
    /// Everything trailing then arrives as stream bytes, undetectably.
    #[test]
    fn an_ipv6_packet_declaring_no_payload_length_is_refused() {
        let mut bytes = ipv6_tcp(
            (IPV6_SERVER, GAME_PORT),
            (IPV6_CLIENT, 51000),
            7200,
            Flags::DATA,
            b"AB",
        );
        // Tripwire on the fixture: without an honest length here first, zeroing
        // the field would prove nothing about what the field controls.
        let declared = u16::from_be_bytes([
            bytes[IPV6_PAYLOAD_LENGTH_AT.start],
            bytes[IPV6_PAYLOAD_LENGTH_AT.start + 1],
        ]);
        assert_eq!(
            usize::from(declared),
            bytes.len() - IPV6_HEADER_LEN,
            "fixture should declare its whole payload before we zero the field"
        );

        bytes[IPV6_PAYLOAD_LENGTH_AT].fill(0);
        bytes.extend_from_slice(b"TRAILER");
        assert!(
            parse_segment(bytes, GAME_PORT_NZ).is_none(),
            "a packet that declares no payload length must be refused, not \
             measured against the bytes that happened to be captured"
        );
    }

    #[test]
    fn parsed_segment_flows_through_reassembler() {
        use crate::stream::Reassembler;
        let bytes = ipv4_tcp(
            ([104, 116, 20, 111], GAME_PORT),
            ([192, 168, 1, 10], 51000),
            1000,
            Flags::DATA,
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
                        segment.syn || segment.fin || segment.rst || !segment.payload.is_empty(),
                        "a segment with neither payload nor a control bit got through"
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
            Flags::DATA,
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
