//! Passive traffic capture and the packet-source abstraction.

mod ip;
// Not under the backend's gate, though `capture::pcap` is its only consumer
// today. It holds no `unsafe`, no raw pointer and no FFI, so the gate bought
// nothing and cost it four of the six verification lanes — including both
// `just verify` test lanes, which is where its ⚠ Untested VLAN cases most
// needed to run. See its module doc.
mod link;

// The one capture backend. Without `pcap-backend` the crate has no way to
// capture at all, and says so (see `app::build_source`).
#[cfg(all(windows, feature = "pcap-backend"))]
mod pcap;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::Result;

pub use ip::parse_segment;

#[cfg(all(windows, feature = "pcap-backend"))]
pub use pcap::PcapSource;

/// A blocking capture source paired with the capability that wakes it at
/// teardown, so the app cannot start a receive thread without retaining its
/// stop handle.
pub(crate) struct CaptureSource {
    pub(crate) packets: Box<dyn PacketSource>,
    pub(crate) stop: Box<dyn CaptureStop>,
}

#[cfg(any(test, all(windows, feature = "pcap-backend")))]
impl CaptureSource {
    pub(crate) fn new(
        packets: impl PacketSource + 'static,
        stop: impl CaptureStop + 'static,
    ) -> Self {
        Self {
            packets: Box::new(packets),
            stop: Box::new(stop),
        }
    }
}

/// One-shot, idempotent wake capability for a blocking [`PacketSource`].
///
/// Implementations must not close a raw OS handle concurrently with receive.
///
/// Infallible because every implementor's wake is a flag store, which left
/// `stop_and_join`'s error branch dead; widen back to `Result` if a future
/// implementor's wake can genuinely fail.
pub(crate) trait CaptureStop: Send {
    fn stop(&mut self);
}

/// Identifies the TCP connection a segment belongs to.
///
/// Endpoints are stored by role (`server` owns `game_port`), not direction of
/// travel. Only one direction is ever captured (see [`parse_segment`]), so a
/// flow and its server-to-client byte stream are the same thing here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FlowKey {
    pub client: SocketAddr,
    pub server: SocketAddr,
}

/// A captured server-to-client TCP segment, normalized for reassembly.
#[derive(Debug, Clone)]
pub struct Segment {
    pub flow: FlowKey,
    /// TCP sequence number of the first byte of `payload`.
    pub seq: u32,
    pub syn: bool,
    /// The server-to-client bytes: the captured frame's own allocation trimmed
    /// in place (see [`parse_segment`]), so `capacity()` stays the whole
    /// frame's for the segment's life — the memory actually retained, and what
    /// `PipelineBudget::admit_capture` charges.
    pub payload: Vec<u8>,
}

// Size canaries: one of these exists per captured packet and is queued by
// value, so a field added here is paid for on every packet. `repr(Rust)`
// layout is unspecified — a failure means re-measure, not work around.
#[cfg(target_pointer_width = "64")]
const _: () = {
    // Two `SocketAddr` (32 each: the IPv6 variant is 28 bytes plus a tag).
    assert!(size_of::<FlowKey>() == 64);
    // 64 (FlowKey) + 24 (Vec) + 4 (seq) + 1 (syn), padded to 96.
    assert!(size_of::<Segment>() == 96);
};

/// A point-in-time readout of what a [`PacketSource`] has counted since it
/// opened: how many frames it pulled off the wire, how many
/// [`parse_segment`] could not turn into a [`Segment`], and how many were
/// admitted to the pipeline.
///
/// `unparsed` alone climbing explains a healthy-looking session that yields
/// nothing; `delivered` staying at zero means the adapters are open but
/// nothing matches the capture filter. See `ui::capture_health::diagnosis`
/// for where that reasoning is rendered.
///
/// `Copy`, not a reference or a guard: [`PacketSource::counters`] takes a
/// few atomic loads and hands back a value, so a caller never holds anything
/// the capture thread could contend on. Every field zero is also
/// [`Default`] — see [`PacketSource::counters`] for why that default is
/// deliberately ambiguous between "this backend counts nothing" and "no
/// packet yet".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureCounters {
    pub(crate) delivered: u64,
    pub(crate) unparsed: u64,
    pub(crate) admitted: u64,
}

// `record_*`/`snapshot` are only ever called from `capture::pcap` (behind
// `pcap-backend`) and `ui::capture_health` (behind `gui`), and none of the
// four feature lanes `just clippy` runs enables `pcap-backend` at all — so
// every field and method below reads as dead code in each of them
// individually, even though `just backends` (which does enable
// `pcap-backend`) proves them live. Same shape as `sys::LoopCounters`.
#[allow(
    dead_code,
    reason = "used by capture::pcap (pcap-backend) and ui::capture_health (gui); no `just clippy` lane enables pcap-backend"
)]
struct CaptureHealthInner {
    delivered: AtomicU64,
    unparsed: AtomicU64,
    admitted: AtomicU64,
}

/// A [`CaptureCounters`] backed by atomics behind a cheap-to-clone [`Arc`], so
/// several owners can read the same live counters without a lock.
///
/// # Why atomics, and why a reader can never deadlock against the capture thread
///
/// Two threads touch this at very different rates: a capture backend
/// increments it once per delivered packet — the sacred path `Funnel::report`
/// (in [`pcap`], the one backend that increments this today) is argued
/// around — while the window reads a snapshot at 4 Hz on the egui thread. A
/// `Mutex` would make that safe too,
/// but every increment would then be one more thing that can contend a lock
/// the UI thread might be holding mid-frame, and every read one more thing
/// that can contend a lock the capture thread might be mid-increment on: the
/// shape a stall (or, if either side ever panicked while holding it, a
/// poison) comes from. `AtomicU64` with `Relaxed` ordering has no such
/// operation: a `load` or `fetch_add` cannot block, so there is no lock
/// either side can be caught holding when the other one runs — a reader on
/// the UI thread and a writer on the capture thread are simply two
/// independent memory operations, never a rendezvous. `Relaxed` is enough
/// because each counter is its own whole message; nothing else is published
/// *through* the ordering — the same reasoning [`pcap`]'s `PcapStop` already
/// gives for the stop flag it shares the same way.
///
/// Constructed once, before the backend that will increment it exists (see
/// `app::setup`), and cloned twice: once into the backend that counts, once
/// into the handles a view reads. Neither clone can outlive the `Arc`'s last
/// owner dropping it, and there is no channel to close or disconnect to
/// detect — a UI reading after the session ended just keeps seeing the last
/// value, which is exactly what a frozen diagnosis should show.
#[derive(Clone, Default)]
#[allow(
    dead_code,
    reason = "read through `CaptureHealth`'s methods, which have the same reachability gap — see `CaptureHealthInner`"
)]
pub(crate) struct CaptureHealth(Arc<CaptureHealthInner>);

impl Default for CaptureHealthInner {
    fn default() -> Self {
        Self {
            delivered: AtomicU64::new(0),
            unparsed: AtomicU64::new(0),
            admitted: AtomicU64::new(0),
        }
    }
}

#[allow(
    dead_code,
    reason = "used by capture::pcap (pcap-backend) and ui::capture_health (gui); no `just clippy` lane enables pcap-backend"
)]
impl CaptureHealth {
    /// Counts one delivered frame and returns the new total, so a caller
    /// that already needs it (a modulus check, a "first packet" branch) pays
    /// no second load for it.
    pub(crate) fn record_delivered(&self) -> u64 {
        self.0.delivered.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Counts one frame [`parse_segment`] could not turn into a [`Segment`].
    pub(crate) fn record_unparsed(&self) {
        self.0.unparsed.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts one frame admitted to the pipeline, and returns the new total
    /// — see [`Self::record_delivered`] for why.
    pub(crate) fn record_admitted(&self) -> u64 {
        self.0.admitted.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The three counters, read together but not *as one atomic group* — a
    /// reader could observe `admitted` from after a packet that `delivered`
    /// was read from before. Nothing here needs the three to agree to the
    /// packet: the diagnosis this feeds reads in the hundreds or thousands,
    /// never the one, and every field is monotonic, so the worst a torn read
    /// produces is a snapshot one packet older than it could have been.
    pub(crate) fn snapshot(&self) -> CaptureCounters {
        CaptureCounters {
            delivered: self.0.delivered.load(Ordering::Relaxed),
            unparsed: self.0.unparsed.load(Ordering::Relaxed),
            admitted: self.0.admitted.load(Ordering::Relaxed),
        }
    }
}

/// Blocking source of TCP segments. Implementations observe traffic without
/// ever modifying it.
pub trait PacketSource: Send {
    /// Blocks until the next TCP segment matching the filter is captured.
    fn next_segment(&mut self) -> Result<Segment>;

    /// Reports, and clears, whether the backend lost captured packets since
    /// the previous call.
    ///
    /// A passive tap never sees already-ACKed bytes again, so a hole from
    /// backend-side loss can never be filled by retransmission — the capture
    /// loop resyncs instead of waiting for a gap fill that will not arrive.
    /// Backends that cannot lose packets keep the default.
    fn take_capture_loss(&mut self) -> bool {
        false
    }

    /// A snapshot of what this source has counted so far. See
    /// [`CaptureCounters`] for what each field distinguishes and
    /// [`CaptureHealth`] for why atomics are what makes reading it safe from
    /// a thread other than the one calling [`Self::next_segment`].
    ///
    /// Defaults to all zero, which reads honestly for two different
    /// backends at once: one that never counts anything, and one that
    /// simply has not captured a packet yet. The window cannot tell those
    /// apart from the counters alone either, and does not try to — both
    /// render the same unalarmed "no traffic yet" sentence rather than a
    /// diagnosis that presumes a fault. A backend that does count (only
    /// [`PcapSource`], today) overrides this with a live [`CaptureHealth`]
    /// snapshot.
    fn counters(&self) -> CaptureCounters {
        CaptureCounters::default()
    }
}
