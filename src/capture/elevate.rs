//! The unelevated half of the capture split: launches [`crate::broker`] with an
//! administrator token and reads raw packets back off its named pipe.
//!
//! # Why a named pipe, and not something simpler
//!
//! An anonymous pipe would be the obvious choice, and it is impossible here.
//! Elevation is not something a process does to a child: `CreateProcessW`
//! refuses outright (`ERROR_ELEVATION_REQUIRED`), and the only way up is
//! `ShellExecuteExW` with the `runas` verb, which hands the request to the
//! AppInfo service. **AppInfo creates the process, not us** — so there is no
//! handle inheritance to piggyback on, no `STARTUPINFO` to fill in, and no
//! shared address space to pass anything through. A named pipe, whose name both
//! sides derive from a nonce on the elevated process's command line, is what is
//! left. Do not "optimize" it into an anonymous pipe; it cannot work.
//!
//! # The three things that make this safe rather than merely working
//!
//! - **The server's identity is verified after connecting.** See
//!   [`verify_server_identity`]. This is the check that closes the squat, not
//!   the nonce.
//! - **The client handle asks for `GENERIC_READ` only.** The pipe carries a
//!   High mandatory label from its elevated creator, and the no-write-up policy
//!   fails any write bit in the access mask — including one asked for and never
//!   used.
//! - **Stopping never closes the pipe handle.** See [`PipeStop`].
//!
//! Everything in here is gated on `windivert-backend` together with the rest of
//! the Windows capture path: without the backend there is no broker to launch,
//! and the two portable lanes of `just verify` build with neither this module
//! nor `windows-sys`.

use std::io::{self, Read};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tracing::{debug, info, warn};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_CANCELLED, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Cryptography::ProcessPrng;
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, ReadFile,
};
use windows_sys::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows_sys::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetProcessId, INFINITE, OpenProcessToken, SetEvent,
    WaitForMultipleObjects, WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use super::CaptureStop;
use crate::broker::pipe_name;
use crate::error::{Error, Result};

/// The argv token that puts the elevated copy of this exe into broker mode.
///
/// Spelled here and matched by the dispatch in `main`. Both sides also agree on
/// the three arguments below; the broker validates every one of them
/// ([`crate::broker::parse_port`] and friends) because this command line is the
/// entire surface the medium-integrity side has on the administrator process.
const BROKER_ARGV_FLAG: &str = "--capture-broker";

/// Nonce length in bytes. Hex-encoded it must come out at exactly
/// [`crate::broker::PIPE_NONCE_HEX_CHARS`] characters, which the unit test below
/// asserts against the broker's own validator rather than against a literal.
const NONCE_BYTES: usize = 16;

/// How long to keep trying to connect before giving up.
///
/// Deliberately shorter than the broker's own 30-second wait for a client, so
/// that when both ends give up the broker is the one still listening — a UI that
/// timed out first leaves an elevated process that notices and exits, whereas
/// the reverse ordering would leave it waiting on a client that is never coming.
/// The UAC prompt is *not* inside this budget: `SEE_MASK_NOASYNC` makes
/// `ShellExecuteExW` return only once the process exists, so everything measured
/// here is broker startup — a `CreateNamedPipeW` that happens before the driver
/// is even touched.
const CONNECT_BUDGET: Duration = Duration::from_secs(15);

/// Pause between two connect attempts. Small enough that the common case (the
/// broker is already listening) costs at most one step, large enough not to spin.
const CONNECT_RETRY_STEP_MS: u32 = 50;

/// How long `WaitNamedPipeW` parks when every instance is busy.
const BUSY_WAIT_MS: u32 = 200;

/// A Win32 handle this value owns and closes exactly once.
struct OwnedHandle(HANDLE);

// SAFETY: a `HANDLE` is a process-wide index into the kernel handle table, not
// a thread-affine pointer; it is neither `Send` nor `Sync` only because the
// alias is spelled as a raw pointer. This wrapper is the sole owner of the
// handle it holds, exposes it by shared reference only, and closes it once from
// `Drop` — so the one race that would matter, two threads closing the same
// handle, cannot arise.
unsafe impl Send for OwnedHandle {}
// SAFETY: as above. Every use through a shared reference is a read of the
// handle value, which the kernel serializes for us.
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a create/open call checked for success
        // before the wrapper was built, this value is the only owner, and `Drop`
        // runs once — so the handle is neither closed twice nor used afterwards.
        unsafe { CloseHandle(self.0) };
    }
}

/// The pipe, and the wake capability that ends a read parked on it.
///
/// Shared between the [`PipeReader`] (which lives on the capture thread) and the
/// [`PipeStop`] (which stays with `CaptureWorker` on the teardown side), and that
/// sharing is what makes the pair safe. `CaptureSource` splits the two: the
/// reader is moved onto the capture thread and can end — and drop — while the
/// stop is still held. A `PipeStop` holding a bare copy of the handle would then
/// be cancelling I/O on a closed, possibly already-recycled handle. Behind an
/// `Arc` the handle is closed by whichever of the two dies last, so a live
/// `PipeStop` always names a live pipe.
struct SharedPipe {
    pipe: OwnedHandle,
    /// Manual-reset, set exactly once by [`PipeStop::stop`] and never reset.
    /// Being sticky is the point — see [`PipeReader::read`].
    stop_event: OwnedHandle,
    /// Mirrors `stop_event` for the reader's benefit: it is what tells
    /// "cancelled on purpose" apart from "the broker died", which are the same
    /// `ERROR_OPERATION_ABORTED` at the Win32 level only for the first one.
    stopped: AtomicBool,
}

/// [`std::io::Read`] over the broker's pipe, for [`super::PipeSource`] to frame.
pub(crate) struct PipeReader {
    channel: Arc<SharedPipe>,
    /// This reader's own completion event. Only one thread ever reads, so one
    /// event is reused for every read rather than created per call.
    read_event: OwnedHandle,
}

impl Read for PipeReader {
    /// Reads once, or reports end of stream when the capture is stopped or the
    /// broker is gone.
    ///
    /// Overlapped, and that is not gratuitous. The obvious implementation is a
    /// plain blocking `ReadFile` woken by `CancelIoEx` from `stop()`, and it
    /// races: `CancelIoEx` only cancels I/O that is *already pending*, so a stop
    /// landing in the window between the loop's last completed read and its next
    /// entry into the kernel cancels nothing, and the read that follows blocks
    /// until the broker writes again — which, with the game idle, can be never.
    /// `CaptureWorker::stop_and_join` has no timeout, so that is a hung window
    /// close, not a slow one. Waiting on the read completion *and* a sticky stop
    /// event closes the window in the only direction that matters: the event is
    /// manual-reset and never cleared, so a stop observed too early is seen by
    /// the pre-check below and a stop observed too late is already signalled
    /// when the wait starts.
    ///
    /// Three endings are reported as a clean `Ok(0)` rather than an error, and
    /// `PipeSource` turns that into its "capture broker exited" case:
    /// `ERROR_BROKEN_PIPE` (the broker closed its end or died — std's own handle
    /// reader maps it the same way, and `PipeSource`'s EOF handling is written
    /// against that mapping), `ERROR_OPERATION_ABORTED` after a deliberate stop,
    /// and the stop pre-check.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.channel.stopped.load(Ordering::Acquire) {
            return Ok(0);
        }

        let mut overlapped = OVERLAPPED {
            hEvent: self.read_event.0,
            ..Default::default()
        };
        // Frames are read in header- and payload-sized chunks, both far below
        // `u32::MAX`; the saturation is here so the cast cannot lie.
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        let mut read: u32 = 0;
        // SAFETY: `self.channel.pipe.0` is the live pipe handle the `Arc` keeps
        // open for at least as long as this reader, and `self.read_event.0` the
        // event this struct owns. `buf` is writable for `len` bytes and is
        // borrowed for the whole call. `overlapped` and `read` are stack slots
        // the kernel may write into until the operation ends, and every path
        // below settles the operation with `GetOverlappedResult(bWait = TRUE)`
        // before this frame returns, so neither is dropped while an I/O still
        // references it. Failure is a zero return plus the thread's last-error
        // slot.
        let started = unsafe {
            ReadFile(
                self.channel.pipe.0,
                buf.as_mut_ptr(),
                len,
                &mut read,
                &mut overlapped,
            )
        };
        if started == 0 {
            // Read before any other Win32 call: `GetLastError` is per-thread and
            // the very next call overwrites it.
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return self.classify(err);
            }

            let handles = [self.read_event.0, self.channel.stop_event.0];
            // SAFETY: `handles` is a live two-element array of valid waitable
            // handles that outlives the call, and the count matches its length.
            let waited = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
            if waited != WAIT_OBJECT_0 {
                // Either the stop event fired or the wait itself failed; both
                // mean this read must not be left pending. `CancelIoEx` is the
                // exact counterpart of `WinDivertShutdown` — it releases the
                // blocked read without touching the handle, which the
                // `CaptureStop` contract forbids closing here.
                //
                // SAFETY: the pipe handle is still live and `overlapped` still
                // identifies this pending read; `CancelIoEx` only marks it.
                unsafe { CancelIoEx(self.channel.pipe.0, &overlapped) };
            }
            // SAFETY: same live handle and the same `overlapped` that started
            // the read; `bWait = TRUE` returns only once the operation has
            // completed or finished cancelling, which is what lets `overlapped`
            // leave this frame safely.
            if unsafe { GetOverlappedResult(self.channel.pipe.0, &overlapped, &mut read, 1) } == 0 {
                return self.classify(io::Error::last_os_error());
            }
        }
        Ok(read as usize)
    }
}

impl PipeReader {
    /// Turns a failed read into either end of stream or a real error.
    fn classify(&self, err: io::Error) -> io::Result<usize> {
        match err.raw_os_error() {
            // The broker closed its end, exited, or was killed. Its own log
            // line, if it managed one, travelled ahead of this as a kind-2
            // frame; there is nothing to add here.
            Some(code) if code == ERROR_BROKEN_PIPE as i32 => Ok(0),
            // Cancelled — but only ours is an ordinary ending. Without the flag
            // an abort we did not ask for would read as a clean shutdown and
            // hide a capture that stopped for a reason.
            Some(code)
                if code == ERROR_OPERATION_ABORTED as i32
                    && self.channel.stopped.load(Ordering::Acquire) =>
            {
                Ok(0)
            }
            _ => Err(err),
        }
    }
}

/// Remote wake for a [`PipeReader`] parked in a read.
///
/// **Does not close the pipe handle**, and must never start to. `CaptureStop`
/// says so in as many words, and the concurrency behind that rule is real here:
/// `CaptureWorker::stop_and_join` calls this from the teardown thread while the
/// capture thread sits inside `ReadFile` on the same handle. Closing it would
/// leave `join()` waiting on a thread blocked on a handle that no longer exists,
/// so every window close would burn the full teardown grace and warn about a
/// capture session outliving the process. Signalling an event the reader is
/// already waiting on costs nothing and is inherently idempotent.
pub(crate) struct PipeStop {
    channel: Arc<SharedPipe>,
}

impl CaptureStop for PipeStop {
    fn stop(&mut self) -> Result<()> {
        // Idempotent by contract. The swap also orders the flag before the
        // event, so a reader that observes the event necessarily observes the
        // flag and classifies its own cancellation correctly.
        if self.channel.stopped.swap(true, Ordering::Release) {
            return Ok(());
        }
        // SAFETY: `stop_event` is the live manual-reset event the `Arc` keeps
        // open for as long as this value exists; `SetEvent` only signals it.
        if unsafe { SetEvent(self.channel.stop_event.0) } == 0 {
            return Err(Error::Capture(format!(
                "could not stop the capture channel: {}",
                io::Error::last_os_error()
            )));
        }
        Ok(())
    }
}

/// Launches the broker with an administrator token and connects to its pipe.
///
/// Blocking, and unapologetically so: it parks on a modal UAC prompt of
/// unbounded duration. [`blocking`] keeps that off the runtime's back.
pub(crate) fn spawn_elevated_broker(port: u16) -> Result<(PipeReader, PipeStop)> {
    blocking(|| spawn_elevated_broker_inner(port))
}

/// Runs one blocking call without starving the runtime.
///
/// Lifted from `actuator::blocking`, deliberately rather than shared: the
/// actuator lives behind its own feature and this module behind another, so a
/// single definition would have to move somewhere neither owns. The flavor probe
/// is the load-bearing part — `block_in_place` panics anywhere but the
/// multi-thread runtime, and the app's tests drive session code on the
/// current-thread runtime (and sometimes with no runtime at all).
fn blocking<T>(call: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(call),
        _ => call(),
    }
}

/// The whole launch, start to connected channel.
///
/// The broker's process handle is deliberately local to this function: it is
/// needed for the connect budget and for the identity check, and for nothing
/// afterwards — the broker's own watchdog is what ties its lifetime to this
/// process's, not this handle. `OwnedHandle` closes it on the way out, on the
/// success and the failure path alike.
fn spawn_elevated_broker_inner(port: u16) -> Result<(PipeReader, PipeStop)> {
    warn_if_already_elevated();

    let nonce = new_pipe_nonce()?;
    let name = pipe_name(&nonce);
    let exe = current_executable_path()?;
    let ui_pid = std::process::id();

    let broker = elevate_self(
        &exe,
        &format!("{BROKER_ARGV_FLAG} --port {port} --pipe {nonce} --ui-pid {ui_pid}"),
    )?;
    let pipe = connect_to_broker(&name, &broker)?;
    verify_server_identity(&pipe, &broker)?;
    info!(port, "capture broker running elevated; channel connected");
    channel(pipe)
}

/// Builds the reader/stop pair over a connected pipe handle.
fn channel(pipe: OwnedHandle) -> Result<(PipeReader, PipeStop)> {
    let channel = Arc::new(SharedPipe {
        pipe,
        stop_event: create_event()?,
        stopped: AtomicBool::new(false),
    });
    let reader = PipeReader {
        channel: Arc::clone(&channel),
        read_event: create_event()?,
    };
    Ok((reader, PipeStop { channel }))
}

/// A manual-reset, initially unsignalled event.
fn create_event() -> Result<OwnedHandle> {
    // SAFETY: a null attributes pointer means "default security" and a null name
    // "unnamed"; the two flags are plain booleans. The handle is null on
    // failure, checked before it is stored.
    let handle = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if handle.is_null() {
        return Err(Error::Capture(format!(
            "could not create the capture channel's event: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(OwnedHandle(handle))
}

/// 128 bits from the OS CSPRNG, hex-encoded.
///
/// `ProcessPrng` is the modern handle-free primitive — the one `getrandom` uses
/// on Windows — so this needs neither a provider handle nor a `rand` dependency.
/// It must not be derived from a pid, a clock, or anything else an observer can
/// reproduce: a guessable nonce is a pipe name a squatter can create first.
/// (Guessing it is not the only way to learn it — the nonce travels on the
/// elevated process's command line, which a same-user process can read — which
/// is exactly why [`verify_server_identity`] exists and why this secret is a
/// speed bump rather than the defence.)
fn new_pipe_nonce() -> Result<String> {
    let mut bytes = [0u8; NONCE_BYTES];
    // SAFETY: `bytes` is a live stack array and exactly its own length is passed
    // as the byte count, so the call cannot write out of bounds. It is
    // documented to always succeed; the check below costs nothing and keeps a
    // future failure from silently producing an all-zero nonce.
    if unsafe { ProcessPrng(bytes.as_mut_ptr(), bytes.len()) } == 0 {
        return Err(Error::Capture(format!(
            "could not generate the capture channel's nonce: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(hex_encode(&bytes))
}

/// Lowercase hex, two characters per byte.
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0xf) as u32, 16).unwrap_or('0'));
    }
    out
}

/// This process's own image path, as a NUL-terminated wide string.
///
/// The loop is the point. `GetModuleFileNameW` does not report how much room it
/// needs: on truncation it fills the buffer, NUL-terminates it, returns `nSize`
/// — the size it was given — and sets `ERROR_INSUFFICIENT_BUFFER`. A single
/// `MAX_PATH` call therefore succeeds *and* lies for any longer path, and what it
/// hands back is a truncated path that would elevate the wrong file or nothing
/// at all. `len == size` is the truncation signal; grow and retry.
fn current_executable_path() -> Result<Vec<u16>> {
    // The Win32 path cap with long paths enabled. Nothing legitimate reaches it,
    // so hitting it means the loop would spin rather than converge.
    const MAX_WIDE_CHARS: usize = 32_768;

    let mut size = 260usize;
    loop {
        let mut buffer = vec![0u16; size];
        // SAFETY: a null module handle asks for the current process's own image.
        // `buffer` is a live allocation of `size` elements and that same count is
        // passed, so the call cannot write past it; it is not read before the
        // success check below.
        let len = unsafe { GetModuleFileNameW(ptr::null_mut(), buffer.as_mut_ptr(), size as u32) }
            as usize;
        if len == 0 {
            return Err(Error::Capture(format!(
                "could not locate this program's own file: {}",
                io::Error::last_os_error()
            )));
        }
        if len < size {
            // `len` excludes the NUL the call wrote at `buffer[len]`; keep it.
            buffer.truncate(len + 1);
            return Ok(buffer);
        }
        if size >= MAX_WIDE_CHARS {
            return Err(Error::Capture(
                "this program's own path is too long to launch a capture helper from".to_owned(),
            ));
        }
        size *= 2;
    }
}

/// Re-launches `exe` with `arguments` under an administrator token.
///
/// Returns a handle to the elevated process, which is both the connect budget's
/// clock (a broker that died is not worth waiting for) and the identity
/// [`verify_server_identity`] compares the pipe's server against.
fn elevate_self(exe: &[u16], arguments: &str) -> Result<OwnedHandle> {
    // Required before `ShellExecuteExW` on this thread. `SEE_MASK_NOASYNC`
    // (below) keeps the call synchronous, so the apartment can be torn down
    // again as soon as it returns.
    let _com = ComScope::enter()?;

    let verb = wide("runas");
    let parameters = wide(arguments);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: u32::try_from(size_of::<SHELLEXECUTEINFOW>()).unwrap_or(u32::MAX),
        // - `NOCLOSEPROCESS` is what makes `hProcess` come back at all.
        // - `NOASYNC` is mandatory, not an optimisation: the documentation
        //   requires it whenever the calling thread has no message loop, and
        //   this runs on a tokio worker. Without it the call may return before
        //   the shell is done with `info`, i.e. before `hProcess` is written.
        // - `FLAG_NO_UI` suppresses the shell's own error dialogs; a failure
        //   belongs in the app's banner, in the app's words.
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: verb.as_ptr(),
        lpFile: exe.as_ptr(),
        lpParameters: parameters.as_ptr(),
        // The broker has no window and no console; anything else would flash a
        // frame on screen at every launch.
        nShow: SW_HIDE,
        ..Default::default()
    };

    // SAFETY: `info` is a live, fully initialised stack struct whose `lpVerb`,
    // `lpFile` and `lpParameters` point at NUL-terminated wide buffers owned by
    // this frame and alive across the call. `SEE_MASK_NOASYNC` guarantees the
    // shell is finished with all of them by the time this returns. On success
    // the call writes `hProcess`, which this frame takes ownership of below.
    let started = unsafe { ShellExecuteExW(&mut info) };
    if started == 0 {
        // Read before any other Win32 call. The documentation points at
        // `GetLastError` here rather than at `hInstApp`, which only carries a
        // legacy error range.
        let err = io::Error::last_os_error();
        // By far the most common outcome in the field: the player closed the
        // prompt, or does not have administrator rights to grant. A raw
        // "os error 1223" would tell them nothing about what to do next.
        if err.raw_os_error() == Some(ERROR_CANCELLED as i32) {
            return Err(Error::Capture(
                "capture requires administrator approval — the Windows prompt was dismissed, \
                 so no traffic can be observed. Restart the app and choose Yes."
                    .to_owned(),
            ));
        }
        return Err(Error::Capture(format!(
            "could not start the capture helper with administrator rights: {err}"
        )));
    }

    if info.hProcess.is_null() {
        // Documented as possible even on success (the shell may hand the request
        // to an already-running instance). It is not survivable here: the
        // process handle is the only proof of *which* process is serving the
        // pipe, and connecting to an unverifiable server is the squat this
        // design exists to refuse. Rare enough to be worth failing loudly over.
        return Err(Error::Capture(
            "Windows started the capture helper without returning a handle to it — refusing to \
             use a capture channel whose owner cannot be verified"
                .to_owned(),
        ));
    }
    Ok(OwnedHandle(info.hProcess))
}

/// COM initialised for the current thread, and uninitialised again on drop.
///
/// `ShellExecuteExW` requires an initialised apartment. Balanced rather than
/// left on: the caller is a tokio worker that goes on to run unrelated tasks.
struct ComScope {
    /// False when the thread already had an apartment of a different model —
    /// that one belongs to whoever created it, so this scope must not end it.
    owned: bool,
}

impl ComScope {
    fn enter() -> Result<Self> {
        // RPC_E_CHANGED_MODE. Not an error for us: it means the thread is
        // already in an apartment, just not the one asked for, and
        // `ShellExecuteExW` is happy with either.
        const CHANGED_MODE: i32 = -2_147_417_850;

        // The two flags are `i32` constants and the parameter is a `u32`
        // bitmask; the cast is the encoding, not a conversion.
        let model = (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32;
        // SAFETY: the reserved pointer is null as documented, and the flags are
        // a plain bitmask. Nothing is borrowed across the call.
        let hr = unsafe { CoInitializeEx(ptr::null(), model) };
        if hr == CHANGED_MODE {
            return Ok(Self { owned: false });
        }
        if hr < 0 {
            return Err(Error::Capture(format!(
                "could not prepare the shell for an elevated launch (COM error {hr:#x})"
            )));
        }
        Ok(Self { owned: true })
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: balances exactly one successful `CoInitializeEx` on this
            // same thread, and runs after the only call that needed it.
            unsafe { CoUninitialize() };
        }
    }
}

/// Opens the broker's pipe, retrying until it exists or the budget runs out.
///
/// The budget is driven by the broker's process handle as much as by the clock:
/// each pause is a wait on that handle, so a broker that failed on startup ends
/// this in milliseconds instead of costing the player fifteen seconds of silence
/// before an error appears.
fn connect_to_broker(name: &str, broker: &OwnedHandle) -> Result<OwnedHandle> {
    let wide_name = wide(name);
    let mut remaining = CONNECT_BUDGET;
    loop {
        if let Some(pipe) = try_open_pipe(&wide_name)? {
            return Ok(pipe);
        }

        let Some(step_ms) = retry_wait_ms(remaining) else {
            return Err(Error::Capture(format!(
                "the capture helper did not open its channel within {}s",
                CONNECT_BUDGET.as_secs()
            )));
        };
        // SAFETY: `broker.0` is the live process handle this frame's caller
        // owns; the wait only reads it.
        let waited = unsafe { WaitForSingleObject(broker.0, step_ms) };
        if waited == WAIT_OBJECT_0 {
            // It exited before serving the pipe, so it never had a channel to
            // explain itself on — kind-2 frames only exist after a connection.
            // The crash log is the only remaining trace.
            return Err(Error::Capture(
                "the capture helper stopped before the capture channel was ready — see \
                 crash.log in the app's local data folder"
                    .to_owned(),
            ));
        }
        if waited != WAIT_TIMEOUT {
            return Err(Error::Capture(format!(
                "waiting for the capture helper: {}",
                io::Error::last_os_error()
            )));
        }
        remaining = remaining.saturating_sub(Duration::from_millis(u64::from(step_ms)));
    }
}

/// One `CreateFileW` attempt. `Ok(None)` means "not there yet, keep trying" and
/// covers exactly the two transient outcomes.
fn try_open_pipe(name: &[u16]) -> Result<Option<OwnedHandle>> {
    // `GENERIC_READ` alone, and never `GENERIC_READ | GENERIC_WRITE` "for
    // symmetry": the pipe inherits a High mandatory label from its elevated
    // creator, and the no-write-up policy fails the open on any write bit in the
    // mask — whether or not a byte is ever written. The channel is one-way by
    // design anyway; nothing travels up it.
    //
    // SAFETY: `name` is a NUL-terminated wide string owned by the caller and
    // alive across the call. Both optional pointers are null, which is
    // "no security attributes" and "no template file". Failure is reported as
    // `INVALID_HANDLE_VALUE`, checked before the handle is owned.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ,
            0,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            ptr::null_mut(),
        )
    };
    if handle != INVALID_HANDLE_VALUE {
        return Ok(Some(OwnedHandle(handle)));
    }
    // Read before any other Win32 call.
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        // The broker exists but has not reached `CreateNamedPipeW` yet.
        Some(code) if code == ERROR_FILE_NOT_FOUND as i32 => Ok(None),
        // Every instance is in use. The documented answer is `WaitNamedPipeW`,
        // which parks in the kernel until one frees up, rather than a sleep that
        // is either too short (spin) or too long (a stall the player feels).
        Some(code) if code == ERROR_PIPE_BUSY as i32 => {
            // SAFETY: same NUL-terminated wide name, alive across the call; the
            // timeout is a plain integer. Its result is deliberately ignored —
            // the retry loop re-attempts either way.
            unsafe { WaitNamedPipeW(name.as_ptr(), BUSY_WAIT_MS) };
            Ok(None)
        }
        _ => Err(Error::Capture(format!(
            "could not open the capture channel: {err}"
        ))),
    }
}

/// How long to park before the next connect attempt: the fixed step, or the rest
/// of the budget when that is shorter, or `None` when the budget is spent.
fn retry_wait_ms(remaining: Duration) -> Option<u32> {
    let left = u32::try_from(remaining.as_millis()).unwrap_or(u32::MAX);
    (left > 0).then(|| left.min(CONNECT_RETRY_STEP_MS))
}

/// Refuses a pipe served by anything other than the process we just launched.
///
/// **This is the check that closes the squat, and it is not optional polish.**
/// `FILE_FLAG_FIRST_PIPE_INSTANCE` protects the *broker*: its create fails if
/// the name is already taken. It does nothing for this side — a squatter who got
/// there first breaks the broker *and* satisfies this process's `CreateFileW`
/// against its own pipe, from which it could then feed the reassembler whatever
/// it liked. The nonce is no defence: it travels on the elevated process's
/// command line, which a medium-integrity process of the same user can read
/// (`Get-CimInstance Win32_Process`). Comparing the pipe's server process id
/// against the one the shell just started for us is what actually establishes
/// that the bytes come from our own broker.
///
/// Both parties being medium integrity, the hole this closes is data integrity,
/// not privilege escalation. It is closed because the design claims it is.
fn verify_server_identity(pipe: &OwnedHandle, broker: &OwnedHandle) -> Result<()> {
    let mut server_pid: u32 = 0;
    // SAFETY: `pipe.0` is the connected pipe handle owned by the caller;
    // `server_pid` is a stack slot written only on success.
    if unsafe { GetNamedPipeServerProcessId(pipe.0, &mut server_pid) } == 0 {
        return Err(Error::Capture(format!(
            "could not identify the process serving the capture channel: {}",
            io::Error::last_os_error()
        )));
    }
    // SAFETY: `broker.0` is the live process handle from `elevate_self`, opened
    // with the access `GetProcessId` needs. Returns zero on failure.
    let broker_pid = unsafe { GetProcessId(broker.0) };
    if broker_pid == 0 {
        return Err(Error::Capture(format!(
            "could not identify the capture helper: {}",
            io::Error::last_os_error()
        )));
    }
    if server_pid != broker_pid {
        return Err(Error::Capture(format!(
            "the capture channel is served by process {server_pid}, not by the capture helper \
             (process {broker_pid}) — refusing to read traffic from it"
        )));
    }
    Ok(())
}

/// Says out loud when the whole privilege split has quietly evaporated.
///
/// If the player right-clicks the exe and picks "Run as administrator" — which
/// the README spent a release teaching them to do — this process is already
/// elevated, `runas` prompts for nothing, and the broker it launches is a peer
/// at the same integrity level rather than a boundary. Everything still works,
/// which is precisely the problem: nothing else in the product would ever
/// mention it. Not fatal, because refusing to run would be a worse answer to
/// "you gave us too many rights".
fn warn_if_already_elevated() {
    match process_is_elevated() {
        Ok(true) => warn!(
            "this app was started as administrator, so the capture helper it launches gains \
             nothing by being separate — no consent prompt appears and both processes run \
             elevated, which removes the privilege separation this build is built around. \
             Start it normally (double-click, no \"Run as administrator\") to get it back."
        ),
        Ok(false) => {}
        // Never fatal: this is a diagnostic about our own token, not a gate.
        Err(err) => debug!(error = %err, "could not read this process's elevation state"),
    }
}

/// True when this process runs with an elevated token.
fn process_is_elevated() -> io::Result<bool> {
    let mut raw_token: HANDLE = ptr::null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no closing
    // and is always valid. `raw_token` is a stack slot written only on success;
    // on failure it stays null and nothing needs closing.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) };
    if opened == 0 {
        return Err(io::Error::last_os_error());
    }
    // Owned from here on, so both paths below close it exactly once.
    let token = OwnedHandle(raw_token);

    let mut elevation = TOKEN_ELEVATION::default();
    let size = u32::try_from(size_of::<TOKEN_ELEVATION>()).unwrap_or(u32::MAX);
    let mut returned: u32 = 0;
    // SAFETY: `token.0` is live for the call. `elevation` is a live stack struct
    // and exactly its own size is passed, so the call cannot write past it;
    // `returned` is another live stack slot. The struct is only read after the
    // success check.
    let read = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            (&raw mut elevation).cast(),
            size,
            &mut returned,
        )
    };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(elevation.TokenIsElevated != 0)
}

/// Null-terminated UTF-16, the shape W-suffixed Win32 calls want.
///
/// The buffer *is* the value: dropping it leaves the caller passing a dangling
/// `as_ptr()` to Win32, so every call site keeps it in a named local that
/// outlives the call.
#[must_use]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::{PIPE_NONCE_HEX_CHARS, parse_pipe_nonce};

    #[test]
    fn hex_encoding_is_lowercase_and_two_characters_per_byte() {
        assert_eq!(hex_encode(&[]), "");
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xf0, 0xff]), "000ff0ff");
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    #[test]
    fn a_generated_nonce_is_exactly_what_the_broker_will_accept() {
        // The two sides agree by construction, not by two literals that could
        // drift: the encoder's output is fed to the broker's own validator.
        let nonce = new_pipe_nonce().expect("nonce");
        assert_eq!(nonce.len(), PIPE_NONCE_HEX_CHARS);
        assert_eq!(parse_pipe_nonce(&nonce).expect("valid nonce"), nonce);
    }

    #[test]
    fn two_generated_nonces_differ() {
        // A constant nonce would be a pipe name anyone can create first. This
        // cannot prove randomness, but it does catch the buffer never being
        // written at all.
        assert_ne!(
            new_pipe_nonce().expect("nonce"),
            new_pipe_nonce().expect("nonce")
        );
    }

    #[test]
    fn the_retry_wait_never_overshoots_the_remaining_budget() {
        assert_eq!(retry_wait_ms(Duration::ZERO), None);
        // Shorter than a step: wait exactly what is left, never a full step.
        assert_eq!(retry_wait_ms(Duration::from_millis(1)), Some(1));
        assert_eq!(
            retry_wait_ms(Duration::from_millis(u64::from(CONNECT_RETRY_STEP_MS) - 1)),
            Some(CONNECT_RETRY_STEP_MS - 1)
        );
        // At or above a step: the fixed step.
        assert_eq!(
            retry_wait_ms(Duration::from_millis(u64::from(CONNECT_RETRY_STEP_MS))),
            Some(CONNECT_RETRY_STEP_MS)
        );
        assert_eq!(retry_wait_ms(CONNECT_BUDGET), Some(CONNECT_RETRY_STEP_MS));
        // Nothing overflows the millisecond cast on the way in.
        assert_eq!(
            retry_wait_ms(Duration::from_secs(u64::from(u32::MAX))),
            Some(CONNECT_RETRY_STEP_MS)
        );
    }

    #[test]
    fn the_retry_budget_is_shorter_than_the_brokers_own_wait_for_a_client() {
        // The broker gives a client 30s (`broker::CONNECT_TIMEOUT_MS`). If this
        // side waited longer, a stalled startup would end with the broker gone
        // and this process still connecting to nothing.
        assert!(CONNECT_BUDGET < Duration::from_secs(30));
    }

    #[test]
    fn the_executable_path_comes_back_nul_terminated_and_non_empty() {
        let path = current_executable_path().expect("own path");
        assert_eq!(path.last(), Some(&0), "Win32 needs the terminator kept");
        assert!(path.len() > 1);
        let text = String::from_utf16_lossy(&path[..path.len() - 1]);
        assert!(
            text.to_ascii_lowercase().ends_with(".exe"),
            "unexpected image path: {text}"
        );
    }
}
