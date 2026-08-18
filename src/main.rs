//! Entry point for the Secret Shop relay.

// The windowed build is a real windowed app: no console opens beside the
// window. Everything player-facing flows through the journal/banner; stdout
// and stdin become inert sinks there (the console build keeps them).
#![cfg_attr(all(windows, feature = "gui"), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::process::ExitCode;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::BoxMakeWriter;

use arkyve_refresh_shop::{Config, app, crash};

// The argv token this dispatch answers to, taken from the module that also
// writes it (`broker::broker_command_line`, used by `capture::elevate`) rather
// than spelled a second time here. `broker` is declared unconditionally for
// exactly this: `capture::elevate` is a private module behind
// `windivert-backend`, while this dispatch has to exist in builds that have no
// backend at all — refusing the flag there is the point — so the shared home had
// to be somewhere both can reach.
use arkyve_refresh_shop::broker::BROKER_ARGV_FLAG;

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

/// This process's arguments, when they ask for the broker role.
///
/// `args_os` plus a lossy conversion rather than `std::env::args()`, which
/// *panics* on an argument that is not valid Unicode. This runs before
/// `crash::install()` — deliberately, see `main` — so that panic would be
/// invisible in every channel the product has, and a replacement character
/// cannot pass the validators downstream anyway.
fn capture_broker_argv() -> Option<Vec<String>> {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    args.iter()
        .any(|arg| arg == BROKER_ARGV_FLAG)
        .then_some(args)
}

/// The broker role, on a build that has a capture backend.
///
/// Every argument is validated by `broker`'s own validators rather than by a
/// second parser written here. Two parsers that could disagree about what
/// `--pipe` accepts is precisely the drift this design cannot afford: that
/// command line is the entire surface the medium-integrity side has on an
/// administrator process, and the side that *runs* elevated has to be the side
/// that decides what it accepts.
#[cfg(all(windows, feature = "windivert-backend"))]
fn run_capture_broker(args: Vec<String>) -> ExitCode {
    let outcome = parse_broker_command(&args)
        .and_then(|(port, nonce, ui_pid)| arkyve_refresh_shop::broker::run(port, &nonce, ui_pid));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Not `fatal()`: that installs a tracing event this process has no
            // subscriber for and opens an error window this process must never
            // own. What actually reaches the player is the kind-2 frame
            // `broker::run` wrote down the pipe before returning; this line is
            // for a developer running the broker by hand from a console build.
            eprintln!("capture broker: {err}");
            ExitCode::FAILURE
        }
    }
}

/// The broker role on a build compiled without `windivert-backend`.
///
/// It cannot capture: there is no driver, no `WinDivertSource`, and no pipe
/// server. Exiting is the only honest answer — falling through into the GUI
/// would open a *second* window, elevated, while the window that asked for
/// capture sat waiting for a channel nobody would ever serve. In practice only
/// a human typing the flag reaches this: the code that writes it lives behind
/// the same feature.
#[cfg(not(all(windows, feature = "windivert-backend")))]
fn run_capture_broker(_args: Vec<String>) -> ExitCode {
    eprintln!(
        "{BROKER_ARGV_FLAG} needs the `windivert-backend` feature: this build has no capture \
         backend, so it cannot serve a capture channel."
    );
    ExitCode::FAILURE
}

/// Pulls `--port`, `--pipe` and `--ui-pid` out of the broker command line.
///
/// Returns the three validated values as a tuple rather than a named struct so
/// that the no-backend build above, which never calls this, does not carry
/// fields nothing reads (every lane is `-D warnings`).
#[cfg(all(windows, feature = "windivert-backend"))]
fn parse_broker_command(args: &[String]) -> arkyve_refresh_shop::Result<(u16, String, u32)> {
    use arkyve_refresh_shop::Error;
    use arkyve_refresh_shop::broker::{parse_pipe_nonce, parse_port, parse_ui_pid};

    let (mut port, mut nonce, mut ui_pid) = (None, None, None);
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            BROKER_ARGV_FLAG => {}
            "--port" => port = Some(parse_port(next_value(&mut rest, "--port")?)?),
            "--pipe" => {
                nonce = Some(parse_pipe_nonce(next_value(&mut rest, "--pipe")?)?.to_owned())
            }
            "--ui-pid" => ui_pid = Some(parse_ui_pid(next_value(&mut rest, "--ui-pid")?)?),
            // The offending token is deliberately not echoed, for the same
            // reason `parse_pipe_nonce` does not echo its own: a mistyped
            // `--pipe` makes the *nonce* the unknown argument, and a shared
            // secret must not be what an error message hands to whoever is
            // reading the output.
            _ => {
                return Err(Error::Capture(
                    "unexpected argument on the capture broker command line".to_owned(),
                ));
            }
        }
    }
    match (port, nonce, ui_pid) {
        (Some(port), Some(nonce), Some(ui_pid)) => Ok((port, nonce, ui_pid)),
        _ => Err(Error::Capture(
            "the capture broker needs --port, --pipe and --ui-pid".to_owned(),
        )),
    }
}

/// The token following a flag, or a named error when the flag ends the line.
#[cfg(all(windows, feature = "windivert-backend"))]
fn next_value<'a>(
    rest: &mut impl Iterator<Item = &'a String>,
    flag: &str,
) -> arkyve_refresh_shop::Result<&'a str> {
    rest.next().map(String::as_str).ok_or_else(|| {
        arkyve_refresh_shop::Error::Capture(format!("{flag} needs a value on the command line"))
    })
}

fn main() -> ExitCode {
    // The very first thing, ahead of the panic hook, the rustls provider, the
    // log subscriber and the config read — because the broker role must do none
    // of them. It installs its own `crash::install()` (one hook, not two), it
    // opens no TLS connection, it has no subscriber to write to (which is why
    // its diagnostics travel down the pipe as frames), and above all it does not
    // read `config.toml`: that file lives in per-user roaming app-data, is
    // writable by any medium-integrity process on the machine, and this is the
    // process holding a kernel driver's handle. Its whole input is the three
    // argv tokens below.
    if let Some(args) = capture_broker_argv() {
        return run_capture_broker(args);
    }

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

    let config_path = config_path();
    // Emitted before the config is even read: a run that dies on an invalid
    // config must still leave its identity in the log file.
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        os = %std::env::consts::OS,
        gui = cfg!(feature = "gui"),
        // Both backends, not just WinDivert: `pcap-backend` is the shipped
        // default now, so logging only the other one would put "no capture
        // backend" in every bug report we receive.
        pcap = cfg!(feature = "pcap-backend"),
        windivert = cfg!(feature = "windivert-backend"),
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
    // filter is a constant inside the elevated broker now — that is what keeps a
    // string from a world-writable file out of a kernel driver's filter compiler
    // — and the receive buffer is pinned to the driver's own maximum. They are
    // still accepted because deleting them would make `Config::load` fail on
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

/// The argv dispatch, and nothing else: everything past it needs a window, a
/// runtime or a driver.
///
/// Gated with the broker itself. The three *values* are validated by
/// `broker::parse_*`, which is unconditional and tested in every lane; what is
/// tested here is only the walk over the command line that feeds them.
#[cfg(all(test, windows, feature = "windivert-backend"))]
mod tests {
    use super::*;

    fn argv(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    /// The one test that ties the two halves of the command line together.
    ///
    /// Everything else in this module feeds the parser literals. This feeds it
    /// the *producer's* output: `broker::broker_command_line` is what
    /// `capture::elevate` hands to `ShellExecuteExW`, and `parse_broker_command`
    /// is what the elevated copy reads it back with. Rename a flag on either
    /// side, reorder the tokens, quote a value — and this fails, where two
    /// independent sets of literals would both have kept passing while the
    /// product silently opened a second elevated window and timed out on a
    /// channel nobody served.
    #[test]
    fn the_command_line_the_launcher_writes_is_the_one_the_dispatch_parses() {
        use arkyve_refresh_shop::broker::broker_command_line;

        let line = broker_command_line(3333, NONCE, 4242);
        // Split the way the shell splits argv before `capture_broker_argv` sees
        // it, so a value that grew a space would break here too.
        let args = argv(&line);
        assert!(
            args.iter().any(|arg| arg == BROKER_ARGV_FLAG),
            "the dispatch would never recognise its own command line: {line}"
        );
        assert_eq!(
            parse_broker_command(&args).expect("the launcher's own command line must parse"),
            (3333, NONCE.to_owned(), 4242)
        );
    }

    #[test]
    fn a_complete_broker_command_line_yields_the_three_validated_values() {
        let args = argv(&format!(
            "--capture-broker --port 3333 --pipe {NONCE} --ui-pid 4242"
        ));
        let (port, nonce, ui_pid) = parse_broker_command(&args).expect("a well-formed command");
        assert_eq!(port, 3333);
        assert_eq!(nonce, NONCE);
        assert_eq!(ui_pid, 4242);
    }

    #[test]
    fn the_order_of_the_three_arguments_does_not_matter() {
        let args = argv(&format!(
            "--ui-pid 7 --pipe {NONCE} --capture-broker --port 1"
        ));
        assert_eq!(
            parse_broker_command(&args).expect("order is not a contract"),
            (1, NONCE.to_owned(), 7)
        );
    }

    #[test]
    fn a_missing_argument_is_refused_rather_than_defaulted() {
        // Defaulting any of the three would mean the elevated side inventing a
        // port, a pipe name or a process to watch.
        for line in [
            "--capture-broker".to_owned(),
            "--capture-broker --port 3333".to_owned(),
            format!("--capture-broker --port 3333 --pipe {NONCE}"),
            format!("--capture-broker --pipe {NONCE} --ui-pid 42"),
        ] {
            let error = parse_broker_command(&argv(&line))
                .expect_err("an incomplete command line must be refused");
            assert!(error.to_string().contains("--port"), "{line}: {error}");
        }
    }

    #[test]
    fn a_flag_with_no_value_after_it_is_refused_by_name() {
        let args = argv(&format!(
            "--capture-broker --pipe {NONCE} --ui-pid 42 --port"
        ));
        let error = parse_broker_command(&args).expect_err("a dangling flag");
        assert!(error.to_string().contains("--port"), "{error}");
    }

    #[test]
    fn the_broker_validators_are_the_ones_that_decide_what_is_acceptable() {
        // Not a second parser: these three all fail inside `broker::parse_*`,
        // which is what the elevated side itself enforces.
        for line in [
            format!("--capture-broker --port 0 --pipe {NONCE} --ui-pid 42"),
            format!("--capture-broker --port 70000 --pipe {NONCE} --ui-pid 42"),
            "--capture-broker --port 3333 --pipe nothex --ui-pid 42".to_owned(),
            format!("--capture-broker --port 3333 --pipe {NONCE} --ui-pid 0"),
        ] {
            assert!(
                parse_broker_command(&argv(&line)).is_err(),
                "{line} must not be accepted"
            );
        }
    }

    #[test]
    fn an_unknown_argument_is_refused_without_echoing_it_back() {
        // A mistyped `--pipe` turns the nonce itself into the unknown argument;
        // a message that quoted it would be the thing that leaked the secret.
        let args = argv(&format!(
            "--capture-broker --pipes {NONCE} --port 1 --ui-pid 2"
        ));
        let error = parse_broker_command(&args).expect_err("an unknown flag");
        let message = error.to_string();
        assert!(!message.contains(NONCE), "the message leaked the nonce");
        assert!(!message.contains("--pipes"), "{message}");
    }
}
