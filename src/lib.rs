//! Secret Shop relay.
//!
//! The capture side *observes* a copy of the game's traffic, reassembles it
//! into an ordered stream, and forwards the raw bytes to the analysis
//! server, which interprets them; the game's network traffic is never
//! altered. On Windows the tool also drives the Secret Shop itself via
//! click emulation (refresh and buy), steered by the decoded snapshots.
//!
//! ```text
//! Npcap tap ─▶ parse IP/TCP ─▶ TCP reassembly ─▶ gate ─▶ WebSocket ─▶ server
//!  (blocking)                  (ordered/dedup)                  ▲         │
//!                                                        snapshots ◀──────┘
//! ```
//!
//! The startup policy at the bottom of this file lives here rather than in
//! `main.rs` so tests can reach it: a binary crate cannot be imported.
//! `main.rs` keeps only the dispatch and the two `run_mode` arms.

// Not in Cargo.toml's `[lints]`: a `[lints]` table applies to every target,
// and the test harness uses `unwrap` in fixtures on purpose.
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::panic))]

pub mod actuator;
pub mod app;
pub mod capture;
pub mod config;
pub mod crash;
// Windows-only and ungated, like `wide` and `system32`: `migrate` (always
// compiled) and `install` (gui) both rewrite a DACL under an elevated token,
// neither can reach the other, and the gate that stops a junction redirecting
// that rewrite must not exist in two copies.
#[cfg(windows)]
pub mod dirhandle;
pub mod domain;
pub mod error;
pub mod journal;
// Only the window offers the Download button, so only the window needs it.
#[cfg(feature = "gui")]
pub mod install;
pub mod migrate;
// Ungated: `install` (gui) and `capture::pcap` (windows + pcap-backend) both
// name the pinned Npcap build and neither can reach the other.
pub mod npcap;
mod render;
pub mod stream;
pub mod sync;
// Windows-only and ungated, like `wide`: `install` (gui) and `capture::pcap`
// (windows + pcap-backend) both resolve a path under System32 and neither can
// reach the other.
#[cfg(windows)]
pub mod system32;
#[cfg(feature = "gui")]
pub mod ui;
pub mod uplink;
pub mod watch;
// Windows-only, and shared by two independently-gated subsystems: see the
// module header for why one copy each was not good enough.
#[cfg(windows)]
pub mod wide;

pub use config::Config;
pub use error::{Error, Result};

/// The one place the product name lives: window titles and the welcome
/// screen must never disagree.
pub const APP_NAME: &str = "Arkyve Refresh Shop";

/// Filesystem-safe app folder name, shared by the per-user data locations:
/// the config under `%APPDATA%` (roaming) and the crash log under
/// `%LOCALAPPDATA%` (local). One constant so the two never diverge.
pub const APP_DIR: &str = "arkyve-refresh-shop";

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// Location of the config file. The app owns this file (the GUI's Setup/Apply
/// writes it) and the player isn't expected to hand-edit it, so it lives out of
/// the way in per-user roaming app-data rather than beside the exe.
#[must_use]
pub fn config_path() -> PathBuf {
    #[cfg(windows)]
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    // `%APPDATA%` has no meaning on a dev machine (mac), so the working
    // directory is the whole policy there.
    #[cfg(not(windows))]
    let appdata: Option<PathBuf> = None;
    config_path_from(appdata.as_deref())
}

/// Pure half of [`config_path`], so the `%APPDATA%`-then-`./config.toml`
/// fallback is testable: a wrong branch means the app reads a config nobody
/// edits.
#[must_use]
pub fn config_path_from(appdata: Option<&Path>) -> PathBuf {
    match appdata {
        Some(appdata) => appdata.join(APP_DIR).join("config.toml"),
        None => PathBuf::from("config.toml"),
    }
}

/// On first run, writes the bundled example to the resolved path: it carries the
/// default hunt `[filter]`, which the relay requires to start.
///
/// Best-effort but never silent, because `Config::default`'s filter is
/// unrestricted and the relay refuses to hunt on one — a failed seed leaves the
/// console build naming a `config.toml` that was never written and the GUI build
/// on "Idle — define a filter first".
pub fn seed_config_if_missing(path: &Path) {
    if path.exists() {
        return;
    }
    const EXAMPLE: &str = include_str!("../config.example.toml");
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(
            dir = %parent.display(), error = ?err,
            "could not create the config directory; the seed below will fail too"
        );
    }
    if let Err(err) = std::fs::write(path, EXAMPLE) {
        tracing::warn!(
            path = %path.display(), error = ?err,
            "could not seed config.toml; starting on built-in defaults, which set no hunt filter"
        );
    }
}

/// Where the rotated log files live, most-preferred first: next to `crash.log`,
/// in per-user *local* app-data (logs are machine-local, they must not roam),
/// then the same leaf under the temp dir.
///
/// The second candidate is the whole point: `%LOCALAPPDATA%\<app>` *set but
/// unwritable* is a measured failure mode (the DACL [`migrate`] repairs, plus
/// `OneDrive` / antivirus / quota), and one candidate alone degrades to an inert
/// stdout there. Same ladder as [`crash`]'s.
#[must_use]
pub fn log_dirs() -> Vec<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    log_dirs_from(local.as_deref(), &std::env::temp_dir())
}

/// Pure ordering behind [`log_dirs`].
#[must_use]
pub fn log_dirs_from(local_appdata: Option<&Path>, temp: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(local) = local_appdata {
        dirs.push(local.join(APP_DIR).join("logs"));
    }
    dirs.push(temp.join(APP_DIR).join("logs"));
    dirs
}

/// What [`install_logging`] had to do to get a writer, so `main` can log it
/// *through the subscriber that call just installed*.
///
/// Deferred like [`migrate::Leftovers`]: everything worth saying happens before
/// there is anything to say it to. In the windowed build "fell back to stdout"
/// means "produced no output at all", so without this the failure conceals
/// itself.
pub struct LogSetup {
    /// Where the log went. `None` when there is no log file at all.
    destination: Option<PathBuf>,
    /// Candidates that refused, most-preferred first. The `InitError` wraps an
    /// `io::Error`, so its `Debug` is what separates "antivirus has it open"
    /// from "the DACL is wrong" from "the disk is full".
    refusals: Vec<(PathBuf, tracing_appender::rolling::InitError)>,
}

impl LogSetup {
    /// Emits what happened. Silent apart from one `debug!` in the normal case.
    pub fn report(&self) {
        for (dir, err) in &self.refusals {
            tracing::error!(
                dir = %dir.display(), error = ?err,
                "could not open a log file in this directory"
            );
        }
        match &self.destination {
            Some(dir) if self.refusals.is_empty() => {
                tracing::debug!(dir = %dir.display(), "logging to file");
            }
            Some(dir) => tracing::warn!(
                dir = %dir.display(),
                "the preferred log directory was unusable; this run's log file is in the fallback directory"
            ),
            // Emitted anyway: the only record of why the file the player was
            // asked for does not exist. `crash.log` has its own ladder and does
            // not go through the subscriber.
            None => tracing::error!(
                "no log file: every candidate directory refused, so this session leaves no diagnostic trail beyond crash.log"
            ),
        }
    }

    /// The directory the log file ended up in, or `None` when there is no file.
    #[must_use]
    pub fn destination(&self) -> Option<&Path> {
        self.destination.as_deref()
    }
}

/// Installs the tracing subscriber over a daily-rotated file, because the
/// windowed build's stdout and stderr are inert sinks.
///
/// The returned guard flushes the non-blocking writer on drop: it MUST live
/// until the end of `main`, or the last (most interesting) lines never hit disk.
/// The [`LogSetup`] beside it must be `report`ed once this call returns; nothing
/// it has to say can be logged from inside.
///
/// Untested on purpose: the subscriber is *process-global*, so a second call —
/// what a second test would be — panics. [`log_dirs_from`] carries the part
/// worth testing.
#[must_use = "the worker guard flushes the log on drop; keep it alive, and `report` the setup"]
pub fn install_logging() -> (
    Option<tracing_appender::non_blocking::WorkerGuard>,
    LogSetup,
) {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt::writer::BoxMakeWriter;

    let mut setup = LogSetup {
        destination: None,
        refusals: Vec::new(),
    };
    let mut guard = None;
    let mut file_writer = None;
    for dir in log_dirs() {
        match tracing_appender::rolling::Builder::new()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix(APP_DIR)
            .filename_suffix("log")
            // A player leaves this running for days: cap the disk footprint.
            .max_log_files(5)
            .build(&dir)
        {
            Ok(appender) => {
                let (writer, worker_guard) = tracing_appender::non_blocking(appender);
                file_writer = Some(writer);
                guard = Some(worker_guard);
                setup.destination = Some(dir);
                break;
            }
            Err(err) => setup.refusals.push((dir, err)),
        }
    }

    let writer = match file_writer {
        Some(writer) => {
            // Tee, because this lane has a real terminal: otherwise
            // `RUST_LOG=…=trace` produces zero output on screen.
            #[cfg(not(feature = "gui"))]
            let writer = {
                use tracing_subscriber::fmt::writer::MakeWriterExt;
                writer.and(std::io::stdout)
            };
            BoxMakeWriter::new(writer)
        }
        // Every candidate refused: stdout rather than no subscriber at all,
        // inert in the windowed build, where `setup.report()` makes it visible.
        None => BoxMakeWriter::new(std::io::stdout),
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            // `try_from_default_env`, not `from_default_env`: a malformed
            // RUST_LOG must not kill the app. `journal=info` keeps the
            // player-facing lines in the file; the crate directive misses them.
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("arkyve_refresh_shop=debug,journal=info,warn")),
        )
        .with_writer(writer)
        // A file is not a terminal: escape codes would only make it unreadable.
        .with_ansi(false)
        .with_target(false)
        .init();
    (guard, setup)
}

/// Config keys that still parse but no longer do anything, in report order.
///
/// Still *accepted* because deleting the fields would make `Config::load` fail
/// on every config file written by an earlier release — which is every config
/// file that exists.
#[must_use]
pub fn retired_keys(config: &Config) -> Vec<String> {
    [config.capture.retired_keys(), config.forward.retired_keys()]
        .into_iter()
        .flatten()
        .collect()
}

/// Which of the three things happened when the retired keys were stripped.
///
/// A value rather than three `warn!` calls in place, so the decision table is
/// testable: past tense, present tense, or "somebody else got there first".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetiredKeys {
    /// Nothing retired: say nothing, and skip the second read entirely.
    Absent,
    /// The keys were on disk and have now been deleted.
    Removed(String),
    /// The rewrite failed, so the keys are still on disk: a "you may want to
    /// act" warning, and it will repeat next launch.
    NotRewritten { keys: String, error: String },
    /// Something rewrote the file between the two reads. Nothing to do, but the
    /// warning comes back next launch, so say so.
    AlreadyGone(String),
}

impl RetiredKeys {
    /// The decision table. `keys` is what the loaded `Config` still carries,
    /// `stripped` is what [`config::persist::strip_retired_keys`] made of the
    /// file.
    #[must_use]
    pub fn classify(keys: &[String], stripped: Result<Option<String>>) -> Self {
        if keys.is_empty() {
            return Self::Absent;
        }
        let still_set = keys.join(", ");
        match stripped {
            Ok(Some(removed)) => Self::Removed(removed),
            Ok(None) => Self::AlreadyGone(still_set),
            // Best-effort throughout: the keys are inert, so failing to delete
            // them costs a log line, never a startup.
            Err(err) => Self::NotRewritten {
                keys: still_set,
                error: err.report(),
            },
        }
    }

    /// Says it out loud, once.
    pub fn report(&self) {
        match self {
            Self::Absent => {}
            Self::Removed(removed) => tracing::warn!(
                keys = %removed,
                "these config keys were already being ignored and have now been removed from config.toml; no setting of yours was changed"
            ),
            Self::NotRewritten { keys, error } => tracing::warn!(
                keys = %keys,
                error = %error,
                "these config keys are accepted but ignored, and will be refused in a later release; config.toml could not be rewritten to drop them"
            ),
            Self::AlreadyGone(keys) => tracing::warn!(
                keys = %keys,
                "these config keys are accepted but ignored, and will be refused in a later release; they were already gone from config.toml when it was re-read, so nothing was written"
            ),
        }
    }
}

/// Strips the retired keys from `path` and reports which of the three things
/// happened.
///
/// Called after `Config::load` on purpose: we only rewrite a file the app has
/// just parsed *and* validated, so a run about to die on an invalid config
/// touches nothing.
pub fn strip_and_report_retired_keys(config: &Config, path: &Path) {
    let keys = retired_keys(config);
    if keys.is_empty() {
        return;
    }
    RetiredKeys::classify(&keys, config::persist::strip_retired_keys(path)).report();
}

/// Every fatal error before the main window opens lands here: the log file
/// always (stderr is inert in the windowed build), stderr, then an error window
/// in the windowed build. The caller logs the *structured* cause first;
/// `message` is player-facing prose, multi-line on purpose, and not logged again
/// here because a `\n` in a field breaks one-event-per-line in the file.
#[must_use = "this is the process exit code"]
pub fn fatal(message: String) -> ExitCode {
    tracing::error!("startup failed");
    eprintln!("{message}");
    #[cfg(feature = "gui")]
    if let Err(err) = ui::show_fatal(message) {
        tracing::error!(error = %err, "the error window could not be shown either");
        eprintln!("error window failed: {err}");
    }
    ExitCode::FAILURE
}

/// Waits out a session task the closing window has already asked to stop, and
/// answers the half of [`exit_code`]'s contract the window cannot see: did the
/// session finish, or did the process walk away from it. Neither failure below
/// can reach `app::supervise`'s `session_failed` flag, because that flag is
/// written *by* the task and here the task is what broke.
///
/// `&mut task`, not `task`: `timeout` takes its future by value, so handing over
/// the `JoinHandle` would *detach* the task on the timeout arm rather than
/// cancel it, and `shutdown_background` would then leak blocking threads. The
/// borrow is valid (`JoinHandle` is `Unpin + Future`) and the explicit abort is
/// what makes that arm mean what it says.
pub async fn teardown_failed(mut task: tokio::task::JoinHandle<()>, grace: Duration) -> bool {
    match tokio::time::timeout(grace, &mut task).await {
        Ok(Ok(())) => false,
        Ok(Err(err)) => {
            tracing::error!(
                error = %err,
                "the session task did not return; its outcome reached neither the banner nor the failed flag"
            );
            true
        }
        Err(_) => {
            task.abort();
            // Drain the cancellation so the task is really gone before the
            // runtime is dropped.
            let _ = task.await;
            tracing::warn!(
                grace_s = grace.as_secs(),
                "session teardown timed out; the task was aborted, and a capture session may briefly outlive the process"
            );
            true
        }
    }
}

/// The exit-code contract, in one place because scripts and smoke checks read
/// it: success needs *both* a cleanly closed window and a session that did not
/// die, an aborted one included ([`teardown_failed`] folds that in).
#[must_use]
pub fn exit_code(window_closed_cleanly: bool, session_failed: bool) -> ExitCode {
    if window_closed_cleanly && !session_failed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RAII scratch directory: removed on drop, including when an assertion
    /// panics.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "arkyve_lib_{tag}_{}_{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create the scratch dir");
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const EXAMPLE: &str = include_str!("../config.example.toml");

    #[test]
    fn seeding_creates_the_parent_and_writes_the_bundled_example() {
        let dir = TempDir::new("seed");
        let path = dir.join("nested").join("config.toml");
        seed_config_if_missing(&path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), EXAMPLE);
    }

    #[test]
    fn seeding_never_overwrites_an_existing_config() {
        // A regression here would destroy the player's settings on every launch.
        let dir = TempDir::new("keep");
        let path = dir.join("config.toml");
        std::fs::write(&path, "# mine\n").unwrap();
        seed_config_if_missing(&path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# mine\n");
    }

    #[test]
    fn the_seeded_example_is_what_config_load_then_reads() {
        let dir = TempDir::new("roundtrip");
        let path = dir.join("config.toml");
        seed_config_if_missing(&path);
        let config = Config::load(&path).expect("the seeded example must load and validate");
        assert!(
            retired_keys(&config).is_empty(),
            "the shipped example must not plant a key we immediately warn about"
        );
    }

    #[test]
    fn config_path_prefers_appdata_and_falls_back_to_the_working_directory() {
        let with = config_path_from(Some(Path::new("C:/Users/x/AppData/Roaming")));
        assert!(with.ends_with(format!("{APP_DIR}/config.toml")));
        assert_eq!(config_path_from(None), PathBuf::from("config.toml"));
    }

    #[test]
    fn the_real_config_path_and_log_dirs_agree_on_the_app_dir() {
        // Both read process env, so only the shape is asserted.
        assert!(config_path().ends_with("config.toml"));
        for dir in log_dirs() {
            assert!(dir.ends_with(format!("{APP_DIR}/logs")));
        }
    }

    #[test]
    fn log_dirs_try_local_appdata_then_temp() {
        let dirs = log_dirs_from(
            Some(Path::new("C:/Users/x/AppData/Local")),
            Path::new("C:/Temp"),
        );
        assert_eq!(dirs.len(), 2);
        assert!(dirs[0].starts_with("C:/Users/x/AppData/Local"));
        assert!(dirs[1].starts_with("C:/Temp"));
        assert!(dirs.iter().all(|d| d.ends_with(format!("{APP_DIR}/logs"))));
    }

    #[test]
    fn without_local_appdata_the_temp_dir_is_the_only_candidate() {
        let dirs = log_dirs_from(None, Path::new("/tmp"));
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].starts_with("/tmp"));
    }

    #[test]
    fn the_retired_key_table_picks_one_arm_per_outcome() {
        let keys = vec![
            "capture.filter".to_owned(),
            "forward.server_to_client".to_owned(),
        ];
        assert_eq!(RetiredKeys::classify(&[], Ok(None)), RetiredKeys::Absent);
        assert_eq!(
            RetiredKeys::classify(&keys, Ok(Some("capture.filter".to_owned()))),
            RetiredKeys::Removed("capture.filter".to_owned())
        );
        assert_eq!(
            RetiredKeys::classify(&keys, Ok(None)),
            RetiredKeys::AlreadyGone("capture.filter, forward.server_to_client".to_owned())
        );
        let err = Error::Config("nope".to_owned());
        assert_eq!(
            RetiredKeys::classify(&keys, Err(err)),
            RetiredKeys::NotRewritten {
                keys: "capture.filter, forward.server_to_client".to_owned(),
                error: "invalid configuration: nope".to_owned(),
            }
        );
    }

    #[test]
    fn a_config_with_no_retired_key_reports_nothing_and_rewrites_nothing() {
        // No keys means the file is never even re-read: the strip costs nothing
        // on a healthy install.
        let dir = TempDir::new("noretired");
        let path = dir.join("config.toml");
        strip_and_report_retired_keys(&Config::default(), &path);
        assert!(!path.exists(), "no retired key must mean no file access");
    }

    #[test]
    fn only_a_clean_window_over_a_live_session_exits_zero() {
        // The documented external contract: scripts and smoke checks read this.
        // `ExitCode` is opaque and not `PartialEq`, so compare its `Debug`.
        assert_eq!(
            format!("{:?}", exit_code(true, false)),
            format!("{:?}", ExitCode::SUCCESS)
        );
        for (window, failed) in [(true, true), (false, false), (false, true)] {
            assert_eq!(
                format!("{:?}", exit_code(window, failed)),
                format!("{:?}", ExitCode::FAILURE),
                "window_closed_cleanly={window} session_failed={failed}"
            );
        }
    }

    /// The three teardowns the closing window can meet, and which of them the
    /// exit code has to call a failure.
    #[tokio::test(start_paused = true)]
    async fn a_session_the_process_had_to_abandon_is_not_a_success() {
        let grace = Duration::from_secs(3);

        let clean = tokio::spawn(async {});
        assert!(
            !teardown_failed(clean, grace).await,
            "a task that returned owes the exit code nothing"
        );

        // Never finishes: the grace expires and the task is aborted mid-unwind,
        // which is the case that leaves a capture session behind.
        let hung = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        let started = tokio::time::Instant::now();
        assert!(teardown_failed(hung, grace).await);
        assert!(
            started.elapsed() >= grace,
            "the abort must come after the grace, not instead of it"
        );

        // `app::supervise` catches a panic in the *session*; a panic in the
        // wrapper around it lands here as a `JoinError` and writes neither the
        // banner nor the failed flag.
        let panicked = tokio::spawn(async {
            panic!("the wrapper itself died");
        });
        assert!(teardown_failed(panicked, grace).await);
    }
}
