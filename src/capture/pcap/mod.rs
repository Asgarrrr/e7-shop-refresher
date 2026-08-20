//! Npcap capture backend: an unelevated, adapter-agnostic tap.
//!
//! `WinDivert`, the backend this replaced, needed a kernel driver load and
//! administrator rights; Npcap runs driverless for ordinary users (`AdminOnly`
//! off by default). The exe still requires administrator for the actuator,
//! not capture: it cannot click a window at higher integrity, and Epic Seven
//! inherits that from STOVE.
//!
//! `wpcap.dll` is loaded by hand because static linking needs the Npcap SDK to
//! build and kills the shipped exe in the Windows loader, before `main`, on any
//! machine without Npcap; `libloading` turns that into an [`Error::Capture`]
//! naming the download page.
//!
//! [`PcapSource::open`] opens *every* adapter rather than selecting one. An
//! idle one costs a parked thread and about a megabyte of kernel ring, nothing
//! per packet, and selection would guess wrong: the dev machine's Ethernet held
//! an APIPA address while Wi-Fi carried the traffic. [`crate::stream`] dedupes
//! by TCP sequence number, so the duplicates cost nothing.
//!
//! [`link`] is the frame-to-IP strip, the only layer with no FFI; [`sys`] is
//! the `wpcap.dll` boundary, deliberately one file — its header says why.

mod link;
mod sys;

use std::num::NonZeroU16;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use super::{CaptureCounters, CaptureHealth, CaptureStop, PacketSource, Segment, parse_segment};
use crate::error::{Error, Result};
use sys::{INSTALL_HINT, READ_TIMEOUT_MS, SNAPLEN, Wpcap, capture_loop, enumerate, open_device};

/// Packet frames between two funnel log lines. Also logged on the very first
/// packet, so a capture about to reject everything says so immediately rather
/// than after five hundred packets.
const FUNNEL_LOG_EVERY: u64 = 500;

/// Stripped frames the funnel holds before a capture thread starts dropping
/// them.
///
/// The one place a captured frame sits *outside*
/// [`crate::stream::PipelineBudget`]: nothing is charged until `admit_capture`,
/// which runs only after [`PacketSource::next_segment`] has dequeued. What this
/// holds is therefore added to the pipeline's stated 32 MiB ceiling rather than
/// counted inside it, and the depth keeps that addition small and *stated*: at
/// most [`SNAPLEN`] an item, so 4 MiB worst case, against an unbounded
/// `channel()` whose worst case was the address space.
///
/// Sixteen is a jitter buffer, not a queue — it only covers the single consumer
/// being descheduled — and it bounds *teardown*, since the receiver drains and
/// parses every queued frame before it can see the disconnect.
const FRAME_QUEUE_DEPTH: usize = 16;

/// Where packets that reach this process go to die.
///
/// `delivered` counts frames pulled off every adapter, already stripped of
/// their link header; `admitted` and `unparsed` are the two ways they end. Only
/// `parse_segment` can drop one here, so `unparsed` alone explains a
/// healthy-looking session that yields nothing, while `delivered` at zero means
/// the adapters are open but the kernel filter matches no traffic.
///
/// A thin wrapper over [`CaptureHealth`], not a fresh set of counters: the
/// atomics it increments are the exact ones [`PcapSource::counters`] hands to
/// the window, so `report`'s `debug!` line and the player's readout can never
/// drift apart — both read [`CaptureHealth::snapshot`], nothing else.
struct Funnel(CaptureHealth);

impl Funnel {
    /// `delivered` is the argument, not a fresh load, because every caller
    /// already has it from [`CaptureHealth::record_delivered`]'s return —
    /// the modulus check this guards costs nothing extra to feed.
    fn report(&self, delivered: u64) {
        if delivered != 1 && !delivered.is_multiple_of(FUNNEL_LOG_EVERY) {
            return;
        }
        log_funnel(self.0.snapshot());
    }
}

/// The rare half of [`Funnel::report`], kept out of line: `report` runs on
/// every delivered packet, so it should carry only the modulus test.
#[cold]
#[inline(never)]
fn log_funnel(counters: CaptureCounters) {
    debug!(
        delivered = counters.delivered,
        admitted = counters.admitted,
        unparsed = counters.unparsed,
        "capture funnel"
    );
}

/// A capture thread's parting message, sent only when it stopped on an error.
///
/// Out of band from the frames, and it has to be: that funnel is bounded, so a
/// report pushed through it could be discarded by the very congestion it exists
/// to survive, and a *blocking* send would deadlock against [`PcapSource`]'s
/// [`Drop`], which joins this thread before dropping the receiver. The unbounded
/// `channel()` stays honest because each thread sends at most one message.
pub(super) struct AdapterFailure {
    pub(super) device: String,
    /// Frames this adapter pulled off the wire before it died.
    ///
    /// Every rung of [`filter_candidates`] admits the game server's source port
    /// and nothing else, so a non-zero count means this adapter was on the path
    /// the game's traffic takes — the reason its death matters and an idle
    /// sibling's does not.
    pub(super) delivered: u64,
    pub(super) error: String,
}

/// The two timings [`PcapSource::next_segment`] runs on.
///
/// Fields rather than constants only so a test can shrink both: what this side
/// of the channel does when a producer dies is otherwise reachable only from a
/// machine with Npcap and a cooperative driver failure.
#[derive(Clone, Copy)]
struct Pacing {
    /// How long the receiver parks on the funnel before looking at anything
    /// else. Matched to [`READ_TIMEOUT_MS`], already the cadence at which this
    /// backend notices anything; a quiet tap costs five wakeups a second.
    poll: Duration,
    /// How long the funnel may stay silent *after* the adapter that was
    /// carrying the game's traffic died, before the session is declared over.
    ///
    /// Deliberately not a bare stall watchdog: silence is indistinguishable
    /// from a player who is not in the shop, so a timer on it would end healthy
    /// sessions. Armed only by an observed death, it asks a question with a
    /// real answer — is anyone *else* still carrying these packets? Where two
    /// adapters see the same frames (Hyper-V's vSwitch beside the physical NIC)
    /// the first packet disarms it. Five seconds is unmeasured against the
    /// game's cadence, just far longer than a duplicate adapter needs to prove
    /// itself; a false positive costs an accurate error and a relaunch, against
    /// a session that hangs forever.
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
    /// Bounded, at [`FRAME_QUEUE_DEPTH`], and never blocking: a blocking send
    /// would park a capture thread outside the driver whenever the consumer
    /// lagged, overflowing the kernel ring behind it. A full funnel instead
    /// drops the newest frame and raises the same `capture_loss` flag the
    /// driver's `ps_drop` raises, so the pipeline re-anchors (`sys::forward`).
    packets: Receiver<Vec<u8>>,
    /// One message per capture thread that died on an error. See
    /// [`AdapterFailure`] for why this is not the channel above.
    failures: Receiver<AdapterFailure>,
    /// Every failure reaped so far, as `device: reason`, so the disconnect at
    /// the end of the session can say what happened and not only that it did.
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
    /// Wraps the [`CaptureHealth`] this source shares with the window; see
    /// [`PacketSource::counters`].
    funnel: Funnel,
}

/// Remote wake for a [`PcapSource`] parked on its channel.
///
/// A flag, not a handle operation: calling `pcap_close` from another thread
/// while a receive is in flight is a use-after-free, which [`CaptureStop`]'s
/// contract in [`super`] forbids after it burned this codebase once. Each
/// capture thread notices within one [`sys::READ_TIMEOUT_MS`] window, closes
/// its own handle and drops its sender; the last one going wakes
/// [`PacketSource::next_segment`] with the disconnect that ends it for good.
///
/// `Relaxed` throughout, on this flag and `capture_loss`: the boolean is the
/// whole message, so there is no payload for `Acquire` to acquire — packets
/// publish through the channel, which carries its own edge.
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
    /// Blocking, and quick: nothing here waits on a human, unlike the backend
    /// it replaced. A device that fails to open, or reports a link type
    /// [`link::LinkStrip`] can't see past, is logged and skipped so one
    /// refusing adapter doesn't block a machine with a dozen virtual ones. Only
    /// zero usable devices is fatal.
    ///
    /// # Errors
    ///
    /// Always [`Error::Capture`], with a player-readable message:
    ///
    /// - `wpcap.dll` could not be loaded, or is missing a symbol — Npcap is not
    ///   installed, or too old. The message names the download page.
    /// - no adapter survived [`sys::open_device`]; [`no_usable_device_error`]
    ///   distinguishes admin-restricted driver, no device at all, and every
    ///   device refused with its own reason.
    /// - a capture thread failed to spawn. [`start_capture_threads`] stops and
    ///   joins the ones that did start, because the `Self` whose [`Drop`] would
    ///   do it is never constructed on this path.
    ///
    /// `health` is not created here: it is handed in already shared with the
    /// window (`app::setup` constructs it before this backend, or any
    /// backend, exists), so the counters this source increments are the same
    /// atomics the player reads — see [`CaptureHealth`] for the rest of that
    /// argument.
    pub(crate) fn open(game_port: NonZeroU16, health: CaptureHealth) -> Result<(Self, PcapStop)> {
        let (wpcap, loaded_from) = Wpcap::load()?;
        // The path names the install mode: `System32\wpcap.dll` rather than
        // `System32\Npcap\wpcap.dll` is the WinPcap-compatible one.
        info!(
            path = %loaded_from.display(),
            version = %wpcap.version(),
            "wpcap.dll loaded"
        );
        let wpcap = Arc::new(wpcap);

        let devices = enumerate(&wpcap)?;
        let filters = filter_candidates(game_port);

        let mut handles = Vec::new();
        let mut refused = Vec::new();
        for device in &devices {
            match open_device(&wpcap, device, &filters) {
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
            // Only the filter every adapter was *asked* for: one that refused
            // it is capturing on a later rung of `filter_candidates` and said
            // so in its own warning.
            filter = %filters[0],
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
                funnel: Funnel(health),
            },
            PcapStop { stop },
        ))
    }
}

/// The kernel filters this backend offers one adapter, most capable first.
///
/// A ladder, not a set: [`open_device`] installs the first rung that adapter's
/// libpcap accepts. Both rungs admit only the game server's own source port, so
/// everything else is still discarded in the driver.
///
/// The tagged arms exist because `tcp and src port {game_port}` compiles, on
/// `DLT_EN10MB`, to an `EtherType` test at offset 12 — where a tagged frame
/// holds `0x8100`, not `0x0800`. The driver discarded every tagged frame before
/// this process woke, leaving `link`'s VLAN handling unreachable and a tagged
/// player with nothing but `delivered == 0` to go on.
///
/// Three facts about the shape, none of them re-derivable from the code:
///
/// - `vlan` is not a predicate: it tests for a tag *and shifts every subsequent
///   decoding offset by four*, for the rest of the expression as the parser
///   reads it, and parentheses do not scope that back. The untagged arm must
///   therefore sit **left** of the first `vlan`, and each further `vlan` shifts
///   four more — which is what silently makes the third arm a double-tag
///   matcher, at exactly the depth `link`'s `MAX_VLAN_TAGS` strips.
/// - `pcap-filter(7)`: "Alternation and concatenation have equal precedence and
///   associate left to right". `and` does **not** bind tighter than `or`, so
///   every arm is parenthesised — unparenthesised, `A or vlan and A` parses as
///   `(A or vlan) and A`, a different filter that compiles.
/// - The ladder falls back per adapter because [`open_device`] treats a filter
///   it cannot install as a refused adapter, so on a libpcap too old to know
///   `vlan` one blind spot would become no capture at all. The second rung is
///   the previous filter, byte for byte.
fn filter_candidates(game_port: NonZeroU16) -> [String; 2] {
    let untagged = format!("tcp and src port {game_port}");
    let tagged = format!("({untagged}) or (vlan and {untagged}) or (vlan and vlan and {untagged})");
    [tagged, untagged]
}

/// Starts one named thread per adapter, and — the reason this is a function at
/// all — stops and joins everything it already started if any spawn fails.
///
/// Returning through a bare `?` here leaks threads permanently: `PcapSource`
/// does not exist yet, so no [`Drop`] sets the stop flag, and a spawned
/// `capture_loop`'s only other exit is the receiver going away — which it
/// learns by *sending*, and most adapters never match the filter and so never
/// send. They sat on 200 ms `pcap_next_ex` timeouts forever, once per relaunch.
///
/// Generic over the adapter only so that path is reachable from a test: `open`
/// instantiates it at `sys::Handle`, which needs a machine with Npcap, while
/// the cleanup being asserted is pure thread lifecycle.
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
/// A capture thread notices the flag within one [`READ_TIMEOUT_MS`] window, so
/// the join costs about that much once, not once per thread.
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
    /// Teardown normally goes through [`PcapStop`] first, so this is usually a
    /// no-op join. It exists for the paths that don't, where a dropped source
    /// would leave threads capturing into a channel nobody reads.
    fn drop(&mut self) {
        stop_and_join(&self.stop, &mut self.threads);
    }
}

impl PcapSource {
    /// Turns the capture threads' parting messages into either an armed
    /// deadline or a log line, depending on whether the dead adapter was
    /// carrying anything.
    ///
    /// Without this path a `pcap_next_ex` error ended one thread while its
    /// siblings kept the channel alive, parking this side in `recv` forever:
    /// `app::ingest` blocks on `next_segment` with no timeout, so nothing
    /// reached the journal and the window went on showing a running session
    /// that would never see another packet.
    fn reap_failures(&mut self) {
        while let Ok(failure) = self.failures.try_recv() {
            let device = short_device_name(&failure.device);
            self.failed.push(format!("{device}: {}", failure.error));
            if failure.delivered == 0 {
                // This backend opens every adapter precisely because it has no
                // reason to believe in any, so an idle one dying must not cost
                // the session. Still recorded: "every capture thread exited" is
                // otherwise unattributable.
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

    /// The fatal half of [`Self::reap_failures`]: an adapter that was
    /// delivering has died and nothing has replaced it. See [`Pacing::blind`]
    /// for why this is armed by a death and not by a timer.
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
                    // Whoever sent it, the tap is not blind: it passed the
                    // kernel filter, so some adapter is still on the server's
                    // path.
                    self.blind_since = None;
                    packet
                }
                // A quiet poll is the only moment this side gets to look at
                // anything but a packet.
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

            let delivered = self.funnel.0.record_delivered();
            // The link header is already gone: its length depends on the
            // adapter, and nothing down here knows which one a packet came
            // from. By value, so the buffer becomes the payload uncopied.
            let Some(segment) = parse_segment(packet, self.game_port) else {
                self.funnel.0.record_unparsed();
                self.funnel.report(delivered);
                continue;
            };

            let admitted = self.funnel.0.record_admitted();
            if admitted == 1 {
                // `parse_segment` admits nothing but the game server, so this
                // proves filter, port, adapter choice and strip all agree; its
                // absence means capture is open but sees nothing. The client's
                // port, not its address: on IPv6 that is the player's globally
                // routable one, in a file they're asked to email.
                info!(
                    payload = segment.payload.len(),
                    syn = segment.syn,
                    server = %segment.flow.server,
                    client_port = segment.flow.client.port(),
                    "first server-to-client segment admitted"
                );
            }
            self.funnel.report(delivered);
            return Ok(segment);
        }
    }

    fn take_capture_loss(&mut self) -> bool {
        // `swap`, not load-then-store: the read-and-clear must be atomic
        // against a capture thread setting it again. `Relaxed` for the reason
        // given at `PcapStop` — there is no payload behind the flag.
        self.capture_loss.swap(false, Ordering::Relaxed)
    }

    /// The live counters the window reads: the exact atomics [`PcapSource::open`]
    /// was handed, read through [`CaptureHealth::snapshot`] — the same call
    /// [`Funnel::report`]'s `debug!` line uses, so the two can never disagree.
    fn counters(&self) -> CaptureCounters {
        self.funnel.0.snapshot()
    }
}

/// Why one adapter didn't make the capture set, collected for the
/// zero-usable-device error.
struct Refusal {
    device: String,
    reason: String,
}

/// Turns "no adapter survived" into a message that names a cause.
///
/// Two failures are indistinguishable from return codes alone: Npcap restricted
/// to administrators hands an unelevated process an empty device list, just as
/// a machine with no adapters does. The registry's `AdminOnly` settles it.
fn no_usable_device_error(devices: &[String], refused: &[Refusal]) -> Error {
    if npcap_admin_only().is_some_and(|value| value != 0) {
        return Error::Capture(
            // No "or run this app elevated": the exe is manifested
            // `requireAdministrator`, so a player reading this has already
            // approved a UAC prompt. Reinstalling is the only lever.
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
    // SAFETY: `subkey` and `value` are NUL-terminated UTF-16 buffers alive
    // across the call. `size` is exactly `data`'s size and `RRF_RT_REG_DWORD`
    // restricts the write to a `DWORD`, so no over-write is possible.
    // `HKEY_LOCAL_MACHINE` needs no close. On failure `data` is untouched,
    // hence the status check before it is read.
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
    /// The channels are handed in so a test can play the capture threads:
    /// [`PcapSource::open`] is unrunnable without Npcap, while what needs
    /// pinning — what this side does when one producer dies with its siblings
    /// still holding the channel open — lives entirely on this side of them.
    fn from_channels(
        packets: Receiver<Vec<u8>>,
        failures: Receiver<AdapterFailure>,
        game_port: NonZeroU16,
        pacing: Pacing,
        health: CaptureHealth,
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
            funnel: Funnel(health),
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
    /// first.
    fn immediate() -> Pacing {
        Pacing {
            poll: Duration::from_millis(5),
            blind: Duration::ZERO,
        }
    }

    /// How long a test waits for a call that must not park forever: generous,
    /// because its only job is to turn "blocks for the life of the process"
    /// into a named failure rather than a hung suite.
    const NEVER_PARKS: Duration = Duration::from_secs(5);

    #[test]
    fn a_dead_adapter_ends_the_session_even_while_its_siblings_hold_the_funnel_open() {
        let (frames, packets) = sync_channel(FRAME_QUEUE_DEPTH);
        let (report, failures) = channel();
        // Siblings still holding a sender, with nothing to say because the
        // kernel filter matches nothing on their adapters. The receiver's only
        // `Err` is the all-senders-gone disconnect, and these two never go.
        let siblings = (frames.clone(), frames);
        let mut source = PcapSource::from_channels(
            packets,
            failures,
            game_port(),
            immediate(),
            CaptureHealth::default(),
        );
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
        let mut source = PcapSource::from_channels(
            packets,
            failures,
            game_port(),
            immediate(),
            CaptureHealth::default(),
        );
        // Delivered nothing, so it was never on the game's path: most of the
        // devices this backend opens are idle by design.
        report
            .send(AdapterFailure {
                device: r"\Device\NPF_{IDLE}".to_owned(),
                delivered: 0,
                error: "pcap_next_ex: the adapter was disabled".to_owned(),
            })
            .expect("the source is alive");
        drop(report);
        // Every capture thread gone is the only thing that may end this source,
        // and the reason must reach the message, not a log line nobody reads.
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
        // `open`'s spawn loop when the third `Builder::spawn` fails with two
        // threads already running. A bare `?` returns before `PcapSource`
        // exists, so nothing sets the flag and nothing joins: two threads
        // spinning forever, each pinning an open `pcap_t` and `wpcap.dll`.
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
                // `capture_loop`'s shape: an adapter matching nothing never
                // sends, so it never learns the receiver is gone and the flag
                // is its only exit.
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

    /// Live smoke check, never run by CI, which has neither an adapter nor
    /// Npcap. Run by hand to confirm the dynamic load, enumeration and
    /// per-adapter open still work:
    ///
    /// ```text
    /// cargo test --no-default-features --features pcap-backend -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs Npcap and a real adapter"]
    fn the_tap_opens_on_this_machine_without_elevation() {
        let (source, mut stop) = PcapSource::open(
            NonZeroU16::new(3333).expect("3333 is not zero"),
            CaptureHealth::default(),
        )
        .expect("open the Npcap tap");
        assert!(!source.threads.is_empty(), "at least one adapter must open");
        println!("adapters capturing: {}", source.threads.len());
        stop.stop();
    }

    /// Splits a filter on its top-level `or`s. Crude on purpose: it only has
    /// to handle the one shape [`filter_candidates`] builds, and a parser that
    /// could handle more would be a second thing to get wrong.
    fn top_level_arms(filter: &str) -> Vec<&str> {
        let mut arms = Vec::new();
        let mut depth = 0usize;
        let mut start = 0usize;
        for (at, &byte) in filter.as_bytes().iter().enumerate() {
            match byte {
                b'(' => depth += 1,
                b')' => depth = depth.saturating_sub(1),
                _ if depth == 0 && filter[at..].starts_with(" or ") => {
                    arms.push(filter[start..at].trim());
                    start = at + " or ".len();
                }
                _ => {}
            }
        }
        arms.push(filter[start..].trim());
        arms
    }

    #[test]
    fn the_untagged_arm_is_left_of_every_vlan_keyword_because_vlan_shifts_what_follows_it() {
        let [tagged, _] = filter_candidates(game_port());
        let arms = top_level_arms(&tagged);
        assert_eq!(arms.len(), 3, "untagged, one tag, two tags: {tagged}");

        // `vlan` shifts every decoding offset after it by four, so an untagged
        // arm written to its right would match tagged frames only — the same
        // outage inverted, unobservable on a machine with no tags.
        let first_vlan = tagged.find("vlan").expect("a vlan arm");
        let untagged_arm = tagged.find("tcp and src port").expect("an untagged arm");
        assert!(
            untagged_arm < first_vlan,
            "the untagged arm must be read before any offset shift: {tagged}"
        );
        assert!(!arms[0].contains("vlan"), "{}", arms[0]);
        // And each further arm adds exactly one more shift on top of the last,
        // which is what makes them one tag and then two.
        assert_eq!(arms[1].matches("vlan").count(), 1, "{}", arms[1]);
        assert_eq!(arms[2].matches("vlan").count(), 2, "{}", arms[2]);

        for arm in &arms {
            // libpcap gives `and` and `or` *equal* precedence, left to right,
            // so an unparenthesised arm would silently regroup: `A or vlan and
            // A` is `(A or vlan) and A`, which compiles and is not this filter.
            assert!(
                arm.starts_with('(') && arm.ends_with(')'),
                "every arm must be parenthesised against libpcap's flat precedence: {arm}"
            );
            assert!(arm.contains("tcp and src port 3333"), "{arm}");
        }
    }

    #[test]
    fn the_fallback_rung_is_the_filter_this_backend_shipped_before_vlan_was_handled() {
        // This rung's whole value is being the *previous* behaviour rather than
        // an approximation: drift makes the fail-safe path itself a change
        // nobody measured.
        let [tagged, untagged] = filter_candidates(game_port());
        assert_eq!(untagged, "tcp and src port 3333");
        assert!(!untagged.contains("vlan"));
        assert!(
            tagged.len() > untagged.len(),
            "the ladder runs most capable first"
        );
    }

    #[test]
    fn a_device_path_is_logged_by_its_trailing_component() {
        assert_eq!(
            short_device_name(r"\Device\NPF_{A1B2-C3D4}"),
            "NPF_{A1B2-C3D4}"
        );
        assert_eq!(short_device_name("lo0"), "lo0");
    }

    /// A minimal admissible IPv4 TCP packet, IP layer down (no Ethernet
    /// framing) — exactly the shape the funnel channel carries, already
    /// stripped by `link`. Rebuilt here rather than imported from
    /// `capture::ip::tests::ipv4_tcp`: that helper is private to its own test
    /// module.
    fn ipv4_tcp_from_game_port(payload: &[u8]) -> Vec<u8> {
        let server = ([104, 116, 20, 111], game_port().get());
        let client = ([192, 168, 1, 10], 51000);
        let builder = etherparse::PacketBuilder::ipv4(server.0, client.0, 64)
            .tcp(server.1, client.1, 1000, 64_240);
        let mut out = Vec::with_capacity(builder.size(payload.len()));
        builder.write(&mut out, payload).expect("write packet");
        out
    }

    /// The trait boundary [`PacketSource::counters`] adds: what it reports
    /// must equal what [`PcapSource::next_segment`] actually dequeued and
    /// classified, not a second count that could drift from it.
    ///
    /// This covers the consumer side of the funnel channel; the producer side
    /// — that `capture_loop`'s own `LoopCounters.delivered` equals the number
    /// of frames it pushed through `forward` into this same channel — is
    /// already pinned by plan 028's tests in `sys::tests`. Together the two
    /// prove there is no seam between "a capture thread sent a frame" and
    /// "the window's counters saw it".
    #[test]
    fn published_counters_report_exactly_what_next_segment_processed() {
        let (frames, packets) = sync_channel(FRAME_QUEUE_DEPTH);
        let (_report, failures) = channel();
        let health = CaptureHealth::default();
        let mut source =
            PcapSource::from_channels(packets, failures, game_port(), immediate(), health.clone());

        frames
            .send(ipv4_tcp_from_game_port(b"AB"))
            .expect("receiver alive");
        frames
            .send(ipv4_tcp_from_game_port(b"CD"))
            .expect("receiver alive");
        frames
            .send(b"not an ip packet at all".to_vec())
            .expect("receiver alive");
        drop(frames);

        assert!(source.next_segment().is_ok(), "first admitted segment");
        assert!(source.next_segment().is_ok(), "second admitted segment");
        // The third frame cannot parse; `next_segment` loops past it and then
        // finds the channel disconnected — the funnel's only other exit.
        assert!(source.next_segment().is_err());

        let counters = source.counters();
        assert_eq!(counters.delivered, 3, "every dequeued frame is delivered");
        assert_eq!(counters.admitted, 2);
        assert_eq!(counters.unparsed, 1);
        // The handle `open` would have shared with the window is the exact
        // one these increments landed on — not a copy that could disagree.
        assert_eq!(health.snapshot(), counters);
    }
}
