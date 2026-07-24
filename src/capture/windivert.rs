//! Native Windows capture backend via WinDivert, in SNIFF mode.
//!
//! `SNIFF` yields a *copy* of each packet while the originals continue intact;
//! `RECV_ONLY` forbids reinjection. Capture is therefore strictly passive — the
//! game's traffic is never altered.

use std::fs;
use std::path::{Path, PathBuf};

use tracing::warn;
use windivert::prelude::*;

use super::{PacketSource, Segment, parse_segment};
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

/// Largest packet WinDivert can deliver (`WINDIVERT_MTU_MAX`). Coalesced
/// receives (RSC/LSO) routinely exceed the wire MTU, so anything smaller as a
/// buffer makes `recv` fail on the first bulk transfer.
const MAX_PACKET_BYTES: usize = 65_575;

pub struct WinDivertSource {
    handle: WinDivert<NetworkLayer>,
    buffer: Vec<u8>,
    game_port: u16,
}

impl WinDivertSource {
    /// Opens a read-only network handle for `filter`. Requires administrator
    /// rights (driver load).
    pub fn open(filter: &str, game_port: u16, buffer_size: usize) -> Result<Self> {
        ensure_runtime_present()?;
        // The DLL loads WinDivert64.sys from the runtime dir during the call below.
        // Re-verify the driver bytes here, mirroring preload_dll's DLL guard, so a
        // .sys swapped in after extraction is not loaded into this elevated process.
        refuse_if_foreign(
            &runtime_dir()?.join(DRIVER_FILE),
            DRIVER_SYS,
            "WinDivert64.sys",
        )?;

        let flags = WinDivertFlags::new().set_sniff().set_recv_only();
        let handle = WinDivert::network(filter, 0, flags)
            .map_err(|err| Error::Capture(format!("WinDivert open: {err}")))?;
        Ok(Self {
            handle,
            // Floor at the driver's own maximum: a smaller buffer turns the
            // first oversized packet into a recv error.
            buffer: vec![0u8; buffer_size.max(MAX_PACKET_BYTES)],
            game_port,
        })
    }
}

impl PacketSource for WinDivertSource {
    fn next_segment(&mut self) -> Result<Segment> {
        loop {
            let packet = match self.handle.recv(&mut self.buffer) {
                Ok(packet) => packet,
                // The driver already dropped this copy: skipping one packet
                // leaves a reassembly gap, while propagating would kill the
                // capture for the rest of the session.
                Err(WinDivertError::Recv(WinDivertRecvError::InsufficientBuffer)) => {
                    warn!("packet larger than the capture buffer — skipped");
                    continue;
                }
                Err(err) => return Err(Error::Capture(format!("recv: {err}"))),
            };

            if let Some(segment) = parse_segment(&packet.data[..], self.game_port) {
                return Ok(segment);
            }
        }
    }
}

/// Materializes the embedded runtime into a private app-data directory and
/// makes it loadable, so a single shipped exe is a complete install and nothing
/// lands beside the exe (the Desktop stays clean). Steps, in order:
///  1. extract `WinDivert.dll` and `WinDivert64.sys` into the runtime dir;
///  2. `LoadLibrary` the DLL by full path, so the *delay-loaded* import binds to
///     this copy — the runtime dir is not on the default search path, and the
///     import must resolve before the first WinDivert call in `open`.
///
/// WinDivert then loads the driver from the DLL's own directory (the runtime
/// dir), where the `.sys` was just written. The `.sys` cannot be avoided:
/// Windows loads a kernel driver only from a file on disk, never from memory.
fn ensure_runtime_present() -> Result<()> {
    let dir = runtime_dir()?;
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
#[cfg(all(windows, feature = "windivert-backend"))]
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

/// Private per-user directory holding the extracted runtime binaries:
/// `%LOCALAPPDATA%\arkyve-refresh-shop`. Local (not roaming) app-data is the
/// right home for machine-specific binaries. Falls back to the exe's own
/// directory if `LOCALAPPDATA` is somehow unset.
fn runtime_dir() -> Result<PathBuf> {
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        return Ok(PathBuf::from(local).join(crate::APP_DIR));
    }
    let exe =
        std::env::current_exe().map_err(|err| Error::Capture(format!("executable path: {err}")))?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Error::Capture("executable directory not found".to_owned()))
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
