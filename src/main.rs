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
use std::time::Duration;

use arkyve_refresh_shop::{
    Config, app, config_path, crash, exit_code, fatal, install_logging, migrate,
    seed_config_if_missing, strip_and_report_retired_keys,
};

/// How long a closing process waits for the session to unwind (capture session
/// closed, capture thread joined, uplink shut) before detaching it. Generous:
/// this only ever runs once, on exit.
///
/// One value for both `run_mode` arms, not two. It bounds a *stuck* teardown,
/// never a healthy one: a healthy one is already bounded from the inside by
/// `app::workers`'s `WORKER_SHUTDOWN_GRACE` (250 ms), which
/// `SessionWorkers::shutdown` spends twice, plus the capture thread join — so
/// three seconds is the same wide margin in either build. The tempting reason
/// to shorten it in the console build — a player watching a terminal rather
/// than a window that has already vanished — is about *feedback* during the
/// wait, and what answers that is the warning `teardown_failed` logs on
/// timeout, which this build's player does see: `install_logging` tees the file
/// writer with `std::io::stdout`. A second number would instead mean a console
/// repro and a windowed repro detach at different points while that warning's
/// `grace_s` field names only one of them.
const TEARDOWN_GRACE: Duration = Duration::from_secs(3);

fn main() -> ExitCode {
    // First statement: everything after this can panic, including the
    // elevated Win32 calls `migrate` is about to make, and a panic before the
    // hook is installed leaves no `crash.log` and no `tracing` line — a
    // silent vanish in the windowed build. The hook has no dependency on
    // `migrate`, so nothing after it needs to come first.
    crash::install();

    // Still ahead of `install_logging`, not just ahead of the hook: the log
    // directory sits inside the `%LOCALAPPDATA%\<app>` a WinDivert build
    // locked to administrators, so undo that first or the first post-upgrade
    // run writes into a directory it cannot open. Findings are reported
    // below, once there is a subscriber.
    let leftovers = migrate::clean_windivert_leftovers();

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
            // failure is redacted at the type, by `error.rs`'s two `From`
            // impls: `Error::ConfigParse` is the upstream `toml::de::Error`
            // with `set_input(None)` called before boxing — the derived
            // `Debug` prints every field, but `input`, the whole file, is now
            // `None` — and `Error::ConfigReparse` carries a `ReparseMessage`,
            // `toml_edit`'s `message()` and nothing else. Neither this line
            // nor `report()` below can reach the offending source line.
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

/// Console-only build: the session runs on the runtime's workers and this
/// thread waits for it, under the same [`TEARDOWN_GRACE`] the windowed arm
/// applies once its window is gone.
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
    use std::sync::{Arc, Mutex};

    use arkyve_refresh_shop::sync::lock_ignoring_poison;

    // A slot rather than the task's own return value, for the reason the
    // windowed arm hands `supervise`'s outcome through `SessionErrorSlot`: the
    // wait below can end before the task is joined, and a verdict written
    // during the grace still has to reach the exit code.
    let outcome: Arc<Mutex<Option<arkyve_refresh_shop::Result<()>>>> = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&outcome);
    // Carries nothing, and says only *when*: the Ctrl+C arm below abandons this
    // receiver, so a verdict sent through it would be the one verdict lost.
    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel::<()>();
    // Spawned rather than run under `block_on`, so this thread keeps a wait it
    // can put a deadline on. The handle is the join point for that deadline.
    let session_task = runtime.spawn(async move {
        let verdict = app::run(config).await;
        // Poison-tolerant and written whole, for the windowed arm's reason:
        // panicking here would report a dead session as a clean exit.
        *lock_ignoring_poison(&slot) = Some(verdict);
        // After the slot, so a wakeup below implies a readable verdict.
        let _ = finished_tx.send(());
    });

    // Ctrl+C is this build's window close, and this listener is the second one:
    // `app::session`'s `session_loop` pins its own and stops the session on the
    // same press, while this one starts the deadline on the teardown that press
    // begins. Without it this thread waits on the join with no bound at all —
    // `app::workers`'s `CaptureWorker::stop_and_join` is an untimed
    // `Thread::join`, and a `stop()` that fails to wake the capture thread never
    // ends it (held by
    // `worker_shutdown_stalls_when_stop_does_not_wake_the_capture_thread`) — and
    // no later press can help, tokio's console control handler having returned
    // TRUE since the first one (`SessionWorkers::shutdown` names the same
    // swallowing for its own second deadline).
    let abandoned = runtime.block_on(async move {
        tokio::select! {
            _ = finished_rx => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        // The windowed arm's call, unchanged and for its stated reason: a
        // session the process had to abandon is a failed session.
        arkyve_refresh_shop::teardown_failed(session_task, TEARDOWN_GRACE).await
    });
    // Not a plain drop: `tokio::io::stdin` parks an uncancelable blocking read,
    // so dropping the runtime hangs exit until the player presses Enter.
    runtime.shutdown_background();
    // No window here, so its half of the contract is vacuously satisfied.
    match lock_ignoring_poison(&outcome).take() {
        Some(Ok(())) => exit_code(true, abandoned),
        Some(Err(err)) => {
            tracing::error!(error = ?err, "the session ended with a fatal error");
            eprintln!("Fatal error: {}", err.report());
            exit_code(true, true)
        }
        // No verdict means the task never reached the line that writes one, so
        // `teardown_failed` took one of its two failure arms — a `JoinError`,
        // or the grace expiring — and `abandoned` is true here by construction.
        // Both arms log; this one also says it on the terminal, because that is
        // where this build's player is looking.
        None => {
            eprintln!(
                "The session did not finish within {} s of being asked to stop; exiting anyway.",
                TEARDOWN_GRACE.as_secs()
            );
            exit_code(true, abandoned)
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

    use arkyve_refresh_shop::ui;

    // Read before setup consumes the config: these seed the window's timing
    // editor, which has no controller home.
    let seed_timings = config.actuator.timings;
    // Same reason, one step stronger: nothing downstream of `setup` keeps these
    // two at all — the backend is baked into a `Surface` moved into the
    // executor — so this is the only moment the window can learn what a restart
    // would currently do. `game_port` is not among them on purpose: it has no
    // widget, so the window has nothing to seed. See `ui::editor::startup`.
    let seed_click_mode = arkyve_refresh_shop::actuator::ClickMode {
        dry_run: config.actuator.dry_run,
        backend: config.actuator.backend,
    };
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
            // Width is pinned: min and max agree, so the window is only ever
            // 440pt wide and the panels are laid out against a single known
            // measure. Height stays free — the journal is the one surface that
            // benefits from more room — and opens at the height the layout was
            // tuned at rather than at the minimum.
            //
            // **`with_max_inner_size` alone does NOT pin it, and every width
            // decision in `src/ui/` rests on that pin.** Windows honours the max
            // for a DRAG of the frame and ignores it for a maximize: seen at
            // ~2000pt with the button, which stretches the token cards into
            // banners and parks an open row's verbs a screen away from the
            // criteria they act on. The button is withdrawn rather than the
            // layout being made fluid — a second width is a second set of
            // measurements for a single-user tool that sits beside the game.
            //
            // This does not close every route (`Win`+`Up` and a title-bar
            // double-click remain); it closes the one a player finds.
            viewport: eframe::egui::ViewportBuilder::default()
                .with_inner_size([ui::WINDOW_WIDTH, 824.0])
                .with_min_inner_size([ui::WINDOW_WIDTH, 460.0])
                .with_max_inner_size([ui::WINDOW_WIDTH, 10_000.0])
                .with_maximize_button(false),
            ..Default::default()
        },
        Box::new(move |cc| {
            Ok(Box::new(ui::ShopApp::new(
                cc,
                handles,
                error,
                seed_timings,
                seed_click_mode,
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
