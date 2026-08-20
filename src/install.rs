//! Fetching the Npcap installer for a player who has none.
//!
//! The window's error banner offers a `Download` button; this is what it drives.
//! It fetches one pinned file, checks it byte for byte, and launches it. It does
//! not install anything itself and it cannot: Npcap's silent installer is the
//! paid OEM product (`docs/npcap-provisioning.md` quotes the licence), so the
//! player still meets Npcap's own setup window and its own licence. What this
//! removes is the trip to a browser, a download folder and back.
//!
//! Which build, from where, and against which hash are not decided here — they
//! are [`crate::npcap`], because `capture::pcap` names the same build in the
//! message a player without Npcap reads and cannot see this module. That file
//! also carries the argument for pinning a hash rather than checking a
//! signature. What is decided here is everything about *how* the file is
//! obtained and handed to Windows.
//!
//! # Why the verified file is held open
//!
//! A pin only binds if the bytes hashed and the bytes executed are the same
//! bytes. They were not: the check and the launch used to be two separate opens
//! of one predictable `%TEMP%` path, which any medium-integrity process of the
//! same user can write. Swap the file between them and the administrator token
//! starts the attacker's binary instead — repeatably, since the success path
//! leaves the file behind and re-verifies it on every later click, and
//! observably, since [`Progress`] narrates each step.
//!
//! So [`Fetcher::fetch_and_check`] returns the open [`File`] it hashed, opened
//! through [`open_locked`] with neither `FILE_SHARE_WRITE` nor
//! `FILE_SHARE_DELETE`, and [`Fetcher::run`] holds it across the spawn. While
//! that handle lives the path cannot be written, replaced, renamed or deleted
//! by anyone, so `CreateProcess` maps the file that was hashed and no other.
//! `FILE_SHARE_READ` has to stay: the image `CreateProcess` maps is a read.
//!
//! The download is written by this module rather than by `curl --output`, for
//! the neighbouring reason: `create_new` refuses to follow a symlink or to
//! reuse anything already sitting at the path, so an elevated write cannot be
//! aimed out of `%TEMP%` by something planted there first.
//!
//! # Why it is not held open *in* `%TEMP%`
//!
//! That handle binds the installer's bytes. It binds nothing about the
//! directory they sit in, and `CreateProcess` puts the image's own directory
//! **first** in the child's DLL search order — ahead of System32, behind only
//! the `KnownDLLs` section. Measured on the dev machine: that section holds 37
//! entries and not one of `version`, `uxtheme`, `dwmapi`, `riched20`, `msimg32`
//! or `winmm` is among them, which is most of what an NSIS setup stub imports.
//! So a `%TEMP%\version.dll` written by exactly the medium-integrity same-user
//! process the section above assumes — `icacls %TEMP%` grants the interactive
//! user `(I)(OI)(CI)(F)` — is loaded into the **elevated** installer without
//! ever touching the file the pin covers. The hash cannot see it: it verifies
//! the exe, and the attack arrives as a sibling.
//!
//! So the download lands in a per-run directory this process creates inside
//! `%TEMP%` and immediately clamps to Administrators and SYSTEM with a
//! protected DACL ([`STAGING_DACL`]) — the same Win32 the `WinDivert` cleanup
//! in [`crate::migrate`] exists to *undo*, used here where it belongs: on a
//! directory whose only content is an executable an administrator token is
//! about to run, and never on the one holding the player's logs.
//!
//! The other half of that finding does not survive contact and is written down
//! so it is not tried again: a parent cannot switch the image directory off for
//! its child. `CreateProcess` takes no search-order flag,
//! `SetDefaultDllDirectories` only ever changes the process that calls it, and
//! `CWDIllegalInDllSearch` is a machine-wide setting rather than something a
//! launcher hands over. The one lever that is real is the child's *current*
//! directory, which is inherited and otherwise points at wherever the player
//! started this app from; [`Fetcher::run`] aims it at the clamped directory too.
//! That covers the current-directory slot in the search order and nothing else.
//! Staging out of `%TEMP%` is what covers the image-directory slot, and there is
//! no substitute for it.
//!
//! What none of this reaches is what the installer does once it is running:
//! Npcap's NSIS stub extracts its own plugins into its own `%TEMP%\ns*.tmp` and
//! loads them from there, and no choice available to this module changes that.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use tracing::{info, warn};

use crate::npcap::{INSTALLER_BYTES, INSTALLER_SHA256, INSTALLER_URL, TEMP_INSTALLER_NAME};

/// Ceiling on the download, in seconds, handed to `curl`. Generous: the measured
/// time is under a second, and a player on hotel Wi-Fi is not a failure.
const FETCH_TIMEOUT_SECS: u32 = 120;

/// Name prefix of the directory the verified installer is staged in.
///
/// Deliberately not [`crate::APP_DIR`]: `%TEMP%\arkyve-refresh-shop\logs` is
/// this crate's fallback log directory (`crate::log_dirs_from`), and
/// [`STAGING_DACL`] applied there would take the log away from every unelevated
/// run — which is the exact failure [`crate::migrate`] ships to undo on machines
/// that already suffered it. The staging directory has to be somewhere nothing
/// else of ours lives, and this prefix is what [`sweep_old_staging`] recognises.
const STAGING_PREFIX: &str = "arkyve-npcap-staging-";

/// The DACL the staging directory is clamped to, in SDDL.
///
/// `D:P` — **protected**, so none of `%TEMP%`'s own ACEs are inherited into it.
/// That is the whole point: measured with `icacls %TEMP%`, the interactive user
/// is granted `(I)(OI)(CI)(F)`, and the interactive user is who the module
/// header's attacker is. `(A;OICI;FA;;;BA)` and `(A;OICI;FA;;;SY)` grant full
/// access to `BUILTIN\Administrators` and `LocalSystem`, inherited by whatever
/// is created inside, and to nobody else. This process holds an administrator
/// token (`build.rs` manifests it) so it is on the inside of that list, and so
/// is the installer it launches; the attacker is not.
const STAGING_DACL: &str = "D:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)";

/// What the banner shows while this runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Progress {
    /// Nothing started, or the last attempt was abandoned.
    #[default]
    Idle,
    /// `curl` is running.
    Fetching,
    /// Downloaded; hashing before anything is executed.
    Checking,
    /// Handed to the shell. Npcap's own installer owns the screen now.
    Launched,
    /// Launched, and then the one-click restart the banner offers did not
    /// happen. The string is player-facing and says why.
    ///
    /// Not [`Progress::Failed`], because the two want opposite remedies. The
    /// download succeeded here; the file is still on disk and still verifies,
    /// so `Failed`'s remedy — start the fetch again — takes
    /// [`Fetcher::fetch_and_check`]'s reuse fast path and puts a *second* Npcap
    /// setup window on screen, while the restart that actually failed is
    /// overwritten by `Launched` and never reported. What this state owes the
    /// player is the restart, again.
    RestartFailed(String),
    /// Gave up before anything was launched. The string is player-facing and
    /// names the step that failed.
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

    /// Current step, for the button's label.
    ///
    /// Poison-tolerant like the rest of this crate's shared state ([`crate::sync`]):
    /// a panicked worker must not take the window's error banner down with it.
    /// The cell holds one `Progress`, replaced whole, so there is no half-written
    /// state for a reader to catch.
    #[must_use]
    pub fn progress(&self) -> Progress {
        crate::sync::lock_ignoring_poison(&self.progress).clone()
    }

    /// Records a failed relaunch — the one failure raised by the window rather
    /// than by the worker, and one that has nowhere else to be seen.
    ///
    /// Lands in [`Progress::RestartFailed`], not `Failed`: see that variant for
    /// why a restart that did not happen must not be offered a download's
    /// remedy.
    pub fn restart_failed(&self, reason: String) {
        warn!(reason = %reason, "the relaunch failed at the window");
        self.set(Progress::RestartFailed(reason));
    }

    fn set(&self, next: Progress) {
        *crate::sync::lock_ignoring_poison(&self.progress) = next;
    }

    /// Starts fetch → check → launch on a worker thread, unless one is already
    /// in flight or an installer has already been launched.
    ///
    /// Off the UI thread on purpose: the window repaints at 4 Hz and a blocking
    /// download would freeze it for as long as the network takes, which on the
    /// failure path this feature exists for is exactly when it would hurt.
    ///
    /// Refused after a launch, because this whole call ends in a `spawn` and
    /// [`fetch_and_check`](Self::fetch_and_check)'s reuse fast path makes
    /// reaching it cheap: Npcap's setup is already on the player's screen and a
    /// second copy of it is not a remedy for anything. The banner offers no such
    /// control, and the refusal here is what keeps that from being the only
    /// thing standing between a click and a second elevated installer.
    ///
    /// The test and the claim share one lock acquisition, which is the whole
    /// reason this is not `if !accepts_a_start(&self.progress())`. That spelling
    /// read the cell, dropped the lock, and left `Idle` in it until the worker
    /// thread got as far as its own first write — inside `fetch_and_check`, one
    /// thread spawn and a `remove_file` later. The window repaints at 4 Hz, so a
    /// second click one frame after the first found `Idle` and spawned a second
    /// worker, and on the reuse fast path that worker verifies and launches: two
    /// elevated Npcap installers, from the guard written to prevent exactly one
    /// of them.
    pub fn start(&self) {
        if !self.claim() {
            return;
        }
        let handle = self.clone();
        // Detached: nothing joins it, and its whole output is the progress cell
        // above. A failure to spawn is reported through the same cell.
        if let Err(err) = std::thread::Builder::new()
            .name("npcap-fetch".to_owned())
            .spawn(move || handle.run())
        {
            self.set(Progress::Failed(format!(
                "could not start the download: {err}"
            )));
        }
    }

    /// Takes the fetch if it is there to take, under one lock acquisition.
    ///
    /// A separate `fn` for the same reason [`accepts_a_start`] is one: `start`
    /// ends in a `spawn`, so a test that exercised it would perform a real
    /// download and a real elevated `CreateProcess`. This is the part with a
    /// rule in it, and it can be called as often as a test likes.
    ///
    /// `true` means the caller now owns the fetch and nobody else can take it.
    fn claim(&self) -> bool {
        let mut progress = crate::sync::lock_ignoring_poison(&self.progress);
        if !accepts_a_start(&progress) {
            return false;
        }
        // The claim, written before the lock is dropped. This is the whole
        // change: reading the cell and then writing it in two acquisitions left
        // `Idle` visible in between, and `fetch_and_check`'s own
        // `set(Progress::Fetching)` — the first write there used to be — was a
        // thread spawn and a `remove_file` away.
        *progress = Progress::Fetching;
        true
    }

    fn run(self) {
        // Before the download, not after it: this is what decides *where* the
        // bytes land, and a file written into `%TEMP%` and then moved would have
        // spent the interval beside anything a same-user process cares to plant.
        if let Err(reason) = staging_ready() {
            warn!(reason = %reason, "the Npcap installer was not obtained");
            self.set(Progress::Failed(reason));
            return;
        }
        let target = installer_path();
        match self.fetch_and_check(&target) {
            Ok(verified) => {
                // `current_dir`, not the inherited one: the child's current
                // directory is a slot in its DLL search order, and this process
                // inherited its own from whatever started the app — a Desktop, a
                // download folder, the game's directory. The clamped staging
                // directory is the one place on this path nothing can be planted
                // in. It does not touch the *image* directory slot; see the
                // module header for why nothing can.
                let spawned = Command::new(&target).current_dir(staging_dir()).spawn();
                // Only now: until `CreateProcess` has returned, this handle is
                // the whole reason the path still holds the hashed bytes. See
                // the module header.
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

    /// Downloads unless a verified copy is already there, then verifies, and
    /// returns the handle the verification was made through.
    ///
    /// Re-clicking after a failed launch therefore costs nothing, and a partial
    /// file from an interrupted run is replaced rather than trusted. The reused
    /// copy goes through the same locked handle as a fresh one, so the fast path
    /// is not the weak path.
    ///
    /// "Already there" now means *within this run*: the staging directory is
    /// created per process and an earlier run's is swept, so a restarted app
    /// downloads again. That is a measured second against a directory whose
    /// contents this process cannot vouch for — see [`staging_dir`].
    fn fetch_and_check(&self, target: &Path) -> Result<File, String> {
        if let Ok(existing) = open_locked(target) {
            if verify(&existing).is_ok() {
                self.set(Progress::Checking);
                return Ok(existing);
            }
            // Explicitly, not by falling out of scope: this handle denies
            // `FILE_SHARE_DELETE`, and the very next thing below is a delete of
            // this path.
            drop(existing);
        }

        // Before the download, not after it: whatever is at the path is not the
        // installer — `verify` just refused it, or there was nothing to open —
        // and a download that then fails must not leave it sitting in the temp
        // directory for someone to run by hand. It is also what lets the write
        // below use `create_new`.
        let _ = std::fs::remove_file(target);

        // No `set(Fetching)` here: `start` claimed the state before this thread
        // existed, which is what makes the claim a claim.
        let bytes = download()?;

        self.set(Progress::Checking);
        write_new(target, &bytes)?;

        // Re-read from disk rather than hashing `bytes`: the claim this
        // function has to support is about the file the caller is going to
        // execute, and the only way to make it is to hash through the handle
        // that will still be open when it does.
        let file = open_locked(target)
            .map_err(|err| format!("the downloaded installer could not be re-opened: {err}"))?;
        match verify(&file) {
            Ok(()) => Ok(file),
            Err(reason) => {
                // Before the delete, for the reason given on the fast path.
                drop(file);
                // A file that fails the check is deleted rather than left for
                // someone to run by hand out of the temp directory.
                let _ = std::fs::remove_file(target);
                Err(reason)
            }
        }
    }
}

/// Whether [`Fetcher::start`] may run from this state.
///
/// A predicate rather than a `matches!` inside `start`, so the rule can be
/// asserted without spawning the worker — a test that let a refusal through
/// would run the real thing: a download and an elevated `CreateProcess`.
fn accepts_a_start(progress: &Progress) -> bool {
    match progress {
        Progress::Idle | Progress::Failed(_) => true,
        // In flight: a second worker would race the first over one path.
        Progress::Fetching | Progress::Checking => false,
        // Already launched: see [`Fetcher::start`].
        Progress::Launched | Progress::RestartFailed(_) => false,
    }
}

/// Fetches the pinned URL into memory.
///
/// `curl.exe` rather than an HTTP client: it ships with Windows 10 1803 and
/// later, so this adds no dependency to a crate that has refused several, and it
/// uses the machine's own proxy and TLS configuration — which a hand-rolled
/// client would have to be taught.
///
/// To stdout rather than `--output`, so that the one process that creates the
/// file is this one; `--max-filesize` keeps that from meaning an unbounded
/// response body in a `Vec`.
///
/// The directory comes from [`crate::system32::directory`] and not from
/// `%SystemRoot%`, which is the same correction the `wpcap.dll` candidates
/// already carry and it belongs here more than there: this process holds an
/// administrator token, and the value picks *the executable it runs*. An
/// attacker who can set that variable for a launcher and plant a
/// `System32\curl.exe` under the path it names gets arbitrary code run elevated
/// — by exactly the medium-integrity same-user process the module header's
/// swap-the-file attack assumes, and one step earlier than the hash pin looks.
/// The pin cannot help: it hashes what curl *printed*, long after curl ran.
fn download() -> Result<Vec<u8>, String> {
    let curl = curl_path();
    if !curl.is_file() {
        return Err("this Windows has no curl.exe to download with".to_owned());
    }
    let out = Command::new(curl)
        // `--disable` first, and it only works first: it tells curl not to read
        // its default config file. On Windows that file is `%APPDATA%\_curlrc`
        // — a path any medium-integrity process of the same user can create,
        // and a config file may set *any* option, `--output` included. Without
        // this flag the same swap-the-file attacker the paragraph above
        // describes does not need to plant a `curl.exe` at all: they drop a
        // `_curlrc` naming an output path and a source, the player clicks
        // Download once, and this process's administrator token writes
        // attacker-chosen bytes wherever they asked. The hash pin cannot see
        // it for the same reason it cannot see a planted `curl.exe`: it hashes
        // what curl printed, long after curl has already written the file.
        .arg("--disable")
        .args(["--fail", "--silent", "--show-error", "--location"])
        .args(["--max-time", &FETCH_TIMEOUT_SECS.to_string()])
        .args(["--max-filesize", &INSTALLER_BYTES.to_string()])
        .arg(INSTALLER_URL)
        .output()
        .map_err(|err| format!("the download could not be started: {err}"))?;
    if !out.status.success() {
        // `--show-error`'s explanation used to reach the console this build does
        // not have; now that stdout is the payload, stderr is captured, and the
        // log is the one place it can be read. The banner keeps the short
        // sentence — a TLS or proxy diagnostic is for whoever opens `logs\*.log`.
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

/// Writes `bytes` to a file this call creates, or fails.
///
/// `create_new`: if anything is already at `path` — a leftover, a symlink aimed
/// somewhere sensitive, a file an attacker re-planted in the moment between the
/// delete above and this call — the open fails and nothing is written. An
/// elevated `curl --output` had no such refusal, which is why the download no
/// longer goes through one.
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

/// Read handle that denies every access another process would need to change
/// what is at this path. See the module header for what rests on it.
///
/// `FILE_SHARE_READ` is the one flag kept: without it `CreateProcess` could not
/// map the image, and with it nobody can do anything but read.
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

/// The fetcher is only reachable from the Windows capture path; these two exist
/// so the module compiles on a dev machine. Neither carries the sharing
/// guarantee, and neither needs to — nothing calls them.
#[cfg(not(windows))]
fn open_locked(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

#[cfg(not(windows))]
fn create_new_locked(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// The `curl.exe` [`download`] will run, by absolute path.
///
/// Two arms for the same reason [`open_locked`] has two: the fetcher is only
/// reachable from the Windows capture path, and this file has to compile on a
/// dev machine. The non-Windows arm names a path that will not exist, which is
/// the branch `download` already handles.
#[cfg(windows)]
fn curl_path() -> PathBuf {
    crate::system32::directory().join("curl.exe")
}

#[cfg(not(windows))]
fn curl_path() -> PathBuf {
    PathBuf::from("curl.exe")
}

/// Where the download lands. The name is version-stamped and lives with the
/// version — see [`TEMP_INSTALLER_NAME`]; the directory is this module's, and
/// the module header is why.
fn installer_path() -> PathBuf {
    staging_dir().join(TEMP_INSTALLER_NAME)
}

/// The directory the verified installer is staged in, chosen once per process.
///
/// Still under `%TEMP%`: that is the right home for something this app launches
/// and then cannot clean up after itself, and it is the only user-scoped
/// location that is not also holding logs or config. Never *at* `%TEMP%`, for
/// the reason in the module header.
///
/// The name carries the process id and the clock, and neither is load-bearing —
/// nothing here trusts an attacker's ignorance of a path. It buys two things a
/// fixed name does not. Two runs of the app cannot collide over one directory,
/// which matters because [`prepare_staging`] insists on creating the directory
/// rather than adopting one. And a name that could be worked out ahead of time
/// could be *occupied* ahead of time, which would turn that same insistence into
/// a Download button an attacker can hold shut for as long as they like.
///
/// Pure: it computes a path and touches no disk, so a test can assert what it
/// chose without an elevated process creating anything.
fn staging_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        std::env::temp_dir().join(format!("{STAGING_PREFIX}{}-{stamp:x}", std::process::id()))
    })
}

/// Creates and clamps the staging directory, once per process.
///
/// Once, and the answer — failure included — is kept: [`staging_dir`] names one
/// path per process, so a second attempt would find the directory this one
/// created and [`prepare_staging`]'s `create_dir` would refuse it. Caching the
/// error is also what keeps a player clicking Download on a machine where the
/// DACL cannot be written from re-running the whole dance each time.
fn staging_ready() -> Result<(), String> {
    static PREPARED: OnceLock<Result<(), String>> = OnceLock::new();
    PREPARED
        .get_or_init(|| prepare_staging(staging_dir()))
        .clone()
}

/// Sweeps what earlier runs left, creates `dir`, clamps it, and proves it empty.
///
/// `create_dir` and not `create_dir_all`: anything already at this path was put
/// there by someone else — [`staging_dir`] picks a fresh name per run — and a
/// refusal is the only safe reading of that. It is also why the sweep runs
/// first and cannot be folded into the create.
///
/// The emptiness check is the second half of the clamp, not a formality. The
/// directory is born with `%TEMP%`'s inherited ACEs on it and only loses them
/// when [`lock_down`] returns, so there is a window — microseconds, and it
/// requires a process already watching the tree — in which a `version.dll` could
/// be dropped in. A file in a directory that was created empty a moment ago is
/// that, and the run is abandoned.
///
/// **What this does not close, stated rather than glossed:** in the same window
/// the directory could be removed and re-created by the attacker, who would then
/// own it, and an owner can rewrite the DACL this function just applied. The
/// emptiness check does not see that — the swapped directory is empty too. The
/// fix is to create the directory with its DACL already on it, atomically, which
/// means `CreateDirectoryW` with a `SECURITY_ATTRIBUTES`; there is no `std` API
/// that passes one, and the `windows-sys` feature that exposes the call —
/// `Win32_Storage_FileSystem` — is not enabled in `Cargo.toml`. Alternatively
/// the swap is *detectable*: an elevated process's objects are owned by
/// `BUILTIN\Administrators` (measured on the dev machine, and the same
/// measurement `crate::migrate`'s header records), a filtered token cannot set
/// that owner, so reading the owner back would tell the two apart.
///
/// What the window can **no longer** do is pass the clamp off as done. Replacing
/// the path with a junction used to be the quiet version of this swap: the DACL
/// landed on the reparse point, this function returned `Ok`, and the installer
/// was then written and launched inside the attacker's directory with the whole
/// staging story intact on paper. [`lock_down`] refuses a reparse point now, so
/// that swap ends the download instead of hollowing it out. What is left is the
/// loud version — the attacker owns a real directory and rewrites its DACL —
/// which costs a download this app abandons.
fn prepare_staging(dir: &Path) -> Result<(), String> {
    sweep_old_staging(dir);

    std::fs::create_dir(dir).map_err(|err| {
        format!(
            "the installer's download directory could not be created ({err}); \
             nothing was downloaded"
        )
    })?;

    if let Err(err) = lock_down(dir, STAGING_DACL) {
        let _ = std::fs::remove_dir(dir);
        return Err(format!(
            "the installer's download directory could not be secured ({err}); \
             nothing was downloaded"
        ));
    }

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

/// Deletes the staging directories of runs that are over.
///
/// Best-effort, and it has to exist: [`STAGING_DACL`] puts these out of reach of
/// the unelevated player, so nothing else on the machine will ever remove them —
/// not the player, not Disk Cleanup. An elevated run leaving an undeletable
/// megabyte in `%TEMP%` on every download is the same kind of litter
/// [`crate::migrate`] was written to clear up, and the cheapest place to not
/// create it is here.
///
/// "Out of reach" is measured, not assumed, and it was the opposite of what was
/// expected: `%TEMP%` grants the interactive user `FILE_DELETE_CHILD`, which is
/// normally enough to delete a child whose own DACL grants nothing, and it is
/// still not enough here — `a_staged_directory_stops_inheriting_the_users_full_control`
/// gets `ERROR_ACCESS_DENIED` from `remove_dir` on an *empty* clamped directory
/// it owns. Being the owner buys `READ_CONTROL` and `WRITE_DAC` and neither of
/// those is `DELETE`. So the sweep genuinely needs the administrator token, and
/// the test has to hand the rights back before it can tidy up.
///
/// A directory belonging to a run that is still going is protected by nothing
/// here — `remove_dir_all` simply fails, because the installer inside is held by
/// [`open_locked`] or mapped by the process it was launched into, and neither
/// grants `FILE_SHARE_DELETE`.
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
        // Both spellings, because the name may have been taken by a file rather
        // than a directory — planted, or left by a crash — and either way this
        // run wants it gone before `create_dir` looks.
        if std::fs::remove_dir_all(&path).is_err() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Puts `sddl`'s DACL on `dir` as a *protected* one, so nothing is inherited
/// from the parent. Called with [`STAGING_DACL`], which is what makes `%TEMP%`'s
/// full-control-for-the-user ACE stop at the staging directory's door.
///
/// The descriptor is parsed from SDDL rather than assembled from `InitializeAcl`
/// plus two `CreateWellKnownSid` calls plus two `AddAccessAllowedAceEx` calls:
/// that is the same two ACEs in five times the `unsafe`, and the string is the
/// readable form of the thing being claimed.
///
/// `PROTECTED_DACL_SECURITY_INFORMATION` is passed alongside
/// `DACL_SECURITY_INFORMATION` rather than left to the `P` in the string, and it
/// is not redundant: `SetSecurityInfo` takes an `ACL`, not a descriptor, so
/// the control word the `P` set is never handed to it. The information argument
/// is the only place the protect bit can come from.
///
/// Addressed through a handle from `crate::dirhandle::open_directory_itself`,
/// which refuses a reparse point outright. `%TEMP%` is writable by any same-user
/// medium-integrity process, so between [`prepare_staging`]'s `create_dir` and
/// this call the directory can be removed and replaced by a junction.
///
/// What that used to cost is worth stating precisely, because the obvious guess
/// is wrong and was measured to be wrong: `SetNamedSecurityInfoW` does *not*
/// follow a junction, so the old name-based call did not put [`STAGING_DACL`] on
/// the target — it put it on the reparse point itself and returned success. The
/// damage was quieter than that and no less total: the protected DACL ends up on
/// an object no file is ever written to, while [`installer_path`] resolves
/// *through* the junction, so the installer is downloaded into, verified in and
/// launched from a directory the attacker still controls. Every guarantee in
/// this module's staging story would hold on paper and none of it on disk.
///
/// Refusing the open is what turns that into a failed download. The gate is
/// shared with `crate::migrate` rather than copied because it is the same
/// argument — act on the object you checked, not on the name you checked.
///
/// `sddl` is a parameter and not the constant inlined for one reason, recorded
/// so it is not "simplified" away: a clamped directory cannot be deleted by an
/// unelevated owner (see [`sweep_old_staging`]), so the test that proves the
/// clamp took has no other way to clean up after itself than to call this again
/// with a DACL that grants the rights back. There is one production caller and
/// it passes [`STAGING_DACL`].
#[cfg(windows)]
fn lock_down(dir: &Path, sddl: &str) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
        SetSecurityInfo,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };

    // Before anything is parsed: if the name no longer refers to a plain
    // directory, there is nothing here worth clamping and the error says which
    // of the two it was.
    // Before anything is parsed: if the name no longer refers to a plain
    // directory, there is nothing here worth clamping and the error says which
    // of the two it was.
    let handle = crate::dirhandle::open_directory_itself(dir)?;

    let sddl: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();

    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `sddl` is a valid null-terminated UTF-16 string owned by this frame
    // and alive across the call. `descriptor` is a live stack slot, written only
    // on success with a single `LocalAlloc` block that owns the ACL as well. The
    // size out-parameter is documented as optional and is not wanted. Failure
    // allocates nothing, which is why the early return below frees nothing.
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

    let mut dacl: *mut ACL = ptr::null_mut();
    let mut present = 0;
    let mut defaulted = 0;
    // SAFETY: `descriptor` is the block the call above allocated and reported
    // success for, and nothing has freed it. The three out-parameters are stack
    // slots that outlive the call. `dacl` is written to point *into* that block,
    // so every read of it below happens before the one `LocalFree`.
    let read =
        unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted) };
    // GetLastError is per-thread and the very next Win32 call clobbers it: read
    // it here, not after the `SetSecurityInfo` below.
    let read_err = std::io::Error::last_os_error();

    let outcome = if read == 0 {
        Err(read_err)
    } else if present == 0 || dacl.is_null() {
        // Never passed on. `SetSecurityInfo` reads a null `pDacl` as
        // "install a *null* DACL", which grants full access to everyone — the
        // same trap `migrate::reset_dacl_to_inherited`'s doc comment is written
        // around, and here it would hand the attacker the very directory this
        // call exists to take away from them. `STAGING_DACL` names two ACEs so
        // this is unreachable; it is checked because the cost of being wrong is
        // the whole fix, silently.
        Err(std::io::Error::other(
            "the staging descriptor parsed with no DACL in it",
        ))
    } else {
        // SAFETY: `handle` is an open directory handle carrying `WRITE_DAC`,
        // kept alive across the call by the borrow, and validated as a plain
        // directory rather than a reparse point. `dacl` is the non-null ACL
        // inside the descriptor above, which the call copies rather than
        // retains. The owner, group and SACL pointers are null, which is "do not
        // change" for the information bits not requested. Failure mode: a
        // `WIN32_ERROR` return, handled below.
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

    // SAFETY: the only `LocalFree` in this function, on the only path that
    // reaches it, freeing the block the parse allocated exactly once. `dacl`
    // pointed into it and is not read again.
    unsafe { LocalFree(descriptor.cast()) };
    outcome
}

/// The fetcher is only reachable from the Windows capture path; this exists so
/// the module compiles on a dev machine. It carries no guarantee, and needs
/// none — nothing calls it, in the same way [`open_locked`]'s non-Windows arm
/// carries no sharing guarantee.
#[cfg(not(windows))]
fn lock_down(_dir: &Path, _sddl: &str) -> std::io::Result<()> {
    Ok(())
}

/// Starts a second copy of this executable, for the caller to follow with a
/// window close.
///
/// # Why a relaunch and not a re-probe
///
/// Nothing technical forces it. Measured: Windows does **not** cache a failed
/// `LoadLibrary` — a path that answered `ERROR_MOD_NOT_FOUND` (126) loads
/// successfully in the same process once the file appears — so re-opening the
/// tap in place would work.
///
/// What forces it is where the failure lands. `build_source` runs inside
/// `Session::run`, and its `?` ends the session; the window survives holding
/// [`SessionHandles`](crate::app::SessionHandles) whose command receiver went
/// with it. Reviving that means `Option<CaptureWorker>` inside `SessionWorkers`,
/// six values kept alive for a second attempt, and a changed teardown path —
/// the one whose comments record that an error there leaves an orphaned live
/// capture session on every launch/close cycle. Against that, a relaunch costs
/// the player one click and loses nothing: at this point the session is dead,
/// the journal holds two lines, and nothing has been typed.
///
/// No second UAC prompt: the exe is manifested `requireAdministrator` and this
/// process already holds the token, so the child inherits it.
///
/// # Errors
///
/// If the running executable's path cannot be read, or the child cannot be
/// spawned. The window keeps its banner in either case — a failed relaunch must
/// not look like a successful one.
pub fn relaunch() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    // The child is deliberately not held: it outlives this process by design,
    // and waiting on it here would deadlock the window that is about to close.
    Command::new(&exe).spawn()?;
    info!(path = %exe.display(), "relaunching after the Npcap install");
    Ok(())
}

/// `Ok(())` only for the exact pinned build.
///
/// Takes the open handle, not the path: re-opening by name here is precisely
/// the gap this module's header is about. `mut file: &File` because `&File`
/// is itself a `Read`, so the caller keeps ownership of the handle it will hold
/// across the spawn.
fn verify(mut file: &File) -> Result<(), String> {
    let size = file
        .metadata()
        .map_err(|err| format!("the downloaded file could not be read: {err}"))?
        .len();
    // Checked before reading: a captive portal or an error page redirected into
    // this file should not be loaded into memory just to be hashed.
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

/// SHA-256 through CNG's one-shot entry point.
///
/// `BCryptHash` with the `BCRYPT_SHA256_ALG_HANDLE` pseudo-handle needs no
/// provider to open and none to close, so the whole digest is a single call and
/// a single `unsafe` block — which is why it is used here rather than the
/// open/create/hash/finish/destroy sequence the same header offers.
#[cfg(windows)]
fn sha256(bytes: &[u8]) -> [u8; 32] {
    use windows_sys::Win32::Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptHash};

    let mut out = [0_u8; 32];
    // SAFETY: the algorithm handle is the documented pseudo-handle constant, the
    // secret is empty (this is a plain hash, not an HMAC), and both the input and
    // output pointers come from slices whose lengths are passed alongside them.
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
        // Leaves `out` zeroed, which cannot equal the pin, so the caller rejects
        // the file. A hash that failed must never read as a hash that matched.
        warn!(status, "BCryptHash failed; treating the file as unverified");
        return [0_u8; 32];
    }
    out
}

#[cfg(not(windows))]
fn sha256(_bytes: &[u8]) -> [u8; 32] {
    // The fetcher is only reachable from the Windows capture path; this exists so
    // the module compiles on a dev machine.
    [0_u8; 32]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_fetcher_is_idle() {
        assert_eq!(Fetcher::new().progress(), Progress::Idle);
    }

    // The pin's own shape is asserted where the pin now lives, in `crate::npcap`
    // — it guards `sha256`'s zeroed failure return, which is this module's, but
    // the constant it guards is not.

    /// The two-clicks case, which is what the banner actually produces: the
    /// window repaints at 4 Hz, and the second frame's click used to find `Idle`
    /// because the only writer of `Fetching` was the worker thread, inside
    /// `fetch_and_check`. Both workers then race one path, and on the reuse fast
    /// path the second one verifies and launches — a second elevated Npcap
    /// installer, out of the guard written to prevent it.
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

    /// The same rule under real contention rather than in sequence. Not because
    /// the UI can produce it — `start` is only ever called from the one egui
    /// thread — but because `Fetcher` is `Clone` and its whole purpose is to be
    /// held by two threads at once, so "exactly one winner" is a property of the
    /// type and not of its current caller.
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

    /// The one thing about this path that matters: it is absolute, it is under
    /// the directory Win32 named, and it is the real `curl.exe`. The property
    /// this replaced — that no `%SystemRoot%` read chooses it — cannot be
    /// asserted from inside the process without setting that variable, which
    /// this crate never does; see [`crate::system32`]'s test for the same note.
    /// What holds it is that the environment read is gone.
    #[cfg(windows)]
    #[test]
    fn the_downloader_runs_the_system_curl_by_absolute_path() {
        let curl = curl_path();
        assert!(curl.is_absolute(), "{curl:?}");
        assert!(
            curl.starts_with(crate::system32::directory()),
            "{curl:?} — an elevated process must not run a curl.exe chosen by its launcher"
        );
        // Not `is_file`: a Windows old enough to lack it is a supported machine
        // and `download` reports it. What must hold is where we looked.
        assert_eq!(curl.file_name(), Some(std::ffi::OsStr::new("curl.exe")));
    }

    #[cfg(windows)]
    #[test]
    fn sha256_matches_the_published_vectors() {
        // NIST's two standard test vectors, so a wrong `BCryptHash` call is
        // caught here rather than by a mysteriously rejected download.
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
        // The whole pin rests on this: between `verify` and `CreateProcess`
        // nobody may write, delete or replace the file. Asserted against the OS
        // rather than trusted, because a share mode is invisible at the call
        // site that depends on it.
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

        // And the denial is only for as long as the handle lives.
        drop(held);
        std::fs::remove_file(&path).expect("the path is free again once dropped");
    }

    #[test]
    fn the_installer_is_never_written_over_something_already_there() {
        // `create_new` is what stands between an elevated write and a symlink
        // planted at the predictable temp path.
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

    /// The finding the staging directory exists for. `open_locked` pins the
    /// installer's bytes and nothing pins its neighbours, while the image's own
    /// directory is first in the child's DLL search order — so an installer run
    /// straight out of `%TEMP%` loads whatever `version.dll` a same-user process
    /// left lying there, into an administrator token, without disturbing the
    /// hash at all.
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

    /// Asserted against the OS rather than against the SDDL string, for the same
    /// reason the share-mode test above is: a DACL is invisible at the call site
    /// that depends on it. Elevation-independent — the owner of a directory
    /// always keeps `READ_CONTROL`, so the read-back works whether `cargo test`
    /// runs filtered (a dev machine) or elevated (the CI runner, see the
    /// `[[bin]]` note in `Cargo.toml`).
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

        // The clamp is real enough that this test cannot tidy up after itself
        // without undoing it: measured, `remove_dir` on the empty directory it
        // just created answers `ERROR_ACCESS_DENIED`, because `%TEMP%`'s
        // `FILE_DELETE_CHILD` does not stand in for a `DELETE` the new DACL does
        // not grant.
        //
        // Undone by name rather than through `lock_down`, and the reason is
        // worth recording because it is surprising: measured with a bare
        // `CreateFileW` on a clamped directory, an unelevated owner is refused
        // `READ_CONTROL`, `WRITE_DAC` and both together alike — the implicit
        // owner rights do not survive `STAGING_DACL`. So `lock_down`'s handle
        // gate cannot re-open what it has just clamped, while the name-based
        // API still can. That costs production nothing — `prepare_staging`
        // clamps a directory it created moments earlier and never re-clamps —
        // and it is only this fixture that needs the other spelling.
        hand_back_full_control(&dir).expect("hand the fixture back");
        std::fs::remove_dir(&dir).expect("the fixture is deletable once un-clamped");
    }

    /// The staging directory lives in `%TEMP%`, which any same-user
    /// medium-integrity process can write. If it is removed and re-created as a
    /// junction between [`prepare_staging`]'s `create_dir` and its clamp, the
    /// old name-based DACL write clamped the *reparse point* and reported
    /// success — measured, not assumed: `SetNamedSecurityInfoW` does not follow
    /// a junction, so the target keeps its own DACL. That is the dangerous
    /// outcome rather than the harmless one, because files resolve the other
    /// way: [`installer_path`] goes *through* the junction, so the protection
    /// sits on an object nothing is written to while the installer lands in the
    /// attacker's directory.
    ///
    /// Hence the assertion below is about the link, not its target. A test
    /// asserting the target was untouched would pass against the unfixed code
    /// too, and prove nothing.
    ///
    /// Deliberately not named with `STAGING_PREFIX`: [`sweep_old_staging`] would
    /// be entitled to delete it out from under a concurrently running test.
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

        // A directory junction needs no privilege at all, unlike a symlink —
        // which is exactly why this is reachable by the attacker in the first
        // place, and why this test can build it on an unelevated machine.
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

        // Checked before the error, so that a regression reports the thing that
        // matters rather than its symptom. Against the unfixed code this reads
        // `SE_DACL_PROTECTED`: the clamp landed on the junction and `lock_down`
        // answered `Ok` — a staging directory that reports itself secured while
        // every file written to it goes somewhere else.
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
        // otherwise have to decide what to do about it.
        std::fs::remove_dir(&link).expect("unlink the junction");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Puts an everyone-full-control DACL back on `dir`, by name.
    ///
    /// Fixture cleanup only, and it is the one thing in this module that is
    /// *deliberately* name-based: see the comment at its call site for why the
    /// handle gate cannot be used here, and note that nothing it does is a
    /// security operation — it hands rights away rather than taking them.
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
        // the call; `descriptor` is a live stack slot written only on success
        // with one `LocalAlloc` block. The size out-parameter is optional.
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
        // the three out-parameters are stack slots outliving the call.
        let read = unsafe {
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
        };
        assert_ne!(read, 0, "read the parsed DACL back");
        assert!(
            present != 0 && !dacl.is_null(),
            "a null DACL grants everyone"
        );

        // SAFETY: `wide` is a valid null-terminated UTF-16 path alive for the
        // call, `dacl` points into the descriptor above and is copied rather
        // than retained, and the owner/group/SACL pointers are null meaning
        // "do not change".
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

    /// Reads back `(control word, ACE count)` for a path's DACL.
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
        // call. `dacl` and `descriptor` are live stack slots; on success the call
        // allocates one `LocalAlloc` block for the descriptor, with `dacl`
        // pointing into it. The owner, group and SACL out-parameters are optional
        // and passed null.
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
        // SAFETY: `descriptor` is the block the call above allocated and reported
        // success for; `control` and `revision` are stack slots outliving the call.
        let ok = unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) };
        assert_ne!(ok, 0, "reading the control word of {dir:?}");
        // SAFETY: `dacl` is the non-null ACL header inside that same live block.
        let aces = unsafe { (*dacl).AceCount };
        // SAFETY: the one `LocalFree`, freeing that block exactly once; neither
        // `dacl` nor `descriptor` is read afterwards.
        unsafe { LocalFree(descriptor.cast()) };

        (control, aces)
    }

    /// The other half of the staging directory: it is per-run, and a run cannot
    /// delete its own on the way out — the installer it launched is still mapped.
    /// Without this sweep an elevated app would leave an undeletable megabyte in
    /// `%TEMP%` on every download, which is the litter `crate::migrate` exists to
    /// clear up, freshly re-created.
    #[test]
    fn an_earlier_runs_staging_directory_is_swept_and_its_neighbours_are_not() {
        let parent = std::env::temp_dir().join(format!("arkyve-sweep-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir(&parent).expect("the scratch parent");

        let stale = parent.join(format!("{STAGING_PREFIX}older-run"));
        let mine = parent.join(format!("{STAGING_PREFIX}this-run"));
        // The name the sweep must never touch: `%TEMP%\arkyve-refresh-shop\logs`
        // is where this crate's logs go when `%LOCALAPPDATA%` is unavailable.
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
        // The relaunch is the one failure the worker cannot report; it must not
        // be able to leave the banner claiming success.
        let fetcher = Fetcher::new();
        fetcher.set(Progress::Launched);
        fetcher.restart_failed("could not restart: access denied".to_owned());
        assert!(matches!(fetcher.progress(), Progress::RestartFailed(reason)
            if reason.contains("could not restart")));
    }

    #[test]
    fn a_failed_restart_is_not_a_failed_download() {
        // The two are told apart here so the banner can offer each its own
        // remedy. Collapsed into one state they shared `Failed`'s, which is a
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
        // Both post-launch states, because `fetch_and_check`'s reuse fast path
        // means a start from either would reach the `spawn` with the file
        // already on disk and already verified — an installer window the player
        // did not ask for a second time.
        for state in [
            Progress::Launched,
            Progress::RestartFailed("could not restart: access denied".to_owned()),
        ] {
            assert!(
                !accepts_a_start(&state),
                "a start from {state:?} must be refused"
            );
        }
        // And the states that mean "nothing is running" still may.
        assert!(accepts_a_start(&Progress::Idle));
        assert!(accepts_a_start(&Progress::Failed("no network".to_owned())));
    }
}
