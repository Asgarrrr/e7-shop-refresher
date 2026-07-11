//! Entry point for the Secret Shop relay.

// The windowed build is a real windowed app: no console opens beside the
// window. Everything player-facing flows through the journal/banner; stdout
// and stdin become inert sinks there (the console build keeps them).
#![cfg_attr(all(windows, feature = "gui"), windows_subsystem = "windows")]

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

use arkyve_refresh_shop::{Config, app};

const CONFIG_PATH: &str = "config.toml";

fn main() -> ExitCode {
    // Before anything can panic: capture panics to a file. In the windowed
    // build stdout/stderr are inert, and a panic on a worker task or the
    // capture thread would otherwise vanish (surfacing only as a bare
    // "session ended").
    install_crash_logger();

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

    let config = match Config::load(CONFIG_PATH) {
        Ok(config) => config,
        Err(err) => {
            return fatal(format!(
                "Invalid configuration: {err}\n\nFix config.toml and restart."
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
    run_mode(runtime, config)
}

/// Installs a global panic hook that appends every panic — on any thread,
/// including tokio workers and the capture thread — to `crash.log` next to the
/// exe, then chains the default hook (stderr, useful in the console build).
fn install_crash_logger() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_owned();
        let location = info
            .location()
            .map(|l| l.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let entry = crash_entry(
            epoch_secs(),
            &thread,
            &location,
            &panic_message(info.payload()),
            &backtrace,
        );
        let _ = append_crash_log(&crash_log_path(), &entry);
        default_hook(info);
    }));
}

/// The panic payload as text (panics carry `&str` or `String`).
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_owned())
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One crash.log record. Pure (time passed in) so it can be tested.
fn crash_entry(
    epoch_secs: u64,
    thread: &str,
    location: &str,
    message: &str,
    backtrace: &str,
) -> String {
    format!(
        "=== panic (epoch {epoch_secs}s) ===\nthread: {thread}\nlocation: {location}\nmessage: {message}\nbacktrace:\n{backtrace}\n\n"
    )
}

fn crash_log_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("crash.log")))
        .unwrap_or_else(|| std::path::PathBuf::from("crash.log"))
}

/// Appends one record, creating the file if needed. Best-effort: the panic
/// hook must never itself panic.
fn append_crash_log(path: &std::path::Path, entry: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(entry.as_bytes())
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
fn run_mode(runtime: tokio::runtime::Runtime, config: Config) -> ExitCode {
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
fn run_mode(runtime: tokio::runtime::Runtime, config: Config) -> ExitCode {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use arkyve_refresh_shop::ui;

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
        "Arkyve Refresh Shop",
        eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default().with_inner_size([640.0, 640.0]),
            ..Default::default()
        },
        Box::new(move |_cc| Ok(Box::new(ui::ShopApp::new(handles, error)))),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_entry_captures_thread_location_and_message() {
        let entry = crash_entry(
            42,
            "capture",
            "src/capture/windivert.rs:60",
            "recv failed",
            "<backtrace>",
        );
        assert!(entry.contains("epoch 42s"));
        assert!(entry.contains("thread: capture"));
        assert!(entry.contains("location: src/capture/windivert.rs:60"));
        assert!(entry.contains("message: recv failed"));
        assert!(entry.contains("<backtrace>"));
    }

    #[test]
    fn append_crash_log_creates_and_appends() {
        let path =
            std::env::temp_dir().join(format!("arkyve_crash_test_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        append_crash_log(&path, "first\n").unwrap();
        append_crash_log(&path, "second\n").unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("first"));
        assert!(body.contains("second"));
        let _ = std::fs::remove_file(&path);
    }
}
