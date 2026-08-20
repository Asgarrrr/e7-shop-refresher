//! Entry point for the Secret Shop relay.
//!
//! Dispatch only. The startup *policy* lives in `lib.rs`, where `cargo test`
//! can reach it. What has to stay here is the crate-level `windows_subsystem`
//! attribute, which is an attribute on the *binary*, and the two `run_mode`
//! arms, which own the OS main thread.

// No console opens beside the window. Everything player-facing flows through
// the journal/banner; stdout and stdin become inert sinks there (the console
// build keeps them).
#![cfg_attr(all(windows, feature = "gui"), windows_subsystem = "windows")]
// Not in Cargo.toml's `[lints]`: a `[lints]` table applies to every target,
// and the test harness uses `unwrap` in fixtures on purpose.
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::panic))]

use std::path::PathBuf;
use std::process::ExitCode;

use arkyve_refresh_shop::{
    Config, app, config_path, crash, exit_code, fatal, install_logging, migrate,
    seed_config_if_missing, strip_and_report_retired_keys,
};

fn main() -> ExitCode {
    // Ahead of `install_logging`: the log directory sits inside the
    // `%LOCALAPPDATA%\<app>` a WinDivert build locked to administrators, so undo
    // that first or the first post-upgrade run writes into a directory it cannot
    // open. Findings are reported below, once there is a subscriber.
    let leftovers = migrate::clean_windivert_leftovers();

    // Before anything can panic: with inert stdout/stderr a worker panic would
    // otherwise surface only as "session ended". The hook's `tracing` line is a
    // no-op until the subscriber exists; the file is the record.
    crash::install();

    // Held to the end of `main`: dropping it flushes the log writer.
    let (_log_guard, log_setup) = install_logging();
    // Cause before effect: a failed DACL reset is *why* the log directory may
    // have refused, so the migrate findings come first.
    leftovers.report();
    log_setup.report();

    // rustls 0.23 panics at the first handshake without a process-level
    // CryptoProvider, and the enabled features alone do not select one. After
    // `install_logging` so a failure can be *reported*: an `.expect()` here is a
    // double-clicked exe doing nothing in the windowed build.
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        tracing::error!("a rustls CryptoProvider was already installed");
        return fatal(
            "Failed to initialise TLS: a crypto provider was already installed.".to_owned(),
        );
    }

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
    // `load_reporting`, not `load`: the salvage warning reaches only the log
    // file, which nobody has opened. Carried to `run_mode` so the journal can
    // say it too, once there is a journal.
    let (config, dropped_ranges) = match Config::load_reporting(&config_path) {
        Ok(loaded) => loaded,
        Err(err) => {
            // Structured for the file, prose for the player: the prose has
            // newlines that would break one-event-per-line in the log.
            // `?err` is safe here specifically because a config parse/reparse
            // failure is redacted at the type: `Error::ConfigParse` and
            // `Error::ConfigReparse` carry `error::TomlLocation`, not the raw
            // `toml`/`toml_edit` error, so neither this line nor `report()`
            // below can render the offending source line.
            tracing::error!(
                error = ?err,
                config_path = %config_path.display(),
                "invalid configuration"
            );
            return fatal(format!(
                "Invalid configuration: {}\n\nFix {} and restart.",
                err.report(),
                config_path.display()
            ));
        }
    };
    strip_and_report_retired_keys(&config, &config_path);
    // No `server_url` here: it can carry a credential, and this file is what the
    // player is asked to send us. `app::run` logs the redacted form once.
    tracing::info!(
        dry_run = config.actuator.dry_run,
        game_port = config.game_port,
        "actuator configured"
    );

    // Built by hand instead of `#[tokio::main]`: in the GUI build, eframe/winit
    // must own the OS main thread.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        // Two is the floor `app/mod.rs`'s "with a single worker, that is a
        // deadlock" assumes; four is the ceiling five IO-bound tasks can use,
        // and the default would reserve 16-24 idle stacks on a gaming CPU.
        .worker_threads(4)
        // Only the capture teardown join uses the blocking pool.
        .max_blocking_threads(4)
        // Named because `crash.rs` is the only post-mortem channel and it
        // records the thread name.
        .thread_name("relay-worker")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!(error = ?err, "the async runtime could not be started");
            return fatal(format!("Failed to start the async runtime: {err}"));
        }
    };
    run_mode(
        runtime,
        config,
        config_path,
        log_setup.destination(),
        dropped_ranges,
    )
}

/// Console-only build: the session blocks the main thread.
///
/// `_log_file` and `_dropped_ranges` are unused on purpose: this lane has a real
/// terminal, so both are already on screen. The windowed arm has no such surface
/// and must say them in the journal.
#[cfg(not(feature = "gui"))]
fn run_mode(
    runtime: tokio::runtime::Runtime,
    config: Config,
    _config_path: PathBuf,
    _log_file: Option<&std::path::Path>,
    _dropped_ranges: arkyve_refresh_shop::config::DroppedRanges,
) -> ExitCode {
    let outcome = runtime.block_on(app::run(config));
    // Not a plain drop: `tokio::io::stdin` parks an uncancelable blocking read,
    // so dropping the runtime hangs exit until the player presses Enter.
    runtime.shutdown_background();
    // No window here, so its half of the contract is vacuously satisfied.
    match outcome {
        Ok(()) => exit_code(true, false),
        Err(err) => {
            tracing::error!(error = ?err, "the session ended with a fatal error");
            eprintln!("Fatal error: {}", err.report());
            exit_code(true, true)
        }
    }
}

/// GUI build: the session runs on the runtime's workers while eframe owns the
/// main thread; the session's outcome lands in the window banner instead of
/// killing the window.
#[cfg(feature = "gui")]
fn run_mode(
    runtime: tokio::runtime::Runtime,
    config: Config,
    config_path: PathBuf,
    log_file: Option<&std::path::Path>,
    dropped_ranges: arkyve_refresh_shop::config::DroppedRanges,
) -> ExitCode {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use arkyve_refresh_shop::ui;

    /// How long the closing window waits for the session to unwind (capture
    /// session closed, capture thread joined, uplink shut). Generous: this
    /// only ever runs once, on exit.
    const TEARDOWN_GRACE: Duration = Duration::from_secs(3);

    // Read before setup consumes the config: these seed the window's timing
    // editor, which has no controller home.
    let seed_timings = config.actuator.timings;
    let (session, handles, shutdown) = app::setup(config);
    // The journal half of the no-log-file report: `LogSetup::report` sent it to a
    // stdout this build does not have, which is the whole problem. Earliest point
    // the journal panel exists, and here rather than in `app::setup` because it
    // is a property of *this* process's startup, not of a session.
    if log_file.is_none() {
        handles.journal.emit_at(
            tracing::Level::WARN,
            &[
                ">> no log file could be opened — this session leaves no diagnostic trail (see Troubleshooting in the README)"
                    .to_owned(),
            ],
        );
    }
    // Same shape, same reason: a salvaged timing range warned only to the log
    // file, and a setting silently not in force costs a player an evening.
    let salvage = dropped_ranges.journal_lines();
    if !salvage.is_empty() {
        handles.journal.emit_at(tracing::Level::WARN, &salvage);
    }
    let error = ui::SessionErrorSlot::default();
    let failed = Arc::new(AtomicBool::new(false));
    let (slot, flag) = (Arc::clone(&error), Arc::clone(&failed));
    let gate = handles.gate.clone();
    // The handle is kept, not discarded: it is both the join point for the
    // teardown below and the reason a panic in this wrapper cannot vanish.
    let session_task = runtime.spawn(async move {
        // `app::supervise` catches a session panic (it must land in the banner,
        // not vanish with a discarded `JoinHandle`) and forces the gate off.
        let (outcome, session_failed) = app::supervise(session.run(), gate).await;
        if session_failed {
            flag.store(true, Ordering::Relaxed);
        }
        // Poison-tolerant: panicking here kills the task silently — no banner,
        // no flag — reporting a dead session as a clean exit. Written once, whole.
        *arkyve_refresh_shop::sync::lock_ignoring_poison(&slot) = Some(outcome);
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
                config_path,
            )))
        }),
    );
    // The window is gone but the session still runs and nothing has told it so.
    // Without this, `shutdown_background` — which signals nothing and waits for
    // nothing — kills the process mid-flight, orphaning a live capture session
    // on every launch/close cycle.
    shutdown.request();
    // A session the process had to abandon is a failed session, so this verdict
    // joins the flag rather than only reaching the log.
    let abandoned = runtime.block_on(arkyve_refresh_shop::teardown_failed(
        session_task,
        TEARDOWN_GRACE,
    ));
    // Still not a plain drop, and still last: the parked stdin read would hang
    // window close until the player presses Enter.
    runtime.shutdown_background();
    let session_failed = failed.load(Ordering::Relaxed) || abandoned;
    match result {
        Ok(()) => exit_code(true, session_failed),
        Err(err) => {
            // Fails for common reasons — no GL context, a stale driver, an RDP
            // session — and stderr is inert in exactly this build, so without
            // this the log stops after "actuator configured".
            tracing::error!(error = %err, "the application window could not be created");
            eprintln!("GUI error: {err}");
            exit_code(false, session_failed)
        }
    }
}
