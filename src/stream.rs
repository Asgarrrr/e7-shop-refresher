//! TCP reassembly.
//!
//! WinDivert operates below TCP, so segments may arrive out of order,
//! duplicated (retransmissions), or overlapping. This layer reconstructs, per
//! half-stream, the ordered byte stream the TCP stack would deliver — which is
//! what the analysis server expects.
//!
//! All work is done in *relative offsets* from the stream origin (the first
//! observed segment). TCP sequence numbers are `u32` and wrap; a segment's
//! offset is derived from its distance to the *currently expected* byte, not
//! to the fixed origin, so the signed `i32` sequence window tracks the stream
//! as it advances. Anchoring the distance to the origin instead would break
//! once a half-stream delivered 2 GiB: the distance would exceed `i32` range
//! and every later segment would look like an already-delivered retransmission.

use std::collections::{BTreeMap, HashMap};

use crate::capture::{Direction, FlowKey, Segment};

/// Cap on out-of-order bytes buffered per half-stream (memory guard).
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;

/// Reassembles traffic from several connections, keyed by (flow, direction).
#[derive(Default)]
pub struct Reassembler {
    halves: HashMap<(FlowKey, Direction), HalfStream>,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Integrates a segment and returns the newly contiguous (ordered) bytes.
    ///
    /// Returns an empty vector when the segment is a duplicate, partially fills
    /// a gap, or still waits on a missing segment. The half-stream is never torn
    /// down on FIN: a reordered FIN arriving before a gap-filling segment must
    /// not discard already-buffered data.
    pub fn push(&mut self, segment: &Segment) -> Vec<u8> {
        let half = self
            .halves
            .entry((segment.flow, segment.direction))
            .or_default();
        half.push(segment.seq, segment.syn, &segment.payload)
    }

    /// Resets all state so the next segment of each flow re-anchors a new
    /// origin. Used after a Shop Watch pause to restart from a clean resync
    /// point rather than a stale `next_off`.
    pub fn clear(&mut self) {
        self.halves.clear();
    }
}

/// Reassembly state of one direction of a connection, in relative offsets.
#[derive(Default)]
struct HalfStream {
    /// Stream origin (sequence number of the first byte); `None` until first seen.
    baseline: Option<u32>,
    /// Offset (from `baseline`) of the next expected byte.
    next_off: i64,
    /// Buffered future segments, keyed by offset (monotonic order, no wrap).
    pending: BTreeMap<i64, Vec<u8>>,
    pending_bytes: usize,
}

impl HalfStream {
    fn push(&mut self, seq: u32, syn: bool, payload: &[u8]) -> Vec<u8> {
        // SYN consumes a sequence number: data starts at seq + 1.
        let data_seq = if syn { seq.wrapping_add(1) } else { seq };
        self.baseline.get_or_insert(data_seq);
        // Measure from the currently expected byte, then shift back to an
        // absolute offset. The distance stays within the TCP window (small),
        // so the i32 span in `seq_diff` never overflows however far the stream
        // has advanced.
        let expected_seq = self.expected_seq();
        let offset = self.next_off + seq_diff(data_seq, expected_seq);

        let mut out = Vec::new();
        self.absorb(offset, payload, &mut out);
        self.drain(&mut out);
        out
    }

    /// Integrates one segment: in order (append), future (buffer), or old (trim).
    fn absorb(&mut self, offset: i64, payload: &[u8], out: &mut Vec<u8>) {
        if payload.is_empty() {
            return;
        }
        if offset > self.next_off {
            self.buffer_future(offset, payload);
            return;
        }

        // offset <= next_off: the segment starts at or before the expected byte.
        let already = (self.next_off - offset) as usize;
        if already < payload.len() {
            out.extend_from_slice(&payload[already..]);
            self.next_off += (payload.len() - already) as i64;
        }
        // else: fully delivered already (retransmission) — ignored.
    }

    fn buffer_future(&mut self, offset: i64, payload: &[u8]) {
        // Keep only the largest segment seen at a given offset.
        if self
            .pending
            .get(&offset)
            .is_none_or(|v| v.len() < payload.len())
        {
            if let Some(old) = self.pending.insert(offset, payload.to_vec()) {
                self.pending_bytes -= old.len();
            }
            self.pending_bytes += payload.len();
        }
        self.relieve_pressure();
    }

    /// Flushes buffered segments that became contiguous once `next_off` advanced.
    fn drain(&mut self, out: &mut Vec<u8>) {
        while let Some((&offset, _)) = self.pending.first_key_value() {
            if offset > self.next_off {
                break; // gap still present.
            }
            let (offset, payload) = self.pending.pop_first().expect("peeked above");
            self.pending_bytes -= payload.len();
            self.absorb(offset, &payload, out);
        }
    }

    /// Sequence number of the next expected byte: `baseline + next_off`, back
    /// in the wrapping `u32` space. `baseline` is always set by the time this
    /// runs (`push` inserts it first).
    fn expected_seq(&self) -> u32 {
        self.baseline
            .unwrap_or(0)
            .wrapping_add(self.next_off as u32)
    }

    /// Under memory pressure, *give up on the current gap*: jump `next_off` to
    /// the nearest buffered segment, which then becomes deliverable (the next
    /// `drain` flushes it). A byte missed out-of-order by a passive tap is never
    /// retransmitted — a discontinuity the server resyncs on beats a permanently
    /// stalled stream.
    fn relieve_pressure(&mut self) {
        if self.pending_bytes <= MAX_PENDING_BYTES {
            return;
        }
        if let Some((&offset, _)) = self.pending.first_key_value()
            && offset > self.next_off
        {
            self.next_off = offset;
        }
    }
}

/// Signed distance `a - b` over the circular sequence-number space.
fn seq_diff(a: u32, b: u32) -> i64 {
    (a.wrapping_sub(b) as i32) as i64
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};

    use super::*;

    fn flow() -> FlowKey {
        FlowKey {
            client: SocketAddr::from((Ipv4Addr::new(192, 168, 1, 10), 51000)),
            server: SocketAddr::from((Ipv4Addr::new(104, 116, 20, 111), 3333)),
        }
    }

    fn seg(seq: u32, syn: bool, fin: bool, payload: &[u8]) -> Segment {
        Segment {
            flow: flow(),
            direction: Direction::ServerToClient,
            seq,
            syn,
            fin,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn in_order() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, false, b"AB")), b"AB");
        assert_eq!(r.push(&seg(1002, false, false, b"CD")), b"CD");
    }

    #[test]
    fn reordering_flushes_multiple_buffered_segments() {
        let mut r = Reassembler::new();
        // Baseline is set by the first observed segment.
        assert_eq!(r.push(&seg(1000, false, false, b"AB")), b"AB");
        // Two future segments arrive out of order: nothing deliverable yet.
        assert!(r.push(&seg(1006, false, false, b"GH")).is_empty());
        assert!(r.push(&seg(1004, false, false, b"EF")).is_empty());
        // Filling the gap flushes everything buffered, in order.
        assert_eq!(r.push(&seg(1002, false, false, b"CD")), b"CDEFGH");
    }

    #[test]
    fn retransmission_is_ignored() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, false, b"AB")), b"AB");
        assert!(r.push(&seg(1000, false, false, b"AB")).is_empty());
    }

    #[test]
    fn overlapping_segment_keeps_only_fresh_tail() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, false, b"ABCD")), b"ABCD");
        // Overlaps "CD" (already seen) and brings "EF".
        assert_eq!(r.push(&seg(1002, false, false, b"CDEF")), b"EF");
    }

    #[test]
    fn syn_sets_the_baseline() {
        let mut r = Reassembler::new();
        // The SYN (seq 999, no data) anchors the origin at 1000.
        assert!(r.push(&seg(999, true, false, b"")).is_empty());
        assert_eq!(r.push(&seg(1000, false, false, b"AB")), b"AB");
    }

    #[test]
    fn gap_filled_out_of_order() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, false, b"AB")), b"AB");
        assert!(r.push(&seg(1004, false, false, b"EF")).is_empty()); // gap.
        assert_eq!(r.push(&seg(1002, false, false, b"CD")), b"CDEF");
    }

    #[test]
    fn fin_does_not_discard_buffered_data() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, false, b"AB")), b"AB");
        // A reordered FIN, ahead of a gap, must not drop its payload.
        assert!(r.push(&seg(1004, false, true, b"EF")).is_empty());
        // The gap-filling segment flushes the FIN's data too.
        assert_eq!(r.push(&seg(1002, false, false, b"CD")), b"CDEF");
    }

    #[test]
    fn reassembles_across_sequence_wrap() {
        let mut r = Reassembler::new();
        // Baseline just before the u32 sequence space wraps.
        assert_eq!(r.push(&seg(0xFFFF_FFFE, false, false, b"AB")), b"AB");
        // The next segment is at 0x0000_0000 (wrap): still contiguous.
        assert_eq!(r.push(&seg(0x0000_0000, false, false, b"CD")), b"CD");
    }

    #[test]
    fn reordering_across_wrap_is_ordered_correctly() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(0xFFFF_FFFE, false, false, b"AB")), b"AB");
        // A post-wrap future segment is buffered, then the gap is filled.
        assert!(r.push(&seg(0x0000_0002, false, false, b"EF")).is_empty());
        assert_eq!(r.push(&seg(0x0000_0000, false, false, b"CD")), b"CDEF");
    }

    #[test]
    fn delivers_far_past_two_gigabytes() {
        // A half-stream that has already delivered 2^31 bytes: the next
        // in-order segment must still be recognised, not dropped as a phantom
        // retransmission (the old origin-anchored offset overflowed i32 here).
        let mut half = HalfStream {
            baseline: Some(0),
            next_off: (1i64 << 31) + 1000,
            ..Default::default()
        };
        let expected = half.expected_seq();
        assert_eq!(half.push(expected, false, b"AB"), b"AB");
        // And the following contiguous segment keeps flowing.
        assert_eq!(half.push(expected.wrapping_add(2), false, b"CD"), b"CD");
    }

    #[test]
    fn clear_resets_baseline_for_resync() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, false, b"AB")), b"AB");
        // After a pause the state is wiped: a far-ahead segment becomes a new
        // origin instead of being buffered forever.
        r.clear();
        assert_eq!(r.push(&seg(9000, false, false, b"XY")), b"XY");
    }
}
