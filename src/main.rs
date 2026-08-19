//! Entry point for the Secret Shop relay.
//!
//! Dispatch only. The startup *policy* — where the config and the log live, how
//! logging degrades when its directory is unwritable, what a player is told
//! about retired config keys, and the exit-code contract — lives in `lib.rs`,
//! where `cargo test` can reach it (see `proj-003`). What has to stay here is
//! the crate-level `windows_subsystem` attribute, which is an attribute on the
//! *binary*, and the two `run_mode` arms, which own the OS main thread.

// The windowed build is a real windowed app: no console opens beside the
// window. Everything player-facing flows through the journal/banner; stdout
// and stdin become inert sinks there (the console build keeps them).
#![cfg_attr(all(windows, feature = "gui"), windows_subsystem = "windows")]
// Shipped code has none of these today (measured: 0 sites in --lib --bins).
// `not(test)` keeps the ratchet off the test harness, where `unwrap` in a
// fixture is correct — 257 sites and rising. The rest of the lint policy
// lives in Cargo.toml's `[lints]`; these two cannot, since a `[lints]` table
// applies to every target including tests.
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::panic))]

use std::path::PathBuf;
use std::process::ExitCode;

use arkyve_refresh_shop::{
    Config, app, config_path, crash, exit_code, fatal, install_logging, migrate,
    seed_config_if_missing, strip_and_report_retired_keys,
};

fn main() -> ExitCode {
    // Ahead of `install_logging`, deliberately: an install that ever ran the
    // WinDivert backend left `%LOCALAPPDATA%\<app>` locked to
    // administrators, and the log directory sits inside it. Undo that first
    // and this run keeps its log; undo it after and the first post-upgrade
    // run writes into a directory it cannot open. Findings are logged below,
    // once there is a subscriber to log them to.
    let leftovers = migrate::clean_windivert_leftovers();

    // Before anything can panic: capture panics to a file. In the windowed
    // build stdout/stderr are inert, so a panic on a worker or the capture
    // thread would otherwise vanish (surfacing only as "session ended"). The
    // hook also emits a `tracing` line — a no-op until the subscriber below
    // exists, deliberately: the file is the primary record.
    crash::install();

    // Held to the end of `main`: dropping it flushes the log writer. The
    // `LogSetup` beside it carries what could not be said before the subscriber
    // existed — including "there is no log file", the one failure in this crate
    // that would otherwise conceal itself completely.
    let (_log_guard, log_setup) = install_logging();
    // Cause before effect: a failed DACL reset is *why* the log directory may
    // have refused, so the migrate findings come first.
    leftovers.report();
    log_setup.report();

    // rustls 0.23 needs a process-level CryptoProvider installed before the
    // first TLS handshake, or `connect_async` panics on any wss:// URL —
    // installed explicitly since the enabled feature set alone doesn't
    // auto-select one. Placed after `install_logging` so a failure can be
    // *reported*: this used to `.expect()`, which in the windowed build
    // meant a double-clicked exe doing nothing, unlike its two neighbouring
    // startup failures, which route through `fatal`. `install_default`
    // fails only if a provider is already installed — a programming
    // invariant, just not a silent one any more.
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
    let config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(err) => {
            // Structured for the file, prose for the player: the two audiences
            // want different things, and the prose carries embedded newlines
            // that would break one-event-per-line in the log.
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
    // player is asked to send us. `app::run` logs the redacted form once, through
    // `config::ServerUrl::redacted`.
    tracing::info!(
        dry_run = config.actuator.dry_run,
        game_port = config.game_port,
        "actuator configured"
    );

    // The runtime is built by hand instead of #[tokio::main]: in the GUI
    // build, eframe/winit must own the OS main thread.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        // Five long-lived tasks (uplink, reassembly, actuator, stdin, session
        // loop), all IO-bound or offloaded through `actuator::blocking` /
        // `spawn_blocking`. Two workers is the floor `app/mod.rs:515-517`'s
        // "with a single worker, that is a deadlock" assumes; four is the
        // ceiling this workload can use, and the default (one per
        // `available_parallelism`) would reserve 16-24 idle worker stacks on a
        // player's gaming CPU, beside the game it is driving.
        .worker_threads(4)
        // Only the capture teardown join uses the blocking pool.
        .max_blocking_threads(4)
        // Named for the same reason every other thread here is (`capture`,
        // `shield`, `pcap-<adapter>`): `crash.rs` is the only post-mortem
        // channel, and it records the thread name.
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
    run_mode(runtime, config, config_path, log_setup.destination())
}

/// Console-only build: the session blocks the main thread, as before.
///
/// `_log_file` is unused here on purpose: when every log directory refuses,
/// `install_logging` falls back to a stdout subscriber, and this is the lane with
/// a real terminal — so "there is no log file" is already on screen. The windowed
/// arm below has no such surface and has to say it in the journal.
#[cfg(not(feature = "gui"))]
fn run_mode(
    runtime: tokio::runtime::Runtime,
    config: Config,
    _config_path: PathBuf,
    _log_file: Option<&std::path::Path>,
) -> ExitCode {
    let outcome = runtime.block_on(app::run(config));
    // Not a plain drop: tokio::io::stdin parks an uncancelable blocking read,
    // and dropping the runtime would hang exit until the player presses Enter.
    runtime.shutdown_background();
    // Same contract as the windowed lane, through the same function. There is no
    // window here, so its half is vacuously satisfied and only the session
    // decides.
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
) -> ExitCode {
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
    // The journal half of the no-log-file report. `LogSetup::report` already
    // logged it at `error` — into a subscriber writing to a stdout this
    // build does not have, which is the whole problem. The journal panel is
    // the only surface a windowed build has, and this is the earliest point
    // it exists. Emitted here rather than in `app::setup` because it is a
    // property of *this* process's startup, not of a session.
    if log_file.is_none() {
        handles.journal.emit_at(
            tracing::Level::WARN,
            &[
                ">> no log file could be opened — this session leaves no diagnostic trail (see Troubleshooting in the README)"
                    .to_owned(),
            ],
        );
    }
    let error = ui::SessionErrorSlot::default();
    let failed = Arc::new(AtomicBool::new(false));
    // Spelled `Arc::clone`, not `.clone()`: refcount bumps, not deep copies —
    // the convention every other shared handle in this crate follows.
    let (slot, flag) = (Arc::clone(&error), Arc::clone(&failed));
    let gate = handles.gate.clone();
    // The handle is kept, not discarded: it is both the join point for the
    // teardown below and the reason a panic in this wrapper cannot vanish.
    let mut session_task = runtime.spawn(async move {
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
                // Moved, not cloned: `AppCreator` is `FnOnce` and nothing reads
                // `config_path` after `run_native`.
                config_path,
            )))
        }),
    );
    // The window is gone but the session still runs on its task, and
    // nothing has told it so: without this, `shutdown_background` (which
    // signals nothing and waits for nothing) would kill the process
    // mid-flight, leaving an orphaned live capture session on every
    // launch/close cycle.
    shutdown.request();
    let joined = runtime.block_on(async {
        // `&mut session_task`, not `session_task`: `timeout` takes its future
        // by value, so handing over the `JoinHandle` would *detach* the task
        // on the timeout arm rather than cancel it, and the next line
        // (`shutdown_background`) would leak blocking threads — exactly the
        // outcome the warning below describes. `JoinHandle` is
        // `Unpin + Future`, so a borrow is a valid branch; aborting turns
        // that accident into the intended path.
        let outcome = tokio::time::timeout(TEARDOWN_GRACE, &mut session_task).await;
        if outcome.is_err() {
            session_task.abort();
            // Drain the cancellation so the task is really gone before the
            // runtime is dropped.
            let _ = session_task.await;
        }
        outcome
    });
    if joined.is_err() {
        tracing::warn!(
            grace_s = TEARDOWN_GRACE.as_secs(),
            "session teardown timed out; the task was aborted, and a capture session may briefly outlive the process"
        );
    }
    // Still not a plain drop, and still last: `tokio::io::stdin` parks a
    // blocking thread, so dropping the runtime would hang window close
    // until the player presses Enter — true of skipping the *runtime* drop
    // only, never of the cooperative teardown above.
    runtime.shutdown_background();
    match result {
        Ok(()) => exit_code(true, failed.load(Ordering::Relaxed)),
        Err(err) => {
            // `eframe::run_native` fails for real, common reasons — no GL
            // context, a stale display driver, an RDP session, a headless
            // service account — and stderr is inert in exactly this build, so
            // without this line the log stops after "actuator configured" and
            // the player sees a double-clicked exe do nothing.
            tracing::error!(error = %err, "the application window could not be created");
            eprintln!("GUI error: {err}");
            exit_code(false, failed.load(Ordering::Relaxed))
        }
    }
}
