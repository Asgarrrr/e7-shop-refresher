//! Entry point for the Secret Shop relay.

// The windowed build is a real windowed app: no console opens beside the
// window. Everything player-facing flows through the journal/banner; stdout
// and stdin become inert sinks there (the console build keeps them).
#![cfg_attr(all(windows, feature = "gui"), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::process::ExitCode;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::BoxMakeWriter;

use arkyve_refresh_shop::{Config, app, crash, migrate};

/// Location of the config file. The app owns this file (the GUI's Setup/Apply
/// writes it); the player isn't expected to hand-edit it, so it lives out of
/// the way in per-user roaming app-data on Windows —
/// `%APPDATA%\arkyve-refresh-shop\config.toml` — rather than beside the exe.
/// If `APPDATA` is somehow unset, or on non-Windows dev machines (mac), it
/// falls back to a `config.toml` in the working directory.
fn config_path() -> PathBuf {
    #[cfg(windows)]
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata)
            .join(arkyve_refresh_shop::APP_DIR)
            .join("config.toml");
    }
    PathBuf::from("config.toml")
}

/// On first run no config file exists yet. Write the bundled example (compiled
/// into the exe) to the resolved path so the player finds a real, commented
/// file at the standard location — and a valid one: the example carries the
/// default hunt `[filter]`, which the relay requires to start. Best-effort:
/// any failure is ignored and `Config::load` falls back to the in-memory
/// defaults, exactly as before.
fn seed_config_if_missing(path: &std::path::Path) {
    if path.exists() {
        return;
    }
    const EXAMPLE: &str = include_str!("../config.example.toml");
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, EXAMPLE);
}

/// Where the rotated log files live: next to `crash.log`, in per-user *local*
/// app-data (logs are machine-local, they must not roam). Falls back to the
/// temp dir when `LOCALAPPDATA` is unset (non-Windows dev machines).
fn log_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(arkyve_refresh_shop::APP_DIR)
        .join("logs")
}

/// Installs the tracing subscriber over a daily-rotated file.
///
/// The windowed build has no console — stdout and stderr are inert sinks —
/// so a stdout subscriber loses every event the moment the app ships. The
/// returned guard flushes the non-blocking writer on drop: it MUST live until
/// the end of `main`, or the last (most interesting) lines never hit disk.
#[must_use = "the worker guard flushes the log on drop; keep it alive"]
fn install_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let (writer, guard) = match tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(arkyve_refresh_shop::APP_DIR)
        .filename_suffix("log")
        // A player leaves this running for days: cap the disk footprint.
        .max_log_files(5)
        .build(log_dir())
    {
        Ok(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            (BoxMakeWriter::new(writer), Some(guard))
        }
        // Unwritable log dir: fall back to stdout rather than to no
        // subscriber at all — inert in the windowed build, real in the
        // console one.
        Err(_) => (BoxMakeWriter::new(std::io::stdout), None),
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            // `try_from_default_env`, not `from_default_env`: a malformed
            // RUST_LOG must not kill the app. `journal=info` keeps the
            // player-facing lines (emitted on that target) in the file — the
            // crate-level directive does not cover them.
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("arkyve_refresh_shop=debug,journal=info,warn")),
        )
        .with_writer(writer)
        // A file is not a terminal: escape codes would only make it unreadable.
        .with_ansi(false)
        .with_target(false)
        .init();
    guard
}

fn main() -> ExitCode {
    // Ahead of `install_logging`, and that ordering is the point: an install
    // that ever ran the WinDivert backend left `%LOCALAPPDATA%\<app>` locked to
    // administrators, and the log directory sits inside it. Undo that first and
    // this run keeps its log; undo it afterwards and the first post-upgrade run
    // writes into a directory it cannot open and loses everything. The findings
    // are logged below, once there is a subscriber to log them to.
    let leftovers = migrate::clean_windivert_leftovers();

    // Before anything can panic: capture panics to a file. In the windowed
    // build stdout/stderr are inert, and a panic on a worker task or the
    // capture thread would otherwise vanish (surfacing only as a bare
    // "session ended").
    crash::install();

    // rustls 0.23 needs a process-level CryptoProvider installed before the
    // first TLS handshake, or connect_async panics on any wss:// URL. Install
    // it explicitly (the enabled feature set alone doesn't auto-select one).
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install the rustls ring CryptoProvider");

    // Held to the end of `main`: dropping it flushes the log writer.
    let _log_guard = install_logging();
    leftovers.report();

    let config_path = config_path();
    // Emitted before the config is even read: a run that dies on an invalid
    // config must still leave its identity in the log file.
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        os = %std::env::consts::OS,
        gui = cfg!(feature = "gui"),
        pcap = cfg!(feature = "pcap-backend"),
        actuator = cfg!(feature = "actuator"),
        config_path = %config_path.display(),
        "arkyve-refresh-shop starting"
    );
    seed_config_if_missing(&config_path);
    let config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(err) => {
            return fatal(format!(
                "Invalid configuration: {err}\n\nFix {} and restart.",
                config_path.display()
            ));
        }
    };
    // Said out loud, once, at the one moment the player might correlate it with
    // something: these keys still parse but no longer do anything. The capture
    // filter is a BPF expression built from `game_port` inside the Npcap
    // backend, and the receive buffer is the snaplen that backend picks. They
    // are still accepted because deleting them would make `Config::load` fail on
    // every config file written by an earlier release, which is every config
    // file that exists.
    if let Some(keys) = config.capture.retired_keys() {
        tracing::warn!(
            keys = %keys,
            "these [capture] keys are accepted but ignored, and will be refused in a later release"
        );
    }
    // No `server_url` here: it can carry a credential (see
    // `app::redacted_server_url`), and this file is what the player is asked
    // to send us.
    tracing::info!(
        dry_run = config.actuator.dry_run,
        game_port = config.game_port,
        "actuator configured"
    );

    // The runtime is built by hand instead of #[tokio::main]: in the GUI
    // build, eframe/winit must own the OS main thread.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => return fatal(format!("Failed to start the async runtime: {err}")),
    };
    run_mode(runtime, config, config_path)
}

/// Every fatal error before the main window opens lands here: the log file
/// always (stderr is inert in the windowed build, and these two failures —
/// invalid config, runtime that won't build — are the likeliest of all),
/// stderr, then an error window in the windowed build.
fn fatal(message: String) -> ExitCode {
    // Field named `reason`, not `message`: `message` is the field tracing
    // itself fills from the format string, and two would collide in the file.
    tracing::error!(reason = %message, "startup failed");
    eprintln!("{message}");
    #[cfg(feature = "gui")]
    if let Err(err) = arkyve_refresh_shop::ui::show_fatal(message) {
        eprintln!("error window failed: {err}");
    }
    ExitCode::FAILURE
}

/// Console-only build: the session blocks the main thread, as before.
#[cfg(not(feature = "gui"))]
fn run_mode(runtime: tokio::runtime::Runtime, config: Config, _config_path: PathBuf) -> ExitCode {
    let outcome = runtime.block_on(app::run(config));
    // Not a plain drop: tokio::io::stdin parks an uncancelable blocking read,
    // and dropping the runtime would hang exit until the player presses Enter.
    runtime.shutdown_background();
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Fatal error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// GUI build: the session runs on the runtime's workers while eframe owns the
/// main thread; the session's outcome lands in the window banner instead of
/// killing the window.
#[cfg(feature = "gui")]
fn run_mode(runtime: tokio::runtime::Runtime, config: Config, config_path: PathBuf) -> ExitCode {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use arkyve_refresh_shop::ui;

    /// How long the closing window waits for the session to unwind (capture
    /// session closed, capture thread joined, uplink shut). Generous: this
    /// only ever runs once, on exit.
    const TEARDOWN_GRACE: Duration = Duration::from_secs(3);

    // Capture the startup timings before setup consumes the config: they seed
    // the window's timing editor (no controller home for them).
    let seed_timings = config.actuator.timings;
    let (session, handles, shutdown) = app::setup(config);
    let error = ui::SessionErrorSlot::default();
    let failed = Arc::new(AtomicBool::new(false));
    let (slot, flag) = (error.clone(), failed.clone());
    let gate = handles.gate.clone();
    // The handle is kept, not discarded: it is both the join point for the
    // teardown below and the reason a panic in this wrapper cannot vanish.
    let session_task = runtime.spawn(async move {
        // app::supervise catches a session panic (it must land in the banner,
        // not vanish with a discarded JoinHandle) and forces the gate off.
        let (outcome, session_failed) = app::supervise(session.run(), gate).await;
        if session_failed {
            flag.store(true, Ordering::Relaxed);
        }
        // Poison-tolerant like the view's own reads (`ui::lock_ignoring_poison`):
        // panicking here would kill this task silently — no banner, no failed
        // flag — and report a dead session as a clean exit.
        *slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(outcome);
    });

    let result = eframe::run_native(
        arkyve_refresh_shop::APP_NAME,
        eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default()
                .with_inner_size([500.0, 560.0])
                .with_min_inner_size([440.0, 460.0]),
            ..Default::default()
        },
        Box::new(move |cc| {
            Ok(Box::new(ui::ShopApp::new(
                cc,
                handles,
                error,
                seed_timings,
                config_path.clone(),
            )))
        }),
    );
    // The window is gone but the session still runs on its task, and nothing
    // has told it so: without this, `shutdown_background` (which signals
    // nothing and waits for nothing) would kill the process mid-flight and
    // skip the whole teardown — leaving an orphaned live capture session in
    // the driver on every launch/close cycle.
    shutdown.request();
    let joined =
        runtime.block_on(async { tokio::time::timeout(TEARDOWN_GRACE, session_task).await });
    if joined.is_err() {
        tracing::warn!(
            grace_s = TEARDOWN_GRACE.as_secs(),
            "session teardown timed out; a capture session may outlive the process"
        );
    }
    // Still not a plain drop, and still last: tokio::io::stdin parks a
    // blocking thread, so dropping the runtime would hang window close until
    // the player presses Enter. That justifies skipping the *runtime* drop,
    // never the cooperative teardown above.
    runtime.shutdown_background();
    match result {
        // A dead session is a failure even when the window closed cleanly:
        // scripts and smoke checks read the exit code.
        Ok(()) if !failed.load(Ordering::Relaxed) => ExitCode::SUCCESS,
        Ok(()) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("GUI error: {err}");
            ExitCode::FAILURE
        }
    }
}
