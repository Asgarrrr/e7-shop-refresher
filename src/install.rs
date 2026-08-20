//! Fetching the Npcap installer for a player who has none.
//!
//! One pinned file, checked byte for byte, then launched. It cannot install
//! anything itself — Npcap's silent installer is the paid OEM product (the
//! Npcap LICENSE at <https://npcap.com>, LICENSE GRANT: the OEM edition
//! "also includes a silent installer for unattended installation"). Build,
//! URL and hash live in [`crate::npcap`].
//!
//! A pin only binds if the bytes hashed and the bytes executed are the same
//! bytes, and any medium-integrity process of the same user can write `%TEMP%`.
//! So [`Fetcher::fetch_and_check`] returns the [`File`] it hashed, opened
//! through [`open_locked`] with neither `FILE_SHARE_WRITE` nor
//! `FILE_SHARE_DELETE`, and [`Fetcher::run`] holds it across the spawn.
//! `FILE_SHARE_READ` has to stay: the image `CreateProcess` maps is a read.
//!
//! That handle binds the installer's bytes, not its neighbours, and
//! `CreateProcess` puts the image's own directory **first** in the child's DLL
//! search order, behind only `KnownDLLs` — measured: 37 entries, holding none of
//! `version`, `uxtheme`, `dwmapi`, `riched20`, `msimg32` or `winmm`, most of
//! what an NSIS setup stub imports. Measured too, `icacls %TEMP%` grants the
//! interactive user `(I)(OI)(CI)(F)`. A planted `%TEMP%\version.dll` therefore
//! loads into the **elevated** installer without touching the file the pin
//! covers, so the download stages into a per-run directory clamped to
//! [`STAGING_DACL`].
//!
//! Do not try to close that from the parent instead — it cannot be done.
//! `CreateProcess` takes no search-order flag, `SetDefaultDllDirectories`
//! changes only its own process, `CWDIllegalInDllSearch` is machine-wide. The
//! child's *current* directory is the one settable slot; the image-directory
//! slot has no substitute for staging.
//!
//! Npcap's NSIS stub loads its own plugins from its own `%TEMP%\ns*.tmp`, and
//! that *was* out of reach for exactly as long as the child inherited the
//! launcher's `%TEMP%` unchanged. It does not have to: `GetTempPath` reads
//! `TMP` then `TEMP` from the child's own environment, not a fixed location,
//! and [`Fetcher::run`] sets both to [`staging_dir`] before spawning — the
//! same clamped directory the installer itself is staged in, so the stub's
//! plugin extraction lands there instead of in the attacker-writable `%TEMP%`
//! this section opens with.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use tracing::{info, warn};

use crate::npcap::{INSTALLER_BYTES, INSTALLER_SHA256, INSTALLER_URL, TEMP_INSTALLER_NAME};

/// Ceiling handed to `curl`, generous against hotel Wi-Fi: the measured download
/// is under a second.
const FETCH_TIMEOUT_SECS: u32 = 120;

/// Prefix of the staging directory's name, and what [`sweep_old_staging`]
/// recognises. Never [`crate::APP_DIR`]: [`STAGING_DACL`] on
/// `%TEMP%\arkyve-refresh-shop\logs` would take the fallback log away from every
/// unelevated run, the failure [`crate::migrate`] ships to undo.
const STAGING_PREFIX: &str = "arkyve-npcap-staging-";

/// The DACL the staging directory is clamped to. `D:P` is **protected**, which
/// is what stops `%TEMP%`'s inherited ACEs (module header) reaching it; the two
/// ACEs admit `BUILTIN\Administrators` and `LocalSystem` and nobody else, and
/// this process's administrator token (`build.rs` manifests it) is inside that.
const STAGING_DACL: &str = "D:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)";

/// What the banner shows while this runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Progress {
    /// Nothing started, or the last attempt was abandoned.
    #[default]
    Idle,
    Fetching,
    Checking,
    Launched,
    /// The one-click restart did not happen. Distinct from [`Progress::Failed`]
    /// because `Failed`'s remedy — fetch again — takes the reuse fast path and
    /// puts a *second* Npcap setup window on screen. String is player-facing.
    RestartFailed(String),
    /// Gave up before anything was launched. String is player-facing.
    Failed(String),
}

/// Handle the window keeps; cheap to clone, since the worker owns the other end.
#[derive(Clone, Default)]
pub struct Fetcher {
    progress: Arc<Mutex<Progress>>,
}

impl Fetcher {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Poison-tolerant like the rest of this crate's shared state: a panicked
    /// worker must not take the error banner down with it.
    #[must_use]
    pub fn progress(&self) -> Progress {
        crate::sync::lock_ignoring_poison(&self.progress).clone()
    }

    /// The one failure raised by the window rather than the worker. See
    /// [`Progress::RestartFailed`] for why not `Failed`.
    pub fn restart_failed(&self, reason: String) {
        warn!(reason = %reason, "the relaunch failed at the window");
        self.set(Progress::RestartFailed(reason));
    }

    fn set(&self, next: Progress) {
        *crate::sync::lock_ignoring_poison(&self.progress) = next;
    }

    /// Off the UI thread because the window repaints at 4 Hz; refused once
    /// launched, since the reuse fast path would put a second elevated Npcap
    /// setup on screen.
    ///
    /// Do not split the test and the claim back into
    /// `if !accepts_a_start(&self.progress())`: that left `Idle` in the cell
    /// until the worker's own first write, a spawn and a `remove_file` later,
    /// and at 4 Hz the next frame's click took it.
    pub fn start(&self) {
        if !self.claim() {
            return;
        }
        let handle = self.clone();
        // Detached; its whole output, a failure to spawn included, is the cell.
        if let Err(err) = std::thread::Builder::new()
            .name("npcap-fetch".to_owned())
            .spawn(move || handle.run())
        {
            self.set(Progress::Failed(format!(
                "could not start the download: {err}"
            )));
        }
    }

    /// Takes the fetch under one lock acquisition. Separate from `start` for the
    /// same reason [`accepts_a_start`] is: a test of `start` downloads and
    /// elevates for real.
    fn claim(&self) -> bool {
        let mut progress = crate::sync::lock_ignoring_poison(&self.progress);
        if !accepts_a_start(&progress) {
            return false;
        }
        // Written before the lock is dropped; see `start`.
        *progress = Progress::Fetching;
        true
    }

    fn run(self) {
        // Before the download: bytes written into `%TEMP%` and then moved would
        // spend the interval beside anything a same-user process planted.
        if let Err(reason) = staging_ready() {
            warn!(reason = %reason, "the Npcap installer was not obtained");
            self.set(Progress::Failed(reason));
            return;
        }
        let target = installer_path();
        match self.fetch_and_check(&target) {
            Ok(verified) => {
                // See `installer_command`'s doc for why `current_dir`, `TEMP`
                // and `TMP` are set the way they are.
                let spawned = installer_command(&target).spawn();
                // Only once `CreateProcess` has returned: until then this handle
                // is why the path still holds the hashed bytes.
                drop(verified);
                match spawned {
                    Ok(_) => {
                        info!(path = %target.display(), "launched the Npcap installer");
                        self.set(Progress::Launched);
                    }
                    Err(err) => {
                        warn!(error = %err, "the Npcap installer would not launch");
                        self.set(Progress::Failed(format!(
                            "the installer downloaded but would not start: {err}"
                        )));
                    }
                }
            }
            Err(reason) => {
                warn!(reason = %reason, "the Npcap installer was not obtained");
                self.set(Progress::Failed(reason));
            }
        }
    }

    /// Returns the handle the verification was made through. The reused copy
    /// takes the same locked handle as a fresh one, so the fast path is not the
    /// weak path; "already there" means within this run only, since the staging
    /// directory is per process and a re-download measures one second.
    fn fetch_and_check(&self, target: &Path) -> Result<File, String> {
        if let Ok(existing) = open_locked(target) {
            if verify(&existing).is_ok() {
                self.set(Progress::Checking);
                return Ok(existing);
            }
            // Explicitly: this handle denies `FILE_SHARE_DELETE` and the next
            // thing below is a delete of this path.
            drop(existing);
        }

        // Whatever is there is not the installer; a later failure must not leave
        // it to be run by hand, and `create_new` below needs the path free.
        let _ = std::fs::remove_file(target);

        // No `set(Fetching)`: `start` claimed the state before this thread
        // existed, which is what makes the claim a claim.
        let bytes = download()?;

        self.set(Progress::Checking);
        write_new(target, &bytes)?;

        // Not a hash of `bytes`: the claim is about the file the caller will
        // execute, so it must be made through the handle held across the spawn.
        let file = open_locked(target)
            .map_err(|err| format!("the downloaded installer could not be re-opened: {err}"))?;
        match verify(&file) {
            Ok(()) => Ok(file),
            Err(reason) => {
                // Dropped before the delete, as on the fast path.
                drop(file);
                let _ = std::fs::remove_file(target);
                Err(reason)
            }
        }
    }
}

/// A predicate rather than a `matches!` inside [`Fetcher::start`], so the rule
/// can be asserted without spawning a worker that downloads and elevates.
fn accepts_a_start(progress: &Progress) -> bool {
    match progress {
        Progress::Idle | Progress::Failed(_) => true,
        // In flight: a second worker would race the first over one path.
        Progress::Fetching | Progress::Checking => false,
        // Already launched: see `Fetcher::start`.
        Progress::Launched | Progress::RestartFailed(_) => false,
    }
}

/// Fetches the pinned URL into memory.
///
/// `curl.exe` rather than an HTTP client: it ships with Windows 10 1803 and
/// later and uses the machine's own proxy and TLS configuration. To stdout
/// rather than `--output`, so the one process creating the file is this one;
/// `--max-filesize` bounds the response body.
///
/// The path comes from [`crate::system32::directory`], never `%SystemRoot%`:
/// this process is elevated, so that variable would pick *the executable it
/// runs* — one step earlier than the pin can see, since the pin hashes what curl
/// printed.
fn download() -> Result<Vec<u8>, String> {
    let curl = curl_path();
    if !curl.is_file() {
        return Err("this Windows has no curl.exe to download with".to_owned());
    }
    let out = Command::new(curl)
        // `--disable`, and it only works first: it stops curl reading
        // `%APPDATA%\_curlrc`, which a same-user process can create and which may
        // set *any* option, `--output` included. The pin cannot see that either.
        .arg("--disable")
        .args(["--fail", "--silent", "--show-error", "--location"])
        .args(["--max-time", &FETCH_TIMEOUT_SECS.to_string()])
        .args(["--max-filesize", &INSTALLER_BYTES.to_string()])
        .arg(INSTALLER_URL)
        .output()
        .map_err(|err| format!("the download could not be started: {err}"))?;
    if !out.status.success() {
        // stdout is the payload, so stderr is captured and the log is the only
        // place a TLS or proxy diagnostic can be read.
        warn!(
            exit = out.status.code().unwrap_or(-1),
            detail = %String::from_utf8_lossy(&out.stderr).trim(),
            url = INSTALLER_URL,
            "curl could not fetch the Npcap installer"
        );
        return Err(format!(
            "the download failed (curl exit {}). The address is in the log.",
            out.status.code().unwrap_or(-1)
        ));
    }
    Ok(out.stdout)
}

/// `create_new` is the point: anything already at `path` — a leftover, a symlink
/// aimed somewhere sensitive, a file re-planted since the delete above — refuses
/// the open. An elevated `curl --output` had no such refusal.
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = create_new_locked(path).map_err(|err| {
        format!(
            "the installer could not be written to {}: {err}",
            path.display()
        )
    })?;
    file.write_all(bytes)
        .map_err(|err| format!("the installer could not be written: {err}"))
}

/// Read handle denying every access another process would need to change what is
/// at this path; the module header is what rests on it. `FILE_SHARE_READ` is the
/// one flag kept — without it `CreateProcess` could not map the image.
#[cfg(windows)]
fn open_locked(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
}

/// Same denial, on the handle the bytes are written through, so the window
/// between the write and [`open_locked`] is not one either.
#[cfg(windows)]
fn create_new_locked(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
}

/// Dev-machine stubs; nothing off Windows reaches the fetcher, so neither
/// carries the sharing guarantee.
#[cfg(not(windows))]
fn open_locked(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

#[cfg(not(windows))]
fn create_new_locked(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// The `curl.exe` [`download`] will run, by absolute path. The non-Windows arm
/// names one that will not exist, a branch `download` already handles.
#[cfg(windows)]
fn curl_path() -> PathBuf {
    crate::system32::directory().join("curl.exe")
}

#[cfg(not(windows))]
fn curl_path() -> PathBuf {
    PathBuf::from("curl.exe")
}

/// Where the download lands: a version-stamped name kept with the version
/// ([`TEMP_INSTALLER_NAME`]) in a directory the module header explains.
fn installer_path() -> PathBuf {
    staging_dir().join(TEMP_INSTALLER_NAME)
}

/// The `Command` [`Fetcher::run`] spawns, built but not spawned — a test can
/// inspect it (`get_envs`, `get_current_dir`) without launching Npcap's
/// installer.
///
/// `current_dir`: the child's current directory is a slot in its DLL search
/// order and the inherited one is wherever the app was started from — not the
/// *image* slot; see the module header.
///
/// `TEMP`/`TMP`: the NSIS stub resolves its plugin directory through
/// `GetTempPath`, which reads `TMP` then `TEMP` — set both, so the stub's own
/// extraction lands in the directory this process already clamped rather than
/// the attacker-writable `%TEMP%` the module header opens with. Not
/// `env_clear`: the child still needs `SystemRoot`, `windir`, `PATH` and the
/// profile variables to run at all, and `__COMPAT_LAYER` plus the proxy
/// variables still reach it unchanged — a curated allowlist is a larger
/// project than this fix.
fn installer_command(target: &Path) -> Command {
    let mut command = Command::new(target);
    command
        .current_dir(staging_dir())
        .env("TEMP", staging_dir())
        .env("TMP", staging_dir());
    command
}

/// Chosen once per process. Under `%TEMP%` because this app cannot clean up
/// after the installer it launches, never *at* `%TEMP%` per the module header.
///
/// The pid and clock are not secrets — nothing trusts an attacker's ignorance of
/// a path. They stop two runs colliding, which matters because
/// [`prepare_staging`] insists on *creating* the directory, and stop a
/// predictable name being occupied first, which would turn that insistence into
/// a Download button an attacker can hold shut. Touches no disk, so a test can
/// assert the choice without creating anything.
fn staging_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        std::env::temp_dir().join(format!("{STAGING_PREFIX}{}-{stamp:x}", std::process::id()))
    })
}

/// The answer is cached, failure included: [`staging_dir`] names one path per
/// process, so a second attempt would hit [`prepare_staging`]'s
/// `create_dir_clamped` refusing the directory this one created.
fn staging_ready() -> Result<(), String> {
    static PREPARED: OnceLock<Result<(), String>> = OnceLock::new();
    PREPARED
        .get_or_init(|| prepare_staging(staging_dir()))
        .clone()
}

/// Sweeps what earlier runs left, creates `dir` already clamped, and proves it
/// empty.
///
/// `create_dir_clamped`, not `create_dir` then `lock_down`: [`staging_dir`]
/// picks a fresh name per run, so anything already there was put there by
/// someone else — which is also why the sweep runs first.
/// `create_dir_clamped` passes [`STAGING_DACL`] to `CreateDirectoryW` as part
/// of the same call that creates the object, so the directory is never born
/// with `%TEMP%`'s inherited ACEs — there is no interval, microsecond or
/// otherwise, in which it exists but is not yet clamped for a `version.dll` to
/// land in. That interval is exactly what the earlier create-then-clamp
/// sequence had, and what let a same-user process win the race, delete the
/// directory before it lost `%TEMP%`'s permissive ACEs, and re-create it under
/// its own ownership: empty, so the check below would have passed, and free to
/// rewrite the DACL. An unelevated owner of a *clamped* directory has no such
/// move — measured in [`sweep_old_staging`], it is refused `DELETE`
/// outright — so nothing outside this call can touch `dir` between
/// `create_dir_clamped` returning and this function's own return.
///
/// `lock_down` does **not** run again here, and that is deliberate, not an
/// oversight: with the DACL applied atomically there is no window left for its
/// reparse-point refusal to guard at this call site, and calling it anyway
/// would cost more than a redundant `SetSecurityInfo` — measured, re-opening
/// an *already-clamped* directory for `WRITE_DAC` is refused even to the
/// process that just created it, unless that process's token actually carries
/// `BUILTIN\Administrators` enabled (true for this app, which always runs
/// elevated, but not for the `cargo test` process that exercises this
/// function directly). `lock_down` stays in this file, `#[cfg(test)]` now: see
/// its own doc for what it still proves through its read-back test. The
/// emptiness check below is belt-and-suspenders for the same reason
/// `lock_down` does not run here at all: creation and clamp are one atomic
/// call, so the check should never observe `dir` non-empty, but it stays cheap
/// and catches the gap if that assumption is ever wrong.
fn prepare_staging(dir: &Path) -> Result<(), String> {
    sweep_old_staging(dir);

    create_dir_clamped(dir, STAGING_DACL).map_err(|err| {
        format!(
            "the installer's download directory could not be created ({err}); \
             nothing was downloaded"
        )
    })?;

    match std::fs::read_dir(dir).map(|mut entries| entries.next().is_some()) {
        Ok(false) => Ok(()),
        Ok(true) => {
            let _ = std::fs::remove_dir_all(dir);
            Err(
                "something was written into the installer's download directory \
                 as it was being created; nothing was downloaded"
                    .to_owned(),
            )
        }
        Err(err) => {
            let _ = std::fs::remove_dir_all(dir);
            Err(format!(
                "the installer's download directory could not be read back ({err}); \
                 nothing was downloaded"
            ))
        }
    }
}

/// Deletes the staging directories of runs that are over. Best-effort, and it
/// has to exist: nothing else on the machine will ever remove them, so an
/// elevated run would leave an undeletable megabyte in `%TEMP%` per download.
///
/// "Undeletable" is measured, and the opposite of what was expected: `%TEMP%`
/// grants the interactive user `FILE_DELETE_CHILD`, normally enough to delete a
/// child whose own DACL grants nothing, yet `remove_dir` on an *empty* clamped
/// directory the user owns still answers `ERROR_ACCESS_DENIED` — owning it buys
/// `READ_CONTROL` and `WRITE_DAC`, neither of which is `DELETE`. So the sweep
/// needs the administrator token.
///
/// A live run's directory survives anyway: `remove_dir_all` fails on the
/// installer held by [`open_locked`] or mapped into the launched process.
fn sweep_old_staging(current: &Path) {
    let Some(parent) = current.parent() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(STAGING_PREFIX)
        {
            continue;
        }
        let path = entry.path();
        if path == current {
            continue;
        }
        // Both spellings: the name may have been taken by a file — planted, or
        // left by a crash — and either way it must be gone before
        // `create_dir_clamped`.
        if std::fs::remove_dir_all(&path).is_err() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Parses `sddl` into a security descriptor, the one
/// `ConvertStringSecurityDescriptorToSecurityDescriptorW` call `lock_down`
/// (test-only, see its own doc) and [`create_dir_clamped`] both act through
/// rather than each carrying their own copy.
///
/// Returns the raw block on success. The caller owns it from that point and
/// must free it with exactly one `LocalFree`, on every exit path — this
/// function never frees it, since the only reason to call it is to still need
/// the descriptor afterward.
#[cfg(windows)]
fn parse_security_descriptor(
    sddl: &str,
) -> std::io::Result<windows_sys::Win32::Security::PSECURITY_DESCRIPTOR> {
    use std::ptr;

    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;

    let sddl: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();

    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `sddl` is a valid null-terminated UTF-16 string alive across the
    // call; `descriptor` is a live stack slot, written on success with one
    // `LocalAlloc` block owning the ACL too; the size out-parameter is optional.
    let built = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    };
    if built == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(descriptor)
}

/// A *protected* DACL, which is what makes `%TEMP%`'s full-control-for-the-user
/// ACE stop at the staging directory's door. SDDL rather than `InitializeAcl`
/// plus two `CreateWellKnownSid` plus two `AddAccessAllowedAceEx`: the same two
/// ACEs in five times the `unsafe`. `PROTECTED_DACL_SECURITY_INFORMATION` is not
/// redundant with the string's `P` — `SetSecurityInfo` takes an `ACL`, not a
/// descriptor, so the control word the `P` set never reaches it.
///
/// Addressed through `crate::dirhandle::open_directory_itself`, which refuses a
/// reparse point — `SetNamedSecurityInfoW` does *not* follow a junction
/// (measured), so a name-based call would clamp the reparse point and report
/// success while [`installer_path`] resolved *through* it. Act on the object
/// you checked, not the name — the same gate `crate::migrate` uses.
///
/// [`prepare_staging`] no longer calls this: it creates `dir` through
/// [`create_dir_clamped`] instead, already clamped, so there is no interval in
/// which an unelevated process could swap the name for a junction — an owner
/// refused `DELETE` (measured, [`sweep_old_staging`]) cannot remove the name
/// to replace it, and `CreateDirectoryW` itself fails outright rather than
/// following a reparse point already sitting on the path. That leaves this
/// function without a caller in this file outside its own test below, which
/// exercises it directly against a directory still in the *pre*-clamp,
/// %TEMP%-inherited state `create_dir_clamped` no longer produces — the one
/// shape this function still needs to handle correctly, and the reason it
/// stays rather than being deleted.
///
/// `sddl` is a parameter, not the constant inlined — do not simplify that away.
/// An unelevated owner cannot delete a clamped directory (see
/// [`sweep_old_staging`]), so the test proving the clamp took can only tidy up
/// by calling this again with a DACL granting the rights back.
///
/// `#[cfg(test)]`: with [`prepare_staging`] no longer a caller, this function
/// is reachable only from its own tests below and the junction-refusal test
/// further down. Kept for what those tests still prove about
/// `crate::dirhandle::open_directory_itself` and `SetSecurityInfo` at the OS
/// level — see the module-level reasoning above — not because production code
/// still calls it.
#[cfg(test)]
#[cfg(windows)]
fn lock_down(dir: &Path, sddl: &str) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo};
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
        PROTECTED_DACL_SECURITY_INFORMATION,
    };

    // Before anything is parsed: a name that is no longer a plain directory has
    // nothing here worth clamping.
    let handle = crate::dirhandle::open_directory_itself(dir)?;

    let descriptor = parse_security_descriptor(sddl)?;

    let mut dacl: *mut ACL = ptr::null_mut();
    let mut present = 0;
    let mut defaulted = 0;
    // SAFETY: `descriptor` is the block allocated above and not yet freed; the
    // three out-parameters are stack slots outliving the call; `dacl` points
    // *into* that block, so every read of it precedes the one `LocalFree`.
    let read =
        unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) };
    // `GetLastError` is clobbered by the very next Win32 call: read it here, not
    // after the `SetSecurityInfo` below.
    let read_err = std::io::Error::last_os_error();

    let outcome = if read == 0 {
        Err(read_err)
    } else if present == 0 || dacl.is_null() {
        // Never passed on: `SetSecurityInfo` reads a null `pDacl` as "install a
        // *null* DACL", granting everyone full access. `STAGING_DACL` names two
        // ACEs so this is unreachable, but being wrong costs the whole fix,
        // silently.
        Err(std::io::Error::other(
            "the staging descriptor parsed with no DACL in it",
        ))
    } else {
        // SAFETY: `handle` is an open directory handle carrying `WRITE_DAC`,
        // alive across the call; `dacl` is the non-null ACL inside the
        // descriptor above and is copied, not retained; the owner, group and
        // SACL pointers are null, "do not change".
        let status = unsafe {
            SetSecurityInfo(
                handle.as_raw_handle(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                dacl,
                ptr::null(),
            )
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(status as i32))
        }
    };

    // SAFETY: the only `LocalFree`, freeing the parse's block exactly once;
    // `dacl` pointed into it and is not read again.
    unsafe { LocalFree(descriptor.cast()) };
    outcome
}

/// Dev-machine stub; nothing off Windows reaches the fetcher. `#[cfg(test)]`
/// for the same reason as the Windows version above: no production caller.
#[cfg(test)]
#[cfg(not(windows))]
fn lock_down(_dir: &Path, _sddl: &str) -> std::io::Result<()> {
    Ok(())
}

/// Creates `dir` with `sddl` already its DACL — `CreateDirectoryW` takes a
/// `SECURITY_ATTRIBUTES` directly, so the object exists and is clamped in the
/// one call. No `std::fs` API passes a `SECURITY_ATTRIBUTES` through, which is
/// why [`prepare_staging`] used to create first and clamp after; see its doc
/// for what that interval cost.
#[cfg(windows)]
fn create_dir_clamped(dir: &Path, sddl: &str) -> std::io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

    let descriptor = parse_security_descriptor(sddl)?;

    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let attrs = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };

    // SAFETY: `wide` is a valid null-terminated UTF-16 path alive across the
    // call; `attrs` is a live stack value whose `lpSecurityDescriptor` is the
    // block `parse_security_descriptor` just allocated and has not yet been
    // freed; `CreateDirectoryW` only reads through both pointers and does not
    // retain either past its return.
    let created = unsafe { CreateDirectoryW(wide.as_ptr(), &attrs) };
    let create_err = std::io::Error::last_os_error();

    // SAFETY: the only `LocalFree`, freeing the parse's block exactly once;
    // `CreateDirectoryW` above only read from it and kept no reference to it.
    unsafe { LocalFree(descriptor.cast()) };

    if created == 0 {
        return Err(create_err);
    }
    Ok(())
}

/// Dev-machine stub; nothing off Windows reaches the fetcher.
#[cfg(not(windows))]
fn create_dir_clamped(dir: &Path, _sddl: &str) -> std::io::Result<()> {
    std::fs::create_dir(dir)
}

/// Starts a second copy of this executable, for the caller to follow with a
/// window close.
///
/// Nothing technical forces it — measured, Windows does **not** cache a failed
/// `LoadLibrary`, so a path that answered `ERROR_MOD_NOT_FOUND` (126) loads in
/// the same process once the file appears. What forces it is where the failure
/// lands: `build_source`'s `?` ends `Session::run`, leaving the window holding
/// [`SessionHandles`](crate::app::SessionHandles) whose command receiver went
/// with it. Reviving that costs an `Option<CaptureWorker>`, six values kept
/// alive for a retry and a changed teardown path; a relaunch costs one click.
/// No second UAC prompt — the exe is manifested `requireAdministrator`.
///
/// # Errors
///
/// If the executable's path cannot be read or the child cannot be spawned. The
/// window keeps its banner either way: a failed relaunch must not look like a
/// successful one.
pub fn relaunch() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    // Not held: it outlives this process by design, and waiting would deadlock
    // the window that is about to close.
    Command::new(&exe).spawn()?;
    info!(path = %exe.display(), "relaunching after the Npcap install");
    Ok(())
}

/// Takes the open handle, not the path: re-opening by name is the gap the module
/// header is about. `mut file: &File` because `&File` is itself a `Read`, so the
/// caller keeps the handle it holds across the spawn.
fn verify(mut file: &File) -> Result<(), String> {
    let size = file
        .metadata()
        .map_err(|err| format!("the downloaded file could not be read: {err}"))?
        .len();
    // Before reading: a captive portal's error page should not be loaded into
    // memory just to be hashed.
    if size != INSTALLER_BYTES {
        return Err(format!(
            "the download is {size} bytes, not the expected {INSTALLER_BYTES} — it is not the installer"
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|err| format!("the downloaded file could not be read: {err}"))?;
    if sha256(&bytes) == INSTALLER_SHA256 {
        Ok(())
    } else {
        Err("the download does not match the expected installer and was discarded".to_owned())
    }
}

/// SHA-256 through CNG's one-shot entry point: the `BCRYPT_SHA256_ALG_HANDLE`
/// pseudo-handle needs no provider opened or closed, so the digest is one call
/// and one `unsafe` block rather than open/create/hash/finish/destroy.
#[cfg(windows)]
fn sha256(bytes: &[u8]) -> [u8; 32] {
    use windows_sys::Win32::Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptHash};

    let mut out = [0_u8; 32];
    // SAFETY: the algorithm handle is the documented pseudo-handle constant, the
    // secret is empty (a plain hash, not an HMAC), and both pointers come from
    // slices whose lengths are passed alongside them.
    let status = unsafe {
        BCryptHash(
            BCRYPT_SHA256_ALG_HANDLE,
            std::ptr::null_mut(),
            0,
            bytes.as_ptr().cast_mut(),
            u32::try_from(bytes.len()).unwrap_or(u32::MAX),
            out.as_mut_ptr(),
            u32::try_from(out.len()).unwrap_or(u32::MAX),
        )
    };
    if status != 0 {
        // `out` stays zeroed, which cannot equal the pin: a hash that failed must
        // never read as a hash that matched.
        warn!(status, "BCryptHash failed; treating the file as unverified");
        return [0_u8; 32];
    }
    out
}

/// Dev-machine stub; nothing on a non-Windows target reaches the fetcher.
#[cfg(not(windows))]
fn sha256(_bytes: &[u8]) -> [u8; 32] {
    [0_u8; 32]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_fetcher_is_idle() {
        assert_eq!(Fetcher::new().progress(), Progress::Idle);
    }

    /// At 4 Hz the second frame's click used to find `Idle`, and the reuse fast
    /// path launched a second elevated Npcap installer.
    #[test]
    fn a_second_click_cannot_take_a_fetch_that_is_already_claimed() {
        let fetcher = Fetcher::new();
        assert!(fetcher.claim(), "the first click owns the fetch");
        assert_eq!(
            fetcher.progress(),
            Progress::Fetching,
            "the claim has to be visible before `start` returns, not once the worker runs"
        );
        assert!(!fetcher.claim(), "the second click must find it taken");
    }

    /// The UI cannot produce this — `start` runs on one egui thread — but
    /// `Fetcher` is `Clone`, so "exactly one winner" belongs to the type rather
    /// than to its current caller.
    #[test]
    fn exactly_one_of_many_racing_claims_wins() {
        let fetcher = Fetcher::new();
        let start = std::sync::Barrier::new(8);
        let won: usize = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let (fetcher, start) = (fetcher.clone(), &start);
                    scope.spawn(move || {
                        start.wait();
                        usize::from(fetcher.claim())
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("a claim must not panic"))
                .sum()
        });
        assert_eq!(won, 1, "one download, not eight");
    }

    /// The property this replaced — that no `%SystemRoot%` read chooses the path
    /// — cannot be asserted without setting that variable. What holds it is that
    /// the environment read is gone.
    #[cfg(windows)]
    #[test]
    fn the_downloader_runs_the_system_curl_by_absolute_path() {
        let curl = curl_path();
        assert!(curl.is_absolute(), "{curl:?}");
        assert!(
            curl.starts_with(crate::system32::directory()),
            "{curl:?} — an elevated process must not run a curl.exe chosen by its launcher"
        );
        // Not `is_file`: a Windows old enough to lack curl is supported and
        // `download` reports it. What must hold is where we looked.
        assert_eq!(curl.file_name(), Some(std::ffi::OsStr::new("curl.exe")));
    }

    #[cfg(windows)]
    #[test]
    fn sha256_matches_the_published_vectors() {
        // NIST's two standard vectors, so a wrong `BCryptHash` call is caught
        // here and not as a mysteriously rejected download.
        let abc = sha256(b"abc");
        assert_eq!(
            abc[..4],
            [0xba, 0x78, 0x16, 0xbf],
            "SHA-256(\"abc\") must start ba7816bf"
        );
        let empty = sha256(b"");
        assert_eq!(
            empty[..4],
            [0xe3, 0xb0, 0xc4, 0x42],
            "SHA-256(\"\") must start e3b0c442"
        );
    }

    #[test]
    fn a_short_file_is_refused_before_it_is_read() {
        let path = std::env::temp_dir().join("arkyve-npcap-test-short.bin");
        std::fs::write(&path, b"not an installer").expect("write the fixture");
        let file = open_locked(&path).expect("open the fixture");
        let err = verify(&file).expect_err("a 16-byte file is not the installer");
        assert!(err.contains("not the expected"), "got: {err}");
        drop(file);
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(windows)]
    #[test]
    fn a_verified_file_cannot_be_swapped_while_its_handle_is_held() {
        // Against the OS, because a share mode is invisible at the call site
        // that depends on it.
        let path = std::env::temp_dir().join("arkyve-npcap-test-locked.bin");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"the verified bytes").expect("write the fixture");

        let held = open_locked(&path).expect("open the fixture");
        assert!(
            std::fs::write(&path, b"the attacker's bytes").is_err(),
            "a held installer handle must refuse a rewrite"
        );
        assert!(
            std::fs::remove_file(&path).is_err(),
            "a held installer handle must refuse a delete"
        );
        assert!(
            OpenOptions::new().write(true).open(&path).is_err(),
            "a held installer handle must refuse a second writer"
        );

        drop(held);
        std::fs::remove_file(&path).expect("the path is free again once dropped");
    }

    #[test]
    fn the_installer_is_never_written_over_something_already_there() {
        // `create_new` stands between an elevated write and a symlink planted at
        // the predictable temp path.
        let path = std::env::temp_dir().join("arkyve-npcap-test-planted.bin");
        std::fs::write(&path, b"planted").expect("write the fixture");
        let err = write_new(&path, b"the installer").expect_err("must refuse an occupied path");
        assert!(err.contains("could not be written"), "got: {err}");
        assert_eq!(
            std::fs::read(&path).expect("the planted file"),
            b"planted",
            "the planted file must be left untouched, not overwritten"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The finding the staging directory exists for: nothing pins the pinned
    /// installer's *neighbours*, so a run out of `%TEMP%` loads a planted
    /// `version.dll` elevated without disturbing the hash.
    #[test]
    fn the_installer_is_never_staged_in_the_shared_temp_root() {
        let temp = std::env::temp_dir();
        let installer = installer_path();
        assert_ne!(
            installer.parent(),
            Some(temp.as_path()),
            "the verified installer must not sit beside whatever a same-user process \
             can plant in %TEMP%: its own directory is first in its DLL search order"
        );
        assert_eq!(installer.parent(), Some(staging_dir()));
        assert_eq!(
            installer.file_name(),
            Some(std::ffi::OsStr::new(TEMP_INSTALLER_NAME))
        );
        assert!(staging_dir().starts_with(&temp), "{:?}", staging_dir());
        assert!(
            staging_dir()
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(STAGING_PREFIX)),
            "{:?} — the sweep only recognises this prefix",
            staging_dir()
        );
        assert_ne!(
            staging_dir(),
            temp.join(crate::APP_DIR).as_path(),
            "and never the fallback log root: an admins-only DACL there is exactly \
             what `crate::migrate` exists to undo"
        );
    }

    /// `Command::get_envs` returns the explicitly-set overrides — exactly the
    /// property under test — without spawning anything, so this runs on every
    /// platform and never launches Npcap's installer.
    #[test]
    fn the_child_environment_points_temp_at_the_staging_directory() {
        let command = installer_command(&installer_path());
        let envs: std::collections::HashMap<_, _> = command.get_envs().collect();

        let staging = Some(staging_dir().as_os_str());
        assert_eq!(
            envs.get(std::ffi::OsStr::new("TEMP")),
            Some(&staging),
            "the NSIS stub reads TMP then TEMP through GetTempPath; both must \
             point at the directory this process already clamped, not whatever \
             %TEMP% the launcher inherited"
        );
        assert_eq!(
            envs.get(std::ffi::OsStr::new("TMP")),
            Some(&staging),
            "GetTempPath is checked in this order — TMP first"
        );
    }

    /// Against the OS, not the SDDL string: a DACL is invisible at the call site
    /// that depends on it. Works filtered or elevated, since a directory's owner
    /// always keeps `READ_CONTROL`.
    #[cfg(windows)]
    #[test]
    fn a_staged_directory_stops_inheriting_the_users_full_control() {
        use windows_sys::Win32::Security::SE_DACL_PROTECTED;

        let dir = std::env::temp_dir().join(format!(
            "{STAGING_PREFIX}dacl-fixture-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir(&dir);
        std::fs::create_dir(&dir).expect("the fixture directory");

        let (before, _) = dacl_of(&dir);
        assert_eq!(
            before & SE_DACL_PROTECTED,
            0,
            "a directory created under %TEMP% inherits its ACEs — including the \
             interactive user's full control. That is the hole."
        );

        lock_down(&dir, STAGING_DACL).expect("clamp the fixture");

        let (after, aces) = dacl_of(&dir);
        assert_ne!(
            after & SE_DACL_PROTECTED,
            0,
            "the staging directory must stop inheriting from %TEMP% entirely"
        );
        assert_eq!(
            aces, 2,
            "exactly the two ACEs `STAGING_DACL` names — Administrators and SYSTEM"
        );

        // Measured: `remove_dir` on the empty directory it just created answers
        // `ERROR_ACCESS_DENIED`, so the test cannot tidy up without un-clamping.
        // Undone by name because — measured with a bare `CreateFileW` — an
        // unelevated owner of a clamped directory is refused `READ_CONTROL`,
        // `WRITE_DAC` and both together, so `lock_down`'s handle gate cannot
        // re-open what it just clamped. Production never re-clamps.
        hand_back_full_control(&dir).expect("hand the fixture back");
        std::fs::remove_dir(&dir).expect("the fixture is deletable once un-clamped");
    }

    /// [`create_dir_clamped`] directly — not [`prepare_staging`], whose
    /// emptiness check afterward opens the directory it just clamped to list
    /// its contents, and that (measured, same as [`lock_down`]'s handle gate
    /// below) needs the elevated token this app always runs under, which a
    /// `cargo test` process does not have. That is a pre-existing property of
    /// [`prepare_staging`], not one this test is about.
    ///
    /// What this test is about is testable unelevated: [`dacl_of`] only needs
    /// `READ_CONTROL`, which the read-back test above shows a clamped
    /// directory's own creator keeps regardless of elevation. No [`lock_down`]
    /// call runs between the directory's creation and this read-back, so it
    /// fails if [`create_dir_clamped`] ever regresses to a plain
    /// `std::fs::create_dir` — exactly the create-then-clamp window step 3
    /// closes.
    #[cfg(windows)]
    #[test]
    fn a_freshly_prepared_staging_directory_is_already_protected() {
        use windows_sys::Win32::Security::SE_DACL_PROTECTED;

        let dir = std::env::temp_dir().join(format!(
            "{STAGING_PREFIX}atomic-fixture-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir(&dir);

        create_dir_clamped(&dir, STAGING_DACL)
            .expect("the directory is created and clamped in the one call");

        let (after, aces) = dacl_of(&dir);
        assert_ne!(
            after & SE_DACL_PROTECTED,
            0,
            "a freshly created staging directory must already be protected — \
             there was no `lock_down` call in between to have done it"
        );
        assert_eq!(
            aces, 2,
            "exactly the two ACEs `STAGING_DACL` names — Administrators and SYSTEM"
        );

        // Same cleanup as the read-back test above, for the same reason: an
        // unelevated owner of a clamped directory cannot delete it.
        hand_back_full_control(&dir).expect("hand the fixture back");
        std::fs::remove_dir(&dir).expect("the fixture is deletable once un-clamped");
    }

    /// Measured: `SetNamedSecurityInfoW` does not follow a junction, so a
    /// junction swapped in before the clamp took the DACL on the *reparse point*
    /// and reported success while [`installer_path`] resolved *through* it.
    /// Hence the assertion is about the link, not the target — asserting the
    /// target was untouched would pass against the unfixed code. Deliberately
    /// not named with `STAGING_PREFIX`, which [`sweep_old_staging`] would be
    /// entitled to delete under a concurrent test.
    #[test]
    #[cfg(windows)]
    fn a_junction_in_place_of_the_staging_directory_is_refused_not_followed() {
        use windows_sys::Win32::Security::SE_DACL_PROTECTED;

        let root = std::env::temp_dir().join(format!(
            "arkyve-staging-junction-fixture-{}",
            std::process::id()
        ));
        let victim = root.join("victim");
        let link = root.join("link");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&victim).expect("the victim directory");

        // A junction needs no privilege, unlike a symlink — which is why the
        // attacker can reach this and why the test can build it unelevated.
        let made = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&victim)
            .output()
            .expect("run mklink");
        assert!(
            made.status.success(),
            "the fixture needs a real junction, not a skipped test: {}",
            String::from_utf8_lossy(&made.stderr)
        );

        let refused = lock_down(&link, STAGING_DACL);

        // Checked before the error, so a regression reports the cause and not
        // the symptom. Against the unfixed code this reads `SE_DACL_PROTECTED`.
        let (link_dacl, _) = dacl_of(&link);
        assert_eq!(
            link_dacl & SE_DACL_PROTECTED,
            0,
            "nothing may be clamped when the path turns out to be a junction"
        );
        assert!(
            refused.is_err(),
            "a reparse point in place of the staging directory must be refused"
        );
        let (victim_dacl, _) = dacl_of(&victim);
        assert_eq!(
            victim_dacl & SE_DACL_PROTECTED,
            0,
            "and the target keeps its own DACL, as it did before the fix too"
        );

        // `remove_dir` unlinks the junction; `remove_dir_all` on the root would
        // have to decide what to do about it.
        std::fs::remove_dir(&link).expect("unlink the junction");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Fixture cleanup only, and *deliberately* name-based — see the call site.
    /// Not a security operation: it hands rights away.
    #[cfg(windows)]
    fn hand_back_full_control(dir: &Path) -> std::io::Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use std::ptr;

        use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
            SetNamedSecurityInfoW,
        };
        use windows_sys::Win32::Security::{
            ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, PSECURITY_DESCRIPTOR,
        };

        let sddl: Vec<u16> = "D:(A;OICI;FA;;;WD)"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let wide: Vec<u16> = dir
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: `sddl` is a valid null-terminated UTF-16 string alive across
        // the call; `descriptor` is a live stack slot written on success with
        // one `LocalAlloc` block; the size out-parameter is optional.
        let built = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        assert_ne!(built, 0, "parse the fixture's give-it-back DACL");

        let mut dacl: *mut ACL = ptr::null_mut();
        let mut present = 0;
        let mut defaulted = 0;
        // SAFETY: `descriptor` is the block allocated above and not yet freed;
        // the out-parameters are stack slots outliving the call.
        let read = unsafe {
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
        };
        assert_ne!(read, 0, "read the parsed DACL back");
        assert!(
            present != 0 && !dacl.is_null(),
            "a null DACL grants everyone"
        );

        // SAFETY: `wide` is a valid null-terminated UTF-16 path alive for the
        // call; `dacl` points into the descriptor above and is copied rather
        // than retained; the owner/group/SACL pointers are null, "do not change".
        let status = unsafe {
            SetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                dacl,
                ptr::null(),
            )
        };

        // SAFETY: the only `LocalFree` on the only path that reaches it.
        unsafe { LocalFree(descriptor.cast()) };

        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(status as i32))
        }
    }

    #[cfg(windows)]
    fn dacl_of(dir: &Path) -> (u16, u16) {
        use std::os::windows::ffi::OsStrExt;
        use std::ptr;

        use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
        use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
        use windows_sys::Win32::Security::{
            ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, PSECURITY_DESCRIPTOR,
        };

        let wide: Vec<u16> = dir
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let mut dacl: *mut ACL = ptr::null_mut();

        // SAFETY: `wide` is a valid null-terminated UTF-16 path alive across the
        // call; `dacl` and `descriptor` are live stack slots, and on success one
        // `LocalAlloc` block holds the descriptor with `dacl` pointing into it;
        // the owner, group and SACL out-parameters are optional, passed null.
        let status = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(status, ERROR_SUCCESS, "reading the DACL of {dir:?}");
        assert!(
            !dacl.is_null(),
            "a null DACL would grant everyone everything"
        );

        let mut control: u16 = 0;
        let mut revision: u32 = 0;
        // SAFETY: `descriptor` is the block allocated above; `control` and
        // `revision` are stack slots outliving the call.
        let ok = unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) };
        assert_ne!(ok, 0, "reading the control word of {dir:?}");
        // SAFETY: `dacl` is the non-null ACL header inside that same live block.
        let aces = unsafe { (*dacl).AceCount };
        // SAFETY: the one `LocalFree`, freeing that block exactly once; neither
        // `dacl` nor `descriptor` is read afterwards.
        unsafe { LocalFree(descriptor.cast()) };

        (control, aces)
    }

    /// A run cannot delete its own staging directory on the way out — the
    /// installer it launched is still mapped — so without the sweep that is an
    /// undeletable megabyte in `%TEMP%` per download.
    #[test]
    fn an_earlier_runs_staging_directory_is_swept_and_its_neighbours_are_not() {
        let parent = std::env::temp_dir().join(format!("arkyve-sweep-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir(&parent).expect("the scratch parent");

        let stale = parent.join(format!("{STAGING_PREFIX}older-run"));
        let mine = parent.join(format!("{STAGING_PREFIX}this-run"));
        // The name the sweep must never touch: the fallback log root, used when
        // `%LOCALAPPDATA%` is unavailable.
        let logs = parent.join(crate::APP_DIR);
        std::fs::create_dir(&stale).expect("the stale sibling");
        std::fs::create_dir(&mine).expect("this run's directory");
        std::fs::create_dir(&logs).expect("the log root");
        std::fs::write(logs.join("keep.log"), b"a player's log").expect("write the log");

        sweep_old_staging(&mine);

        assert!(!stale.exists(), "an earlier run's directory must be swept");
        assert!(
            mine.is_dir(),
            "the sweep must not delete the directory this run is about to use"
        );
        assert!(
            logs.join("keep.log").is_file(),
            "the sweep must not reach anything but its own prefix"
        );

        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn a_window_side_failure_lands_in_the_same_cell() {
        // The one failure the worker cannot report; it must not leave the banner
        // claiming success.
        let fetcher = Fetcher::new();
        fetcher.set(Progress::Launched);
        fetcher.restart_failed("could not restart: access denied".to_owned());
        assert!(matches!(fetcher.progress(), Progress::RestartFailed(reason)
            if reason.contains("could not restart")));
    }

    #[test]
    fn a_failed_restart_is_not_a_failed_download() {
        // Collapsed into one state they share `Failed`'s remedy, which is a
        // second Npcap installer.
        let fetcher = Fetcher::new();
        fetcher.set(Progress::Launched);
        fetcher.restart_failed("could not restart: access denied".to_owned());
        assert!(
            !matches!(fetcher.progress(), Progress::Failed(_)),
            "a failed restart must not read as a failed download"
        );
    }

    #[test]
    fn starting_twice_does_not_launch_two_workers() {
        let fetcher = Fetcher::new();
        fetcher.set(Progress::Fetching);
        fetcher.start();
        assert_eq!(fetcher.progress(), Progress::Fetching);
    }

    #[test]
    fn nothing_starts_a_second_installer_once_one_is_running() {
        // Both post-launch states: `fetch_and_check`'s reuse fast path means a
        // start from either reaches the `spawn` with the file already verified.
        for state in [
            Progress::Launched,
            Progress::RestartFailed("could not restart: access denied".to_owned()),
        ] {
            assert!(
                !accepts_a_start(&state),
                "a start from {state:?} must be refused"
            );
        }
        assert!(accepts_a_start(&Progress::Idle));
        assert!(accepts_a_start(&Progress::Failed("no network".to_owned())));
    }
}
