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
    ensure_file_present(&dir, DLL_FILE, RUNTIME_DLL)?;
    ensure_file_present(&dir, DRIVER_FILE, DRIVER_SYS)?;
    // Best-effort: the LGPL text traveling with the DLL is a distribution
    // obligation, not a runtime dependency, so a failure to write it must not
    // block capture.
    let _ = ensure_file_present(&dir, LICENSE_FILE, LICENSE_TEXT);
    preload_dll(&dir.join(DLL_FILE))?;
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

/// Loads `WinDivert.dll` by absolute path so the delay-load thunk finds it
/// already mapped (matched by base name) instead of searching for it — the
/// runtime dir is not on the DLL search path. The handle is intentionally
/// leaked: the DLL must stay resident for the whole session.
fn preload_dll(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
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
        // Replacement failed but the file is already present: most likely locked
        // because loaded by a running instance (the DLL mapped, or the driver
        // service up). Either way the loaded copy will be reused — continue
        // rather than abort startup.
        Err(err) if target.exists() => {
            warn!(error = %err, path = %target.display(),
                "runtime file present but not replaceable (already loaded?) — reusing it");
            Ok(())
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
