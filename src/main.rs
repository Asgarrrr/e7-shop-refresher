//! Entry point for the Secret Shop relay.

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

use arkyve_refresh_shop::{Config, app};

const CONFIG_PATH: &str = "config.toml";

fn main() -> ExitCode {
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
            eprintln!("Invalid configuration: {err}");
            // In the windowed build the console may not be readable (or
            // visible at all): show the error where the player looks.
            #[cfg(feature = "gui")]
            let _ = arkyve_refresh_shop::ui::show_fatal(format!("Invalid configuration: {err}"));
            return ExitCode::FAILURE;
        }
    };

    // The runtime is built by hand instead of #[tokio::main]: in the GUI
    // build, eframe/winit must own the OS main thread.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("Fatal error: {err}");
            return ExitCode::FAILURE;
        }
    };
    run_mode(runtime, config)
}

/// Console-only build: the session blocks the main thread, as before.
#[cfg(not(feature = "gui"))]
fn run_mode(runtime: tokio::runtime::Runtime, config: Config) -> ExitCode {
    match runtime.block_on(app::run(config)) {
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
    runtime.spawn(async move {
        // Inner task: a panic in the session must land in the banner too,
        // not vanish with a discarded JoinHandle.
        let outcome = match tokio::spawn(session.run()).await {
            Ok(Ok(())) => "session ended — restart the app to reconnect".to_owned(),
            Ok(Err(err)) => {
                flag.store(true, Ordering::Relaxed);
                format!("session error: {err}")
            }
            Err(panic) => {
                flag.store(true, Ordering::Relaxed);
                format!("session crashed: {panic}")
            }
        };
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
