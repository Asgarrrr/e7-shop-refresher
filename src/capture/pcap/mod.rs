//! Npcap capture backend: an unelevated, adapter-agnostic tap.
//!
//! # Why this exists
//!
//! `WinDivert`, the backend this replaced, needed a kernel driver load and
//! administrator rights — the deleted elevated broker, named pipe and UAC
//! prompt (3 274 lines) existed only to contain that. Npcap can run
//! driverless for ordinary users (`AdminOnly` off by default); a measured
//! probe confirmed non-elevated capture (82/82 packets parsed, Wi-Fi,
//! `DLT_EN10MB`), and that probe became this backend (see
//! `docs/capture-backend-choice.md`). The exe still requires administrator,
//! but for the actuator, not capture: it can't click a window at higher
//! integrity, and Epic Seven inherits that from STOVE. See `build.rs`.
//!
//! # Why `wpcap.dll` is loaded by hand
//!
//! Static linking (`wpcap.lib`, as the `pcap` crate does) needs the Npcap
//! SDK to build and kills the shipped exe in the Windows loader, before
//! `main`, on any machine without Npcap. `libloading` keeps the binary
//! startable everywhere and turns a missing Npcap into an ordinary
//! [`Error::Capture`] naming the download page.
//!
//! # Why every adapter is opened, and none is selected
//!
//! [`PcapSource::open`] opens every device, one thread and BPF filter each.
//! An idle adapter costs a parked thread and about a megabyte of kernel
//! ring, nothing per packet. That buys away adapter-selection heuristics:
//! the dev machine's Ethernet held an APIPA address while Wi-Fi carried the
//! traffic, so "default route" or "real IP" would guess wrong, and it
//! survives adapter switches with no extra code. Duplicate packets are
//! already handled — [`crate::stream`] dedupes by TCP sequence number for
//! ordinary retransmissions anyway.
//!
//! # How the three files divide it
//!
//! This root holds [`PacketSource`]: channel, funnel counters, lifecycle,
//! diagnostics. [`link`] is the frame-to-IP-packet strip, the only layer
//! with no FFI. [`sys`] is the `wpcap.dll` boundary, one file deliberately;
//! its header says why.

mod link;
mod sys;

use std::num::NonZeroU16;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use super::{CaptureStop, PacketSource, Segment, parse_segment};
use crate::error::{Error, Result};
use sys::{INSTALL_HINT, READ_TIMEOUT_MS, SNAPLEN, Wpcap, capture_loop, enumerate, open_device};

/// Packet frames between two funnel log lines. Every captured packet passes
/// through the funnel, plus once on the very first packet, so a capture about
/// to reject everything says so immediately, not after five hundred packets.
const FUNNEL_LOG_EVERY: u64 = 500;

/// Stripped frames the funnel holds before a capture thread starts dropping
/// them.
///
/// This queue is the one place a captured frame sits *outside*
/// [`crate::stream::PipelineBudget`]: nothing is charged until
/// `admit_capture`, which runs after [`PacketSource::next_segment`] has
/// already dequeued. So whatever it holds is added to the pipeline's stated
/// 32 MiB ceiling rather than counted inside it, and the depth exists to keep
/// that addition small and *stated*. An item is at most [`SNAPLEN`] = 262 144
/// bytes, so sixteen of them are 4 MiB — 12.5 % on top of the ceiling, against
/// the unbounded `channel()` this replaced, which had no ceiling at all and no
/// counter to say how far past one it had gone. At the frame sizes actually
/// measured here it is far less: the largest receive-coalesced packet the
/// feasibility probe saw was 48 870 bytes (764 KiB for sixteen), and a
/// full-MTU frame gives 24 KiB.
///
/// Sixteen is a jitter buffer, not a queue. N capture threads each memcpy a
/// frame while one consumer parses it, and the depth only has to cover the
/// consumer being descheduled — the kernel filter admits one TCP port's
/// server-to-client traffic, not a link's worth. It also bounds *teardown*,
/// which the unbounded version did not: the receiver drains and parses every
/// queued frame before it can see the disconnect.
const FRAME_QUEUE_DEPTH: usize = 16;

// --- Public source ---------------------------------------------------------

/// Where packets that reach this process go to die.
///
/// `delivered` counts frames pulled off every adapter, stripped of their
/// link header; `admitted` and `unparsed` are the two ways those frames end.
/// Only `parse_segment` refusing a frame — malformed, or not the game server
/// talking — can drop a packet here, so it alone explains a healthy-looking
/// session that yields nothing. `delivered` staying at zero means the
/// adapters are open but the kernel filter matches no traffic.
#[derive(Default)]
struct Funnel {
    delivered: u64,
    unparsed: u64,
    admitted: u64,
}

impl Funnel {
    fn report(&self) {
        if self.delivered != 1 && !self.delivered.is_multiple_of(FUNNEL_LOG_EVERY) {
            return;
        }
        log_funnel(self);
    }
}

/// The rare half of [`Funnel::report`], kept out of line: `report` runs twice
/// per delivered packet — the hottest path in this crate — so it should carry
/// only the modulus test.
#[cold]
#[inline(never)]
fn log_funnel(funnel: &Funnel) {
    debug!(
        delivered = funnel.delivered,
        admitted = funnel.admitted,
        unparsed = funnel.unparsed,
        "capture funnel"
    );
}

/// A capture thread's parting message, sent only when it stopped on an error.
///
/// Out of band from the frames, and it has to be. The frame funnel is
/// bounded, so a report pushed through it could be discarded by the very
/// congestion it exists to survive; a *blocking* send of it would be worse
/// still, because that funnel's receiver is a field of [`PcapSource`], dropped
/// only after that struct's [`Drop`] has already tried to join this thread —
/// a producer parked in `send` would be waiting on the thread that is waiting
/// on it. This channel is therefore the unbounded `channel()`, which stays
/// honest because it is bounded by construction: each capture thread sends at
/// most one message, on one path, so the queue can never hold more messages
/// than the machine has adapters.
pub(super) struct AdapterFailure {
    pub(super) device: String,
    /// Frames this adapter pulled off the wire before it died.
    ///
    /// Every one of them passed the kernel filter `tcp and src port
    /// {game_port}`, so a non-zero count means this adapter was on the path
    /// the game server's traffic actually takes — which is the whole reason
    /// its death matters while an idle sibling's does not.
    pub(super) delivered: u64,
    pub(super) error: String,
}

/// The two timings [`PcapSource::next_segment`] runs on.
///
/// Fields rather than constants only so a test can shrink both: the behaviour
/// they pace — what this side of the channel does when a producer dies — is
/// otherwise reachable only from a machine with Npcap and a cooperative
/// driver failure. They travel together because neither means anything
/// without the other.
#[derive(Clone, Copy)]
struct Pacing {
    /// How long the receiver parks on the funnel before looking at anything
    /// else. Matched to [`READ_TIMEOUT_MS`] because that is already the
    /// cadence at which anything in this backend notices anything; a quiet
    /// tap costs five wakeups a second on one thread.
    poll: Duration,
    /// How long the funnel may stay silent *after* the adapter that was
    /// carrying the game's traffic died, before the session is declared over.
    ///
    /// Not a bare stall watchdog, and deliberately not one: silence alone is
    /// indistinguishable from a player who is not in the shop, so a timer on
    /// silence would end healthy sessions. This one is armed only by an
    /// observed death, which makes it a question with a real answer — is
    /// anyone *else* still carrying these packets? On a machine where two
    /// adapters see the same frames (Hyper-V's vSwitch beside the physical
    /// NIC; the module header's dedupe note assumes exactly that), the answer
    /// is yes and the first packet to arrive disarms this. Five seconds is
    /// not measured against the game's traffic cadence — nothing here has
    /// measured that — it is chosen as far longer than a duplicate adapter
    /// needs to prove itself, and its false-positive cost is an accurate
    /// error and the relaunch the app already offers, against a session that
    /// hangs forever.
    blind: Duration,
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            poll: Duration::from_millis(READ_TIMEOUT_MS.cast_unsigned().into()),
            blind: Duration::from_secs(5),
        }
    }
}

/// A [`PacketSource`] fed by one capture thread per adapter.
pub struct PcapSource {
    /// Stripped IP packets, funnelled from every capture thread.
    ///
    /// Bounded, at [`FRAME_QUEUE_DEPTH`]. This was an unbounded channel, and
    /// the argument for that is worth keeping because half of it still
    /// holds: a bounded channel *would* park a capture thread outside the
    /// driver whenever the consumer lagged, overflowing the kernel ring
    /// behind it and turning a transient stall into unrecoverable loss. True
    /// of a **blocking** bounded send, which is why nothing here blocks — a
    /// full funnel drops the newest frame, counts it, and raises the same
    /// `capture_loss` flag the driver's own `ps_drop` raises, so the pipeline
    /// re-anchors rather than stalls (see `sys::forward`). The other half of
    /// that argument was wrong: the kernel filter rate-limits producers to
    /// one TCP port's traffic, which bounds the *average* rate and says
    /// nothing at all about depth, so the queue's worst case was the
    /// process's address space — memory
    /// [`crate::stream::PipelineBudget`] cannot see, for the reason
    /// [`FRAME_QUEUE_DEPTH`] gives.
    packets: Receiver<Vec<u8>>,
    /// One message per capture thread that died on an error. See
    /// [`AdapterFailure`] for why this is not the channel above.
    failures: Receiver<AdapterFailure>,
    /// Every failure reaped so far, as `device: reason`, so that the
    /// disconnect at the end of the session can say what happened instead of
    /// reporting only that it happened.
    failed: Vec<String>,
    /// When the adapter carrying the traffic died and nothing has arrived
    /// since. Cleared by the next packet, whoever delivers it.
    blind_since: Option<Instant>,
    pacing: Pacing,
    game_port: NonZeroU16,
    /// Set when any capture thread's `pcap_stats` drop counter moves, or when
    /// one has to drop a frame because this funnel is full; cleared by
    /// `take_capture_loss`.
    capture_loss: Arc<AtomicBool>,
    /// Shared with [`PcapStop`], and with every capture thread.
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    funnel: Funnel,
}

/// Remote wake for a [`PcapSource`] parked on its channel.
///
/// A flag, not a handle operation: calling `pcap_close` from another thread
/// while a receive is in flight is a use-after-free — [`CaptureStop`]'s
/// contract in [`super`] forbids that, after it burned this codebase once.
/// Teardown only stores `true`; each capture thread notices within one
/// [`sys::READ_TIMEOUT_MS`] window, closes its own handle, and drops its
/// sender. When the last sender goes, the receiver in
/// [`PacketSource::next_segment`] wakes with a disconnect, which is the one
/// thing that ends that call for good — it also wakes every [`Pacing::poll`],
/// but only to look around and park again. Idempotent: storing `true` twice
/// is storing `true`.
///
/// `Relaxed` throughout, on this flag and `capture_loss`: the boolean is the
/// whole message, so there's no payload for `Acquire` to acquire — teardown
/// never touches another thread's handle, and packets publish through the
/// channel, which carries its own edge.
pub(crate) struct PcapStop {
    stop: Arc<AtomicBool>,
}

impl CaptureStop for PcapStop {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl PcapSource {
    /// Opens every usable adapter and starts capturing.
    ///
    /// Blocking, and quick: enumeration plus one `pcap_open_live` and one
    /// filter compile per device — nothing waits on a human, unlike the
    /// backend it replaced, which put a UAC prompt in the middle of this
    /// call. A device that fails to open, or reports a link type
    /// [`link::LinkStrip`] can't see past, is logged and skipped so one
    /// refusing adapter doesn't block a machine with a dozen virtual ones.
    /// Only zero usable devices is fatal.
    ///
    /// # Errors
    ///
    /// Always [`Error::Capture`], with a player-readable message, from one of
    /// three causes:
    ///
    /// - `wpcap.dll` could not be loaded from either candidate path, or is
    ///   missing one of the thirteen symbols this backend needs — Npcap is
    ///   not installed, or too old. Message names the download page.
    /// - no adapter survived [`sys::open_device`]; [`no_usable_device_error`]
    ///   distinguishes admin-restricted driver, no device at all, and every
    ///   device refused with its own reason.
    /// - a capture thread failed to spawn — the one cause unrelated to the
    ///   Npcap install. [`start_capture_threads`] stops and joins the ones
    ///   that did start before this returns; [`Drop`] cannot do it, because
    ///   the `Self` whose `Drop` it would be is never constructed on this
    ///   path.
    pub(crate) fn open(game_port: NonZeroU16) -> Result<(Self, PcapStop)> {
        let (wpcap, loaded_from) = Wpcap::load()?;
        // No `plain_name_resolved` field any more: there is no plain name to
        // resolve, because resolving one is what let a `wpcap.dll` beside the
        // exe run at high integrity. The path carries what that boolean did —
        // ending in `System32\wpcap.dll` rather than `System32\Npcap\wpcap.dll`
        // is the WinPcap-compatible install.
        info!(
            path = %loaded_from.display(),
            version = %wpcap.version(),
            "wpcap.dll loaded"
        );
        let wpcap = Arc::new(wpcap);

        let devices = enumerate(&wpcap)?;
        // Only the game server's own source port: everything else is
        // discarded in the driver rather than copied to user space first.
        let filter = format!("tcp and src port {game_port}");

        let mut handles = Vec::new();
        let mut refused = Vec::new();
        for device in &devices {
            match open_device(&wpcap, device, &filter) {
                Ok(handle) => handles.push(handle),
                Err(reason) => {
                    warn!(device = %device, reason = %reason, "skipping adapter");
                    refused.push(Refusal {
                        device: device.clone(),
                        reason,
                    });
                }
            }
        }

        if handles.is_empty() {
            return Err(no_usable_device_error(&devices, &refused));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let capture_loss = Arc::new(AtomicBool::new(false));
        let (sender, packets) = sync_channel(FRAME_QUEUE_DEPTH);
        let (failed_sender, failures) = channel();
        // `handle.device` is borrowed only for `format!`, fully evaluated
        // before any closure exists, so the thread name needs no clone.
        let named: Vec<(String, _)> = handles
            .into_iter()
            .map(|handle| {
                (
                    format!("pcap-{}", short_device_name(&handle.device)),
                    handle,
                )
            })
            .collect();
        let mut threads = Vec::with_capacity(named.len());
        start_capture_threads(&stop, &mut threads, named, |name, handle| {
            let sender = sender.clone();
            let failed_sender = failed_sender.clone();
            let stop = Arc::clone(&stop);
            let capture_loss = Arc::clone(&capture_loss);
            std::thread::Builder::new().name(name).spawn(move || {
                capture_loop(handle, &sender, &failed_sender, &stop, &capture_loss);
            })
        })?;
        // The original senders must go, or the receivers could never observe a
        // disconnect and `next_segment` would block forever after teardown.
        drop(sender);
        drop(failed_sender);

        info!(
            adapters = threads.len(),
            skipped = refused.len(),
            filter = %filter,
            snaplen = SNAPLEN,
            queue_depth = FRAME_QUEUE_DEPTH,
            "Npcap capture open (passive copy; originals untouched)"
        );

        Ok((
            Self {
                packets,
                failures,
                failed: Vec::new(),
                blind_since: None,
                pacing: Pacing::default(),
                game_port,
                capture_loss,
                stop: Arc::clone(&stop),
                threads,
                funnel: Funnel::default(),
            },
            PcapStop { stop },
        ))
    }
}

/// Starts one named thread per adapter, and — the reason this is a function at
/// all — stops and joins everything it already started if any spawn fails.
///
/// The loop this replaces returned through a bare `?`, before `PcapSource` was
/// constructed, so no [`Drop`] existed to run: the flag was never set, the
/// `Vec<JoinHandle>` was dropped (which detaches), and every thread already
/// spawned kept its `pcap_t` and its `Arc<Wpcap>` alive for the life of the
/// process. They could not even exit on their own. `capture_loop`'s only other
/// way out is the receiver going away, which it learns about by *sending* — and
/// [`PcapSource::open`] deliberately opens every device, so most of those
/// threads are on adapters that will never match the filter and never send
/// anything. They sat on 200 ms `pcap_next_ex` timeouts forever. The app
/// reports a failed capture and offers a relaunch, so it compounded once per
/// attempt.
///
/// Generic over the adapter only so that failure path is reachable from a test:
/// `open` instantiates it at `sys::Handle`, which needs a live `pcap_t` and
/// therefore a machine with Npcap, while the cleanup being asserted is pure
/// thread lifecycle.
fn start_capture_threads<A>(
    stop: &AtomicBool,
    threads: &mut Vec<JoinHandle<()>>,
    adapters: Vec<(String, A)>,
    spawn: impl Fn(String, A) -> std::io::Result<JoinHandle<()>>,
) -> Result<()> {
    for (name, adapter) in adapters {
        match spawn(name, adapter) {
            Ok(thread) => threads.push(thread),
            Err(err) => {
                stop_and_join(stop, threads);
                return Err(Error::Capture(format!("spawning a capture thread: {err}")));
            }
        }
    }
    Ok(())
}

/// Sets the stop flag and joins, the one way capture threads are ever ended.
///
/// Shared by [`start_capture_threads`]'s abandoned-open path and
/// [`PcapSource::drop`] because they are the same operation on the same
/// invariant, not because the second happened to be convenient: a capture
/// thread notices the flag within one [`READ_TIMEOUT_MS`] window, so the join
/// costs about that much once, not once per thread.
fn stop_and_join(stop: &AtomicBool, threads: &mut Vec<JoinHandle<()>>) {
    stop.store(true, Ordering::Relaxed);
    for thread in threads.drain(..) {
        if thread.join().is_err() {
            warn!("a capture thread panicked");
        }
    }
}

impl Drop for PcapSource {
    /// Stops and joins the capture threads.
    ///
    /// Teardown normally goes through [`PcapStop`] first, so this is usually
    /// a no-op join of already-finished threads. It exists for the paths
    /// that don't — otherwise a dropped source with its stop handle still
    /// alive would leave threads capturing into a channel nobody reads.
    fn drop(&mut self) {
        stop_and_join(&self.stop, &mut self.threads);
    }
}

impl PcapSource {
    /// Turns the capture threads' parting messages into either an armed
    /// deadline or a log line, depending on whether the dead adapter was
    /// carrying anything.
    ///
    /// There was no such path at all before, and its absence was the whole
    /// bug: a `pcap_next_ex` error warned and ended one thread, its siblings
    /// kept the channel alive, and this side stayed parked in `recv` forever.
    /// `app::ingest` blocks on `next_segment` with no timeout, so its
    /// `fatal.blocking_send` never fired, nothing reached the journal, and the
    /// window went on showing a running session that would never see another
    /// packet. Five adapters open and the one carrying the game disabled, its
    /// driver reset, or Npcap upgraded under it — that was the shape of it.
    fn reap_failures(&mut self) {
        while let Ok(failure) = self.failures.try_recv() {
            let device = short_device_name(&failure.device);
            self.failed.push(format!("{device}: {}", failure.error));
            if failure.delivered == 0 {
                // This backend opens every adapter precisely because it has no
                // reason to believe in any of them, so an idle one dying costs
                // nothing and must not cost the session either. It is still
                // kept, for the disconnect message: "every capture thread
                // exited" is otherwise unattributable.
                warn!(
                    device,
                    error = %failure.error,
                    "an idle adapter stopped capturing"
                );
                continue;
            }
            warn!(
                device,
                delivered = failure.delivered,
                error = %failure.error,
                "the adapter carrying the game's traffic stopped capturing"
            );
            self.blind_since.get_or_insert_with(Instant::now);
        }
    }

    /// The fatal half of [`Self::reap_failures`], separated because it is a
    /// question about *elapsed silence* rather than about a message: an
    /// adapter that was delivering has died and nothing has replaced it. See
    /// [`Pacing::blind`] for why this is armed by a death and not by a timer.
    fn report_if_blind(&self) -> Result<()> {
        let Some(since) = self.blind_since else {
            return Ok(());
        };
        if since.elapsed() < self.pacing.blind {
            return Ok(());
        }
        Err(Error::Capture(format!(
            "the adapter that was carrying the game's traffic stopped capturing, and no packet \
             has reached this process in the {:?} since — the tap is blind: {}",
            self.pacing.blind,
            self.failed.join("; ")
        )))
    }

    /// Every capture thread exited: either the stop flag was set (teardown) or
    /// every adapter errored out. Either way this is the end of the source,
    /// and the reasons collected on the way are the only account of which.
    fn tap_closed(&self) -> Error {
        if self.failed.is_empty() {
            return Error::Capture(
                "every Npcap capture thread exited — the tap is closed".to_owned(),
            );
        }
        Error::Capture(format!(
            "every Npcap capture thread exited — the tap is closed: {}",
            self.failed.join("; ")
        ))
    }
}

impl PacketSource for PcapSource {
    fn next_segment(&mut self) -> Result<Segment> {
        loop {
            let packet = match self.packets.recv_timeout(self.pacing.poll) {
                Ok(packet) => {
                    // Whoever sent this, the tap is not blind: a frame that
                    // got here passed the kernel filter, so some adapter is
                    // still on the game server's path.
                    self.blind_since = None;
                    packet
                }
                // The funnel went quiet for one poll — the only moment this
                // side of the channel gets to look at anything but a packet,
                // and the only moment at which looking is worth anything.
                Err(RecvTimeoutError::Timeout) => {
                    self.reap_failures();
                    self.report_if_blind()?;
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.reap_failures();
                    return Err(self.tap_closed());
                }
            };

            self.funnel.delivered += 1;
            // The link header is already gone: each capture thread strips its
            // own adapter's framing, since the strip length depends on the
            // adapter and nothing down here knows which one a packet came
            // from. What arrives is a raw IP packet, the only shape
            // `parse_segment` accepts, handed over by value so this buffer
            // becomes the segment's payload rather than being copied.
            let Some(segment) = parse_segment(packet, self.game_port) else {
                self.funnel.unparsed += 1;
                self.funnel.report();
                continue;
            };

            self.funnel.admitted += 1;
            if self.funnel.admitted == 1 {
                // Anything admitted was sent by the game server —
                // `parse_segment` accepts nothing else — so this line proves
                // the filter, port, adapter choice, and link-layer strip all
                // agree. Its absence in a session log means capture is open
                // but sees nothing from the game server. Logs the client's
                // port, not its address: on IPv6 that address is the
                // player's globally routable one, in a file they're asked to
                // email.
                info!(
                    payload = segment.payload.len(),
                    syn = segment.syn,
                    server = %segment.flow.server,
                    client_port = segment.flow.client.port(),
                    "first server-to-client segment admitted"
                );
            }
            self.funnel.report();
            return Ok(segment);
        }
    }

    fn take_capture_loss(&mut self) -> bool {
        // `swap`, not load-then-store: the read-and-clear must be atomic
        // against a capture thread setting it again. `Relaxed` because an RMW
        // on one location is ordered against every other RMW on it
        // regardless, and there's no payload behind the flag (see [`PcapStop`]).
        self.capture_loss.swap(false, Ordering::Relaxed)
    }
}

// --- No usable device: saying which of the three things happened -----------

/// Why one adapter didn't make the capture set, collected for the
/// zero-usable-device error.
struct Refusal {
    device: String,
    reason: String,
}

/// Turns "no adapter survived" into a message that names a cause.
///
/// Two failures are indistinguishable from return codes alone: Npcap
/// restricted to administrators hands back an empty device list (or
/// access-denied opens) from an unelevated process, identical to a machine
/// with no adapters at all. The registry's `AdminOnly` value settles it — 0
/// on the machine this backend was measured on.
fn no_usable_device_error(devices: &[String], refused: &[Refusal]) -> Error {
    if npcap_admin_only().is_some_and(|value| value != 0) {
        return Error::Capture(
            // No "or run this app elevated": the shipped exe carries a
            // `requireAdministrator` manifest, so a player reading this has
            // already approved a UAC prompt and has nothing left to raise.
            // That half of the advice dates from the unelevated-capture probe
            // and pointed at a non-fix. Reinstalling is the only lever.
            "Npcap is installed but its driver is restricted to administrators \
             (HKLM\\SYSTEM\\CurrentControlSet\\Services\\npcap\\Parameters\\AdminOnly is set): \
             reinstall Npcap with \"Restrict Npcap driver's access to Administrators\" unchecked"
                .to_owned(),
        );
    }
    if devices.is_empty() {
        return Error::Capture(format!(
            "Npcap enumerated no capture device at all — if this machine has a working \
             network adapter, the driver is probably not running: {INSTALL_HINT}"
        ));
    }
    let reasons = refused
        .iter()
        .map(|refusal| format!("{}: {}", short_device_name(&refusal.device), refusal.reason))
        .collect::<Vec<_>>()
        .join("; ");
    Error::Capture(format!(
        "none of the {} adapter(s) Npcap enumerated could be captured on — {reasons}",
        devices.len()
    ))
}

/// The `AdminOnly` value of the Npcap service, or `None` when it is unset or
/// unreadable (which Npcap treats as permissive).
fn npcap_admin_only() -> Option<u32> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW,
    };

    let subkey = wide(r"SYSTEM\CurrentControlSet\Services\npcap\Parameters");
    let value = wide("AdminOnly");
    let mut data: u32 = 0;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: `subkey` and `value` are NUL-terminated UTF-16 buffers owned by
    // this frame and alive across the call. `data` and `size` are stack slots;
    // `size` is initialized to exactly `data`'s size and `RRF_RT_REG_DWORD`
    // restricts the call to writing a `DWORD` into it, so no over-write is
    // possible. `HKEY_LOCAL_MACHINE` is a predefined key that needs no close.
    // Failure mode: a `WIN32_ERROR` return, with `data` untouched — hence the
    // status check before it is read.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut data).cast(),
            &mut size,
        )
    };
    (status == ERROR_SUCCESS).then_some(data)
}

/// NUL-terminated UTF-16, as every `...W` entry point wants it.
///
/// `actuator::win` had a byte-identical copy of this, down to the sizing
/// argument; both now read [`crate::wide`], which carries that argument once.
use crate::wide::wide;

/// `\Device\NPF_{GUID}` is unreadable in a log line; the GUID alone is enough to
/// tell two adapters apart.
fn short_device_name(device: &str) -> &str {
    device.rsplit('\\').next().unwrap_or(device)
}

#[cfg(test)]
impl PcapSource {
    /// A source with no adapters behind it, for the tests below.
    ///
    /// The two channels are handed in so a test can play the part of the
    /// capture threads. [`PcapSource::open`] is unrunnable anywhere without
    /// Npcap and a real adapter (which is why the smoke test below is
    /// `#[ignore]`d), and what needs pinning here — what this side does when
    /// one producer dies while its siblings hold the channel open — lives
    /// entirely on this side of them.
    fn from_channels(
        packets: Receiver<Vec<u8>>,
        failures: Receiver<AdapterFailure>,
        game_port: NonZeroU16,
        pacing: Pacing,
    ) -> Self {
        Self {
            packets,
            failures,
            failed: Vec::new(),
            blind_since: None,
            pacing,
            game_port,
            capture_loss: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
            funnel: Funnel::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::sync_channel;

    use super::*;

    fn game_port() -> NonZeroU16 {
        NonZeroU16::new(3333).expect("3333 is not zero")
    }

    /// The production pacing with both delays collapsed: these tests assert
    /// *what* happens when an adapter dies, not how long the backend waits
    /// first, and the wait is a field precisely so they need not pay it.
    fn immediate() -> Pacing {
        Pacing {
            poll: Duration::from_millis(5),
            blind: Duration::ZERO,
        }
    }

    /// How long a test waits for a call that must not park forever. Generous:
    /// its only job is to turn "blocks for the life of the process" into a
    /// named failure instead of a suite that hangs.
    const NEVER_PARKS: Duration = Duration::from_secs(5);

    #[test]
    fn a_dead_adapter_ends_the_session_even_while_its_siblings_hold_the_funnel_open() {
        let (frames, packets) = sync_channel(FRAME_QUEUE_DEPTH);
        let (report, failures) = channel();
        // The siblings: still capturing, still holding a sender, and with
        // nothing to say because the kernel filter matches nothing on their
        // adapters. That alone used to be enough to park `next_segment` in
        // `recv` for the rest of the process's life — the receiver's only
        // `Err` is the all-senders-gone disconnect, and these two never go.
        let siblings = (frames.clone(), frames);
        let mut source = PcapSource::from_channels(packets, failures, game_port(), immediate());
        report
            .send(AdapterFailure {
                device: r"\Device\NPF_{THE-ONE-THAT-MATTERED}".to_owned(),
                delivered: 7,
                error: "pcap_next_ex: the driver was reset".to_owned(),
            })
            .expect("the source is alive");

        let (done, ended) = channel();
        std::thread::spawn(move || {
            let _ = done.send(source.next_segment().err().map(|err| err.to_string()));
        });
        let outcome = ended.recv_timeout(NEVER_PARKS).expect(
            "next_segment must not park forever when the adapter that was delivering dies \
             and its idle siblings keep the funnel open",
        );

        let reported = outcome.expect("a dead delivering adapter is the end of the source");
        assert!(
            reported.contains("THE-ONE-THAT-MATTERED"),
            "the message must name the adapter: {reported}"
        );
        assert!(
            reported.contains("the driver was reset"),
            "and the driver's own reason: {reported}"
        );
        drop(siblings);
    }

    #[test]
    fn an_idle_adapter_dying_costs_its_own_thread_and_nothing_else() {
        let (frames, packets) = sync_channel(FRAME_QUEUE_DEPTH);
        let (report, failures) = channel();
        let sibling = frames.clone();
        let mut source = PcapSource::from_channels(packets, failures, game_port(), immediate());
        // Delivered nothing, so it was never on the game's path: this backend
        // opens every device it can, and most of them are idle by design.
        report
            .send(AdapterFailure {
                device: r"\Device\NPF_{IDLE}".to_owned(),
                delivered: 0,
                error: "pcap_next_ex: the adapter was disabled".to_owned(),
            })
            .expect("the source is alive");
        drop(report);
        // Every capture thread is now gone, which is the only thing that may
        // end this source — and the reason it ended is carried into the
        // message rather than left in a log line nobody correlates.
        drop(sibling);
        drop(frames);

        let reported = source
            .next_segment()
            .expect_err("the funnel is closed")
            .to_string();
        assert!(reported.contains("NPF_{IDLE}"), "{reported}");
        assert!(reported.contains("the adapter was disabled"), "{reported}");
    }

    #[test]
    fn a_failed_spawn_stops_and_joins_the_capture_threads_that_did_start() {
        // The shape of `open`'s spawn loop when the third `Builder::spawn`
        // fails with two threads already running. Before, the `?` on that line
        // returned before `PcapSource` existed, so nothing set the flag and
        // nothing joined: two threads spinning on a stop flag that would never
        // be set, each holding an open `pcap_t` and pinning `wpcap.dll`, for
        // the life of the process — and again on the next relaunch.
        let stop = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicUsize::new(0));
        let attempts = AtomicUsize::new(0);
        let mut threads = Vec::new();
        let adapters = vec![
            ("pcap-a".to_owned(), ()),
            ("pcap-b".to_owned(), ()),
            ("pcap-c".to_owned(), ()),
        ];

        let failure = start_capture_threads(&stop, &mut threads, adapters, |name, ()| {
            if attempts.fetch_add(1, Ordering::Relaxed) == 2 {
                return Err(std::io::Error::other("the process is out of threads"));
            }
            let stop = Arc::clone(&stop);
            let exited = Arc::clone(&exited);
            std::thread::Builder::new().name(name).spawn(move || {
                // `capture_loop`'s shape: the flag is the only exit these ever
                // get, since an adapter matching nothing never sends and so
                // never learns that the receiver is gone.
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                exited.fetch_add(1, Ordering::Relaxed);
            })
        })
        .expect_err("the third spawn fails");

        assert!(
            failure.to_string().contains("out of threads"),
            "the OS error is what the player's log needs: {failure}"
        );
        assert!(
            stop.load(Ordering::Relaxed),
            "the orphans' only exit is the stop flag"
        );
        assert_eq!(
            exited.load(Ordering::Relaxed),
            2,
            "both threads must have been joined, not detached and left spinning"
        );
        assert!(threads.is_empty(), "nothing is handed back to leak");
    }

    /// Live smoke check, never run by CI (`#[ignore]`; CI has neither an
    /// adapter nor Npcap). Run by hand on a machine with Npcap to confirm the
    /// dynamic load, enumeration, and per-adapter open still work:
    ///
    /// ```text
    /// cargo test --no-default-features --features pcap-backend -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs Npcap and a real adapter"]
    fn the_tap_opens_on_this_machine_without_elevation() {
        let (source, mut stop) = PcapSource::open(NonZeroU16::new(3333).expect("3333 is not zero"))
            .expect("open the Npcap tap");
        assert!(!source.threads.is_empty(), "at least one adapter must open");
        println!("adapters capturing: {}", source.threads.len());
        stop.stop();
    }

    #[test]
    fn a_device_path_is_logged_by_its_trailing_component() {
        assert_eq!(
            short_device_name(r"\Device\NPF_{A1B2-C3D4}"),
            "NPF_{A1B2-C3D4}"
        );
        assert_eq!(short_device_name("lo0"), "lo0");
    }
}
