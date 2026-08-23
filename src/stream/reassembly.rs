//! Sequence-space reassembly: ordering, deduplication, gaps, SYN incarnations,
//! and the one-shot initial anchor burst. The relative-offset rule is on the
//! parent module. This half decides *what* to buffer; [`super::budget`] decides
//! *if there is room*.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap};

use tracing::{error, warn};

use super::budget::{
    BudgetedChunk, BudgetedSegment, PipelineBudget, fits_pending, pending_after_release,
};
use super::{INITIAL_ANCHOR_MAX_BYTES, INITIAL_ANCHOR_MAX_SEGMENTS, ResyncCause};
use crate::capture::FlowKey;
#[cfg(test)]
use crate::capture::Segment;

/// Cap on tracked streams: reconnections or a port-wide flood could mint keys
/// without bound, each buffering up to `MAX_PENDING_BYTES`. Stalest entry evicted once reached.
///
/// Left at 64 now that a flow is retired when its connection ends
/// ([`Reassembler::push_budgeted`]). It was reconsidered, because until that
/// landed this number *was* the steady state rather than a ceiling: the game
/// opens a short connection every ~1.7 s, nothing ever removed the dead one, and
/// 64 slots of its own clones filled inside two minutes, after which every new
/// connection evicted one — 46 of them in ~90 s in the field. What the cap
/// bounds now is the residue, flows whose end was never sent or never captured,
/// plus whatever a forged-source-port flood mints. Nothing measures how large
/// that residue is, so there is no number to move it *to*; the reading that
/// would justify moving it is `ResyncCause::StreamReclaimed` climbing again.
const MAX_STREAMS: usize = 64;

/// Segments held during the one-shot initial anchor window.
///
/// Ordering is isolated per flow: each flow's slots are replaced with its
/// sequence-ordered segments, which preserves inter-flow cadence and leaves
/// [`Reassembler`] the sole authority for overlap, dedup, gaps and SYNs.
pub(crate) struct InitialBurst {
    segments: Vec<BudgetedSegment>,
    payload_bytes: usize,
}

impl InitialBurst {
    pub(crate) fn new() -> Self {
        Self {
            segments: Vec::new(),
            payload_bytes: 0,
        }
    }

    pub(crate) fn would_exceed(&self, segment: &BudgetedSegment) -> bool {
        self.segments.len() >= INITIAL_ANCHOR_MAX_SEGMENTS
            || self
                .payload_bytes
                .checked_add(segment.payload().len())
                .is_none_or(|bytes| bytes > INITIAL_ANCHOR_MAX_BYTES)
    }

    /// Admits `segment` into the burst.
    ///
    /// # Panics
    ///
    /// Panics unless [`Self::would_exceed`] was checked first.
    pub(crate) fn push(&mut self, segment: BudgetedSegment) {
        assert!(
            !self.would_exceed(&segment),
            "initial anchor burst limits must be checked before insertion"
        );
        self.payload_bytes += segment.payload().len();
        self.segments.push(segment);
    }

    #[cfg(test)]
    fn push_test(&mut self, segment: Segment) {
        self.push(
            PipelineBudget::new()
                .admit_capture(segment)
                .expect("test segment fits the production capture quota"),
        );
    }

    /// Whether the burst has reached either cap. `>=` on both terms: the byte
    /// total rarely lands on 262 144 exactly, and equality alone would leave
    /// the bound resting on `would_exceed`.
    pub(crate) fn is_at_limit(&self) -> bool {
        self.segments.len() >= INITIAL_ANCHOR_MAX_SEGMENTS
            || self.payload_bytes >= INITIAL_ANCHOR_MAX_BYTES
    }

    pub(crate) fn into_ordered(self) -> Vec<BudgetedSegment> {
        // The arrival order of the *flows*, taken before the per-flow sort below
        // destroys it. The replay at the end reads it back to drop each flow's
        // sorted segments into that flow's own original slots; without it,
        // inter-flow cadence would be `HashMap` iteration order.
        //
        // The `collect` is load-bearing despite `clippy::needless_collect`: the
        // loop below moves `self.segments`, so a lazy iterator borrowing it
        // cannot still be alive at the replay. Taking the lint's advice is
        // `E0505: cannot move out of self.segments because it is borrowed`
        // (compiled, not guessed).
        let slots: Vec<_> = self.segments.iter().map(|segment| segment.flow).collect();
        // One flow is the ordinary case; several is the port-wide accident.
        let mut flows: HashMap<_, Vec<BudgetedSegment>> = HashMap::with_capacity(1);
        for segment in self.segments {
            flows.entry(segment.flow).or_default().push(segment);
        }
        for segments in flows.values_mut() {
            // `seq_diff` is not a transitive order on its own, so sort against
            // an origin: a valid TCP window is smaller than the signed
            // half-space, so every segment measures from it unambiguously.
            let origin = segments
                .iter()
                .map(segment_data_seq)
                .reduce(|earliest, candidate| {
                    if seq_diff(candidate, earliest) < 0 {
                        candidate
                    } else {
                        earliest
                    }
                })
                .expect("a burst flow is never empty");
            segments.sort_by_key(|segment| seq_diff(segment_data_seq(segment), origin));
            // Reversed so the replay below can `pop` from the back.
            segments.reverse();
        }

        slots
            .into_iter()
            .map(|key| {
                flows
                    .get_mut(&key)
                    .and_then(Vec::pop)
                    .expect("every burst slot has one segment")
            })
            .collect()
    }
}

/// Reassembles traffic from several connections, keyed by flow.
#[derive(Default)]
pub struct Reassembler {
    streams: HashMap<FlowKey, HalfStream>,
    /// Monotonic activity stamp, bumped per segment; the eviction clock.
    clock: u64,
    /// Flows whose connection has ended, and where each one ended.
    ///
    /// Retiring a flow frees its `baseline` and `next_off` — which are also the
    /// only things that make a second copy of a segment recognisable as history.
    /// `capture::pcap` opens *every* adapter and justifies it by saying this
    /// module dedupes by sequence number, so a machine with two adapters on the
    /// same traffic sees every segment twice; before this map, the duplicate of
    /// a FIN *carrying payload* — the ordinary way the game's connections end —
    /// re-anchored a fresh stream and delivered those bytes a second time. The
    /// game opens a connection every ~1.7s, so that was a corrupted byte stream
    /// at the end of each one, on the default configuration.
    ///
    /// One `u32` per dead flow, capped at [`MAX_STREAMS`] and evicted stalest
    /// first, which is the same discipline `streams` itself follows.
    retired: HashMap<FlowKey, RetiredFlow>,
}

/// Where a retired flow ended: the sequence one past its last delivered byte,
/// and when it was retired, for eviction.
#[derive(Clone, Copy)]
struct RetiredFlow {
    end_seq: u32,
    at: u64,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Integrates a segment, returning the bytes that became contiguous.
    ///
    /// # Retiring a flow whose connection has ended
    ///
    /// A closed connection used to keep its slot until something else needed it:
    /// `capture::parse_segment` refused every zero-payload non-SYN segment, so
    /// FIN and RST never arrived here at all. The game opens a short connection
    /// every ~1.7 s (~7 segments, ~6.6 KB each), so all [`MAX_STREAMS`] slots
    /// held its own dead clones within about two minutes and every new
    /// connection then evicted one — 46 evictions in ~90 s in the field, each
    /// re-anchoring a flow that had nothing left to deliver. Both flags now reach
    /// here, and a flow is dropped the moment its connection is over.
    ///
    /// The two ends are not the same event, so they are not handled alike:
    ///
    /// - **FIN** is orderly, and its guarantee is exactly what makes retirement
    ///   safe: it is the last sequence number the server will use, so once
    ///   delivery is contiguous through it there is nothing left to wait for.
    ///   Until then the flow stays — a FIN that overtakes a gap must not be
    ///   allowed to throw away the bytes buffered behind it, which is the one
    ///   way this optimisation could silently cost a shop refresh.
    /// - **RST** is an abort and carries no such promise; it can arrive with a
    ///   gap still open, and nothing will ever fill it. The flow is dropped
    ///   anyway — an aborted connection cannot deliver another byte, so holding
    ///   its slot buys nothing — but if it was still holding a segment behind a
    ///   gap those bytes are lost, and that is counted
    ///   ([`ResyncCause::ConnectionReset`]).
    ///
    /// An RST is honoured only when it sits exactly at the next expected byte.
    /// A passive tap can check nothing else about a segment, and an off-window
    /// RST is precisely what an off-path attacker forges against a connection
    /// they cannot read; RFC 5961 §3.2 tightened real receivers to that same
    /// test for that same reason, and here it costs one comparison. An RST that
    /// fails it is ignored and its flow ages out the ordinary way.
    ///
    /// An orderly close records no re-anchor at all. A flow that delivered
    /// everything it received and then ended did not lose continuity — it
    /// stopped — and counting one per connection would trade the 46 spurious
    /// events this removes for about 35 a minute. An abort records one only when
    /// it stranded bytes, which is a loss rather than an ending.
    pub(crate) fn push_budgeted(&mut self, segment: BudgetedSegment) -> ReassemblyOutcome {
        let key = segment.flow;
        let dropped_capacity = segment.capacity();
        let budget = segment.budget();
        self.clock += 1;
        if segment.syn && self.syn_starts_new_incarnation(&segment) {
            self.streams.remove(&key);
        }
        if segment.syn {
            // A SYN is the one thing allowed to declare a new incarnation on a
            // reused four-tuple, so it also ends the old one's history: the
            // sequence space restarts and the retired bound no longer describes
            // anything.
            self.retired.remove(&key);
        }
        if !self.streams.contains_key(&key) {
            // A close for a flow nothing is tracking has nothing to end, and
            // admitting it would be worse than useless: it would anchor a
            // baseline on a connection already over, and could evict a live flow
            // to make room for one retired on the very next line. Nothing can
            // follow it either — a FIN is the last sequence number the server
            // will use, and an RST is the end of the connection outright — so
            // the entry could only ever be retired empty.
            if is_bare_close(&segment) {
                return ReassemblyOutcome::Chunks(Vec::new());
            }
            // History, not data. Without this a second adapter's copy of the
            // closing segment anchors a fresh stream on bytes already
            // delivered — see [`Reassembler::retired`].
            if self
                .retired
                .get(&key)
                .is_some_and(|ended| ends_at_or_before(&segment, ended.end_seq))
            {
                return ReassemblyOutcome::Chunks(Vec::new());
            }
            if self.streams.len() >= MAX_STREAMS {
                self.evict_stalest(&budget);
            }
        }
        let clock = self.clock;
        let half = self.streams.entry(key).or_default();
        half.last_active = clock;
        let outcome = half.push(
            segment.seq,
            segment.syn,
            segment.fin,
            segment.rst,
            segment.into_payload(),
        );
        // Read while the borrow is alive, acted on after it ends: `retire` needs
        // the map this `&mut` is holding. Read on the pressure path too and
        // discarded there — that path drops the flow anyway, and one field
        // comparison is cheaper than a second match to skip it.
        let retirement = half.retirement();
        match outcome {
            HalfOutcome::Chunks(chunks) => {
                if let Some(retirement) = retirement {
                    self.retire(&key, retirement, &budget);
                }
                ReassemblyOutcome::Chunks(chunks)
            }
            HalfOutcome::Pressure(cause) => {
                // Sized to the failure. `MAX_PENDING_BYTES` is a *per-stream*
                // cap and half of `REASSEMBLY_STAGE_BYTES`, so one stream fills
                // it with the shared pool still slack; capture is port-wide
                // (`tcp and src port …`, no host filter), so a stray flow can
                // pile 8 MiB behind one lost segment. Clearing every stream for
                // that cost the game's own flow its `baseline` and `next_off`
                // mid-message — a shop refresh — over a quota it never touched.
                // Shared-stage exhaustion is the opposite: the bytes that must
                // come back before *anyone* can buffer again are the other
                // streams' pending buffers, so the wide reset is the recovery.
                match cause {
                    PressureCause::Stream => {
                        self.streams.remove(&key);
                    }
                    PressureCause::Shared => self.clear(),
                }
                // Drop metrics identify only the packet that caused recovery;
                // chunks discarded by the reset above — one stream's or every
                // stream's — are collateral, not extra captures.
                budget.record_drop(dropped_capacity);
                budget.record_resync(cause.resync_cause());
                warn_reassembly_pressure(&budget, cause);
                ReassemblyOutcome::Pressure
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn push(&mut self, segment: &Segment) -> Vec<u8> {
        let admitted = PipelineBudget::new()
            .admit_capture(segment.clone())
            .expect("test segment fits the production capture quota");
        flatten_chunks(self.push_budgeted(admitted))
    }

    /// Whether this SYN starts a new incarnation of an already tracked
    /// connection, so the caller can drop the stale sequence space. Only the
    /// handshake SYN-ACK and its retransmissions reach here — the client's own
    /// SYN travels the uncaptured direction — and a SYN on an untracked flow
    /// simply anchors.
    fn syn_starts_new_incarnation(&self, segment: &BudgetedSegment) -> bool {
        debug_assert!(segment.syn);

        let Some(half) = self.streams.get(&segment.flow) else {
            return false;
        };
        // A retransmitted SYN carries the sequence number already anchored on.
        if half.syn_seq == Some(segment.seq) {
            return false;
        }
        // The handshake SYN arriving late, after data anchored this stream
        // mid-flight at exactly the byte that SYN would have produced.
        if half.syn_seq.is_none() && half.baseline == Some(segment.seq.wrapping_add(1)) {
            return false;
        }
        true
    }

    /// Drops the least-recently-active stream, only when a new key would exceed
    /// `MAX_STREAMS`.
    ///
    /// An eviction is counted only when it *cost* something, which [`EvictionLoss`]
    /// decides once and both the counter and the warning below then read.
    ///
    /// # What this path is, and what it stopped being
    ///
    /// The rationale that used to stand here described neither the filter nor
    /// what the code does. It claimed port-wide capture let foreign flows crowd
    /// the table and that the stalest flow between two shop refreshes is the
    /// game's own; the kernel filter admits one source port and `parse_segment`
    /// admits only that server, so there are no foreign flows, and the stalest
    /// entry is not the live flow gone quiet.
    ///
    /// What actually filled the table was that `parse_segment` discarded FIN and
    /// RST, so a closed connection was never retired: the game opening a short
    /// connection every ~1.7 s filled all 64 slots with its own dead clones
    /// inside two minutes, after which every new connection evicted one. A
    /// patched build logged 46 evictions in ~90 s, every one with
    /// `buffered_bytes=0`, and counting them all as anchor losses made
    /// `dominant_resync` name them and paint a healthy run amber — worse the
    /// longer the run went.
    ///
    /// Both halves of that are now fixed, in the order they should be read:
    /// [`EvictionLoss`] stopped calling a lossless eviction a fault, and
    /// [`Self::push_budgeted`] stopped producing them, by dropping a flow when
    /// its connection ends. This path is the backstop it was designed as again —
    /// for flows whose end never arrives — and an eviction on a healthy run is
    /// now rare enough to be worth reading rather than scrolling past.
    ///
    /// No `record_drop` in either case, for the reason [`Self::push_budgeted`]
    /// states.
    fn evict_stalest(&mut self, budget: &PipelineBudget) {
        let Some(&key) = self
            .streams
            .iter()
            .min_by_key(|(_, half)| half.last_active)
            .map(|(key, _)| key)
        else {
            return;
        };
        let evicted = self
            .streams
            .remove(&key)
            .expect("the key was just read out of this map");
        // `clock` counts segments across *all* flows, so this is how many other
        // packets went by while the stream said nothing: the one number that
        // separates "crowded out" from "went quiet".
        let segments_since_active = self.clock.saturating_sub(evicted.last_active);
        let loss = EvictionLoss::of(&evicted);
        budget.record_resync(loss.cause);
        warn_stream_evicted(
            budget,
            loss,
            evicted,
            key.client.port(),
            segments_since_active,
        );
    }

    /// Drops a flow whose connection has ended, freeing its slot.
    ///
    /// The mirror of [`Self::evict_stalest`] and deliberately quieter than it:
    /// eviction takes a slot from a flow that had not asked to give it up, and
    /// says so; this gives back a slot the connection no longer wants. The
    /// silence is the point — an orderly close happens about every 1.7 s, and a
    /// line per connection would bury the eviction warning that is now the
    /// signal something is wrong.
    ///
    /// The one loud case is an abort that stranded bytes. Those were received
    /// and charged, they are discarded here undelivered, and the gap in front of
    /// them can never be filled: same shape as [`ResyncCause::StreamEvicted`],
    /// different cause, because the stream table had nothing to do with it.
    /// Records where a flow ended, evicting the stalest bound when the map is
    /// full. Capped rather than aged: the duplicate this exists to catch is the
    /// same packet off a second adapter, which arrives within milliseconds, so
    /// holding [`MAX_STREAMS`] connections' worth of history — well over a
    /// minute at the game's ~1.7s cadence — is far more than the window needs.
    fn remember_retired(&mut self, key: FlowKey, end_seq: u32) {
        if self.retired.len() >= MAX_STREAMS
            && !self.retired.contains_key(&key)
            && let Some(stalest) = self
                .retired
                .iter()
                .min_by_key(|(_, flow)| flow.at)
                .map(|(key, _)| *key)
        {
            self.retired.remove(&stalest);
        }
        self.retired.insert(
            key,
            RetiredFlow {
                end_seq,
                at: self.clock,
            },
        );
    }

    fn retire(&mut self, key: &FlowKey, retirement: Retirement, budget: &PipelineBudget) {
        let retired = self
            .streams
            .remove(key)
            .expect("the flow was just pushed into through this same key");
        let delivered_bytes = retired.next_off;
        let stranded_bytes = retired.pending_bytes;
        // Taken before the drop below, which is what frees the only other copy
        // of this bound.
        self.remember_retired(*key, retired.expected_seq());
        // Before the snapshot below, so the pool total it prints is the one that
        // already has these bytes back — the rule `warn_stream_evicted` follows.
        drop(retired);
        match retirement {
            Retirement::Closed => {}
            Retirement::Aborted => {
                if stranded_bytes > 0 {
                    budget.record_resync(ResyncCause::ConnectionReset);
                    warn_connection_reset(
                        budget,
                        key.client.port(),
                        delivered_bytes,
                        stranded_bytes,
                    );
                }
            }
        }
    }

    /// Resets all state so each flow re-anchors on its next segment. Used
    /// after a Shop Watch pause to avoid resyncing from a stale `next_off`.
    pub fn clear(&mut self) {
        self.streams.clear();
    }
}

/// Whether a segment is nothing but the end of a connection — no bytes, and no
/// SYN to anchor with. The only shape [`Reassembler::push_budgeted`] refuses to
/// create a stream entry for.
fn is_bare_close(segment: &BudgetedSegment) -> bool {
    (segment.fin || segment.rst) && !segment.syn && segment.payload().is_empty()
}

/// Whether every byte this segment carries was already delivered by a flow that
/// has since retired at `end_seq` — one past that flow's last byte.
///
/// Wrap-relative, like every other sequence comparison here: `seq_diff` reads
/// the difference in the signed half-space, so this stays right across the 2^32
/// boundary. Anything ending *after* the retired bound carries something new and
/// is left to open a fresh stream.
fn ends_at_or_before(segment: &BudgetedSegment, end_seq: u32) -> bool {
    let len = u32::try_from(segment.payload().len()).unwrap_or(u32::MAX);
    seq_diff(segment.seq.wrapping_add(len), end_seq) <= 0
}

/// How a tracked flow's connection ended, once [`HalfStream`] can say the flow
/// is safe to drop.
#[derive(Clone, Copy)]
enum Retirement {
    /// FIN, with delivery contiguous through its sequence position and nothing
    /// buffered: every byte the server sent has gone downstream in order, and no
    /// later segment on this connection can exist.
    Closed,
    /// RST at the next expected byte. The connection is over whatever state the
    /// flow was in, so it goes — but unlike a close this promises nothing about
    /// what had been delivered.
    Aborted,
}

/// The captured (server-to-client) half of a connection, in relative offsets.
#[derive(Default)]
struct HalfStream {
    /// Last `Reassembler::clock` value this stream was active; eviction key.
    last_active: u64,
    /// Stream origin (sequence number of the first byte); `None` until first seen.
    baseline: Option<u32>,
    syn_seq: Option<u32>,
    /// Offset (from `baseline`) of the next expected byte.
    next_off: i64,
    /// Buffered future segments, keyed by offset (monotonic order, no wrap).
    pending: BTreeMap<i64, BudgetedChunk>,
    pending_bytes: usize,
    /// Offset of the sequence number the FIN occupies — one past the server's
    /// last byte — once one has been seen in the window.
    ///
    /// Earliest wins, and only from a FIN that does not end behind `next_off`;
    /// see the two guards where it is written.
    fin_off: Option<i64>,
    /// An in-sequence RST was seen. Not an offset, because unlike a FIN there is
    /// nothing to wait to reach: an abort is effective where it lands.
    reset: bool,
}

impl HalfStream {
    fn push(
        &mut self,
        seq: u32,
        syn: bool,
        fin: bool,
        rst: bool,
        payload: BudgetedChunk,
    ) -> HalfOutcome {
        // Before `baseline`: `syn_starts_new_incarnation` reads both to tell a
        // retransmitted SYN from a fresh incarnation.
        if syn {
            self.syn_seq.get_or_insert(seq);
        }
        // SYN consumes a sequence number: data starts at seq + 1.
        let data_seq = if syn { seq.wrapping_add(1) } else { seq };
        self.baseline.get_or_insert(data_seq);
        // Measured from the currently expected byte, then shifted back to
        // absolute: the distance stays inside the TCP window, so `seq_diff`'s
        // i32 span never overflows.
        let expected_seq = self.expected_seq();
        let offset = self.next_off + seq_diff(data_seq, expected_seq);

        // Both read `offset` and `next_off` as they stand *before* the absorb
        // below moves either. FIN sits one past this segment's own bytes,
        // wherever those land; RST is accepted only where the stream is
        // currently expecting a byte — see `Reassembler::push_budgeted` on RFC
        // 5961 §3.2 and why a passive tap owes itself that test.
        if fin {
            let at = offset + payload.as_slice().len() as i64;
            // Two guards, both of which the RST line below already had and the
            // FIN had neither.
            //
            // In-window: a FIN ending *behind* the byte this stream is waiting
            // for describes a connection that has already delivered past it, so
            // it is a stale copy or a forgery — and honouring it retired a live
            // flow. Half the sequence space maps behind `next_off`, so this was
            // reachable by accident, and it was silent: `retirement` counts a
            // close, not a resync, so nothing reported the teardown.
            //
            // Earliest wins, not first seen: a FIN names one fixed position, so
            // a second naming a *different* one is not new information. Taking
            // the first let a single forged FIN far ahead pin `fin_off` beyond
            // anything the connection would reach, and the flow then never
            // retired at all — the slot churn retirement exists to prevent.
            if at >= self.next_off {
                self.fin_off = Some(self.fin_off.map_or(at, |seen| seen.min(at)));
            }
        }
        self.reset |= rst && offset == self.next_off;

        // `with_capacity(1)`: `Vec::new()`'s first `push` of a 48-byte element
        // jumps to capacity 4. `SmallVec` was declined — a dependency, plus 48
        // bytes inlined into two by-value returns per packet, for a malloc
        // nothing here has profiled.
        let mut out = Vec::with_capacity(1);
        if let Err(cause) = self.absorb(offset, payload, &mut out) {
            return HalfOutcome::Pressure(cause);
        }
        if let Err(cause) = self.drain(&mut out) {
            return HalfOutcome::Pressure(cause);
        }
        HalfOutcome::Chunks(out)
    }

    /// Whether this flow can be dropped now, and on what grounds.
    ///
    /// The FIN arm insists on both halves of "nothing is left", and they cover
    /// different failures.
    ///
    /// `next_off >= at` is the one that stops a FIN overtaking a gap: while a
    /// hole in front of the FIN is still open, delivery has not reached the
    /// FIN's position, and the flow keeps both its slot and the bytes buffered
    /// past that hole. Tearing the half-stream down where the FIN *arrives*
    /// would discard them silently, which is the one way retiring flows could
    /// cost a shop refresh.
    ///
    /// `pending.is_empty()` covers what the first test cannot see — a segment
    /// buffered *past* the FIN, which leaves delivery contiguous through the FIN
    /// and bytes held all the same. No well-behaved server sends one, and this
    /// module does not get to assume it is talking to one: capture is port-wide,
    /// and a segment that parses is a segment that lands here.
    ///
    /// An abort answers before either test, and deliberately: it is true whether
    /// or not the stream is whole, and [`Reassembler::retire`] is where what that
    /// cost is decided.
    fn retirement(&self) -> Option<Retirement> {
        if self.reset {
            return Some(Retirement::Aborted);
        }
        let closed = self.fin_off.is_some_and(|at| self.next_off >= at) && self.pending.is_empty();
        closed.then_some(Retirement::Closed)
    }

    /// Integrates one segment: in order (append), future (buffer), or old (trim).
    fn absorb(
        &mut self,
        offset: i64,
        mut payload: BudgetedChunk,
        out: &mut Vec<BudgetedChunk>,
    ) -> Result<(), PressureCause> {
        if payload.as_slice().is_empty() {
            return Ok(());
        }
        if offset > self.next_off {
            return self.buffer_future(offset, payload);
        }

        // `try_from`, not `as`: a negative distance would silently become
        // ~1.8e19, fail the length check below, and freeze this half-stream.
        let Ok(already) = usize::try_from(self.next_off - offset) else {
            report_absorb_invariant(self.next_off, offset);
            // Deliver nothing, but report no pressure: no quota refused
            // anything, and inventing one would throw away an anchor over an
            // arithmetic bug in this function.
            return Ok(());
        };
        if already < payload.as_slice().len() {
            if already != 0 {
                payload.drain_front(already);
            }
            self.next_off += payload.as_slice().len() as i64;
            out.push(payload);
        }
        Ok(())
    }

    /// Buffers a segment that sits past the gap, or reports which quota refused
    /// it. The per-stream cap is checked first, being the tighter of the two by
    /// construction: it attributes an overflow to the stream that caused it even
    /// when the shared pool happens to be nearly full as well.
    fn buffer_future(
        &mut self,
        offset: i64,
        mut payload: BudgetedChunk,
    ) -> Result<(), PressureCause> {
        let capacity = payload.capacity();
        // One `entry` probe: a held chunk is displaced only once the new one
        // has cleared both quotas.
        match self.pending.entry(offset) {
            Entry::Occupied(_) => {
                // First copy wins, which is what a TCP endpoint delivers and
                // what `absorb` already does for bytes past the gap: a byte it
                // has handed on is never revisited. This kept the *longest*
                // instead, so which copy of an ambiguous overlap reached the
                // server depended on whether the gap ahead of it had closed yet
                // — the classic reassembly-ambiguity divergence, and two
                // opposite rules inside one module.
                //
                // The bytes a longer duplicate carries past the held one are not
                // lost: they arrive again in the next in-order segment, or the
                // stream resyncs. Preferring them would mean trusting a second
                // writer over a first, which is the property being refused.
            }
            Entry::Vacant(slot) => {
                let Some(total) = fits_pending(self.pending_bytes, capacity) else {
                    return Err(PressureCause::Stream);
                };
                if !payload.try_retag_pending() {
                    return Err(PressureCause::Shared);
                }
                self.pending_bytes = total;
                slot.insert(payload);
            }
        }
        Ok(())
    }

    /// Flushes buffered segments that became contiguous once `next_off` advanced.
    fn drain(&mut self, out: &mut Vec<BudgetedChunk>) -> Result<(), PressureCause> {
        while let Some((&offset, _)) = self.pending.first_key_value() {
            if offset > self.next_off {
                break; // gap still present.
            }
            let (offset, payload) = self.pending.pop_first().expect("peeked above");
            self.pending_bytes = pending_after_release(self.pending_bytes, payload.capacity());
            self.absorb(offset, payload, out)?;
        }
        Ok(())
    }

    /// Sequence number of the next expected byte: `baseline + next_off`, back in
    /// the wrapping `u32` space (`push` always sets `baseline` before this runs).
    fn expected_seq(&self) -> u32 {
        // `next_off` is non-negative and read mod 2^32, so keeping the low 32
        // bits is the intended conversion; the `u64` detour keeps that
        // truncation unsigned, avoiding sign extension.
        let offset = (self.next_off as u64) as u32;
        self.baseline.unwrap_or(0).wrapping_add(offset)
    }
}

/// The rare branch of [`HalfStream::absorb`], kept off the per-packet path.
#[cold]
#[inline(never)]
fn report_absorb_invariant(next_off: i64, offset: i64) {
    error!(next_off, offset, "reassembly invariant violated");
    debug_assert!(offset <= next_off, "absorb offset exceeds next_off");
}

/// The rare branch of the pressure arm: the budget mutex and seven fields
/// belong off the per-packet path.
#[cold]
#[inline(never)]
fn warn_reassembly_pressure(budget: &PipelineBudget, cause: PressureCause) {
    let stats = budget.snapshot();
    // The same counters accompany a flood on an unrelated port and a genuine
    // pool exhaustion; only the second is a reason to revisit the quotas.
    let scope = match cause {
        PressureCause::Stream => "one stream's own pending cap; that flow re-anchors",
        PressureCause::Shared => "the shared reassembly quota; every flow re-anchors",
    };
    warn!(
        scope,
        cause = cause.resync_cause().label(),
        current_total = stats.current_total,
        capture_bytes = stats.current_capture,
        pending_bytes = stats.current_reassembly,
        outbound_bytes = stats.current_outbound,
        dropped_segments = stats.dropped_segments,
        dropped_bytes = stats.dropped_bytes,
        resyncs = stats.resyncs,
        "reassembly pending-byte pressure; state cleared for a fresh anchor"
    );
}

/// What one eviction cost: the [`ResyncCause`] the counters see, and the wording
/// the warning prints.
///
/// One value with two readers, because they were two readings of one fact and
/// the fact was decided twice. The cause was hard-coded at the call site and the
/// wording derived from `pending_bytes` a line later inside the warning — so the
/// log could say "nothing had been delivered from it" while the counter recorded
/// the same fault a lost shop refresh records. Now the call site cannot name a
/// cause the warning contradicts, because there is only one place that decides.
struct EvictionLoss {
    cause: ResyncCause,
    detail: &'static str,
}

impl EvictionLoss {
    /// Bytes lost, or only a position forgotten.
    ///
    /// `pending` is the only field of a [`HalfStream`] holding bytes. With it
    /// empty, everything this flow ever received has already gone downstream in
    /// order; what the eviction discards is `baseline` and `next_off`, which is
    /// the ability to recognise a *future* arrival as history or as out of order.
    /// That cost only lands if the flow speaks again, and when it does the result
    /// is the immediate suffix that
    /// `every_arrival_order_yields_the_immediate_suffix_of_the_stream` pins as
    /// this reassembler's defined behaviour on any fresh anchor — not damage. So
    /// `next_off > 0` with nothing pending is [`ResyncTier::Housekeeping`]: the
    /// decoder re-anchors, and no byte was thrown away for it to re-anchor over.
    ///
    /// `pending_bytes > 0` is different in kind, not in degree. Those bytes were
    /// received, charged against the reassembly quota, and are dropped here
    /// undelivered; the gap in front of them is now permanent, and no later
    /// segment can supply what was behind it.
    ///
    /// [`ResyncTier::Housekeeping`]: super::ResyncTier::Housekeeping
    fn of(evicted: &HalfStream) -> Self {
        if evicted.pending_bytes > 0 {
            Self {
                cause: ResyncCause::StreamEvicted,
                detail: "a flow buffering behind a gap; its half-received message is gone",
            }
        } else if evicted.next_off > 0 {
            Self {
                cause: ResyncCause::StreamReclaimed,
                detail: "a flow mid-stream with an empty buffer; its decoder re-anchors on the \
                         next segment, having lost no byte",
            }
        } else {
            Self {
                cause: ResyncCause::StreamReclaimed,
                detail: "a flow that had only anchored; nothing had been delivered from it",
            }
        }
    }
}

/// The rare branch of [`Reassembler::evict_stalest`], out of line like
/// [`warn_reassembly_pressure`].
///
/// Owns the evicted stream rather than borrowing it: its leases release as it
/// drops, and the pool total is only worth printing once those bytes are back.
/// The absent `record_drop` is not an omission — nothing was *refused* here,
/// and [`Reassembler::push_budgeted`] states the rule; counting the discarded
/// gap buffer would make `dropped_segments` mean one thing under quota pressure
/// and another under table pressure, so its bytes go in the warning instead.
///
/// Still `warn!` for the reclaimed case, which is not a fault: the counters stop
/// calling it one, and this line is the only instrument that can tell whether the
/// explanation for the churn is right.
///
/// `client_port` and not the client address, on `capture::pcap`'s precedent in
/// the "first server-to-client segment admitted" line: on IPv6 the address is the
/// player's globally routable one, in a file they may be asked to email. It is
/// here to settle what `segments_since_active` means — it read 442 on all 46
/// lines of the session [`Reassembler::evict_stalest`] describes, and
/// steady-state churn of dead clones predicts
/// exactly that, since one ~7-segment connection every ~1.7 s against 64 slots
/// makes the stalest entry always the flow from 64 periods ago. Distinct ports
/// across the lines confirm that reading; a port that repeats means something is
/// minting the same key again and the explanation is wrong.
#[cold]
#[inline(never)]
fn warn_stream_evicted(
    budget: &PipelineBudget,
    loss: EvictionLoss,
    evicted: HalfStream,
    client_port: u16,
    segments_since_active: u64,
) {
    let delivered_bytes = evicted.next_off;
    let buffered_bytes = evicted.pending_bytes;
    drop(evicted);
    let stats = budget.snapshot();
    warn!(
        loss = loss.detail,
        cause = loss.cause.label(),
        stream_cap = MAX_STREAMS,
        client_port,
        segments_since_active,
        delivered_bytes,
        buffered_bytes,
        pending_bytes = stats.current_reassembly,
        resyncs = stats.resyncs,
        "stream table full: the stalest flow lost its slot to a new connection and must re-anchor"
    );
}

/// The rare branch of [`Reassembler::retire`] — the only one of the two
/// retirements that costs anything — out of line like [`warn_stream_evicted`].
///
/// Takes the two byte counts rather than the stream, unlike that function: the
/// flow is already dropped by the time this is called, which is what makes
/// `pending_bytes` below the pool total *after* the stranded bytes came back.
///
/// `client_port` and not the client address, on the precedent both that function
/// and `capture::pcap`'s "first server-to-client segment admitted" line set.
#[cold]
#[inline(never)]
fn warn_connection_reset(
    budget: &PipelineBudget,
    client_port: u16,
    delivered_bytes: i64,
    stranded_bytes: usize,
) {
    let stats = budget.snapshot();
    warn!(
        cause = ResyncCause::ConnectionReset.label(),
        client_port,
        delivered_bytes,
        stranded_bytes,
        pending_bytes = stats.current_reassembly,
        resyncs = stats.resyncs,
        "the server aborted a connection that was still holding bytes behind a gap; \
         nothing will retransmit them"
    );
}

/// Which of the two independent quotas refused to buffer a segment.
///
/// Separate limits, not two readings of one: [`MAX_PENDING_BYTES`] bounds a
/// single stream's gap buffer and is half the reassembly stage's share, so
/// either can be reached with the other still slack. The distinction exists so
/// [`Reassembler::push_budgeted`] can size the recovery to the failure.
///
/// [`MAX_PENDING_BYTES`]: super::MAX_PENDING_BYTES
#[derive(Clone, Copy)]
enum PressureCause {
    /// This one stream filled its own pending-byte cap.
    Stream,
    /// The reassembly stage's shared quota is full, whoever holds it.
    Shared,
}

impl PressureCause {
    /// The same distinction in the counters' vocabulary.
    ///
    /// Two enums rather than one because they answer different questions:
    /// this one sizes the *recovery* (reset one stream, or all of them), and
    /// [`ResyncCause`] names the *reason* for a reader who will never see this
    /// function. They happen to be parallel today; nothing requires them to
    /// stay that way, and folding them together would make the recovery
    /// policy hostage to the vocabulary a window renders.
    const fn resync_cause(self) -> ResyncCause {
        match self {
            Self::Stream => ResyncCause::ReassemblyStream,
            Self::Shared => ResyncCause::ReassemblyShared,
        }
    }
}

enum HalfOutcome {
    Chunks(Vec<BudgetedChunk>),
    Pressure(PressureCause),
}

/// What [`Reassembler::push_budgeted`] did with a segment.
pub(crate) enum ReassemblyOutcome {
    /// The bytes that became contiguous, in order. Empty is normal: a duplicate,
    /// a partial gap fill, or a segment still waiting on a predecessor.
    Chunks(Vec<BudgetedChunk>),
    /// A pending-byte quota was exhausted and the segment's flow lost its place:
    /// its state is gone and the caller must re-anchor, not wait. Whether the
    /// other flows were cleared too is deliberately not reported — the caller
    /// re-arms one anchor window either way.
    Pressure,
}

#[cfg(test)]
fn flatten_chunks(outcome: ReassemblyOutcome) -> Vec<u8> {
    match outcome {
        ReassemblyOutcome::Chunks(chunks) => chunks
            .into_iter()
            .flat_map(|chunk| chunk.into_parts().0)
            .collect(),
        ReassemblyOutcome::Pressure => Vec::new(),
    }
}

fn segment_data_seq(segment: &BudgetedSegment) -> u32 {
    if segment.syn {
        segment.seq.wrapping_add(1)
    } else {
        segment.seq
    }
}

/// Signed distance `a - b` over the circular sequence-number space.
const fn seq_diff(a: u32, b: u32) -> i64 {
    (a.wrapping_sub(b) as i32) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{MAX_PENDING_BYTES, flow, flow_from, sized_seg, test_budget};

    fn seg_in(flow: FlowKey, seq: u32, syn: bool, payload: &[u8]) -> Segment {
        Segment {
            flow,
            seq,
            syn,
            fin: false,
            rst: false,
            payload: Vec::from(payload),
        }
    }

    fn seg(seq: u32, syn: bool, payload: &[u8]) -> Segment {
        seg_in(flow(), seq, syn, payload)
    }

    fn seg_on(flow: FlowKey, seq: u32, payload: &[u8]) -> Segment {
        seg_in(flow, seq, false, payload)
    }

    /// The orderly close: the FIN sits at `seq`, optionally carrying the last
    /// bytes of the stream ahead of it.
    fn fin_on(flow: FlowKey, seq: u32, payload: &[u8]) -> Segment {
        Segment {
            fin: true,
            ..seg_in(flow, seq, false, payload)
        }
    }

    /// The abort.
    fn rst_on(flow: FlowKey, seq: u32) -> Segment {
        Segment {
            rst: true,
            ..seg_in(flow, seq, false, b"")
        }
    }

    fn flatten_half(outcome: HalfOutcome) -> Vec<u8> {
        match outcome {
            HalfOutcome::Chunks(chunks) => chunks
                .into_iter()
                .flat_map(|chunk| chunk.into_parts().0)
                .collect(),
            HalfOutcome::Pressure(_) => Vec::new(),
        }
    }

    /// The shared arm: 64 streams, none near its own cap, exhaust the stage
    /// between them, so every anchor deserves to go. The wide `clear()` shows up
    /// as a reassembly total of zero, not the 63 buffers an eviction would leave.
    #[test]
    fn pending_bytes_are_global_across_sixty_four_streams() {
        let budget = test_budget(4096, 4096, 1024, 4096);
        let mut reassembler = Reassembler::new();
        for port in 0..64u16 {
            let flow = flow_from(port + 1);
            drop(flatten_chunks(reassembler.push_budgeted(
                budget.admit_capture(sized_seg(flow, 1000, 1, 16)).unwrap(),
            )));
            let outcome = reassembler
                .push_budgeted(budget.admit_capture(sized_seg(flow, 2000, 1, 16)).unwrap());
            assert!(matches!(outcome, ReassemblyOutcome::Chunks(ref chunks) if chunks.is_empty()));
            assert!(budget.snapshot().current_total <= 4096);
        }
        assert_eq!(budget.snapshot().current_reassembly, 1024);
        // Exactly the check the failing stream is about to run: its own cap has
        // room, so this stays a shared-arm test even if the constants move.
        assert!(
            fits_pending(16, 16).is_some(),
            "the per-stream cap must not be the limit under test here"
        );
        assert!(matches!(
            reassembler.push_budgeted(
                budget
                    .admit_capture(sized_seg(flow_from(1), 3000, 1, 16))
                    .unwrap()
            ),
            ReassemblyOutcome::Pressure
        ));
        assert_eq!(budget.snapshot().current_reassembly, 0);
        drop(reassembler);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    /// The per-stream arm, and the reason the two are told apart at all: a
    /// stranger on the port fills its own 8 MiB cap while the flow that matters
    /// is mid-message, and losing the game flow's baseline there costs a shop
    /// refresh, so the recovery must stop at the stream that overflowed.
    ///
    /// Every shared quota is four times the per-stream cap, so `try_retag_pending`
    /// cannot fail and this stays a test of `fits_pending`.
    #[test]
    fn a_stream_that_fills_its_own_pending_cap_leaves_every_other_anchor_alone() {
        const CHUNK: usize = 64 * 1024;
        const SHARED: usize = MAX_PENDING_BYTES * 4;
        let budget = test_budget(SHARED, SHARED, SHARED, SHARED);
        let mut reassembler = Reassembler::new();
        let push = |reassembler: &mut Reassembler, segment: Segment| {
            flatten_chunks(reassembler.push_budgeted(budget.admit_capture(segment).unwrap()))
        };
        let game = flow_from(51000);
        let stranger = flow_from(51001);

        assert_eq!(push(&mut reassembler, seg_on(game, 1000, b"AB")), b"AB");
        // The stranger anchors, then loses the byte at 1001 forever.
        assert_eq!(push(&mut reassembler, seg_on(stranger, 1000, b"A")), b"A");

        let capacity_chunks = MAX_PENDING_BYTES / CHUNK;
        for index in 0..capacity_chunks {
            let seq = 2000 + (index * CHUNK) as u32;
            assert!(
                push(&mut reassembler, sized_seg(stranger, seq, CHUNK, CHUNK)).is_empty(),
                "chunk {index} sits behind the gap and delivers nothing"
            );
        }
        let before = budget.snapshot();
        assert_eq!(before.current_reassembly, MAX_PENDING_BYTES);
        assert!(
            before.current_reassembly + CHUNK <= SHARED,
            "the shared quota must still have room, or this proves nothing"
        );

        let overflow = 2000 + (capacity_chunks * CHUNK) as u32;
        assert!(matches!(
            reassembler.push_budgeted(
                budget
                    .admit_capture(sized_seg(stranger, overflow, CHUNK, CHUNK))
                    .unwrap()
            ),
            ReassemblyOutcome::Pressure
        ));

        assert!(
            !reassembler.streams.contains_key(&stranger),
            "the stream that overflowed must lose its state and re-anchor"
        );
        assert!(
            reassembler.streams.contains_key(&game),
            "another flow's private cap is not this flow's problem: its anchor must survive"
        );
        let survivor = &reassembler.streams[&game];
        assert_eq!(survivor.baseline, Some(1000));
        assert_eq!(survivor.next_off, 2);
        // A retransmission is recognisable as history only while the anchor
        // holds: a cleared flow re-anchors on it and delivers "AB" twice, a
        // duplicate a decoder can resync from but cannot un-see.
        assert!(
            push(&mut reassembler, seg_on(game, 1000, b"AB")).is_empty(),
            "the surviving flow must still know 1000 is behind it"
        );
        assert_eq!(push(&mut reassembler, seg_on(game, 1002, b"CD")), b"CD");

        assert_eq!(budget.snapshot().current_reassembly, 0);
        drop(reassembler);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    #[test]
    fn pressure_clears_state_and_next_segment_reanchors() {
        let budget = test_budget(256, 256, 8, 256);
        let mut reassembler = Reassembler::new();
        drop(flatten_chunks(reassembler.push_budgeted(
            budget.admit_capture(sized_seg(flow(), 1000, 2, 8)).unwrap(),
        )));
        assert!(matches!(
            reassembler.push_budgeted(
                budget
                    .admit_capture(sized_seg(flow(), 2000, 2, 16))
                    .unwrap()
            ),
            ReassemblyOutcome::Pressure
        ));
        let output = flatten_chunks(
            reassembler.push_budgeted(budget.admit_capture(sized_seg(flow(), 9000, 2, 8)).unwrap()),
        );
        assert_eq!(output, b"XX");
        assert_eq!(budget.snapshot().resyncs, 1);
        drop(reassembler);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    #[test]
    fn gap_fill_moves_chunks_without_exceeding_budget() {
        let budget = test_budget(128, 128, 64, 128);
        let mut reassembler = Reassembler::new();
        drop(flatten_chunks(reassembler.push_budgeted(
            budget.admit_capture(seg(1000, false, b"AB")).unwrap(),
        )));
        assert!(matches!(
            reassembler.push_budgeted(
                budget.admit_capture(seg(1004, false, b"EF")).unwrap()
            ),
            ReassemblyOutcome::Chunks(ref chunks) if chunks.is_empty()
        ));
        let chunks = match reassembler
            .push_budgeted(budget.admit_capture(seg(1002, false, b"CD")).unwrap())
        {
            ReassemblyOutcome::Chunks(chunks) => chunks,
            ReassemblyOutcome::Pressure => panic!("unexpected pressure"),
        };
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].as_slice(), b"CD");
        assert_eq!(chunks[1].as_slice(), b"EF");
        assert!(budget.snapshot().current_total <= 128);
        drop(chunks);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    fn collect_arrivals(segments: &[Segment; 3], permutation: [usize; 3]) -> Vec<u8> {
        let mut reassembler = Reassembler::new();
        let mut output = Vec::new();
        for index in permutation {
            output.extend(reassembler.push(&segments[index]));
        }
        output
    }

    fn collect_anchored(segments: &[Segment; 3], permutation: [usize; 3]) -> Vec<u8> {
        let mut burst = InitialBurst::new();
        for index in permutation {
            burst.push_test(segments[index].clone());
        }
        let mut reassembler = Reassembler::new();
        burst
            .into_ordered()
            .into_iter()
            .flat_map(|segment| flatten_chunks(reassembler.push_budgeted(segment)))
            .collect()
    }

    #[test]
    fn initial_anchor_burst_orders_all_six_permutations() {
        let segments = [
            seg(1000, false, b"AB"),
            seg(1002, false, b"CD"),
            seg(1004, false, b"EF"),
        ];

        for permutation in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            assert_eq!(
                collect_anchored(&segments, permutation),
                b"ABCDEF",
                "arrival permutation {permutation:?}"
            );
        }
    }

    #[test]
    fn initial_anchor_burst_order_is_wrap_safe_and_overlap_stays_centralized() {
        let wrapped = [
            seg(u32::MAX - 1, false, b"AB"),
            seg(0, false, b"CD"),
            seg(2, false, b"EF"),
        ];
        assert_eq!(collect_anchored(&wrapped, [2, 0, 1]), b"ABCDEF");

        let overlap = [
            seg(1000, false, b"ABCD"),
            seg(1002, false, b"CDEF"),
            seg(1006, false, b"GH"),
        ];
        assert_eq!(collect_anchored(&overlap, [1, 2, 0]), b"ABCDEFGH");
    }

    /// Sorting is per flow but the *slots* are global: interleaved connections
    /// come back each ordered, with the alternation intact.
    #[test]
    fn initial_anchor_burst_preserves_inter_flow_slots() {
        let first = flow();
        let second = flow_from(52000);
        let mut burst = InitialBurst::new();
        burst.push_test(seg_on(first, 1002, b"CD"));
        burst.push_test(seg_on(second, 2002, b"WX"));
        burst.push_test(seg_on(first, 1000, b"AB"));
        burst.push_test(seg_on(second, 2000, b"UV"));

        let ordered = burst.into_ordered();
        let observed: Vec<_> = ordered
            .iter()
            .map(|segment| (segment.flow, segment.seq))
            .collect();
        assert_eq!(
            observed,
            vec![(first, 1000), (second, 2000), (first, 1002), (second, 2002)]
        );
    }

    #[test]
    fn initial_anchor_all_six_permutations_keep_the_immediate_suffix() {
        let segments = [
            seg(1000, false, b"AB"),
            seg(1002, false, b"CD"),
            seg(1004, false, b"EF"),
        ];
        let cases: [([usize; 3], &[u8]); 6] = [
            ([0, 1, 2], b"ABCDEF"),
            ([0, 2, 1], b"ABCDEF"),
            ([1, 0, 2], b"CDEF"),
            ([1, 2, 0], b"CDEF"),
            ([2, 0, 1], b"EF"),
            ([2, 1, 0], b"EF"),
        ];

        for (permutation, expected) in cases {
            assert_eq!(
                collect_arrivals(&segments, permutation),
                expected,
                "arrival permutation {permutation:?}"
            );
        }
    }

    /// The suffix rule for **every** arrival order of six segments, at four
    /// origins, two of which straddle the `u32` wrap. Exhaustive (2 880 cases)
    /// rather than sampled: the space is small enough, and the table above
    /// covers the two properties below only for n = 3.
    #[test]
    fn every_arrival_order_yields_the_immediate_suffix_of_the_stream() {
        for origin in [1_000_u32, u32::MAX - 5, u32::MAX - 11, 0] {
            let payloads: [&[u8]; 6] = [b"AB", b"CD", b"EF", b"GH", b"IJ", b"KL"];
            let whole: Vec<u8> = payloads.concat();
            let segments: Vec<Segment> = payloads
                .iter()
                .enumerate()
                .map(|(index, bytes)| {
                    // Origins near `u32::MAX` wrap under the segments.
                    seg(origin.wrapping_add(index as u32 * 2), false, bytes)
                })
                .collect();

            for order in permutations(payloads.len()) {
                let mut reassembler = Reassembler::new();
                let mut delivered = Vec::new();
                for index in order.iter().copied() {
                    delivered.extend(reassembler.push(&segments[index]));
                }
                // 1. A *suffix*, never a permutation and never a gap: the
                //    analysis server resyncs from any point, but cannot
                //    survive reordered or hole-punched bytes.
                assert!(
                    whole.ends_with(&delivered),
                    "origin {origin}, order {order:?} delivered {delivered:?}, not a suffix"
                );
                // 2. The suffix starts at the first segment to arrive, which
                //    is the arrival that anchors the stream.
                let anchor = order[0];
                assert_eq!(
                    delivered.len(),
                    whole.len() - anchor * 2,
                    "origin {origin}, order {order:?} did not anchor on segment {anchor}"
                );
            }
        }
    }

    /// The three algebraic properties `seq_diff` is defined by, over a lattice
    /// of bases including every wrap boundary. The interesting inputs are
    /// exactly those boundaries — `0`, `u32::MAX`, `i32::MAX`, where the signed
    /// reading flips — which a uniform generator reaches with probability ~0.
    #[test]
    fn seq_diff_is_antisymmetric_and_wrap_relative() {
        let bases = [
            0_u32,
            1,
            1_000,
            0x7FFF_FFFF,
            0x8000_0000,
            0x8000_0001,
            u32::MAX - 1,
            u32::MAX,
        ];
        let deltas: [i64; 9] = [-1_000, -2, -1, 0, 1, 2, 1_000, 65_535, 1_048_576];
        for base in bases {
            // 1. Reflexive: zero distance from itself, at every base.
            assert_eq!(seq_diff(base, base), 0, "base {base}");
            for delta in deltas {
                let other = base.wrapping_add(delta as u32);
                // 2. Relative distance: a wrap is invisible, which is how
                //    `HalfStream::push` tracks a stream past 2 GiB.
                assert_eq!(seq_diff(other, base), delta, "base {base}, delta {delta}");
                // 3. Antisymmetric. No `delta` reaches `i32::MIN`, which
                //    cannot be negated: half the space apart, "ahead" and
                //    "behind" are the same answer.
                assert_eq!(
                    seq_diff(base, other),
                    -delta,
                    "base {base}, delta {delta} is not antisymmetric"
                );
            }
        }
        // 2^31 apart reads as `i32::MIN` both ways: why `MAX_PENDING_BYTES` and
        // the anchor logic bound how far out of order a segment may be.
        assert_eq!(seq_diff(0, 0x8000_0000), i64::from(i32::MIN));
        assert_eq!(seq_diff(0x8000_0000, 0), i64::from(i32::MIN));
    }

    /// Every permutation of `0..n`, lexicographic and dependency-free.
    fn permutations(n: usize) -> Vec<Vec<usize>> {
        if n == 0 {
            return vec![Vec::new()];
        }
        let mut out = Vec::new();
        for head in 0..n {
            for mut rest in permutations(n - 1) {
                // Shift tail indices at or above `head` up by one, so `rest`
                // becomes a permutation of `0..n` minus `head`.
                for index in &mut rest {
                    if *index >= head {
                        *index += 1;
                    }
                }
                rest.insert(0, head);
                out.push(rest);
            }
        }
        out
    }

    #[test]
    fn initial_anchor_suffix_characterization_is_wrap_safe() {
        let segments = [
            seg(u32::MAX - 1, false, b"AB"),
            seg(0, false, b"CD"),
            seg(2, false, b"EF"),
        ];

        assert_eq!(collect_arrivals(&segments, [0, 2, 1]), b"ABCDEF");
        assert_eq!(collect_arrivals(&segments, [2, 0, 1]), b"EF");
    }

    #[test]
    fn initial_anchor_overlap_keeps_only_bytes_after_the_immediate_suffix() {
        let mut reassembler = Reassembler::new();
        let arrivals = [
            seg(1002, false, b"CDEF"),
            seg(1000, false, b"ABCD"),
            seg(1002, false, b"CDEF"),
            seg(1006, false, b"GH"),
        ];
        let mut output = Vec::new();
        for segment in arrivals {
            output.extend(reassembler.push(&segment));
        }

        assert_eq!(output, b"CDEFGH");
    }

    /// Each flow anchors on its own first segment: `1000` after `1002` is
    /// history on `first`, while the same seq is the origin on `second`.
    #[test]
    fn initial_anchor_is_isolated_by_flow() {
        let mut reassembler = Reassembler::new();
        let first = flow();
        let second = flow_from(52000);

        assert_eq!(reassembler.push(&seg_on(first, 1002, b"CD")), b"CD");
        assert_eq!(reassembler.push(&seg_on(second, 1000, b"XY")), b"XY");

        assert!(reassembler.push(&seg_on(first, 1000, b"AB")).is_empty());
        assert_eq!(reassembler.push(&seg_on(second, 1002, b"Z!")), b"Z!");
    }

    #[test]
    fn in_order() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
        assert_eq!(r.push(&seg(1002, false, b"CD")), b"CD");
    }

    #[test]
    fn reordering_flushes_multiple_buffered_segments() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
        assert!(r.push(&seg(1006, false, b"GH")).is_empty());
        assert!(r.push(&seg(1004, false, b"EF")).is_empty());
        assert_eq!(r.push(&seg(1002, false, b"CD")), b"CDEFGH");
    }

    #[test]
    fn retransmission_is_ignored() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
        assert!(r.push(&seg(1000, false, b"AB")).is_empty());
    }

    #[test]
    fn overlapping_segment_keeps_only_fresh_tail() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, b"ABCD")), b"ABCD");
        assert_eq!(r.push(&seg(1002, false, b"CDEF")), b"EF"); // "CD" already seen.
    }

    #[test]
    fn syn_sets_the_baseline() {
        let mut r = Reassembler::new();
        assert!(r.push(&seg(999, true, b"")).is_empty()); // SYN anchors origin at 1000.
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
    }

    #[test]
    fn gap_filled_out_of_order() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
        assert!(r.push(&seg(1004, false, b"EF")).is_empty());
        assert_eq!(r.push(&seg(1002, false, b"CD")), b"CDEF");
    }

    #[test]
    fn reassembles_across_sequence_wrap() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(0xFFFF_FFFE, false, b"AB")), b"AB");
        assert_eq!(r.push(&seg(0x0000_0000, false, b"CD")), b"CD");
    }

    #[test]
    fn reordering_across_wrap_is_ordered_correctly() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(0xFFFF_FFFE, false, b"AB")), b"AB");
        assert!(r.push(&seg(0x0000_0002, false, b"EF")).is_empty());
        assert_eq!(r.push(&seg(0x0000_0000, false, b"CD")), b"CDEF");
    }

    #[test]
    fn delivers_far_past_two_gigabytes() {
        // A half-stream past 2^31 delivered bytes: the next in-order segment
        // must still be recognised, where an origin-relative offset overflowed.
        let mut half = HalfStream {
            baseline: Some(0),
            next_off: (1i64 << 31) + 1000,
            ..Default::default()
        };
        let expected = half.expected_seq();
        let budget = PipelineBudget::new();
        let first = budget
            .admit_capture(seg(expected, false, b"AB"))
            .unwrap()
            .into_payload();
        assert_eq!(
            flatten_half(half.push(expected, false, false, false, first)),
            b"AB"
        );
        let second = budget
            .admit_capture(seg(expected.wrapping_add(2), false, b"CD"))
            .unwrap()
            .into_payload();
        assert_eq!(
            flatten_half(half.push(expected.wrapping_add(2), false, false, false, second)),
            b"CD"
        );
    }

    #[test]
    fn tracked_stream_count_is_bounded() {
        let mut r = Reassembler::new();
        // A forged-source-port flood on the game port must not grow the map.
        for port in 0..(MAX_STREAMS as u32 * 3) {
            r.push(&seg_on(flow_from(port as u16), 1000, b"AB"));
        }
        assert_eq!(r.streams.len(), MAX_STREAMS);
    }

    #[test]
    fn eviction_keeps_the_active_flow() {
        let mut r = Reassembler::new();
        let hot = flow_from(1);
        r.push(&seg_on(hot, 1000, b"AB"));
        for port in 100..(100 + MAX_STREAMS as u32 * 2) {
            r.push(&seg_on(flow_from(port as u16), 1000, b"XY"));
            r.push(&seg_on(hot, 1002, b"CD"));
        }
        assert_eq!(r.streams.len(), MAX_STREAMS);
        assert!(r.streams.contains_key(&hot));
    }

    /// The eviction path end to end, for the case that costs bytes: a flow is
    /// mid-message with a segment buffered behind a gap when the table fills, so
    /// those bytes are discarded undelivered. The point is that the loss is
    /// *counted*, and counted as a fault, on the same `resyncs` number the
    /// pressure arm uses.
    ///
    /// Uses `push_budgeted` against one shared budget: the `push` helper mints a
    /// throwaway `PipelineBudget` per call and would read zero counters.
    #[test]
    fn evicting_a_crowded_out_stream_counts_the_lost_anchor() {
        let budget = PipelineBudget::new();
        let mut reassembler = Reassembler::new();
        let push = |reassembler: &mut Reassembler, segment: Segment| {
            flatten_chunks(reassembler.push_budgeted(budget.admit_capture(segment).unwrap()))
        };
        let game = flow();

        assert_eq!(push(&mut reassembler, seg_on(game, 1000, b"AB")), b"AB");
        // Mid-message: a segment past a gap, buffered and waiting.
        assert!(push(&mut reassembler, seg_on(game, 1004, b"EF")).is_empty());
        assert_eq!(budget.snapshot().resyncs, 0);

        // 63 newcomers fill the table; the 64th is the first key with nowhere
        // to go, and the game's stream is by then the stalest.
        for port in 1..=(MAX_STREAMS as u16) {
            push(&mut reassembler, seg_on(flow_from(port), 1000, b"XY"));
        }

        assert_eq!(reassembler.streams.len(), MAX_STREAMS);
        assert!(
            !reassembler.streams.contains_key(&game),
            "the quiet flow is the stalest one, so this test must be evicting it"
        );
        assert_eq!(
            budget.snapshot().resyncs,
            1,
            "an evicted stream lost its anchor and must be counted like every other anchor loss"
        );
        assert_eq!(
            budget.snapshot().dominant_resync(),
            Some(ResyncCause::StreamEvicted),
            "buffered bytes were discarded undelivered, so this is a fault and must name itself"
        );
        // Pinned so it stays declined: nothing was *refused* here — the segment
        // that triggered the eviction was admitted — and `push_budgeted` rules
        // that chunks a recovery throws away are collateral, not extra
        // captures. The discarded gap buffer is in the warning's bytes instead.
        assert_eq!(budget.snapshot().dropped_segments, 0);
        // The evicted stream's buffered segment gave its lease back on the way out.
        assert_eq!(budget.snapshot().current_reassembly, 0);

        assert_eq!(push(&mut reassembler, seg_on(game, 9000, b"ZZ")), b"ZZ");
        drop(reassembler);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    /// The defect, at the layer that decides it: an eviction that discarded no
    /// byte must not reach the player's verdict.
    ///
    /// Both shapes of it, because both are the same cause and only one of them
    /// is obvious. `delivered` has handed every byte it received downstream, in
    /// order, with nothing buffered — it loses `baseline` and `next_off`, which
    /// is a position, not data — and `anchored` never delivered anything at all.
    /// This was the observed regime, back when `parse_segment` discarded FIN and
    /// RST and the table filled with the game's own closed connections: a
    /// patched build logged 46 evictions in ~90 s with `buffered_bytes=0` on
    /// every line. Retiring a flow on its close took that source away
    /// (`repeated_short_connections_leave_the_table_empty_instead_of_full`), so
    /// what this pins is the classification and not a rate — the flows that
    /// still reach eviction are the ones whose end never arrived, and they are
    /// still not faults.
    #[test]
    fn evicting_a_flow_that_lost_no_byte_is_counted_but_is_not_a_fault() {
        let budget = PipelineBudget::new();
        let mut reassembler = Reassembler::new();
        let push = |reassembler: &mut Reassembler, segment: Segment| {
            flatten_chunks(reassembler.push_budgeted(budget.admit_capture(segment).unwrap()))
        };
        let delivered = flow_from(50_000);
        let anchored = flow_from(50_001);

        assert_eq!(
            push(&mut reassembler, seg_on(delivered, 1000, b"AB")),
            b"AB"
        );
        assert!(push(&mut reassembler, seg_in(anchored, 999, true, b"")).is_empty());
        assert_eq!(budget.snapshot().current_reassembly, 0);

        // Newcomers fill the table; the two stalest entries are the two above.
        for port in 1..=(MAX_STREAMS as u16) {
            push(&mut reassembler, seg_on(flow_from(port), 1000, b"XY"));
        }

        assert_eq!(reassembler.streams.len(), MAX_STREAMS);
        for evicted in [delivered, anchored] {
            assert!(
                !reassembler.streams.contains_key(&evicted),
                "the two quiet flows are the stalest, so this test must be evicting them"
            );
        }

        let stats = budget.snapshot();
        assert_eq!(
            stats.resyncs_by_cause[ResyncCause::StreamReclaimed.index()],
            2,
            "both are real events and both deserve a number"
        );
        assert_eq!(
            stats.resyncs_by_cause[ResyncCause::StreamEvicted.index()],
            0,
            "neither discarded a byte, so neither may be counted as a loss"
        );
        assert_eq!(
            stats.dominant_resync(),
            None,
            "nothing was lost, so the run has no fault to name"
        );
        assert_eq!(stats.dropped_segments, 0);

        drop(reassembler);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    /// The orderly close, end to end: the flow gives its slot back the moment
    /// the FIN's own sequence position has been delivered through, and does it
    /// without recording a re-anchor — nothing re-anchored, the connection ended.
    #[test]
    fn a_flow_gives_its_slot_back_once_delivery_reaches_the_fin() {
        let budget = PipelineBudget::new();
        let mut reassembler = Reassembler::new();
        let push = |reassembler: &mut Reassembler, segment: Segment| {
            flatten_chunks(reassembler.push_budgeted(budget.admit_capture(segment).unwrap()))
        };
        let game = flow();

        assert_eq!(push(&mut reassembler, seg_on(game, 1000, b"AB")), b"AB");
        assert_eq!(reassembler.streams.len(), 1);
        // The last bytes and the FIN in one segment, which is the ordinary shape.
        assert_eq!(push(&mut reassembler, fin_on(game, 1002, b"CD")), b"CD");

        assert!(
            reassembler.streams.is_empty(),
            "a connection that has ended must not keep a slot in the table"
        );
        assert_eq!(
            budget.snapshot().resyncs,
            0,
            "a flow that delivered everything and then closed did not re-anchor"
        );
        assert_eq!(budget.snapshot().dropped_segments, 0);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    /// The one way retiring flows could silently cost a shop refresh: a FIN that
    /// overtakes a gap, with a segment already buffered behind it.
    ///
    /// It must not tear the half-stream down where it lands. The flow stays,
    /// still holding those bytes and still charged for them, until the gap fills
    /// — and *then* retires, having delivered every one of them in order.
    #[test]
    fn a_fin_that_overtakes_a_gap_does_not_discard_the_bytes_behind_it() {
        let budget = PipelineBudget::new();
        let mut reassembler = Reassembler::new();
        let push = |reassembler: &mut Reassembler, segment: Segment| {
            flatten_chunks(reassembler.push_budgeted(budget.admit_capture(segment).unwrap()))
        };
        let game = flow();

        assert_eq!(push(&mut reassembler, seg_on(game, 1000, b"AB")), b"AB");
        assert!(push(&mut reassembler, seg_on(game, 1004, b"EF")).is_empty());
        assert!(budget.snapshot().current_reassembly > 0, "EF is buffered");

        // The close arrives while 1002..1004 is still missing.
        assert!(push(&mut reassembler, fin_on(game, 1006, b"")).is_empty());
        assert!(
            reassembler.streams.contains_key(&game),
            "the flow still owes bytes it received, so the FIN may not retire it"
        );
        assert!(
            budget.snapshot().current_reassembly > 0,
            "and those bytes must still be held, not quietly dropped"
        );

        // The gap fills, everything comes out in order, and only now is the flow
        // over.
        assert_eq!(push(&mut reassembler, seg_on(game, 1002, b"CD")), b"CDEF");
        assert!(reassembler.streams.is_empty());
        assert_eq!(budget.snapshot().resyncs, 0);
        assert_eq!(budget.snapshot().dropped_segments, 0);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    /// The other half of the same rule, for the shape this code does not get to
    /// rule out: a segment buffered *past* the FIN, which no well-behaved server
    /// sends but the game's port is open to anyone.
    ///
    /// Delivery has reached the FIN, so the first half of the retirement test is
    /// satisfied and only "nothing is buffered" holds the flow back. It must
    /// hold it back: those bytes were received and charged, and a retirement is
    /// not allowed to discard bytes silently — the eviction path exists to
    /// account for that, and this one does not.
    #[test]
    fn a_fin_does_not_retire_a_flow_still_holding_bytes_past_it() {
        let budget = PipelineBudget::new();
        let mut reassembler = Reassembler::new();
        let push = |reassembler: &mut Reassembler, segment: Segment| {
            flatten_chunks(reassembler.push_budgeted(budget.admit_capture(segment).unwrap()))
        };
        let game = flow();

        assert_eq!(push(&mut reassembler, seg_on(game, 1000, b"AB")), b"AB");
        assert!(push(&mut reassembler, seg_on(game, 1010, b"ZZ")).is_empty());
        assert!(push(&mut reassembler, fin_on(game, 1002, b"")).is_empty());

        assert!(
            reassembler.streams.contains_key(&game),
            "bytes this pipeline holds may not vanish with the flow that holds them"
        );
        assert!(budget.snapshot().current_reassembly > 0);
        assert_eq!(budget.snapshot().resyncs, 0);
        drop(reassembler);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    /// Two copies of one offset disagreeing: the first is delivered.
    ///
    /// The buffer used to keep the *longest*, while `absorb` keeps the first —
    /// so which copy the server received depended on whether the gap ahead had
    /// closed yet. A TCP endpoint delivers the first, and one module cannot hold
    /// two opposite rules for the same question.
    #[test]
    fn the_first_copy_of_a_contradictory_overlap_is_the_one_delivered() {
        let budget = PipelineBudget::new();
        let mut reassembler = Reassembler::new();
        let mut push = |segment: Segment| {
            flatten_chunks(reassembler.push_budgeted(budget.admit_capture(segment).unwrap()))
        };
        let game = flow();

        assert_eq!(push(seg_on(game, 1000, b"AB")), b"AB");
        // Both land past the gap at 1002, so both are buffered rather than
        // delivered — the window where the two rules disagreed.
        assert!(push(seg_on(game, 1004, b"EF")).is_empty());
        assert!(push(seg_on(game, 1004, b"xxxx")).is_empty());

        assert_eq!(
            push(seg_on(game, 1002, b"CD")),
            b"CDEF",
            "the longer late copy must not displace the one already held"
        );
    }

    /// A FIN ending behind the window does not retire a live flow.
    ///
    /// The RST one line below it in `HalfStream::push` has always been checked
    /// against `next_off` — RFC 5961 §3.2, which the module argues a passive tap
    /// owes itself. The FIN had no such test, and half the sequence space maps
    /// behind `next_off`, so a stale or crafted copy tore the flow down: the
    /// stream lost its `baseline`, the next segment re-anchored, and delivery
    /// stopped being a suffix. Silently, too — a close is not counted as a
    /// resync, so nothing reported it.
    #[test]
    fn a_fin_behind_the_window_does_not_retire_the_flow() {
        let budget = PipelineBudget::new();
        let mut reassembler = Reassembler::new();
        let push = |reassembler: &mut Reassembler, segment: Segment| {
            flatten_chunks(reassembler.push_budgeted(budget.admit_capture(segment).unwrap()))
        };
        let game = flow();

        assert_eq!(push(&mut reassembler, seg_on(game, 1000, b"AB")), b"AB");
        assert!(push(&mut reassembler, fin_on(game, 1000, b"")).is_empty());

        assert!(
            reassembler.streams.contains_key(&game),
            "a close that names a byte already delivered ends nothing"
        );
        // And the stream still reads as one: without the guard the flow was
        // gone, so this re-anchored at 1002 and delivered out of order.
        assert_eq!(push(&mut reassembler, seg_on(game, 1002, b"CD")), b"CD");
        assert_eq!(budget.snapshot().resyncs, 0);
    }

    /// A FIN far ahead cannot pin the close out of reach.
    ///
    /// `fin_off` took the first writer, so one segment naming a position the
    /// connection would never reach kept `retirement` waiting forever and the
    /// flow held its slot until eviction — the churn retirement was added to
    /// remove. A FIN names one fixed position, so the earliest is the only one
    /// that can be right.
    #[test]
    fn a_fin_far_ahead_does_not_outrank_the_one_that_ends_the_stream() {
        let budget = PipelineBudget::new();
        let mut reassembler = Reassembler::new();
        let push = |reassembler: &mut Reassembler, segment: Segment| {
            flatten_chunks(reassembler.push_budgeted(budget.admit_capture(segment).unwrap()))
        };
        let game = flow();

        assert_eq!(push(&mut reassembler, seg_on(game, 1000, b"AB")), b"AB");
        // Ahead of everything this connection will send.
        assert!(push(&mut reassembler, fin_on(game, 1002 + (1 << 30), b"")).is_empty());
        assert!(push(&mut reassembler, fin_on(game, 1002, b"")).is_empty());

        assert!(
            !reassembler.streams.contains_key(&game),
            "the real close must retire the flow whatever a later-numbered FIN claimed"
        );
    }

    /// The second adapter's copy of a closing segment is history, not data.
    ///
    /// `capture::pcap` opens every adapter and justifies it by saying this
    /// module dedupes by sequence number, so on a two-adapter machine every
    /// segment arrives twice. Retirement frees the `baseline`/`next_off` pair
    /// that recognises a duplicate, and a FIN carrying the connection's last
    /// bytes is not a *bare* close, so before [`Reassembler::retired`] the
    /// duplicate re-anchored a fresh stream and delivered `CD` again — `ABCDCD`,
    /// at the end of every connection, on the default configuration.
    ///
    /// Delivery must be a suffix of the stream. Repeating four bytes is neither
    /// a permutation nor a gap, so nothing downstream can detect it.
    #[test]
    fn a_duplicate_of_the_closing_segment_is_not_delivered_twice() {
        let budget = PipelineBudget::new();
        let mut reassembler = Reassembler::new();
        let mut push = |segment: Segment| {
            flatten_chunks(reassembler.push_budgeted(budget.admit_capture(segment).unwrap()))
        };
        let game = flow();

        assert_eq!(push(seg_on(game, 1000, b"AB")), b"AB");
        // The same packet, off the other adapter.
        assert!(push(seg_on(game, 1000, b"AB")).is_empty());
        assert_eq!(push(fin_on(game, 1002, b"CD")), b"CD");

        assert!(
            push(fin_on(game, 1002, b"CD")).is_empty(),
            "the closing segment's duplicate must not be delivered a second time"
        );
        // The same shape a plain retransmission takes, which the open-every-
        // adapter design also relies on being free.
        assert!(push(seg_on(game, 1000, b"AB")).is_empty());
    }

    /// An abort is not a close. It retires the flow whatever state it was in —
    /// an aborted connection can never deliver another byte, so its slot is dead
    /// weight — and when that state included a segment behind a gap, those bytes
    /// are gone and the run is told so.
    #[test]
    fn an_abort_retires_the_flow_and_counts_the_bytes_it_stranded() {
        let budget = PipelineBudget::new();
        let mut reassembler = Reassembler::new();
        let push = |reassembler: &mut Reassembler, segment: Segment| {
            flatten_chunks(reassembler.push_budgeted(budget.admit_capture(segment).unwrap()))
        };
        let game = flow();

        assert_eq!(push(&mut reassembler, seg_on(game, 1000, b"AB")), b"AB");
        assert!(push(&mut reassembler, seg_on(game, 1004, b"EF")).is_empty());

        assert!(push(&mut reassembler, rst_on(game, 1002)).is_empty());

        assert!(reassembler.streams.is_empty());
        let stats = budget.snapshot();
        assert_eq!(
            stats.resyncs_by_cause[ResyncCause::ConnectionReset.index()],
            1
        );
        assert_eq!(
            stats.dominant_resync(),
            Some(ResyncCause::ConnectionReset),
            "received bytes were discarded undelivered, so this is a fault and must name itself"
        );
        assert_eq!(
            stats.resyncs_by_cause[ResyncCause::StreamEvicted.index()],
            0,
            "the stream table had nothing to do with this and must not be blamed for it"
        );
        // Nothing was *refused*, so the rule `push_budgeted` states holds here too.
        assert_eq!(stats.dropped_segments, 0);
        assert_eq!(stats.current_reassembly, 0, "the stranded lease came back");
        assert_eq!(budget.snapshot().current_total, 0);
    }

    /// The ordinary abort — one with nothing outstanding — costs the run
    /// nothing, and is counted as nothing, exactly like an orderly close.
    #[test]
    fn an_abort_with_nothing_outstanding_is_counted_as_nothing() {
        let budget = PipelineBudget::new();
        let mut reassembler = Reassembler::new();
        let push = |reassembler: &mut Reassembler, segment: Segment| {
            flatten_chunks(reassembler.push_budgeted(budget.admit_capture(segment).unwrap()))
        };
        let game = flow();

        assert_eq!(push(&mut reassembler, seg_on(game, 1000, b"AB")), b"AB");
        assert!(push(&mut reassembler, rst_on(game, 1002)).is_empty());

        assert!(reassembler.streams.is_empty());
        assert_eq!(budget.snapshot().resyncs, 0);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    /// An RST anywhere but the next expected byte is ignored, and the flow it
    /// named carries on. A passive tap can check nothing else about a segment,
    /// and an off-window RST is what an off-path attacker forges at a connection
    /// they cannot read — the test RFC 5961 §3.2 tightened real receivers to.
    #[test]
    fn a_reset_away_from_the_next_expected_byte_is_ignored() {
        let mut reassembler = Reassembler::new();
        let game = flow();
        assert_eq!(reassembler.push(&seg_on(game, 1000, b"AB")), b"AB");

        // Ahead of the stream, then behind it. Neither is where the server
        // could be aborting.
        assert!(reassembler.push(&rst_on(game, 1010)).is_empty());
        assert!(reassembler.push(&rst_on(game, 1000)).is_empty());

        assert!(
            reassembler.streams.contains_key(&game),
            "an out-of-window reset must not tear down a live flow"
        );
        assert_eq!(
            reassembler.push(&seg_on(game, 1002, b"CD")),
            b"CD",
            "and the flow must still know where it was"
        );
    }

    /// A close for a flow nothing is tracking is not a reason to track one.
    ///
    /// Two failures it rules out: anchoring a baseline on a connection already
    /// over, and — the expensive one — evicting a live flow to make room for an
    /// entry that would be retired on the very next line.
    #[test]
    fn a_close_for_an_untracked_flow_neither_anchors_nor_evicts() {
        let budget = PipelineBudget::new();
        let mut reassembler = Reassembler::new();
        let push = |reassembler: &mut Reassembler, segment: Segment| {
            flatten_chunks(reassembler.push_budgeted(budget.admit_capture(segment).unwrap()))
        };

        assert!(push(&mut reassembler, fin_on(flow(), 1000, b"")).is_empty());
        assert!(push(&mut reassembler, rst_on(flow(), 1000)).is_empty());
        assert!(reassembler.streams.is_empty());

        // A full table of live flows, then a stranger's close.
        for port in 1..=(MAX_STREAMS as u16) {
            push(&mut reassembler, seg_on(flow_from(port), 1000, b"XY"));
        }
        assert_eq!(reassembler.streams.len(), MAX_STREAMS);
        assert!(push(&mut reassembler, fin_on(flow_from(9000), 1000, b"")).is_empty());

        assert_eq!(reassembler.streams.len(), MAX_STREAMS);
        assert_eq!(
            budget.snapshot().resyncs,
            0,
            "nothing may lose its anchor to a flow that was never going to exist"
        );
        drop(reassembler);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    /// The field scenario, which is the whole point of retiring flows: the game
    /// opens a short connection roughly every 1.7 s — ~7 segments each — and
    /// used to leave every one of them in the table. Sixty-four slots filled in
    /// about two minutes, and from then on every new connection evicted one: 46
    /// evictions in ~90 s, on a run in which nothing was wrong.
    ///
    /// Four times `MAX_STREAMS` connections here, so a table that still filled
    /// would have overflowed three times over.
    #[test]
    fn repeated_short_connections_leave_the_table_empty_instead_of_full() {
        let budget = PipelineBudget::new();
        let mut reassembler = Reassembler::new();
        let push = |reassembler: &mut Reassembler, segment: Segment| {
            flatten_chunks(reassembler.push_budgeted(budget.admit_capture(segment).unwrap()))
        };

        for connection in 0..(MAX_STREAMS as u16 * 4) {
            // A fresh ephemeral client port each time, as a real reconnection
            // has: the same key would be reused rather than accumulate.
            let flow = flow_from(51_000u16.wrapping_add(connection));
            let base = 1_000_u32.wrapping_add(u32::from(connection) * 10_000);
            // SYN-ACK, six data segments, FIN — the ~7 segments measured.
            assert!(push(&mut reassembler, seg_in(flow, base, true, b"")).is_empty());
            for index in 0..6u32 {
                let seq = base.wrapping_add(1 + index * 2);
                assert_eq!(push(&mut reassembler, seg_on(flow, seq, b"AB")), b"AB");
            }
            assert!(push(&mut reassembler, fin_on(flow, base.wrapping_add(13), b"")).is_empty());
            assert!(
                reassembler.streams.is_empty(),
                "connection {connection} was closed and must not still hold a slot"
            );
        }

        let stats = budget.snapshot();
        assert_eq!(
            stats.resyncs, 0,
            "256 clean connections cost this run nothing; they used to cost it a re-anchor each"
        );
        assert_eq!(
            stats.resyncs_by_cause[ResyncCause::StreamReclaimed.index()],
            0,
            "no flow was crowded out, because none of them was still there to crowd"
        );
        assert_eq!(stats.dominant_resync(), None);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    #[test]
    fn clear_resets_baseline_for_resync() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
        r.clear();
        assert_eq!(r.push(&seg(9000, false, b"XY")), b"XY");
    }

    #[test]
    fn a_new_syn_resets_the_reused_flow_and_leaves_every_other_one_alone() {
        let mut r = Reassembler::new();
        let reused = flow();
        let unrelated = flow_from(52000);

        assert!(r.push(&seg(999, true, b"")).is_empty());
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
        assert!(r.push(&seg(1004, false, b"EF")).is_empty()); // buffered, gap-bound.
        assert_eq!(r.push(&seg_on(unrelated, 3000, b"UV")), b"UV");

        assert!(r.push(&seg(8999, true, b"")).is_empty());

        let fresh = &r.streams[&reused];
        assert_eq!(fresh.baseline, Some(9000));
        assert_eq!(fresh.syn_seq, Some(8999));
        assert_eq!(fresh.next_off, 0);
        assert!(
            fresh.pending.is_empty(),
            "the previous incarnation's gap buffer must not survive the reset"
        );
        let untouched = &r.streams[&unrelated];
        assert_eq!(untouched.baseline, Some(3000));
        assert_eq!(untouched.next_off, 2);
    }

    #[test]
    fn same_syn_retransmission_preserves_pending_data() {
        let mut r = Reassembler::new();
        assert!(r.push(&seg(999, true, b"")).is_empty());
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");
        assert!(r.push(&seg(1004, false, b"EF")).is_empty());

        assert!(r.push(&seg(999, true, b"")).is_empty());
        assert_eq!(r.push(&seg(1002, false, b"CD")), b"CDEF");
        assert!(r.push(&seg(1004, false, b"EF")).is_empty());
    }

    #[test]
    fn late_matching_syn_does_not_reset_midstream_anchor() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, b"AB")), b"AB");

        assert!(r.push(&seg(999, true, b"")).is_empty());
        assert_eq!(r.push(&seg(1002, false, b"CD")), b"CD");

        let half = &r.streams[&flow()];
        assert_eq!(half.baseline, Some(1000));
        assert_eq!(half.syn_seq, Some(999));
        assert_eq!(half.next_off, 4);
    }

    #[test]
    fn data_bearing_syn_is_delivered_once() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(999, true, b"AB")), b"AB");
        assert!(r.push(&seg(999, true, b"AB")).is_empty());
    }

    #[test]
    fn new_syn_handles_wrapped_data_sequence() {
        let mut r = Reassembler::new();
        assert!(r.push(&seg(u32::MAX, true, b"")).is_empty());
        assert_eq!(r.streams[&flow()].baseline, Some(0));
        assert_eq!(r.push(&seg(0, false, b"AB")), b"AB");
    }
}
