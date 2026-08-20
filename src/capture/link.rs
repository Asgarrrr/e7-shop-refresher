//! Link-layer stripping: a captured frame in, the IP packet inside it out.
//!
//! No `wpcap.dll` symbol and no raw pointer, so this is the seam that could be
//! cut without touching the soundness argument in `capture::pcap::sys` — and it
//! has been. This module sits outside the `pcap-backend` gate its only consumer
//! lives under, which is what lets its tests run in all six verification lanes
//! rather than the two that build a backend.
//!
//! That matters more here than anywhere else in the capture path, because this
//! is where the next bug will land: the VLAN path below is `⚠ Untested` against
//! real hardware, and a wrong strip length doesn't fail loudly — it hands
//! [`parse_segment`](crate::capture::parse_segment) bytes off by a few.
//!
//! The cost of being un-gated is the `dead_code` allow below: in a lane with no
//! `pcap-backend`, nothing outside this file's own tests refers to any of it.
//! The allow is written to apply only in those lanes, so an item that really
//! did go unused in the shipped build would still be caught.

// Measured, not assumed: without this, `cargo clippy --no-default-features
// --all-targets` reports twelve `never used` warnings here — every constant,
// both types, `ip_bytes` and `ethernet_payload_offset` — because the lib target
// compiles without `#[cfg(test)]`, and `capture::pcap` is the only non-test
// caller any of them has.
//
// `cfg_attr` rather than a bare `#![allow]` so the silence is scoped to exactly
// the lanes where the gap is real. In a build that *does* enable the backend,
// dead code in this file means a genuine mistake and still fails the lane.
#![cfg_attr(
    not(all(windows, feature = "pcap-backend")),
    allow(
        dead_code,
        reason = "capture::pcap is the only consumer and it is feature-gated; this module deliberately is not"
    )
)]

use std::ffi::c_int;

// Link types this module knows how to strip. Anything else is skipped with a
// named log line rather than guessed at — see module doc for why.
const DLT_NULL: c_int = 0;
const DLT_EN10MB: c_int = 1;
const DLT_RAW: c_int = 12;
/// Some libpcap builds number "raw IP, no link layer" differently.
const DLT_RAW_ALT: c_int = 101;

/// How to get from a captured frame to the IP packet inside it.
///
/// Chosen per device from `pcap_datalink()`, never hardcoded: one machine hands
/// out Ethernet framing on a NIC, a four-byte pseudo-header on loopback, and
/// bare IP on some VPN interfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LinkStrip {
    /// `DLT_EN10MB`: 14 bytes, plus 4 for each VLAN tag.
    Ethernet,
    /// A header of fixed size with no optional parts — `DLT_NULL` (4) or
    /// `DLT_RAW` (0).
    Fixed(usize),
}

/// 802.1Q tag protocol identifier.
const TPID_8021Q: u16 = 0x8100;
/// 802.1ad ("`QinQ`") service tag protocol identifier.
const TPID_8021AD: u16 = 0x88A8;
/// Bytes of Ethernet header before the `EtherType` field.
const ETHERTYPE_OFFSET: usize = 12;
/// Stacked VLAN tags tolerated before giving up: two covers 802.1ad's outer
/// tag plus an inner 802.1Q one, as deep as a consumer machine will produce.
const MAX_VLAN_TAGS: usize = 2;

/// A link type this module cannot strip to an IP packet. Carries the raw `DLT`
/// value so the caller can name it in the reason it logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct UnsupportedDatalink(pub(super) c_int);

impl TryFrom<c_int> for LinkStrip {
    type Error = UnsupportedDatalink;

    /// `Err` for a link type this module cannot decode — the caller must skip the
    /// device rather than guess a length.
    fn try_from(datalink: c_int) -> Result<Self, Self::Error> {
        match datalink {
            DLT_EN10MB => Ok(Self::Ethernet),
            DLT_NULL => Ok(Self::Fixed(4)),
            DLT_RAW | DLT_RAW_ALT => Ok(Self::Fixed(0)),
            other => Err(UnsupportedDatalink(other)),
        }
    }
}

impl LinkStrip {
    /// The IP packet inside `frame`, or `None` if the frame is too short or
    /// carries a link header this cannot see past.
    pub(super) fn ip_bytes<'a>(&self, frame: &'a [u8]) -> Option<&'a [u8]> {
        match self {
            Self::Fixed(len) => frame.get(*len..),
            Self::Ethernet => frame.get(ethernet_payload_offset(frame)?..),
        }
    }
}

/// Where the IP packet starts inside an Ethernet frame, accounting for VLAN
/// tags.
///
/// A tagged frame pushes `EtherType` four bytes further per tag, so a fixed
/// 14-byte strip would hand `parse_segment` the tag stack's last four bytes
/// followed by the IP header.
///
/// ⚠ **Untested.** The measured machine has `VlanSupport=0`, so no tagged frame
/// was ever observed; this exists so a player who does run tagged VLANs doesn't
/// see a silent parse failure. Symptom if broken: `unparsed` climbing in
/// lockstep with `delivered` — visible in the window's capture-health row
/// (`ui::capture_health`) as "traffic is being captured, but none of it looks
/// like the game's", without anyone needing a debug build to see it.
fn ethernet_payload_offset(frame: &[u8]) -> Option<usize> {
    let mut at = ETHERTYPE_OFFSET;
    // `<=` so `MAX_VLAN_TAGS` tags are accepted and the (MAX+1)-th falls
    // through to `None`.
    for _ in 0..=MAX_VLAN_TAGS {
        let field = frame.get(at..at + 2)?;
        let ethertype = u16::from_be_bytes([field[0], field[1]]);
        if ethertype != TPID_8021Q && ethertype != TPID_8021AD {
            return Some(at + 2);
        }
        at += 4;
    }
    None
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;

    use super::*;
    use crate::capture::parse_segment;

    /// Builds an Ethernet frame with `tags` VLAN tags in front of an IPv4
    /// `EtherType`, followed by `payload`.
    fn ethernet_frame(tags: &[u16], payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![0xAAu8; ETHERTYPE_OFFSET];
        for tag in tags {
            frame.extend_from_slice(&tag.to_be_bytes());
            frame.extend_from_slice(&[0x00, 0x64]); // priority/VID, unread
        }
        frame.extend_from_slice(&0x0800u16.to_be_bytes()); // IPv4
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn every_link_type_this_backend_accepts_maps_to_its_own_strip_length() {
        assert_eq!(LinkStrip::try_from(DLT_EN10MB), Ok(LinkStrip::Ethernet));
        assert_eq!(LinkStrip::try_from(DLT_NULL), Ok(LinkStrip::Fixed(4)));
        assert_eq!(LinkStrip::try_from(DLT_RAW), Ok(LinkStrip::Fixed(0)));
        assert_eq!(LinkStrip::try_from(DLT_RAW_ALT), Ok(LinkStrip::Fixed(0)));
    }

    #[test]
    fn an_unknown_link_type_yields_no_strip_so_the_device_is_skipped_rather_than_guessed_at() {
        // 105 is DLT_IEEE802_11 — real 802.11 framing, the case the ADR
        // rejected NIC capture over; must not be guessed.
        for datalink in [105, 127, 143, -1, 1000] {
            assert_eq!(
                LinkStrip::try_from(datalink),
                Err(UnsupportedDatalink(datalink)),
                "DLT {datalink}"
            );
        }
    }

    #[test]
    fn a_fixed_strip_removes_exactly_its_header_and_refuses_a_frame_shorter_than_it() {
        assert_eq!(
            LinkStrip::Fixed(0).ip_bytes(b"\x45raw ip"),
            Some(b"\x45raw ip".as_slice())
        );
        assert_eq!(
            LinkStrip::Fixed(4).ip_bytes(b"\x02\x00\x00\x00ip"),
            Some(b"ip".as_slice())
        );
        // A loopback header with nothing behind it is an empty packet, not a
        // failure; one byte short of the header is a failure.
        assert_eq!(
            LinkStrip::Fixed(4).ip_bytes(b"\x02\x00\x00\x00"),
            Some(b"".as_slice())
        );
        assert_eq!(LinkStrip::Fixed(4).ip_bytes(b"\x02\x00\x00"), None);
    }

    #[test]
    fn an_untagged_ethernet_frame_loses_exactly_fourteen_bytes() {
        let frame = ethernet_frame(&[], b"ip packet");
        assert_eq!(ethernet_payload_offset(&frame), Some(14));
        assert_eq!(
            LinkStrip::Ethernet.ip_bytes(&frame),
            Some(b"ip packet".as_slice())
        );
    }

    #[test]
    fn a_vlan_tag_pushes_the_ip_packet_four_bytes_further_along() {
        for tpid in [TPID_8021Q, TPID_8021AD] {
            let frame = ethernet_frame(&[tpid], b"ip packet");
            assert_eq!(ethernet_payload_offset(&frame), Some(18), "tpid {tpid:#x}");
            assert_eq!(
                LinkStrip::Ethernet.ip_bytes(&frame),
                Some(b"ip packet".as_slice()),
                "tpid {tpid:#x}"
            );
        }
    }

    #[test]
    fn a_double_tagged_qinq_frame_loses_both_tags() {
        let frame = ethernet_frame(&[TPID_8021AD, TPID_8021Q], b"ip packet");
        assert_eq!(ethernet_payload_offset(&frame), Some(22));
        assert_eq!(
            LinkStrip::Ethernet.ip_bytes(&frame),
            Some(b"ip packet".as_slice())
        );
    }

    #[test]
    fn a_vlan_stack_deeper_than_this_strips_is_refused_rather_than_mis_stripped() {
        let frame = ethernet_frame(&[TPID_8021AD, TPID_8021Q, TPID_8021Q], b"ip packet");
        assert_eq!(ethernet_payload_offset(&frame), None);
        assert_eq!(LinkStrip::Ethernet.ip_bytes(&frame), None);
    }

    #[test]
    fn an_ethernet_frame_too_short_to_hold_its_ethertype_is_refused() {
        assert_eq!(ethernet_payload_offset(&[0xAA; 13]), None);
        assert_eq!(LinkStrip::Ethernet.ip_bytes(&[0xAA; 13]), None);
        // Exactly fourteen bytes: a header and an empty packet, which is legal.
        assert_eq!(
            LinkStrip::Ethernet.ip_bytes(&ethernet_frame(&[], b"")),
            Some(b"".as_slice())
        );
    }

    #[test]
    fn the_stripped_bytes_of_a_real_ethernet_frame_parse_as_a_segment() {
        // A wrong strip length shows up here as `None`.
        use etherparse::PacketBuilder;

        const GAME_PORT: u16 = 3333;
        let game_port = NonZeroU16::new(GAME_PORT).expect("3333 is not zero");
        let builder = PacketBuilder::ipv4([104, 116, 20, 111], [192, 168, 1, 10], 64)
            .tcp(GAME_PORT, 51_000, 1000, 64_240);
        let mut packet = Vec::with_capacity(builder.size(2));
        builder.write(&mut packet, b"AB").expect("write packet");

        let frame = ethernet_frame(&[], &packet);
        let ip = LinkStrip::Ethernet.ip_bytes(&frame).expect("strip");
        let segment = parse_segment(ip.to_vec(), game_port).expect("parse");
        assert_eq!(segment.seq, 1000);
        assert_eq!(segment.payload, b"AB");
    }
}
