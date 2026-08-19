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

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use tracing::{info, warn};

use crate::npcap::{INSTALLER_BYTES, INSTALLER_SHA256, INSTALLER_URL, TEMP_INSTALLER_NAME};

/// Ceiling on the download, in seconds, handed to `curl`. Generous: the measured
/// time is under a second, and a player on hotel Wi-Fi is not a failure.
const FETCH_TIMEOUT_SECS: u32 = 120;

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
        let target = installer_path();
        match self.fetch_and_check(&target) {
            Ok(verified) => {
                let spawned = Command::new(&target).spawn();
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
/// version — see [`TEMP_INSTALLER_NAME`].
fn installer_path() -> PathBuf {
    std::env::temp_dir().join(TEMP_INSTALLER_NAME)
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
