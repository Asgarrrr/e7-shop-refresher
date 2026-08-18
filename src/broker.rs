//! The elevated half of the capture split: opens the WinDivert handle, and
//! pushes raw packets down a named pipe to the unelevated UI process.
//!
//! # What runs here, and what deliberately does not
//!
//! This module is the *entire* administrator surface of the product. It opens
//! the driver handle — the one step that genuinely needs the privilege — copies
//! bytes out of it, and writes them to a pipe. It parses nothing: `parse_segment`
//! is the code that chews on unauthenticated bytes off the wire, and it stays on
//! the far end of the pipe in a medium-integrity process. It reads no config
//! file either, because `%APPDATA%\arkyve-refresh-shop\config.toml` is writable
//! by any medium-integrity process on the machine and a WinDivert filter string
//! is compiled by a kernel driver.
//!
//! # Its whole input from the low-privilege side
//!
//! Three argv tokens: a port, a 32-hex-character pipe nonce, and the UI's
//! process id. No free-form string ever crosses upward — the capture filter is
//! a constant here with a validated `u16` interpolated into it. The three
//! validators below are pure and unit-tested precisely because they are the
//! complete attack surface; keep them free of side effects.
//!
//! # Its diagnostics channel
//!
//! Not `tracing`. The broker installs no subscriber, and the shipped build has
//! no console (`windows_subsystem = "windows"`), so every `info!`/`warn!` on this
//! side — including the ones inside `WinDivertSource` — is inert. What the
//! player's log file actually receives are the kind-1 (diagnostic) and kind-2
//! (fatal) frames this module writes. A failure that does not travel down the
//! pipe dies with this process.

use crate::error::{Error, Result};

/// Length of the pipe nonce, in hexadecimal characters — 128 bits.
pub const PIPE_NONCE_HEX_CHARS: usize = 32;

/// The argv token that puts a copy of this exe into broker mode.
///
/// One exe, two roles: without this flag the process is the window the player
/// sees, with it the process is the short-lived administrator half that opens
/// the WinDivert handle and does nothing else. There is no second binary to
/// extract and elevate — a program written by a medium-integrity process and
/// then launched as administrator is an elevation pattern this design refuses,
/// so the image that gets elevated is exactly the one the player double-clicked.
///
/// Lives here, unconditionally, for the same reason as [`pipe_name`]: the two
/// sides cannot be allowed to drift. `capture::elevate` writes this token onto
/// the elevated command line and the argv dispatch in `main` is what answers to
/// it, and *the dispatch has to exist in builds that have no `elevate` at all* —
/// refusing the flag on a build with no capture backend is the whole point of
/// that arm. Two literals would have compiled happily while disagreeing, and the
/// symptom would have been a second, elevated, invisible window plus a UI timing
/// out on a channel nobody serves.
pub const BROKER_ARGV_FLAG: &str = "--capture-broker";

/// Validates the `--port` token: a TCP port, never zero.
///
/// Pure and side-effect-free on purpose. This value is interpolated into the
/// WinDivert filter expression that a kernel driver compiles inside this
/// elevated process, so "it is a `u16`" is not a formality — it is what makes
/// the filter a constant with a hole in it rather than a string from a
/// lower-privileged process.
pub fn parse_port(raw: &str) -> Result<u16> {
    match raw.parse::<u16>() {
        Ok(port) if port != 0 => Ok(port),
        _ => Err(Error::Capture(format!(
            "--port must be a TCP port between 1 and 65535, not {raw:?}"
        ))),
    }
}

/// Validates the `--pipe` token: exactly [`PIPE_NONCE_HEX_CHARS`] hex digits.
///
/// The nonce is pasted straight into a `\\.\pipe\...` name, so the character set
/// is the guard: anything outside `[0-9a-fA-F]` could steer the name somewhere
/// else entirely. Length is fixed rather than bounded so that a short nonce —
/// which would be guessable, and so squattable — is refused as loudly as a
/// malformed one.
///
/// The offending value is not echoed back: it is a shared secret, and the error
/// says everything a reader needs without putting it in a log file.
pub fn parse_pipe_nonce(raw: &str) -> Result<&str> {
    if raw.len() == PIPE_NONCE_HEX_CHARS && raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(raw)
    } else {
        Err(Error::Capture(format!(
            "--pipe must be exactly {PIPE_NONCE_HEX_CHARS} hexadecimal characters"
        )))
    }
}

/// Validates the `--ui-pid` token: a process id, never zero.
///
/// Zero is refused because it is not a process id a caller can mean: it is the
/// System Idle Process, and `OpenProcess(0)` fails in a way that would be read
/// as "the UI is already gone" instead of "this argument is nonsense".
pub fn parse_ui_pid(raw: &str) -> Result<u32> {
    match raw.parse::<u32>() {
        Ok(pid) if pid != 0 => Ok(pid),
        _ => Err(Error::Capture(format!(
            "--ui-pid must be a non-zero process id, not {raw:?}"
        ))),
    }
}

/// The pipe name both ends derive from the nonce.
///
/// One function so the two sides cannot drift; the nonce must have been through
/// [`parse_pipe_nonce`] first, which is what keeps this a formatting helper
/// rather than a name-injection site.
#[must_use]
pub fn pipe_name(nonce: &str) -> String {
    format!(r"\\.\pipe\arkyve-{nonce}")
}

/// The complete command line the unelevated side hands to the elevated copy.
///
/// Here rather than in `capture::elevate` so that the side which *writes* the
/// command line and the side which *parses* it can be bolted together by a
/// single test (`the_command_line_the_launcher_writes_is_the_one_the_dispatch_parses`
/// in `main.rs`) instead of trusting four literals — the flag and the three
/// argument names — to stay in agreement across two files, one of which does not
/// exist in every build. The values are formatted, never quoted or escaped:
/// [`parse_port`], [`parse_pipe_nonce`] and [`parse_ui_pid`] on the receiving end
/// only accept tokens with no whitespace and no shell-significant characters, so
/// there is nothing here that a space could split in two.
#[must_use]
pub fn broker_command_line(port: u16, pipe_nonce: &str, ui_pid: u32) -> String {
    format!("{BROKER_ARGV_FLAG} --port {port} --pipe {pipe_nonce} --ui-pid {ui_pid}")
}

#[cfg(all(windows, feature = "windivert-backend"))]
pub use elevated::run;

/// Everything that needs Win32 and the capture backend.
///
/// Gated as one block rather than per item: `just verify`'s two lanes build
/// without `windivert-backend` (and therefore without `windows-sys`, which is an
/// optional dependency that feature turns on), so none of this may even be
/// parsed for name resolution there. The validators above stay outside the gate
/// — they are pure Rust, and their tests are the only automated coverage this
/// file can have.
#[cfg(all(windows, feature = "windivert-backend"))]
mod elevated {
    use std::io::{self, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA,
        ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_OUTBOUND, SYNCHRONIZE,
        WriteFile,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, ExitProcess, INFINITE, OpenProcess, OpenProcessToken,
        PROCESS_QUERY_LIMITED_INFORMATION, WaitForMultipleObjects, WaitForSingleObject,
    };

    use super::pipe_name;
    use crate::capture::{
        CaptureStop, FRAME_FLAG_CAPTURE_LOSS, FRAME_KIND_DIAGNOSTIC, FRAME_KIND_FATAL,
        FRAME_KIND_PACKET, MAX_PACKET_BYTES, WinDivertSource, write_frame,
    };
    use crate::error::{Error, Result};

    /// Kernel-side write quota for the pipe. Generous on purpose: it is the
    /// first shock absorber between a burst of captured packets and a UI thread
    /// that is momentarily busy elsewhere. It is not the *last* one — see
    /// [`OUTBOUND_QUEUE_FRAMES`] — because a full quota blocks `WriteFile`, and
    /// a blocked writer that also owned the receive loop would let the driver's
    /// queue overflow with nothing counting the loss.
    const PIPE_OUT_BUFFER_BYTES: u32 = 1024 * 1024;

    /// How many frames may sit between the receive loop and the writer thread.
    ///
    /// This bound is the reason [`FRAME_FLAG_CAPTURE_LOSS`] means something. The
    /// pipe introduced a blocking point the WinDivert backend never had: while
    /// the broker is parked in `WriteFile` it is not in `recv`, and the driver
    /// silently discards whatever overflows its own queue — a hole in the byte
    /// stream that no retransmission can ever fill, because a passive tap never
    /// sees already-ACKed bytes twice. Moving the write onto its own thread with
    /// a *bounded* queue in front of it converts that invisible driver-side loss
    /// into a drop this process makes deliberately, counts, and reports on the
    /// next frame that gets through. At roughly 1.5 KiB per captured packet this
    /// is a couple of megabytes of slack on top of the pipe's own buffer.
    const OUTBOUND_QUEUE_FRAMES: usize = 1024;

    /// Deliveries between two diagnostic frames, matching the WinDivert funnel's
    /// own cadence. The first delivery also reports, so a capture that is about
    /// to see nothing useful says so immediately rather than after five hundred
    /// packets — and so the log file proves the pipe carried a frame at all.
    const DIAGNOSTIC_EVERY: u64 = 500;

    /// How long to wait for the unelevated end to connect before giving up.
    ///
    /// Bounded, never `INFINITE`. By the time this code runs the UAC prompt has
    /// already been accepted, so a connect that has not happened within this
    /// budget is not a slow user — it is a UI that died, was killed, or never
    /// intended to connect, and an unbounded wait would strand an elevated
    /// process (and the loaded kernel driver) for the rest of the session. They
    /// would accumulate across launches.
    const CONNECT_TIMEOUT_MS: u32 = 30_000;

    /// A Win32 handle this frame owns and closes exactly once.
    struct OwnedHandle(HANDLE);

    // SAFETY: a `HANDLE` is a process-wide index into the kernel handle table,
    // not a thread-affine pointer; it is `!Send` only because the alias is spelled
    // as a raw pointer. This wrapper is the sole owner of the handle it holds and
    // is moved rather than copied, so the one race that would matter — two
    // threads closing the same handle — cannot arise.
    unsafe impl Send for OwnedHandle {}

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: `self.0` came from a create/open call that was checked for
            // success before the wrapper was built, this value is the only owner,
            // and `Drop` runs once — so the handle is neither closed twice nor
            // used afterwards.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// A handle deliberately kept open for the whole process lifetime, and
    /// therefore safe to copy onto another thread.
    ///
    /// Used for exactly one thing: the UI process handle. The watchdog thread
    /// below parks on it in an unbounded wait and never returns, so no scope can
    /// meaningfully own it — closing it from the main thread would pull the
    /// handle out from under a live wait. The process is short-lived and
    /// single-purpose, and Windows reclaims the handle at exit; this is the same
    /// deliberate leak `preload_dll` makes with the WinDivert module handle.
    #[derive(Clone, Copy)]
    struct SharedHandle(HANDLE);

    // SAFETY: same reasoning as `OwnedHandle`, minus the ownership question —
    // nothing ever closes this handle, so no thread can invalidate another's use
    // of it.
    unsafe impl Send for SharedHandle {}

    impl SharedHandle {
        /// The raw handle.
        ///
        /// A method rather than a field read, and that matters at exactly one
        /// call site: closures capture *fields*, so writing `ui.0` inside a
        /// `move` closure would capture the bare `HANDLE` — a raw pointer, and
        /// therefore not `Send` — instead of this wrapper, which is the thing
        /// that vouches for sending it. A method call captures the whole value.
        fn raw(self) -> HANDLE {
            self.0
        }
    }

    /// Null-terminated UTF-16, the shape W-suffixed Win32 calls want.
    ///
    /// The buffer *is* the value: dropping it leaves the caller passing a
    /// dangling `as_ptr()` to Win32, so every call site keeps it in a named
    /// local that outlives the call.
    #[must_use]
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    /// Copies a NUL-terminated UTF-16 string out of Win32-owned memory.
    ///
    /// # Safety
    ///
    /// `raw` must point at a readable, NUL-terminated UTF-16 sequence that stays
    /// valid for the duration of the call.
    unsafe fn wide_to_string(raw: *const u16) -> String {
        let mut len = 0usize;
        // SAFETY: the caller guarantees a NUL terminator, so the scan stops
        // inside the allocation; each read is of an initialized `u16`.
        while unsafe { *raw.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `raw` is readable for `len` elements by the scan above, and the
        // slice is copied out before this function returns, so it never outlives
        // the Win32 allocation.
        let chars = unsafe { std::slice::from_raw_parts(raw, len) };
        String::from_utf16_lossy(chars)
    }

    /// What travels from the receive loop to the writer thread.
    ///
    /// Diagnostics and the fatal cause go through the same queue as packets so
    /// that exactly one thread ever touches the pipe handle: an overlapped write
    /// is only safe to keep simple while it is strictly serial.
    enum Outgoing {
        Packet { flags: u8, payload: Vec<u8> },
        Diagnostic(String),
        Fatal(String),
    }

    /// The pipe handle as a blocking [`Write`], so [`write_frame`] can serialize
    /// straight onto it.
    ///
    /// The handle is overlapped because bounding [`ConnectNamedPipe`] requires it
    /// (see [`wait_for_client`]), and a handle opened `FILE_FLAG_OVERLAPPED`
    /// stays overlapped for every later operation — so each write has to supply
    /// its own `OVERLAPPED` and wait for completion. Only one thread ever writes,
    /// so a single reusable event is enough.
    struct PipeWriter {
        pipe: OwnedHandle,
        event: OwnedHandle,
    }

    impl Write for PipeWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            let mut overlapped = OVERLAPPED {
                hEvent: self.event.0,
                ..Default::default()
            };
            // Frames are at most `MAX_PACKET_BYTES` plus a six-byte header, so
            // this saturation is unreachable; it is here so the cast cannot lie.
            let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
            let mut written: u32 = 0;
            // SAFETY: `self.pipe.0` is the live pipe handle this struct owns and
            // `self.event.0` the live event it owns, both closed only when this
            // struct drops — which cannot happen while `&mut self` is borrowed
            // here. `buf` is readable for `len` bytes and is not moved during the
            // call. `overlapped` and `written` are stack slots that must outlive
            // the I/O, and every path below either observes completion or returns
            // only after the operation has ended, so neither is dropped while the
            // kernel can still write into it. Failure is reported by a zero
            // return plus the thread's last-error slot.
            let started = unsafe {
                WriteFile(
                    self.pipe.0,
                    buf.as_ptr(),
                    len,
                    &mut written,
                    &mut overlapped,
                )
            };
            if started == 0 {
                // Read before any other Win32 call: `GetLastError` is per-thread
                // and the very next call overwrites it.
                let err = io::Error::last_os_error();
                if err.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                    return Err(err);
                }
                // SAFETY: same live handle and the same `overlapped` that started
                // the pending write; `bWait = TRUE` means this only returns once
                // the operation has finished, so `overlapped` is quiescent by the
                // time this frame ends.
                if unsafe { GetOverlappedResult(self.pipe.0, &overlapped, &mut written, 1) } == 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(written as usize)
        }

        fn flush(&mut self) -> io::Result<()> {
            // A named pipe write is already through to the kernel; there is no
            // user-space buffer of ours to push.
            Ok(())
        }
    }

    /// Runs the elevated capture broker until the unelevated end goes away.
    ///
    /// `port` and `ui_pid` must have been through [`super::parse_port`] and
    /// [`super::parse_ui_pid`], `pipe_nonce` through [`super::parse_pipe_nonce`].
    ///
    /// Returns `Ok(())` for the ordinary ending — the UI closed the pipe or
    /// exited — and `Err` only for a failure the player should be told about,
    /// which by then has also been written down the pipe as a kind-2 frame.
    ///
    /// `pub` rather than `pub(crate)` for the same reason as `write_frame` and
    /// `recv_packet`: this crate is a lib plus a bin, only an item reachable from
    /// the crate root escapes `dead_code`, every lane builds with `-D warnings`,
    /// and the argv dispatch that calls this does not exist yet.
    pub fn run(port: u16, pipe_nonce: &str, ui_pid: u32) -> Result<()> {
        // First, before anything can panic. There is no stderr in the shipped
        // build and no tracing subscriber on this side, so without the hook a
        // broker panic is invisible in every channel the product has.
        crate::crash::install();

        // Second, before the pipe and before the driver. See `spawn_ui_watchdog`
        // for why the ordering is load-bearing.
        let ui = open_ui_process(ui_pid)?;
        spawn_ui_watchdog(ui)?;

        let pipe = create_pipe(pipe_nonce, &ui_owner_sid(ui)?)?;
        let event = create_event()?;
        wait_for_client(&pipe, &event, ui)?;
        let mut writer = PipeWriter { pipe, event };

        // A constant with a validated `u16` in it — the product decision that
        // makes this whole split possible. No string from the low-privilege side
        // reaches the driver's filter compiler.
        let filter = format!("tcp and tcp.SrcPort == {port}");
        // `open_raw` runs `ensure_runtime_present()` itself: extraction, the
        // runtime-dir DACL and the delay-load preload all happen inside it. Do
        // not call it again out here.
        let (mut source, stop) = match WinDivertSource::open_raw(&filter, port, MAX_PACKET_BYTES) {
            Ok(pair) => pair,
            Err(err) => {
                // The single most likely startup failure there is, and the whole
                // reason kind 2 exists. This process has no console and no
                // subscriber, so a cause that does not go down the pipe dies here
                // and the player is left with a banner naming nothing.
                let _ = write_frame(
                    &mut writer,
                    FRAME_KIND_FATAL,
                    0,
                    fatal_text(&err).as_bytes(),
                );
                return Err(err);
            }
        };

        let (tx, rx) = sync_channel(OUTBOUND_QUEUE_FRAMES);
        let peer_gone = Arc::new(AtomicBool::new(false));
        let writing = spawn_writer(writer, rx, stop, Arc::clone(&peer_gone))?;

        let received = pump(&mut source, &tx, &peer_gone);
        if let Err(err) = &received {
            // Best-effort: if the queue is full the pipe is wedged anyway, and
            // its closure already tells the far end the broker is gone.
            let _ = tx.try_send(Outgoing::Fatal(fatal_text(err)));
        }
        // Ends the writer's iteration once whatever is queued has drained.
        drop(tx);
        let written = writing.join().unwrap_or_else(|_| {
            Err(Error::Capture(
                "the capture broker's writer thread panicked".to_owned(),
            ))
        });
        received.and(written)
    }

    /// Opens the UI process for the watchdog, the connect wait, and the SID
    /// lookup — one handle serving all three.
    ///
    /// `PROCESS_QUERY_LIMITED_INFORMATION` is the weakest access that still lets
    /// `OpenProcessToken` succeed, and `SYNCHRONIZE` is what makes the handle
    /// waitable. Nothing here needs `PROCESS_QUERY_INFORMATION`, so it is not
    /// asked for.
    fn open_ui_process(ui_pid: u32) -> Result<SharedHandle> {
        // SAFETY: every argument is a plain integer and nothing is borrowed
        // across the call. The returned handle is null on failure, checked below
        // before it is stored or used.
        let handle =
            unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, ui_pid) };
        if handle.is_null() {
            // Read before any other Win32 call: `GetLastError` is per-thread and
            // the next call overwrites it.
            let err = io::Error::last_os_error();
            return Err(Error::Capture(format!(
                "the process that asked for capture (pid {ui_pid}) is already gone ({err})"
            )));
        }
        Ok(SharedHandle(handle))
    }

    /// Ties this elevated process's lifetime to the UI's.
    ///
    /// Started before the pipe and before the driver, and that ordering is the
    /// point. Without this thread the broker only learns the UI is gone on its
    /// next `WriteFile`, and it only writes when a packet arrives — with the
    /// filter pinned to the game's port and the player anywhere but the shop, no
    /// packet arrives for minutes. "Open the app, then close it without opening
    /// the shop" would leave an elevated process and a loaded kernel driver
    /// resident indefinitely, one more per launch. The same wait also covers the
    /// UI killed during the UAC prompt, the UI that crashed, and the UI that
    /// never connects at all — none of which the pipe can report.
    ///
    /// The thread is detached: it either outlives everything or ends the process.
    fn spawn_ui_watchdog(ui: SharedHandle) -> Result<()> {
        std::thread::Builder::new()
            .name("ui-watchdog".to_owned())
            .spawn(move || {
                // SAFETY: `ui.raw()` is the process handle opened with `SYNCHRONIZE`
                // above and never closed by anyone, so it stays valid across this
                // unbounded wait; `WaitForSingleObject` only reads it.
                unsafe { WaitForSingleObject(ui.raw(), INFINITE) };
                // Any return means "stop being resident": either the UI exited,
                // or the wait itself failed and this process can no longer tell
                // whether it did. Both answers are "leave" — a broker that cannot
                // observe its client is exactly the stranded elevated process
                // this thread exists to prevent. Exiting closes the WinDivert
                // handle, which is what unregisters the filter with the driver.
                //
                // SAFETY: `ExitProcess` takes an integer and does not return.
                unsafe { ExitProcess(0) }
            })
            .map(|_detached| ())
            .map_err(|err| Error::Capture(format!("could not start the capture watchdog: {err}")))
    }

    /// The SID of the account that owns the UI process, as SDDL text.
    ///
    /// **Not this process's own token user.** Under over-the-shoulder elevation —
    /// a standard user who types an administrator's credentials at the prompt —
    /// the broker and the UI run as two different accounts, and an ACE naming the
    /// broker's own SID would lock the UI out of the pipe it is about to open.
    /// The UI's owner is the only correct answer, and the UI's pid is the only
    /// thing this process knows about it.
    ///
    /// Reuses the watchdog's handle: `PROCESS_QUERY_LIMITED_INFORMATION` is
    /// exactly the access `OpenProcessToken` documents as sufficient, so no
    /// second `OpenProcess` is needed.
    fn ui_owner_sid(ui: SharedHandle) -> Result<String> {
        let mut raw_token: HANDLE = std::ptr::null_mut();
        // SAFETY: `ui.raw()` is the live process handle from `open_ui_process`, held
        // open for the whole process. `raw_token` is a stack slot written only on
        // success; on failure it stays null and nothing needs closing.
        let opened = unsafe { OpenProcessToken(ui.raw(), TOKEN_QUERY, &mut raw_token) };
        if opened == 0 {
            let err = io::Error::last_os_error();
            return Err(Error::Capture(format!(
                "could not read the app's process token: {err}"
            )));
        }
        // Owned from here on, so every path below closes it exactly once.
        let token = OwnedHandle(raw_token);

        // Two-call idiom: a null buffer of length zero is the documented way to
        // ask for the size. The call is *expected* to fail, so its return value
        // is ignored and `needed` is what is checked.
        let mut needed: u32 = 0;
        // SAFETY: `token.0` is live for the call. Passing a null buffer with a
        // zero length cannot write anywhere; `needed` is a stack slot that
        // outlives the call and is the only thing written.
        unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            let err = io::Error::last_os_error();
            return Err(Error::Capture(format!(
                "could not size the app's token information: {err}"
            )));
        }

        // `TOKEN_USER` starts with a pointer, so the buffer has to be
        // pointer-aligned; a `Vec<u8>` only promises byte alignment, and reading
        // a `TOKEN_USER` out of one would be undefined behaviour that happens to
        // work. A `Vec<u64>` is over-aligned on every target this ships on, and
        // rounding the length up costs at most seven bytes.
        let mut buffer = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
        let capacity = u32::try_from(buffer.len() * size_of::<u64>()).unwrap_or(u32::MAX);
        // SAFETY: `buffer` is a live, `u64`-aligned allocation of exactly
        // `capacity` bytes and that same count is handed to the call, so it
        // cannot write out of bounds. `needed` is a live stack slot. The buffer
        // is only interpreted as a `TOKEN_USER` after the success check below.
        let filled = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                capacity,
                &mut needed,
            )
        };
        if filled == 0 {
            let err = io::Error::last_os_error();
            return Err(Error::Capture(format!(
                "could not read the app's token information: {err}"
            )));
        }

        // SAFETY: on success the buffer holds a `TOKEN_USER` whose `User.Sid`
        // points at a SID inside that same allocation. `buffer` is not moved,
        // resized or dropped between this read and the conversion below, so the
        // pointer stays valid for the `ConvertSidToStringSidW` call.
        let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };

        let mut raw_sid_text: *mut u16 = std::ptr::null_mut();
        // SAFETY: `sid` points into the still-live `buffer`. On success the call
        // writes a `LocalAlloc`'d, NUL-terminated UTF-16 string into
        // `raw_sid_text`, a stack slot alive for the call; that allocation is
        // freed exactly once below. On failure nothing is allocated.
        if unsafe { ConvertSidToStringSidW(sid, &mut raw_sid_text) } == 0 {
            let err = io::Error::last_os_error();
            return Err(Error::Capture(format!(
                "could not render the app's account SID: {err}"
            )));
        }
        // SAFETY: the call above reported success, so `raw_sid_text` is a
        // readable NUL-terminated UTF-16 string, and it is still allocated here.
        let text = unsafe { wide_to_string(raw_sid_text) };
        // SAFETY: the same `LocalAlloc`'d block, freed once, after its last read.
        unsafe { LocalFree(raw_sid_text.cast()) };
        Ok(text)
    }

    /// Creates the single-instance, outbound-only pipe the UI will connect to.
    ///
    /// Two details are protections rather than decoration:
    ///
    /// - `PIPE_REJECT_REMOTE_CLIENTS`. The default is `PIPE_ACCEPT_REMOTE_CLIENTS`,
    ///   and a named pipe's *default* security descriptor grants read access to
    ///   Everyone and to the anonymous account. Without this flag, a failure to
    ///   build the descriptor below would publish the captured game traffic over
    ///   SMB to the network. Do not drop it as redundant with the DACL; it is the
    ///   floor under the DACL.
    /// - `FILE_FLAG_FIRST_PIPE_INSTANCE` plus `nMaxInstances = 1`. A name already
    ///   taken then fails here rather than quietly becoming a second instance
    ///   behind someone else's.
    ///
    /// The descriptor itself grants full control to SYSTEM and Administrators
    /// (this process, and anything that could already debug it) and read-only
    /// access to the UI's owner. `D:P` makes it protected: no inherited ACE
    /// widens it.
    fn create_pipe(nonce: &str, owner_sid: &str) -> Result<OwnedHandle> {
        let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GR;;;{owner_sid})"));
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `sddl` is a NUL-terminated UTF-16 buffer owned by this frame
        // and alive for the call. On success the call writes a `LocalAlloc`'d
        // descriptor into `descriptor`, a live stack slot; that block is freed
        // exactly once below, on both the success and the failure path of the
        // pipe creation. The size out-parameter is optional and not wanted.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if converted == 0 {
            let err = io::Error::last_os_error();
            return Err(Error::Capture(format!(
                "could not build the capture channel's permissions: {err}"
            )));
        }

        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
            lpSecurityDescriptor: descriptor,
            // Nothing is inherited: the UI cannot spawn this process (the AppInfo
            // service does), so an inheritable handle would only be reachable by
            // children this process never creates.
            bInheritHandle: 0,
        };
        let name = wide(&pipe_name(nonce));
        // SAFETY: `name` and `attributes` are owned by this frame and outlive the
        // call; `attributes.lpSecurityDescriptor` points at the descriptor
        // allocated just above, still live. `CreateNamedPipeW` copies what it
        // needs and retains neither pointer. It reports failure with
        // `INVALID_HANDLE_VALUE`, checked below.
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_OUTBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                PIPE_OUT_BUFFER_BYTES,
                0,
                0,
                &attributes,
            )
        };
        // Captured before `LocalFree`, which would overwrite the thread's
        // last-error slot.
        let err = io::Error::last_os_error();
        // SAFETY: the block allocated by the conversion above, freed exactly once
        // here — on the success and the failure path alike — after
        // `CreateNamedPipeW` has finished reading it.
        unsafe { LocalFree(descriptor.cast()) };

        if handle == INVALID_HANDLE_VALUE {
            // With `FILE_FLAG_FIRST_PIPE_INSTANCE`, "access denied" does not mean
            // a permission problem the player can fix: it means the name is
            // already taken, i.e. something else got there first. A raw Win32
            // dump would send them looking in the wrong place entirely.
            let reason = if err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
                "another process is already using the capture channel — close any other \
                 copy of the app and try again"
                    .to_owned()
            } else {
                format!("could not open the capture channel: {err}")
            };
            return Err(Error::Capture(reason));
        }
        Ok(OwnedHandle(handle))
    }

    /// The manual-reset event that carries overlapped completions.
    ///
    /// One event serves the connect and every later write because the pipe is
    /// only ever touched by one thread at a time, and `WriteFile` resets the
    /// event itself when it starts an overlapped operation.
    fn create_event() -> Result<OwnedHandle> {
        // SAFETY: a null attributes pointer means "default security", a null name
        // means "unnamed"; the two flags are plain booleans. The returned handle
        // is null on failure, checked before use.
        let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if handle.is_null() {
            let err = io::Error::last_os_error();
            return Err(Error::Capture(format!(
                "could not create the capture channel's completion event: {err}"
            )));
        }
        Ok(OwnedHandle(handle))
    }

    /// Waits for the UI to connect, bounded three ways: by the UI's own process
    /// handle, by [`CONNECT_TIMEOUT_MS`], and by nothing else.
    ///
    /// A plain blocking `ConnectNamedPipe` is what the obvious implementation
    /// reaches for, and it is a trap: a UI killed during the UAC prompt — or one
    /// whose own connect budget expired — never connects, and this process would
    /// sit elevated forever holding the kernel driver, once per launch. Overlapped
    /// I/O is used purely so the wait can also watch the UI process handle and
    /// wake when it dies. The watchdog thread covers the same case; both exist
    /// because the cost of getting this wrong is a permanently stuck elevated
    /// process.
    fn wait_for_client(pipe: &OwnedHandle, event: &OwnedHandle, ui: SharedHandle) -> Result<()> {
        let mut overlapped = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        // SAFETY: both handles are live and owned by the caller for longer than
        // this frame. `overlapped` is a stack slot the kernel writes into until
        // the operation ends; every exit path below settles the operation with
        // `GetOverlappedResult` before this frame returns, so it is never dropped
        // while an I/O still references it.
        let connected = unsafe { ConnectNamedPipe(pipe.0, &mut overlapped) };
        if connected == 0 {
            // Read before any other Win32 call.
            let err = io::Error::last_os_error();
            let code = err.raw_os_error();
            if code == Some(ERROR_PIPE_CONNECTED as i32) {
                // The client got in between `CreateNamedPipeW` and this call; the
                // pipe is already connected and no I/O is pending.
                return Ok(());
            }
            if code != Some(ERROR_IO_PENDING as i32) {
                return Err(Error::Capture(format!(
                    "could not listen on the capture channel: {err}"
                )));
            }
        }

        let handles = [event.0, ui.raw()];
        // SAFETY: `handles` is a live two-element array of handles that outlives
        // the call; both are valid waitable objects. The count matches the array
        // length, so the call cannot read past it.
        let waited = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, CONNECT_TIMEOUT_MS) };
        let outcome = if waited == WAIT_OBJECT_0 {
            Ok(())
        } else if waited == WAIT_OBJECT_0 + 1 {
            Err(Error::Capture(
                "the app closed before the capture channel was connected".to_owned(),
            ))
        } else if waited == WAIT_TIMEOUT {
            Err(Error::Capture(format!(
                "the app did not connect to the capture channel within {}s",
                CONNECT_TIMEOUT_MS / 1000
            )))
        } else {
            let err = io::Error::last_os_error();
            Err(Error::Capture(format!(
                "waiting for the app to connect to the capture channel: {err}"
            )))
        };

        if outcome.is_err() {
            // SAFETY: `pipe.0` is still live and `overlapped` still identifies the
            // pending connect; `CancelIoEx` only marks it for cancellation.
            unsafe { CancelIoEx(pipe.0, &overlapped) };
        }
        let mut transferred: u32 = 0;
        // SAFETY: `bWait = TRUE` makes this return only once the connect has
        // completed or finished cancelling, which is what lets `overlapped` leave
        // this frame safely. Its own result is irrelevant — `outcome` already
        // says what happened.
        unsafe { GetOverlappedResult(pipe.0, &overlapped, &mut transferred, 1) };
        outcome
    }

    /// Starts the thread that owns the pipe and drains the queue onto it.
    ///
    /// Generic over the stop capability because `WinDivertSource::open_raw`'s
    /// stop type is not nameable outside the capture module; all this side needs
    /// is that it is a [`CaptureStop`], which the trait already guarantees is
    /// `Send`.
    fn spawn_writer<S: CaptureStop + 'static>(
        mut writer: PipeWriter,
        rx: Receiver<Outgoing>,
        mut stop: S,
        peer_gone: Arc<AtomicBool>,
    ) -> Result<std::thread::JoinHandle<Result<()>>> {
        std::thread::Builder::new()
            .name("broker-writer".to_owned())
            .spawn(move || {
                let outcome = writer_loop(&mut writer, &rx, &peer_gone);
                // On every exit path, not just the peer-gone one. The receive
                // loop is parked inside the driver and only this wake gets it
                // out: a UI that closed the pipe while the game sat idle would
                // otherwise leave this elevated process resident until the next
                // packet, which off the shop screen can be minutes away or never.
                let _ = stop.stop();
                outcome
            })
            .map_err(|err| {
                Error::Capture(format!("could not start the capture channel writer: {err}"))
            })
    }

    /// Writes queued frames until the queue closes or the pipe stops accepting.
    fn writer_loop(
        writer: &mut PipeWriter,
        rx: &Receiver<Outgoing>,
        peer_gone: &AtomicBool,
    ) -> Result<()> {
        for message in rx {
            let (kind, flags, payload): (u8, u8, &[u8]) = match &message {
                Outgoing::Packet { flags, payload } => (FRAME_KIND_PACKET, *flags, payload),
                Outgoing::Diagnostic(text) => (FRAME_KIND_DIAGNOSTIC, 0, text.as_bytes()),
                Outgoing::Fatal(text) => (FRAME_KIND_FATAL, 0, text.as_bytes()),
            };
            if let Err(err) = write_frame(writer, kind, flags, payload) {
                if is_peer_gone(&err) {
                    peer_gone.store(true, Ordering::Release);
                    return Ok(());
                }
                return Err(Error::Capture(format!("capture channel write: {err}")));
            }
        }
        Ok(())
    }

    /// True when the write failed because the unelevated end is gone.
    ///
    /// Three codes, not one. `ERROR_BROKEN_PIPE` (109) is the one everybody
    /// checks, but a named pipe also answers `ERROR_NO_DATA` (232) when the
    /// client end has closed while this end still holds its own open, and
    /// `ERROR_PIPE_NOT_CONNECTED` (233) once the connection has been torn down.
    /// Recognising only the first would turn an ordinary window close into a
    /// crash-log entry and an error banner on the next launch.
    fn is_peer_gone(err: &io::Error) -> bool {
        matches!(
            err.raw_os_error(),
            Some(code)
                if code == ERROR_BROKEN_PIPE as i32
                    || code == ERROR_NO_DATA as i32
                    || code == ERROR_PIPE_NOT_CONNECTED as i32
        )
    }

    /// Receives packets and hands them to the writer thread.
    ///
    /// Stays in `recv` as much as it possibly can: everything downstream of here
    /// is a `try_send` that cannot block, because any time spent not receiving is
    /// time the driver spends discarding packets nobody counts.
    fn pump(
        source: &mut WinDivertSource,
        tx: &SyncSender<Outgoing>,
        peer_gone: &AtomicBool,
    ) -> Result<()> {
        let mut dropped: u64 = 0;
        let mut loss_pending = false;
        loop {
            let payload = match source.recv_packet() {
                // Copied out immediately: the slice borrows the receive buffer
                // and is only valid until the next receive, while the frame has
                // to survive a trip through the queue to another thread.
                Ok(bytes) => bytes.to_vec(),
                Err(err) => {
                    // The writer shuts the capture down when the far end goes
                    // away, and that shutdown is what this failing receive is.
                    // A closed UI is a normal exit, not something to report.
                    return if peer_gone.load(Ordering::Acquire) {
                        Ok(())
                    } else {
                        Err(err)
                    };
                }
            };

            let flags = if loss_pending {
                FRAME_FLAG_CAPTURE_LOSS
            } else {
                0
            };
            match tx.try_send(Outgoing::Packet { flags, payload }) {
                // The bit rides the first frame that gets through *after* the
                // loss, which is exactly what the far end needs: it marks the
                // point in the byte stream where the hole is.
                Ok(()) => loss_pending = false,
                Err(TrySendError::Full(_)) => {
                    dropped += 1;
                    loss_pending = true;
                }
                // The writer is gone. Whether that was normal is its verdict to
                // give, not this loop's; `run` joins it and reports.
                Err(TrySendError::Disconnected(_)) => return Ok(()),
            }

            let (delivered, oversized) = source.raw_counters();
            if delivered == 1 || delivered.is_multiple_of(DIAGNOSTIC_EVERY) {
                // Dropped on the floor if the queue is full: a diagnostic must
                // never be the thing that stalls the receive loop.
                let _ = tx.try_send(Outgoing::Diagnostic(format!(
                    "capture funnel: delivered={delivered} oversized={oversized} \
                     dropped_to_backpressure={dropped}"
                )));
            }
        }
    }

    /// The text to put in a kind-2 frame.
    ///
    /// Unwraps `Error::Capture` instead of formatting the error: the unelevated
    /// end wraps whatever arrives back into `Error::Capture`, so the `Display`
    /// form would reach the player as "network capture: network capture: ...".
    fn fatal_text(err: &Error) -> String {
        match err {
            Error::Capture(text) => text.clone(),
            other => other.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_is_accepted_only_when_it_is_a_non_zero_u16() {
        assert_eq!(parse_port("3333").unwrap(), 3333);
        assert_eq!(parse_port("1").unwrap(), 1);
        assert_eq!(parse_port("65535").unwrap(), 65535);
    }

    #[test]
    fn a_zero_out_of_range_or_non_numeric_port_is_rejected() {
        for raw in ["0", "65536", "-1", "", " 3333", "3333 ", "0x0d05", "abc"] {
            assert!(
                parse_port(raw).is_err(),
                "{raw:?} must not pass port validation"
            );
        }
    }

    #[test]
    fn a_nonce_is_accepted_only_when_it_is_exactly_thirty_two_hex_characters() {
        let lower = "0123456789abcdef0123456789abcdef";
        let upper = "0123456789ABCDEF0123456789ABCDEF";
        assert_eq!(lower.len(), PIPE_NONCE_HEX_CHARS);
        assert_eq!(parse_pipe_nonce(lower).unwrap(), lower);
        assert_eq!(parse_pipe_nonce(upper).unwrap(), upper);
    }

    #[test]
    fn a_nonce_of_the_wrong_length_or_alphabet_is_rejected() {
        for raw in [
            "",
            "0123456789abcdef0123456789abcde",   // one short
            "0123456789abcdef0123456789abcdef0", // one long
            "0123456789abcdef0123456789abcdeg",  // 'g' is not hex
            "0123456789abcdef0123456789abcde-",  // would steer the pipe name
            "0123456789abcdef0123456789abcd\\e", // ditto
            "0123456789abcdef 123456789abcdef",
        ] {
            assert!(
                parse_pipe_nonce(raw).is_err(),
                "{raw:?} must not pass nonce validation"
            );
        }
    }

    #[test]
    fn the_rejection_message_never_echoes_the_nonce_back() {
        // It is a shared secret; a validation failure must not be what puts it
        // in a log file.
        let raw = "deadbeefdeadbeefdeadbeefdeadbee";
        let err = parse_pipe_nonce(raw).expect_err("short nonce must fail");
        assert!(!err.to_string().contains(raw), "the message leaked it");
    }

    #[test]
    fn a_ui_pid_is_accepted_only_when_it_is_a_non_zero_u32() {
        assert_eq!(parse_ui_pid("1").unwrap(), 1);
        assert_eq!(parse_ui_pid("4294967295").unwrap(), u32::MAX);
    }

    #[test]
    fn a_zero_negative_or_over_range_ui_pid_is_rejected() {
        for raw in ["0", "-1", "4294967296", "", "12 ", "1.0", "pid"] {
            assert!(
                parse_ui_pid(raw).is_err(),
                "{raw:?} must not pass pid validation"
            );
        }
    }

    #[test]
    fn the_broker_command_line_leads_with_the_role_flag_and_carries_the_three_arguments() {
        let nonce = "0123456789abcdef0123456789abcdef";
        let line = broker_command_line(3333, nonce, 4242);
        // The flag first, because that is what `capture_broker_argv` scans for
        // before anything else in `main` has run.
        assert!(line.starts_with(BROKER_ARGV_FLAG), "{line}");
        assert_eq!(
            line,
            format!("{BROKER_ARGV_FLAG} --port 3333 --pipe {nonce} --ui-pid 4242")
        );
    }

    #[test]
    fn every_token_of_the_broker_command_line_survives_being_split_on_whitespace() {
        // The elevated side receives argv already split by the shell, so a value
        // that could contain a space would silently become two arguments — and
        // the one that carries a secret would be the one that broke.
        let line = broker_command_line(1, "0123456789ABCDEF0123456789ABCDEF", u32::MAX);
        assert_eq!(line.split_whitespace().count(), 7, "{line}");
    }

    #[test]
    fn the_pipe_name_is_a_local_pipe_carrying_the_nonce() {
        let nonce = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            pipe_name(nonce),
            r"\\.\pipe\arkyve-0123456789abcdef0123456789abcdef"
        );
    }
}
