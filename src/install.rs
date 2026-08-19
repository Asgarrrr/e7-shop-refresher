//! Fetching the Npcap installer for a player who has none.
//!
//! The window's error banner offers a `Download` button; this is what it drives.
//! It fetches one pinned file, checks it byte for byte, and launches it. It does
//! not install anything itself and it cannot: Npcap's silent installer is the
//! paid OEM product (`docs/npcap-provisioning.md` quotes the licence), so the
//! player still meets Npcap's own setup window and its own licence. What this
//! removes is the trip to a browser, a download folder and back.
//!
//! # Why a pinned hash rather than a signature check
//!
//! An elevated process that downloads and runs an executable is the shape of the
//! thing this app is not, so the check is not optional. Authenticode verification
//! through `WinVerifyTrust` would answer "signed by Nmap Software LLC" — but the
//! URL already names one exact build, so the stronger and much smaller check is
//! the bytes themselves. A signature check accepts any file that vendor ever
//! signed; [`INSTALLER_SHA256`] accepts the one whose signature was verified by
//! hand (`CN=Nmap Software LLC`, DigiCert-issued, valid to 2027, timestamped)
//! and whose hash was then confirmed on a second, independent download.
//!
//! The usual objection to hash pinning is that it rots at every release. It does
//! — and so does the URL beside it, which names `npcap-1.88.exe`. They rot
//! together and are bumped together, which is what makes the pin honest rather
//! than a hostage to fortune.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use tracing::{info, warn};

/// The one build this module will run. Pinned with [`INSTALLER_SHA256`]; change
/// neither without the other.
///
/// Wireshark's build mirror rather than npcap.com, because npcap.com does not
/// answer: 6–9 s to first byte, and its own installer URL failed outright at 19 s
/// with the TLS handshake never completing. This one answers in 0.27 s.
const INSTALLER_URL: &str = "https://dev-libs.wireshark.org/windows/packages/Npcap/npcap-1.88.exe";

/// SHA-256 of `npcap-1.88.exe`, measured on two independent downloads:
/// `a2f4ec1e5ea353ff67efd24b2ebf081ba44532410fae8d5e146af0310aa4f56b`.
const INSTALLER_SHA256: [u8; 32] = [
    0xa2, 0xf4, 0xec, 0x1e, 0x5e, 0xa3, 0x53, 0xff, 0x67, 0xef, 0xd2, 0x4b, 0x2e, 0xbf, 0x08, 0x1b,
    0xa4, 0x45, 0x32, 0x41, 0x0f, 0xae, 0x8d, 0x5e, 0x14, 0x6a, 0xf0, 0x31, 0x0a, 0xa4, 0xf5, 0x6b,
];

/// Expected size, checked before the file is read into memory so a redirect to
/// an HTML error page cannot become a multi-megabyte allocation.
const INSTALLER_BYTES: u64 = 1_320_424;

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
    /// Gave up. The string is player-facing and names the step that failed.
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
    /// Poison-tolerant like the rest of this crate's shared state: a panicked
    /// worker must not take the window's error banner down with it.
    #[must_use]
    pub fn progress(&self) -> Progress {
        self.progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Records a failure raised by the window rather than by the worker — the
    /// relaunch is the only one, and it has nowhere else to be seen.
    pub fn fail(&self, reason: String) {
        warn!(reason = %reason, "the install flow failed at the window");
        self.set(Progress::Failed(reason));
    }

    fn set(&self, next: Progress) {
        *self
            .progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
    }

    /// Starts fetch → check → launch on a worker thread, unless one is already
    /// in flight.
    ///
    /// Off the UI thread on purpose: the window repaints at 4 Hz and a blocking
    /// download would freeze it for as long as the network takes, which on the
    /// failure path this feature exists for is exactly when it would hurt.
    pub fn start(&self) {
        if matches!(self.progress(), Progress::Fetching | Progress::Checking) {
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

    fn run(self) {
        let target = installer_path();
        match self.fetch_and_check(&target) {
            Ok(()) => match Command::new(&target).spawn() {
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
            },
            Err(reason) => {
                warn!(reason = %reason, "the Npcap installer was not obtained");
                self.set(Progress::Failed(reason));
            }
        }
    }

    /// Downloads unless a verified copy is already there, then verifies.
    ///
    /// Re-clicking after a failed launch therefore costs nothing, and a partial
    /// file from an interrupted run is replaced rather than trusted.
    fn fetch_and_check(&self, target: &Path) -> Result<(), String> {
        if target.is_file() && verify(target).is_ok() {
            self.set(Progress::Checking);
            return Ok(());
        }

        self.set(Progress::Fetching);
        // `curl.exe` rather than an HTTP client: it ships with Windows 10 1803
        // and later, so this adds no dependency to a crate that has refused
        // several, and it uses the machine's own proxy and TLS configuration —
        // which a hand-rolled client would have to be taught.
        let curl =
            PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
                .join("System32")
                .join("curl.exe");
        if !curl.is_file() {
            return Err("this Windows has no curl.exe to download with".to_owned());
        }
        let status = Command::new(curl)
            .args(["--fail", "--silent", "--show-error", "--location"])
            .args(["--max-time", &FETCH_TIMEOUT_SECS.to_string()])
            .arg("--output")
            .arg(target)
            .arg(INSTALLER_URL)
            .status()
            .map_err(|err| format!("the download could not be started: {err}"))?;
        if !status.success() {
            let _ = std::fs::remove_file(target);
            return Err(format!(
                "the download failed (curl exit {}). The address is in the log.",
                status.code().unwrap_or(-1)
            ));
        }

        self.set(Progress::Checking);
        verify(target).inspect_err(|_| {
            // A file that fails the check is deleted rather than left for
            // someone to run by hand out of the temp directory.
            let _ = std::fs::remove_file(target);
        })
    }
}

/// Where the download lands. One deterministic name, so a second attempt reuses
/// or replaces it instead of littering.
fn installer_path() -> PathBuf {
    std::env::temp_dir().join("arkyve-npcap-1.88.exe")
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
fn verify(path: &Path) -> Result<(), String> {
    let size = std::fs::metadata(path)
        .map_err(|err| format!("the downloaded file could not be read: {err}"))?
        .len();
    // Checked before reading: a captive portal or an error page redirected into
    // this file should not be loaded into memory just to be hashed.
    if size != INSTALLER_BYTES {
        return Err(format!(
            "the download is {size} bytes, not the expected {INSTALLER_BYTES} — it is not the installer"
        ));
    }
    let bytes = std::fs::read(path)
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

    #[test]
    fn the_pinned_hash_is_thirty_two_bytes_and_not_all_zero() {
        // A zeroed pin would match `sha256`'s own failure return, which is how a
        // failed hash could have read as a verified file.
        assert_eq!(INSTALLER_SHA256.len(), 32);
        assert!(INSTALLER_SHA256.iter().any(|b| *b != 0));
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
        let err = verify(&path).expect_err("a 16-byte file is not the installer");
        assert!(err.contains("not the expected"), "got: {err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_window_side_failure_lands_in_the_same_cell() {
        // The relaunch is the one failure the worker cannot report; it must not
        // be able to leave the banner claiming success.
        let fetcher = Fetcher::new();
        fetcher.set(Progress::Launched);
        fetcher.fail("could not restart: access denied".to_owned());
        assert!(matches!(fetcher.progress(), Progress::Failed(reason)
            if reason.contains("could not restart")));
    }

    #[test]
    fn starting_twice_does_not_launch_two_workers() {
        let fetcher = Fetcher::new();
        fetcher.set(Progress::Fetching);
        fetcher.start();
        assert_eq!(fetcher.progress(), Progress::Fetching);
    }
}
