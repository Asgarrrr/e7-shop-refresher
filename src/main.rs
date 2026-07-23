//! Entry point for the Secret Shop relay.

// The windowed build is a real windowed app: no console opens beside the
// window. Everything player-facing flows through the journal/banner; stdout
// and stdin become inert sinks there (the console build keeps them).
#![cfg_attr(all(windows, feature = "gui"), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

use arkyve_refresh_shop::{Config, app, crash};

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

fn main() -> ExitCode {
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

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("arkyve_refresh_shop=info,warn")),
        )
        .with_target(false)
        .init();

    let config_path = config_path();
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

/// Every fatal error before the main window opens lands here: stderr always,
/// an error window in the windowed build (which has no console to read).
fn fatal(message: String) -> ExitCode {
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

    use arkyve_refresh_shop::ui;

    // Capture the startup timings before setup consumes the config: they seed
    // the window's timing editor (no controller home for them).
    let seed_timings = config.actuator.timings;
    let (session, handles) = app::setup(config);
    let error = ui::SessionErrorSlot::default();
    let failed = Arc::new(AtomicBool::new(false));
    let (slot, flag) = (error.clone(), failed.clone());
    let gate = handles.gate.clone();
    runtime.spawn(async move {
        // app::supervise catches a session panic (it must land in the banner,
        // not vanish with a discarded JoinHandle) and forces the gate off.
        let (outcome, session_failed) = app::supervise(session.run(), gate).await;
        if session_failed {
            flag.store(true, Ordering::Relaxed);
        }
        *slot.lock().expect("error slot poisoned") = Some(outcome);
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
    // Not a plain drop: tokio::io::stdin parks a blocking thread, and dropping
    // the runtime would hang window close until the player presses Enter.
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
