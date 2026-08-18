//! Native Windows capture backend via WinDivert, in SNIFF mode.
//!
//! `SNIFF` yields a *copy* of each packet while the originals continue intact;
//! `RECV_ONLY` forbids reinjection. Capture is therefore strictly passive — the
//! game's traffic is never altered.

use std::fs;
use std::path::{Path, PathBuf};

use tracing::{info, warn};
use windivert::prelude::*;

use super::{CaptureStop, MAX_PACKET_BYTES};
use crate::error::{Error, Result};

/// User-mode WinDivert library, embedded in the executable and extracted into
/// the runtime dir at startup. It is *delay-loaded* (see `build.rs`): the import
/// is resolved on the first WinDivert call — after `ensure_runtime_present`
/// below has written and `LoadLibrary`'d this copy — not at process load, when
/// nothing has run yet. So the exe carries its own DLL and ships as a lone file.
const RUNTIME_DLL: &[u8] = include_bytes!("../../vendor/windivert/WinDivert.dll");
const DLL_FILE: &str = "WinDivert.dll";

/// Signed kernel driver, embedded in the executable and extracted at runtime.
///
/// WinDivert.dll loads the driver from its own module directory
/// (`GetModuleFileName`) — the runtime dir, where the DLL also sits — so the exe
/// drops the `.sys` there alongside it. Together with the embedded DLL above,
/// distribution is a single exe: both runtime files ride inside it and are
/// self-extracted on first run.
///
/// This `.sys` (WinDivert 2.2.2) must stay aligned with `WinDivert.dll` and the
/// user-mode bindings in `windivert-sys` (both under `vendor/windivert/`).
/// WinDivert only requires a matching *major* version (>= 2), so a minor drift
/// is tolerated, but a major bump would force replacing all three together.
const DRIVER_SYS: &[u8] = include_bytes!("../../vendor/windivert/WinDivert64.sys");
const DRIVER_FILE: &str = "WinDivert64.sys";

/// WinDivert.dll is LGPL, so its license must accompany the library wherever the
/// library goes. Since we embed the DLL and re-materialize it on the end user's
/// machine, we extract the license text beside it — the redistributed exe would
/// otherwise carry the library with no license at all.
const LICENSE_TEXT: &[u8] = include_bytes!("../../vendor/windivert/LICENSE");
const LICENSE_FILE: &str = "WinDivert-LICENSE.txt";

/// Where delivered packets go to die, on this side of the pipe.
///
/// The abandoned NIC-level capture attempt was diagnosed in minutes because
/// every discard path was counted; this is the same instrument, sized to
/// WinDivert's much shorter path. Only one thing can drop a packet here — the
/// driver refusing an oversized one — because this struct now runs exclusively
/// inside the elevated broker, which parses nothing. The counters that describe
/// *parsing* moved to `PipeSource`, the other end of the pipe, together with the
/// code that produces them.
///
/// Neither number is logged from here: the broker installs no tracing
/// subscriber and the shipped build has no console, so a `debug!` on this side
/// would go nowhere. [`WinDivertSource::raw_counters`] is how they travel — down
/// the pipe, as diagnostic frames, into the UI's log file.
#[derive(Default)]
struct Funnel {
    delivered: u64,
    oversized: u64,
}

pub struct WinDivertSource {
    handle: WinDivert<NetworkLayer>,
    buffer: Vec<u8>,
    funnel: Funnel,
}

/// Remote wake for a [`WinDivertSource`] parked in `recv`.
///
/// `WinDivertShutdown` is the library's own teardown primitive: it stops the
/// driver queueing new packets onto the handle and releases the blocked
/// `recv`, which then drains whatever is already queued and finally reports
/// `NoData`. It never closes the handle — the capture thread still owns it —
/// so there is no race between this and an in-flight receive. Idempotent by
/// construction: a second shutdown on an already-shut handle is a no-op, and
/// once the source has been dropped the `Weak` fails to upgrade and this is a
/// silent success.
///
/// `pub(crate)` only so that it can appear in [`WinDivertSource::open_raw`]'s
/// signature without tripping the private-interface lint; it stays unnameable
/// outside this module (`mod windivert` is private), so callers hold it by
/// inference or behind a `CaptureStop` bound.
pub(crate) struct WinDivertStop {
    shutdown: ShutdownHandle,
}

impl CaptureStop for WinDivertStop {
    fn stop(&mut self) -> Result<()> {
        self.shutdown
            .shutdown()
            .map_err(|err| Error::Capture(format!("WinDivert shutdown: {err}")))
    }
}

impl WinDivertSource {
    /// Opens a strictly passive network handle for `filter`. Requires
    /// administrator rights (driver load), and therefore only ever runs inside
    /// the elevated broker.
    ///
    /// There is deliberately no `open` returning a boxed [`super::CaptureSource`]
    /// beside this one any more. Such a constructor existed while the app opened
    /// the driver in its own process; it erased the source into a
    /// `Box<dyn PacketSource>`, whose only receive path is `next_segment` — the
    /// *parsing* entry point. The broker needs the exact opposite: the concrete
    /// source, so it can call [`WinDivertSource::recv_packet`] and forward raw
    /// bytes without ever interpreting one, plus the stop handle as a separate
    /// value it can move onto its writer thread (that thread is what wakes a
    /// receive parked in the driver when the unelevated end closes the pipe).
    /// The app now gets its `CaptureSource` from the pipe instead, so this is
    /// the only way in.
    ///
    /// The returned stop type is intentionally unnameable outside this module;
    /// callers bind it by inference or behind a `CaptureStop` bound.
    pub(crate) fn open_raw(
        filter: &str,
        game_port: u16,
        buffer_size: usize,
    ) -> Result<(Self, WinDivertStop)> {
        ensure_runtime_present()?;
        // The DLL loads WinDivert64.sys from the runtime dir during the call below.
        // Re-verify the driver bytes here, mirroring preload_dll's DLL guard, so a
        // .sys swapped in after extraction is not loaded into this elevated process.
        refuse_if_foreign(
            &runtime_dir()?.join(DRIVER_FILE),
            DRIVER_SYS,
            "WinDivert64.sys",
        )?;

        // The two flags that make this a tap rather than a man-in-the-middle,
        // and the least invasive mode WinDivert offers:
        //
        // - `SNIFF` (`WINDIVERT_FLAG_SNIFF`): the driver hands us a *copy* of
        //   each matching packet and lets the original continue to its
        //   destination untouched. Without it, WinDivert *diverts*: the packet
        //   is removed from the stack and the game's connection stalls until
        //   we reinject it.
        // - `RECV_ONLY` (`WINDIVERT_FLAG_RECV_ONLY`): the handle is incapable
        //   of sending. `WinDivertSend` on it fails, so no code path — present
        //   or added later — can inject or modify a packet.
        //
        // Together they are what backs the README's promise that the game's
        // traffic is never altered: capture is read-only, out-of-band, and
        // invisible to both endpoints. Do not add `set_drop`, remove
        // `set_sniff`, or drop `set_recv_only` without re-reading that promise.
        let flags = WinDivertFlags::new().set_sniff().set_recv_only();
        // Priority 0: with SNIFF there is no packet ordering to contend for,
        // so no reason to outrank any other WinDivert consumer on the machine.
        let handle = WinDivert::network(filter, 0, flags)
            .map_err(|err| Error::Capture(format!("WinDivert open: {err}")))?;
        let stop = WinDivertStop {
            shutdown: handle.shutdown_handle(),
        };
        // Floor at the driver's own maximum: a smaller buffer turns the first
        // oversized packet into a recv error.
        let buffer_bytes = buffer_size.max(MAX_PACKET_BYTES);
        // The *effective* filter, after the config-level override and the
        // buffer floor have been applied — the single fact that explains an
        // empty capture, and the one no other log line carries.
        info!(
            filter,
            game_port,
            buffer_bytes,
            mode = "sniff+recv_only",
            "WinDivert capture open (passive copy; originals untouched)"
        );
        Ok((
            Self {
                handle,
                buffer: vec![0u8; buffer_bytes],
                funnel: Funnel::default(),
            },
            stop,
        ))
    }

    /// The raw half of the funnel: `(delivered, oversized)`.
    ///
    /// Exists for the elevated broker, which has no tracing subscriber and no
    /// console — any `debug!` on this side is inert, so these two numbers can
    /// only reach the log file by riding a diagnostic frame down the pipe.
    /// `oversized` in particular is otherwise unobservable from outside:
    /// [`WinDivertSource::recv_packet`] swallows the skip.
    pub(crate) fn raw_counters(&self) -> (u64, u64) {
        (self.funnel.delivered, self.funnel.oversized)
    }

    /// Blocks until the driver delivers the next matching packet, then hands
    /// back its raw IP bytes. Parses nothing, and must never start to.
    ///
    /// This is a privilege boundary, not an internal convenience, and that is
    /// the only reason it is separate from [`PacketSource::next_segment`]. The
    /// elevated capture broker owns the WinDivert handle — opening it is the
    /// one step on this path that genuinely requires administrator rights — but
    /// it must not *interpret* a single byte the handle gives it: `parse_segment`
    /// is the code that chews on unauthenticated, attacker-shaped input off the
    /// wire, and it belongs on the unelevated side of the pipe, at the far end
    /// of these bytes. Until this method existed the only way to receive from a
    /// `WinDivertSource` was `next_segment`, which parses as an inseparable part
    /// of receiving; a broker built on that would have dragged the parser back
    /// up into the administrator process and silently undone the whole point of
    /// the split. So: raw receive here, parsing layered strictly on top of it,
    /// and nothing in between.
    ///
    /// The returned slice borrows the receive buffer and is only valid until the
    /// next receive. That is the honest shape of these bytes, and the reason
    /// this hands back a borrow rather than a length for the caller to re-slice
    /// with: the buffer is never cleared between packets, so a stale or
    /// hand-computed length would quietly read the tail of an older, longer
    /// packet instead of failing.
    ///
    /// Returns only when a packet arrives, when [`WinDivertStop`] releases the
    /// blocked receive, or on a handle error. Declared `pub` rather than
    /// `pub(crate)` for the same reason as the frame helpers in
    /// [`super`]: this crate is a lib plus a bin, so only an item reachable from
    /// the crate root escapes `dead_code`, and every lane builds with
    /// `-D warnings` while the broker — its sole external caller — does not
    /// exist yet.
    pub fn recv_packet(&mut self) -> Result<&[u8]> {
        // Split across two functions so the retry loop never holds a borrow of
        // the buffer across an iteration: returning `&self.buffer[..]` from
        // inside the loop would stretch that borrow over the whole function
        // under NLL and collide with the next iteration's `recv`. A plain
        // `usize` crosses that boundary instead, and the slicing happens once,
        // here. `recv` fills the buffer from offset zero and reports how many
        // bytes it wrote, so this is exactly the packet it just delivered.
        let len = self.recv_packet_len()?;
        Ok(&self.buffer[..len])
    }

    /// The receive loop behind [`WinDivertSource::recv_packet`]: returns the
    /// length of the packet now sitting at the front of `self.buffer`.
    fn recv_packet_len(&mut self) -> Result<usize> {
        loop {
            match self.handle.recv(&mut self.buffer) {
                Ok(packet) => {
                    let len = packet.data.len();
                    self.funnel.delivered += 1;
                    return Ok(len);
                }
                // The driver already dropped this copy: skipping one packet
                // leaves a reassembly gap, while propagating would kill the
                // capture for the rest of the session.
                Err(WinDivertError::Recv(WinDivertRecvError::InsufficientBuffer)) => {
                    self.funnel.delivered += 1;
                    self.funnel.oversized += 1;
                    warn!("packet larger than the capture buffer — skipped");
                }
                Err(err) => return Err(Error::Capture(format!("recv: {err}"))),
            }
        }
    }
}

// No `impl PacketSource for WinDivertSource`, and that absence is deliberate.
// The parsing wrapper that used to live here — `parse_segment` plus the
// `admitted` / `unparsed` / `server_to_client` counters and the
// "first server-to-client segment admitted" line — moved wholesale to
// `PipeSource`, on the far side of the pipe. It is the single largest piece of
// work the elevated process no longer does: `parse_segment` chews on
// unauthenticated bytes off the wire, and it now does so in a medium-integrity
// process. Anything added back here would quietly undo that.

/// Materializes the embedded runtime into a private app-data *subdirectory* and
/// makes it loadable, so a single shipped exe is a complete install and nothing
/// lands beside the exe (the Desktop stays clean). Steps, in order:
///  1. migrate a pre-`runtime\` install off the app-data root (best-effort);
///  2. extract `WinDivert.dll` and `WinDivert64.sys` into the runtime dir;
///  3. `LoadLibrary` the DLL by full path, so the *delay-loaded* import binds to
///     this copy — the runtime dir is not on the default search path, and the
///     import must resolve before the first WinDivert call in `open`.
///
/// WinDivert then loads the driver from the DLL's own directory (the runtime
/// dir), where the `.sys` was just written. The `.sys` cannot be avoided:
/// Windows loads a kernel driver only from a file on disk, never from memory.
///
/// The runtime lives one level below `%LOCALAPPDATA%\arkyve-refresh-shop`
/// precisely so that [`harden_runtime_dir`]'s admins-only DACL — which
/// `SetNamedSecurityInfoW` propagates down into every child holding an
/// auto-inherited DACL — cannot reach `logs\` or `crash.log`, which sit at the
/// root and must stay writable by an ordinary, non-elevated process.
fn ensure_runtime_present() -> Result<()> {
    let dir = runtime_dir()?;
    // Before creating anything: an install from a build that extracted into the
    // app-data *root* left that root locked to admins/SYSTEM, and `logs\` /
    // `crash.log` inherit from it. Undo that first so the new subdirectory is
    // not created underneath a DACL that is about to be reset anyway.
    migrate_legacy_runtime_root();
    fs::create_dir_all(&dir)
        .map_err(|err| Error::Capture(format!("runtime dir {}: {err}", dir.display())))?;
    // Best-effort defense-in-depth: restrict the directory to admins/SYSTEM so a
    // non-elevated same-user process can't win the extract-then-load race by
    // planting a file here first. A failure only logs — the byte re-verifies
    // above and in `preload_dll`/`refuse_if_foreign` remain the real guard.
    if let Err(err) = harden_runtime_dir(&dir) {
        warn!(dir = %dir.display(), error = %err,
            "could not restrict runtime dir permissions — relying on byte re-verify");
    }
    ensure_file_present(&dir, DLL_FILE, RUNTIME_DLL)?;
    ensure_file_present(&dir, DRIVER_FILE, DRIVER_SYS)?;
    // Best-effort: the LGPL text traveling with the DLL is a distribution
    // obligation, not a runtime dependency, so a failure to write it must not
    // block capture.
    let _ = ensure_file_present(&dir, LICENSE_FILE, LICENSE_TEXT);
    preload_dll(&dir.join(DLL_FILE), RUNTIME_DLL)?;
    Ok(())
}

/// Restricts `dir`'s DACL to Administrators and SYSTEM only, dropping inherited
/// ACEs, so a non-elevated process running as the same user can no longer write
/// into the runtime directory this elevated process loads from. This process
/// itself runs elevated (an Administrators member), so its own access is
/// unaffected; it is defense-in-depth on top of `refuse_if_foreign`'s byte
/// re-verify, not a replacement for it — callers must treat failure as
/// non-fatal.
///
/// `dir` must be the runtime *subdirectory*, never the app-data root: the ACEs
/// below are inheritable and `SetNamedSecurityInfoW` pushes them into every
/// child with an auto-inherited DACL. See [`runtime_dir`].
fn harden_runtime_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        BuildTrusteeWithSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, SE_FILE_OBJECT, SetEntriesInAclW,
        SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    };
    use windows_sys::Win32::Security::{
        ACL, CreateWellKnownSid, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSID, SECURITY_MAX_SID_SIZE, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `system_sid`/`admins_sid` are stack buffers sized to
    // `SECURITY_MAX_SID_SIZE`, the documented upper bound for any SID,
    // written by `CreateWellKnownSid` and checked for success before use.
    // `BuildTrusteeWithSidW` stores a pointer into those still-live buffers
    // inside each `EXPLICIT_ACCESS_W::Trustee`, and both buffers outlive the
    // `SetEntriesInAclW` call that reads them. `SetEntriesInAclW` allocates
    // `new_dacl` via `LocalAlloc` on success (checked via its `WIN32_ERROR`
    // return); it is always freed with `LocalFree` after
    // `SetNamedSecurityInfoW` has consumed it, on both the success and error
    // paths. `wide` is a valid null-terminated UTF-16 path kept alive for the
    // whole call.
    unsafe {
        let mut system_sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut system_sid_len = system_sid.len() as u32;
        if CreateWellKnownSid(
            WinLocalSystemSid,
            ptr::null_mut(),
            system_sid.as_mut_ptr() as PSID,
            &mut system_sid_len,
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }

        let mut admins_sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut admins_sid_len = admins_sid.len() as u32;
        if CreateWellKnownSid(
            WinBuiltinAdministratorsSid,
            ptr::null_mut(),
            admins_sid.as_mut_ptr() as PSID,
            &mut admins_sid_len,
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }

        let mut entries = [EXPLICIT_ACCESS_W::default(); 2];
        for entry in &mut entries {
            entry.grfAccessPermissions = FILE_ALL_ACCESS;
            entry.grfAccessMode = GRANT_ACCESS;
            entry.grfInheritance = SUB_CONTAINERS_AND_OBJECTS_INHERIT;
            entry.Trustee.TrusteeForm = TRUSTEE_IS_SID;
            entry.Trustee.TrusteeType = TRUSTEE_IS_UNKNOWN;
        }
        BuildTrusteeWithSidW(&mut entries[0].Trustee, system_sid.as_mut_ptr() as PSID);
        BuildTrusteeWithSidW(&mut entries[1].Trustee, admins_sid.as_mut_ptr() as PSID);

        let mut new_dacl: *mut ACL = ptr::null_mut();
        let status = SetEntriesInAclW(2, entries.as_ptr(), ptr::null(), &mut new_dacl);
        if status != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(status as i32));
        }

        let result = SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            new_dacl,
            ptr::null(),
        );
        LocalFree(new_dacl as _);

        if result != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(result as i32));
        }
    }

    Ok(())
}

/// Leaf component isolating the extracted runtime from everything else the app
/// keeps under `%LOCALAPPDATA%\arkyve-refresh-shop` (`logs\`, `crash.log`).
const RUNTIME_SUBDIR: &str = "runtime";

/// Private per-user directory holding the extracted runtime binaries:
/// `%LOCALAPPDATA%\arkyve-refresh-shop\runtime`. Local (not roaming) app-data is
/// the right home for machine-specific binaries.
///
/// The `runtime` component is not cosmetic. [`harden_runtime_dir`] puts a
/// protected admins/SYSTEM-only DACL on whatever this returns, and
/// `SetNamedSecurityInfoW` propagates that downward into every child whose DACL
/// is auto-inherited. Pointed at the app-data root it would therefore also lock
/// `logs\` and `crash.log`, which a non-elevated process has to be able to
/// write; the app would then lose its log file *silently* (`install_logging`
/// falls back to an inert sink). One dedicated leaf keeps the blast radius to
/// the two files that genuinely need it.
///
/// Falls back to a `runtime` subdirectory of the exe's own directory if
/// `LOCALAPPDATA` is somehow unset — a subdirectory there too, and for the same
/// reason: the hardening would otherwise land on whatever folder the exe was
/// double-clicked from (a Desktop, a Downloads folder), making the user's own
/// directory admins-only.
fn runtime_dir() -> Result<PathBuf> {
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        return Ok(PathBuf::from(local)
            .join(crate::APP_DIR)
            .join(RUNTIME_SUBDIR));
    }
    let exe =
        std::env::current_exe().map_err(|err| Error::Capture(format!("executable path: {err}")))?;
    exe.parent()
        .map(|dir| dir.join(RUNTIME_SUBDIR))
        .ok_or_else(|| Error::Capture("executable directory not found".to_owned()))
}

/// The app-data root a pre-`runtime\` build extracted into, and hardened.
///
/// `None` when `LOCALAPPDATA` is unset. The exe-directory fallback of
/// [`runtime_dir`] is deliberately *not* migrated: undoing a DACL and deleting
/// files in a folder the user chose (a Desktop, a shared network share) is a far
/// more surprising action than leaving an exotic, effectively-unreachable
/// configuration alone.
fn legacy_runtime_root() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|local| PathBuf::from(local).join(crate::APP_DIR))
}

/// Files a pre-`runtime\` build dropped straight into the app-data root.
const LEGACY_RUNTIME_FILES: [&str; 3] = [DLL_FILE, DRIVER_FILE, LICENSE_FILE];

/// Undoes a pre-`runtime\` install: resets the app-data root to inherited
/// permissions and deletes the runtime files stranded there.
///
/// Only the elevated side can do this. The old build ran wholly elevated, so the
/// objects it created are owned by `Administrators`, not by the user — a
/// medium-integrity process has neither `WRITE_DAC` nor delete rights on them.
///
/// Entirely best-effort: every failure logs and capture proceeds. The one thing
/// worth noticing is the ordering — this runs long after `install_logging()` in
/// the same launch, so the *first* post-upgrade run still writes its log into a
/// directory it cannot open and loses it. The `info!` below is emitted so the
/// second run's log file explains where the first one went.
fn migrate_legacy_runtime_root() {
    let Some(root) = legacy_runtime_root() else {
        return;
    };
    if !root.is_dir() {
        return;
    }

    let mut reset_dacl = false;
    match dacl_is_protected(&root) {
        // Not our doing, but nothing else ever protects this directory, and the
        // only way out is to undo it: a protected DACL here is exactly the
        // pre-`runtime\` layout's footprint.
        Ok(true) => match reset_dacl_to_inherited(&root) {
            Ok(()) => reset_dacl = true,
            Err(err) => warn!(dir = %root.display(), error = %err,
                "could not restore inherited permissions on the app-data root — \
                 logs and crash.log may stay unwritable without administrator rights"),
        },
        Ok(false) => {}
        Err(err) => warn!(dir = %root.display(), error = %err,
            "could not read the app-data root permissions — skipping the runtime migration"),
    }

    let mut removed: Vec<&str> = Vec::new();
    for name in LEGACY_RUNTIME_FILES {
        let stale = root.join(name);
        match fs::remove_file(&stale) {
            Ok(()) => removed.push(name),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => warn!(path = %stale.display(), error = %err,
                "could not delete a stale runtime file left at the app-data root"),
        }
    }

    if reset_dacl || !removed.is_empty() {
        info!(
            root = %root.display(),
            reset_dacl,
            removed = %removed.join(", "),
            "migrated the extracted runtime into its own subdirectory — a previous \
             version had locked the app-data root to administrators, so this run's \
             log file (written before the migration) is likely missing"
        );
    }
}

/// True when `dir`'s DACL carries `SE_DACL_PROTECTED`, i.e. inheritance from its
/// parent is switched off. That flag is the signature of a pre-`runtime\`
/// install: nothing else in this app sets it, and it is what keeps a
/// non-elevated process out of `logs\` and `crash.log`.
fn dacl_is_protected(dir: &Path) -> std::io::Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, PSECURITY_DESCRIPTOR,
        SE_DACL_PROTECTED,
    };

    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `wide` is a valid null-terminated UTF-16 path, owned by this frame
    // and alive across the whole call. The four `null_mut()` out-parameters are
    // documented as optional (we want the descriptor, not the owner/group/ACL
    // pointers into it). `descriptor` is written by `GetNamedSecurityInfoW` only
    // when it returns `ERROR_SUCCESS`, which is checked before any read; on
    // success it points at a single `LocalAlloc` block that owns the ACL as
    // well, freed exactly once with `LocalFree` on both the success and the
    // error path below, and never touched afterwards. `control`/`revision` are
    // stack slots that outlive `GetSecurityDescriptorControl`. Failure mode: a
    // missing or inaccessible directory returns a `WIN32_ERROR` and nothing is
    // allocated or freed.
    unsafe {
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let status = GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        );
        if status != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(status as i32));
        }

        let mut control: u16 = 0;
        let mut revision: u32 = 0;
        let ok = GetSecurityDescriptorControl(descriptor, &mut control, &mut revision);
        // GetLastError is per-thread and the very next Win32 call clobbers it:
        // read it before `LocalFree`, not after.
        let err = std::io::Error::last_os_error();
        LocalFree(descriptor.cast());
        if ok == 0 {
            return Err(err);
        }
        Ok(control & SE_DACL_PROTECTED != 0)
    }
}

/// Puts `dir` back on inherited permissions: no explicit ACEs of its own, and
/// auto-inheritance from the parent switched back on. Children whose DACL is
/// auto-inherited (`logs\`, `crash.log`) are recomputed by the same call.
///
/// The ACL passed in is deliberately **empty but not null**, and that
/// distinction is the whole point of this function. `SetSecurityInfo`'s
/// documentation is explicit: `DACL_SECURITY_INFORMATION` with a `NULL` `pDacl`
/// does not mean "no DACL to set", it installs a *null DACL*, which grants FULL
/// ACCESS TO EVERYONE. Doing that here would make the parent of the directory an
/// elevated process loads `WinDivert64.sys` from world-writable — strictly worse
/// than the over-broad DACL we are undoing. A zero-ACE ACL plus
/// `UNPROTECTED_DACL_SECURITY_INFORMATION` is the actual "reset to inherited"
/// spelling: nothing granted explicitly, everything granted by inheritance.
/// Do not "simplify" the ACL away.
fn reset_dacl_to_inherited(dir: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
    use windows_sys::Win32::Security::{
        ACL, ACL_REVISION, DACL_SECURITY_INFORMATION, InitializeAcl,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    };

    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // An `ACL` header is 8 bytes and needs 4-byte alignment; a `u32` array gives
    // that alignment for free, and the extra room costs nothing on the stack.
    let mut acl_buf = [0u32; 16];

    // SAFETY: `acl_buf` is a `u32`-aligned stack buffer, larger than the `ACL`
    // header `InitializeAcl` writes into it, and its length is passed as the
    // exact byte size of that same buffer — so `InitializeAcl` cannot write out
    // of bounds. The buffer is only read as an `ACL` after that call has
    // reported success, and it outlives `SetNamedSecurityInfoW`, which does not
    // retain the pointer. `wide` is a valid null-terminated UTF-16 path alive
    // for the whole call. The owner, group and SACL pointers are null, which is
    // "do not change" for the information bits we did not request. Failure mode:
    // a `WIN32_ERROR` return (typically `ERROR_ACCESS_DENIED` when not
    // elevated), which the caller treats as non-fatal.
    unsafe {
        if InitializeAcl(
            acl_buf.as_mut_ptr().cast::<ACL>(),
            std::mem::size_of_val(&acl_buf) as u32,
            ACL_REVISION,
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }

        let result = SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            acl_buf.as_ptr().cast::<ACL>(),
            ptr::null(),
        );
        if result != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(result as i32));
        }
    }

    Ok(())
}

/// Refuses a runtime file that is readable but is NOT our embedded copy — the
/// just-before-load guard that shrinks the check-to-load TOCTOU window. A
/// momentarily-unreadable file (e.g. an antivirus lock) falls through: the
/// loader maps it via an image section and tolerates that, so a genuine file
/// must not be failed on a transient read error.
fn refuse_if_foreign(path: &Path, expected: &[u8], label: &str) -> Result<()> {
    if matches!(std::fs::read(path), Ok(bytes) if bytes != expected) {
        return Err(Error::Capture(format!(
            "{} does not match the embedded {label} — refusing to load it",
            path.display()
        )));
    }
    Ok(())
}

/// Loads `WinDivert.dll` by absolute path so the delay-load thunk finds it
/// already mapped (matched by base name) instead of searching for it — the
/// runtime dir is not on the DLL search path. The handle is intentionally
/// leaked: the DLL must stay resident for the whole session.
fn preload_dll(path: &Path, expected: &[u8]) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;

    // Verify the on-disk bytes are our embedded copy right before loading, so a
    // file swapped in after the earlier extraction check is not loaded. This
    // shrinks (does not fully close) the check-to-load TOCTOU window.
    refuse_if_foreign(path, expected, "WinDivert.dll")?;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is a valid null-terminated UTF-16 path for the call's
    // duration; LoadLibraryW only reads it and returns a handle (null on error).
    let handle = unsafe { LoadLibraryW(wide.as_ptr()) };
    if handle.is_null() {
        return Err(Error::Capture(format!(
            "loading {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Writes `bytes` as `name` in `dir` when the file is missing or differs.
/// Idempotent and safe alongside another running instance (which may hold the
/// file locked because it is loaded).
fn ensure_file_present(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let target = dir.join(name);

    // Compare *content*, not just length, so a corrupt or differently-versioned
    // file of the same size is replaced. Identical content is left untouched,
    // which also avoids writing a file locked by an already-running instance.
    if file_has_content(&target, bytes) {
        return Ok(());
    }

    // Atomic write (temp file then rename) so no one reads a half-written file
    // and concurrent first launches stay safe.
    match atomic_replace(dir, name, &target, bytes) {
        Ok(()) => Ok(()),
        // Replacement failed but the file is already present. Reuse ONLY our own
        // identical copy (a second running instance holds the real DLL locked so
        // the rename fails). A file that exists but does NOT match the embedded
        // bytes is foreign — refuse it rather than load attacker-controlled code
        // into this elevated process.
        Err(err) if target.exists() => {
            if file_has_content(&target, bytes) {
                warn!(error = %err, path = %target.display(),
                    "runtime file locked but identical to the embedded copy — reusing it");
                Ok(())
            } else {
                Err(Error::Capture(format!(
                    "{} is locked with content that does not match this build ({err}) — \
                     close any other running instance of the app and retry (an in-place \
                     upgrade cannot replace a locked runtime file); refusing to load a \
                     runtime file that does not match the embedded copy",
                    target.display()
                )))
            }
        }
        Err(err) => Err(Error::Capture(format!(
            "extracting {}: {err} — the app-data directory must be writable",
            target.display()
        ))),
    }
}

/// True if `path` exists and holds exactly `expected`.
fn file_has_content(path: &Path, expected: &[u8]) -> bool {
    fs::read(path).is_ok_and(|content| content == expected)
}

/// Writes `bytes` to a temp file in the same directory, then renames it onto
/// `target` (atomic replace via `MoveFileEx` on Windows).
fn atomic_replace(dir: &Path, name: &str, target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Per-process temp name so two simultaneous first launches don't clash.
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&tmp, bytes)?;
    if let Err(err) = fs::rename(&tmp, target) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("arkyve_{tag}_{}.bin", std::process::id()))
    }

    #[test]
    fn the_runtime_dir_is_a_leaf_below_the_app_data_root_never_the_root_itself() {
        let dir = runtime_dir().unwrap();
        assert_eq!(
            dir.file_name().and_then(|name| name.to_str()),
            Some(RUNTIME_SUBDIR)
        );
        // The parent is what holds `logs\` and `crash.log`; hardening must never
        // be pointed at it, so the two paths must not be the same directory.
        let parent = dir.parent().expect("runtime dir has a parent");
        assert_ne!(parent, dir.as_path());
        if let Some(root) = legacy_runtime_root() {
            assert_eq!(parent, root.as_path());
            assert_eq!(
                root.file_name().and_then(|name| name.to_str()),
                Some(crate::APP_DIR)
            );
        }
    }

    #[test]
    fn ensure_file_present_creates_the_file_inside_a_freshly_created_subdirectory() {
        // Mirrors the first launch after the move: the `runtime` leaf does not
        // exist yet and everything has to land one level below the app-data root.
        let root = temp_path("runtime_root").with_extension("d");
        let dir = root.join(RUNTIME_SUBDIR);
        std::fs::create_dir_all(&dir).unwrap();

        ensure_file_present(&dir, "embedded.bin", b"embedded").unwrap();
        let written = dir.join("embedded.bin");
        assert!(file_has_content(&written, b"embedded"));
        // Nothing may be dropped at the root beside the leaf directory.
        assert!(!root.join("embedded.bin").exists());

        // Idempotent, and it replaces content that differs.
        ensure_file_present(&dir, "embedded.bin", b"embedded").unwrap();
        ensure_file_present(&dir, "embedded.bin", b"replaced").unwrap();
        assert!(file_has_content(&written, b"replaced"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_has_content_matches_only_exact_bytes() {
        let p = temp_path("fhc");
        std::fs::write(&p, b"embedded").unwrap();
        assert!(file_has_content(&p, b"embedded"));
        assert!(!file_has_content(&p, b"planted!!"));
        assert!(!file_has_content(&p.with_extension("missing"), b"x"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn preload_refuses_a_mismatched_file() {
        // The verify guard rejects before LoadLibraryW is ever reached.
        let p = temp_path("preload");
        std::fs::write(&p, b"not the real dll").unwrap();
        assert!(preload_dll(&p, b"the embedded bytes").is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn refuse_if_foreign_rejects_a_mismatch_and_accepts_a_match() {
        let p = temp_path("refuse");
        std::fs::write(&p, b"embedded").unwrap();
        assert!(refuse_if_foreign(&p, b"embedded", "test").is_ok());
        assert!(refuse_if_foreign(&p, b"planted!!", "test").is_err());
        assert!(refuse_if_foreign(&p.with_extension("missing"), b"x", "test").is_ok());
        let _ = std::fs::remove_file(&p);
    }
}
