//! Npcap capture backend: an unelevated, adapter-agnostic tap.
//!
//! # Why this exists
//!
//! The backend this replaced (`WinDivert`) needed a kernel driver load, which
//! needs administrator rights, which is what the product's elevated broker, its
//! named pipe and its UAC prompt existed to contain — 3 274 lines of it. Npcap's
//! installer offers to leave its driver open to ordinary users (the `AdminOnly`
//! option, off by default), and a measured probe on the development machine
//! confirmed a **non-elevated** process capturing the game's traffic: 82 packets
//! matched, 82 parsed, 0 unparsed, on the Wi-Fi adapter, `DLT_EN10MB`. That
//! probe became this backend, it held up over the whole pipeline, and the
//! elevated architecture was then deleted — see `docs/capture-backend-choice.md`.
//!
//! The exe is still manifested `requireAdministrator`, and not for this module:
//! the actuator cannot click a window that runs at higher integrity than this
//! process, and Epic Seven inherits high integrity from STOVE. See `build.rs`.
//!
//! # Why `wpcap.dll` is loaded by hand
//!
//! The `pcap` crate links `wpcap.lib` statically. That would make the build
//! require the Npcap SDK, and — worse — would make the shipped exe die *in the
//! Windows loader, before `main`* on any machine without Npcap installed, with
//! no message the player could act on. Loading the DLL through `libloading`
//! keeps the binary startable everywhere and turns "Npcap is not installed"
//! into an ordinary [`Error::Capture`] naming the download page.
//!
//! # Why every adapter is opened, and none is selected
//!
//! [`PcapSource::open`] enumerates the machine's devices and opens *all* of
//! them, one thread and one handle each, each with its own kernel-side BPF
//! filter. The filter is compiled into the driver per handle, so an adapter
//! that carries no game traffic costs a parked thread and about a megabyte of
//! kernel ring — nothing per packet, because the packets it does not match
//! never leave the driver.
//!
//! That price buys the removal of every adapter heuristic. The machine this was
//! measured on has an Ethernet interface holding an APIPA address while Wi-Fi
//! carries the traffic, so "pick the adapter with a default route" or "pick the
//! one with a real IP" would both have guessed wrong at least once; opening all
//! of them also survives a mid-session Wi-Fi/Ethernet switch, a VPN coming up,
//! or a docking station appearing, with no code at all. The cost of seeing the
//! same packet on two adapters is likewise already paid: [`crate::stream`]
//! dedupes by TCP sequence number, because it must already tolerate ordinary
//! retransmissions.
//!
//! # How the three files divide it
//!
//! This root holds the [`PacketSource`] itself — the channel, the funnel
//! counters, the lifecycle and the diagnostics a player reads when no adapter
//! could be opened. [`link`] is the pure frame-to-IP-packet strip, the one layer
//! with no FFI in it. [`sys`] is the `wpcap.dll` boundary, and it is deliberately
//! one file rather than three: its header says why.

mod link;
mod sys;

use std::num::NonZeroU16;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::thread::JoinHandle;

use tracing::{debug, info, warn};

use super::{CaptureStop, PacketSource, Segment, parse_segment};
use crate::error::{Error, Result};
use sys::{DLL_CANDIDATES, INSTALL_HINT, SNAPLEN, Wpcap, capture_loop, enumerate, open_device};

/// How many packet frames between two funnel lines on this side. Every captured
/// packet passes through the funnel, so the line is periodic — plus once on the
/// very first packet, so a capture that is about to reject everything says so
/// immediately instead of after five hundred packets.
const FUNNEL_LOG_EVERY: u64 = 500;

// --- Public source ---------------------------------------------------------

/// Where packets that reach this process go to die.
///
/// `delivered` counts frames pulled off *n* adapters and stripped of their link
/// header; `admitted` and `unparsed` are the two ways those frames end. Exactly
/// one thing can drop a packet between the driver and the reassembler —
/// `parse_segment` refusing it, whether because the bytes are malformed or
/// because they are not the game server talking — and it is plausible on its
/// own as the reason a healthy-looking session yields nothing. `delivered`
/// staying at zero is itself the headline result: the adapters are open but the
/// kernel filter matches no traffic.
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

/// The rare half of [`Funnel::report`], out of line: `report` itself is called
/// twice per delivered packet, on the only path in this crate that runs per
/// captured packet, and all it should carry is the modulus test.
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

/// A [`PacketSource`] fed by one capture thread per adapter.
pub struct PcapSource {
    /// Stripped IP packets, funnelled from every capture thread.
    ///
    /// Unbounded on purpose. A bounded channel would park a capture thread
    /// outside the driver whenever the reassembler lagged, and the driver's ring
    /// would overflow behind it — turning a transient consumer stall into real,
    /// unrecoverable capture loss. Unbounded lets the consumer catch up; the
    /// producers are already rate-limited by the kernel filter, which admits
    /// only one TCP port's server-to-client traffic.
    packets: Receiver<Vec<u8>>,
    game_port: NonZeroU16,
    /// Set by any capture thread whose `pcap_stats` drop counter moved, and left
    /// set until the capture loop asks for it.
    capture_loss: Arc<AtomicBool>,
    /// Shared with [`PcapStop`], and with every capture thread.
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    funnel: Funnel,
}

/// Remote wake for a [`PcapSource`] parked on its channel.
///
/// A flag, not a handle operation, and that is the whole design. `pcap_close`
/// from another thread while a receive is in flight is a use-after-free waiting
/// to happen — the [`CaptureStop`] contract in [`super`] forbids exactly that,
/// after it burned this codebase once. So teardown only stores `true`; each
/// capture thread notices within one [`sys::READ_TIMEOUT_MS`] window, closes its
/// own handle, and drops its sender. When the last sender goes, the receiver in
/// [`PacketSource::next_segment`] wakes with a disconnect, which is how the
/// blocking call ends.
///
/// Idempotent by construction: storing `true` twice is storing `true`.
///
/// `Relaxed` throughout, on this flag and on `capture_loss`: the boolean *is* the
/// whole message. Nothing is written before the store and read after the load, so
/// there is no payload for an `Acquire` to acquire — precisely because teardown
/// never touches another thread's handle (see above), and because the packets
/// themselves publish through the channel, which carries its own edge.
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
    /// Blocking, and quick: enumeration plus one `pcap_open_live` and one filter
    /// compile per device. Nothing in it waits on a human — the backend it
    /// replaced put a UAC prompt in the middle of this call.
    ///
    /// A device that fails to open, or reports a link type [`link::LinkStrip`]
    /// cannot see past, is logged and skipped: a machine with a dozen virtual
    /// adapters should not be blocked by whichever one of them refuses. Only
    /// *zero* usable devices is fatal.
    ///
    /// # Errors
    ///
    /// Always [`Error::Capture`], with a message written for a player reading a
    /// log file, from one of three causes:
    ///
    /// - `wpcap.dll` could not be loaded from either candidate path, or answered
    ///   without one of the thirteen symbols this backend needs — Npcap is not
    ///   installed, or is too old. The message names the download page.
    /// - no adapter survived [`sys::open_device`]. [`no_usable_device_error`] then
    ///   distinguishes the three shapes of that: the driver restricted to
    ///   administrators (which is why the registry is consulted), no capture
    ///   device at all, and every enumerated device refused with its own reason.
    /// - a capture thread could not be spawned. Unlike the two above this leaves
    ///   the already-spawned threads to be joined by [`Drop`], and is the only one
    ///   that is not about the machine's Npcap install.
    pub(crate) fn open(game_port: NonZeroU16) -> Result<(Self, PcapStop)> {
        let (wpcap, loaded_from) = Wpcap::load()?;
        info!(
            path = loaded_from,
            version = %wpcap.version(),
            plain_name_resolved = loaded_from == DLL_CANDIDATES[0],
            "wpcap.dll loaded"
        );
        let wpcap = Arc::new(wpcap);

        let devices = enumerate(&wpcap)?;
        // Only the game server's own source port: the shop response is the only
        // traffic this product decodes, and everything else is discarded in the
        // driver rather than copied to user space and thrown away here.
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
        let (sender, packets) = channel();
        let mut threads = Vec::with_capacity(handles.len());
        for handle in handles {
            let sender = sender.clone();
            let stop = Arc::clone(&stop);
            let capture_loss = Arc::clone(&capture_loss);
            // `handle.device` is borrowed only for the `format!`, which is fully
            // evaluated before the closure literal below exists — so the thread
            // name needs no clone of it.
            let thread = std::thread::Builder::new()
                .name(format!("pcap-{}", short_device_name(&handle.device)))
                .spawn(move || capture_loop(handle, &sender, &stop, &capture_loss))
                .map_err(|err| Error::Capture(format!("spawning a capture thread: {err}")))?;
            threads.push(thread);
        }
        // The original sender must go, or the receiver could never observe a
        // disconnect and `next_segment` would block forever after teardown.
        drop(sender);

        info!(
            adapters = threads.len(),
            skipped = refused.len(),
            filter = %filter,
            snaplen = SNAPLEN,
            "Npcap capture open (passive copy; originals untouched)"
        );

        Ok((
            Self {
                packets,
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

impl Drop for PcapSource {
    /// Stops and joins the capture threads.
    ///
    /// Teardown normally goes through [`PcapStop`] first, so this is usually a
    /// no-op join of already-finished threads. It exists for the paths that do
    /// not — a dropped source with its stop handle still alive would otherwise
    /// leave *n* threads capturing into a channel nobody reads.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            if thread.join().is_err() {
                warn!("a capture thread panicked");
            }
        }
    }
}

impl PacketSource for PcapSource {
    fn next_segment(&mut self) -> Result<Segment> {
        loop {
            // A disconnect means every capture thread has exited: either the
            // stop flag was set (teardown, and the caller is already shutting
            // down) or every adapter errored out. Either way there will never be
            // another packet, so this is the end of the source.
            let packet = self.packets.recv().map_err(|_| {
                Error::Capture("every Npcap capture thread exited — the tap is closed".to_owned())
            })?;

            self.funnel.delivered += 1;
            // The link header is already gone: each capture thread strips its
            // own adapter's framing, because the strip length is a property of
            // the adapter and nothing down here knows which one a packet came
            // from. What arrives is a raw IP packet, which is the only shape
            // `parse_segment` has ever accepted — and it is handed over *by
            // value*, so the frame buffer this thread just received off the
            // channel becomes the segment's payload instead of being copied into
            // a second one and dropped.
            let Some(segment) = parse_segment(packet, self.game_port) else {
                self.funnel.unparsed += 1;
                self.funnel.report();
                continue;
            };

            self.funnel.admitted += 1;
            if self.funnel.admitted == 1 {
                // Anything admitted at all was sent *by* the game server —
                // `parse_segment` accepts nothing else — so this line is the
                // proof that the filter, the port, the adapter choice and the
                // link-layer strip all agree. Its *absence* in a session log
                // means capture is open but sees nothing from the game server.
                // The client's *port*, not its address: on IPv6 that address is
                // the player's globally routable one, in a file they are asked to
                // email, and the port alone already proves the agreement.
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
        // `swap` and not a load-then-store, because the read-and-clear must be
        // atomic against a capture thread setting it again; `Relaxed`, because an
        // RMW on one location is ordered against every other RMW on it regardless,
        // and there is no payload behind the flag (see [`PcapStop`]).
        self.capture_loss.swap(false, Ordering::Relaxed)
    }
}

// --- No usable device: saying which of the three things happened -----------

/// Why one adapter did not make it into the capture set. Collected so that a
/// zero-usable-device failure can say what actually happened on each one.
struct Refusal {
    device: String,
    reason: String,
}

/// Turns "no adapter survived" into a message that names a cause.
///
/// The two failures worth telling apart are indistinguishable from the return
/// codes alone: Npcap present but restricted to administrators hands back an
/// empty device list (or access-denied opens) from an unelevated process, which
/// looks identical to a machine that simply has no adapters. `AdminOnly` in the
/// registry is what settles it — on the machine this backend was measured on it
/// reads 0.
fn no_usable_device_error(devices: &[String], refused: &[Refusal]) -> Error {
    if npcap_admin_only().is_some_and(|value| value != 0) {
        return Error::Capture(
            "Npcap is installed but its driver is restricted to administrators \
             (HKLM\\SYSTEM\\CurrentControlSet\\Services\\npcap\\Parameters\\AdminOnly is set): \
             reinstall it with \"Restrict Npcap driver's access to Administrators\" unchecked, \
             or run this app elevated"
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
/// Sized up front rather than collected: `EncodeUtf16::size_hint`'s lower bound is
/// `ceil(len / 3)`, which is what `collect` reserves, so the obvious spelling
/// allocates small and then grows. `len + 1` is exact for ASCII and a safe
/// over-estimate otherwise.
fn wide(text: &str) -> Vec<u16> {
    let mut wide = Vec::with_capacity(text.len() + 1);
    wide.extend(text.encode_utf16());
    wide.push(0);
    wide
}

/// `\Device\NPF_{GUID}` is unreadable in a log line; the GUID alone is enough to
/// tell two adapters apart.
fn short_device_name(device: &str) -> &str {
    device.rsplit('\\').next().unwrap_or(device)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live smoke check, never run by CI (`#[ignore]`, and CI has neither an
    /// adapter nor Npcap). Run it by hand on a machine with Npcap to confirm the
    /// dynamic load, the enumeration and the per-adapter open all still work:
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
