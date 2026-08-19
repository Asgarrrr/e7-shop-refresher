//! Segment builders shared by `pressure`, `ingest` and `reassembly` tests: each
//! needs to mint a [`Segment`], and duplicating the flow key risks the copies
//! drifting apart. Everything else stays beside the tests that use it.

use std::net::{Ipv4Addr, SocketAddr};

use crate::capture::{FlowKey, Segment};

pub(super) fn initial_anchor_segment(seq: u32, payload: &[u8]) -> Segment {
    initial_anchor_segment_in(
        FlowKey {
            client: SocketAddr::from((Ipv4Addr::new(192, 168, 1, 10), 51000)),
            server: SocketAddr::from((Ipv4Addr::new(104, 116, 20, 111), 3333)),
        },
        seq,
        false,
        payload,
    )
}

pub(super) fn initial_anchor_segment_in(
    flow: FlowKey,
    seq: u32,
    syn: bool,
    payload: &[u8],
) -> Segment {
    Segment {
        flow,
        seq,
        syn,
        payload: Vec::from(payload),
    }
}

pub(super) fn segment_with_capacity(seq: u32, len: usize, capacity: usize) -> Segment {
    let mut payload = Vec::with_capacity(capacity);
    payload.resize(len, b'X');
    let mut segment = initial_anchor_segment(seq, &[]);
    segment.payload = payload;
    segment
}
