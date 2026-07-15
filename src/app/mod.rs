//! Orchestration: capture -> reassembly -> gate -> uplink -> controller -> display.

mod session;

use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::actuator::{ActuatorHandle, Mode, SnapshotEpoch, plan};
use crate::capture::{Direction, PacketSource, Segment};
use crate::config::ForwardConfig;
use crate::domain::control::{Controller, Limits};
use crate::domain::filter::Filter;
use crate::journal::EventLog;
use crate::render::print_controls;
use crate::stream::Reassembler;
use crate::uplink::UplinkEvent;
use crate::watch::WatchGate;
use crate::{Config, Result};

use session::session_loop;

/// Event flowing from the capture thread to reassembly.
enum CaptureEvent {
    /// A TCP segment to reassemble.
    Segment(Segment),
    /// Shop Watch was just re-enabled after a pause: the reassembler must
    /// re-anchor a fresh origin (the bytes during the pause are lost).
    Resync,
}

/// A command into the session loop, decoupled from its source: the stdin
/// task and the GUI push the same values through the same channel (stdin
/// never produces the `Set*` variants); the actuator produces only
/// `ActuatorFailed`.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Start,
    Stop,
    /// Start or stop, depending on the current status.
    Toggle,
    /// Live filter retune; applies from the next new shop.
    SetFilter(Filter),
    /// Live limits retune; checked before the next refresh.
    SetLimits(Limits),
    /// Live click-timing retune; applies to the next queued job.
    SetTimings(plan::Timings),
    /// Actuator-side halt; the executor is its only producer.
    ActuatorFailed,
}

/// Cheap clones of the shared session state, for a view (the GUI) running
/// beside the session loop: read `status()`/`progress()`/`last_snapshot()`/
/// `checklist()` under short locks, send [`Command`]s, read the journal.
#[derive(Clone)]
pub struct SessionHandles {
    pub controller: Arc<Mutex<Controller>>,
    pub commands: mpsc::Sender<Command>,
    pub gate: WatchGate,
    pub journal: EventLog,
}

/// The owned half of [`setup`]: everything the relay pipeline consumes.
pub struct Session {
    config: Config,
    handles: SessionHandles,
    command_rx: mpsc::Receiver<Command>,
    actuator: ActuatorHandle,
    job_rx: mpsc::Receiver<plan::Job>,
}

/// Builds the shared session state and hands out clones before any fallible
/// work runs: a view keeps live handles even when [`Session::run`] fails
/// later (bad filter, no capture backend).
pub fn setup(config: Config) -> (Session, SessionHandles) {
    // Gate off at startup: the session starts Idle and the player arms it
    // with `start`.
    let gate = WatchGate::new(false);
    let journal = EventLog::default();
    let (command_tx, command_rx) = mpsc::channel::<Command>(16);
    let mut controller = Controller::new(config.filter.clone(), config.limits.clone());
    if actuator_mode(&config) == Mode::Live {
        // Only real clicking gets watchdog deadlines: Off is player-paced
        // advice and DryRun never produces wire feedback — a deadline would
        // self-halt both.
        controller.enable_recovery();
    }
    let controller = Arc::new(Mutex::new(controller));
    let handles = SessionHandles {
        controller,
        commands: command_tx,
        gate,
        journal,
    };
    let (job_tx, job_rx) = mpsc::channel::<plan::Job>(8);
    let timings = Arc::new(Mutex::new(config.actuator.timings));
    let actuator = ActuatorHandle::new(
        actuator_mode(&config),
        SnapshotEpoch::default(),
        job_tx,
        timings,
    );
    let session = Session {
        config,
        handles: handles.clone(),
        command_rx,
        actuator,
        job_rx,
    };
    (session, handles)
}

/// Live clicking unless the config asks for a dry run.
#[cfg(all(windows, feature = "actuator"))]
fn actuator_mode(config: &Config) -> Mode {
    if config.actuator.dry_run {
        Mode::DryRun
    } else {
        Mode::Live
    }
}

/// No input backend compiled: decisions stay advice whatever the config
/// says.
#[cfg(not(all(windows, feature = "actuator")))]
fn actuator_mode(_config: &Config) -> Mode {
    Mode::Off
}

/// Console-only entry point: [`setup`] + [`Session::run`], discarding the
/// view handles.
pub async fn run(config: Config) -> Result<()> {
    // The console has no filter editor: an unrestricted filter can only be
    // fixed in config.toml, so fail fast. The GUI path (setup +
    // `Session::run`) boots instead and refuses arming until a filter is set.
    if config.filter.is_unrestricted() {
        return Err(crate::Error::Config(
            "no [filter] criteria in config.toml — define what to hunt (see config.example.toml)"
                .to_owned(),
        ));
    }
    let (session, _handles) = setup(config);
    session.run().await
}

impl Session {
    /// Runs the relay and blocks until shutdown (Ctrl+C or end of stream).
    pub async fn run(self) -> Result<()> {
        let Self {
            config,
            handles,
            command_rx,
            actuator,
            job_rx,
        } = self;
        let SessionHandles {
            controller,
            commands,
            gate,
            journal,
        } = handles;
        let (segment_tx, segment_rx) = mpsc::channel::<CaptureEvent>(8_192);
        let (raw_tx, raw_rx) = mpsc::channel::<Vec<u8>>(1_024);
        let (message_tx, message_rx) = mpsc::channel::<UplinkEvent>(256);
        // One fatal-failure channel, several producers: the capture thread
        // (recv error or panic) and a supervisor per worker task. The session
        // loop breaks on the first message and reports it.
        let (fatal_tx, fatal_rx) = mpsc::channel::<String>(4);

        // Blocking capture on a dedicated thread (WinDivert::recv is synchronous).
        // Wrapped in catch_unwind so a panic surfaces as a fatal message instead
        // of a silently dropped sender (which reads as a clean "session ended").
        let source = build_source(&config)?;
        let capture_gate = gate.clone();
        let capture_fatal = fatal_tx.clone();
        std::thread::Builder::new()
            .name("capture".to_owned())
            .spawn(move || {
                let fatal = capture_fatal.clone();
                let run = std::panic::AssertUnwindSafe(|| {
                    capture_loop(source, segment_tx, capture_gate, capture_fatal);
                });
                if std::panic::catch_unwind(run).is_err() {
                    let _ = fatal.blocking_send("capture thread panicked".to_owned());
                }
            })?;

        // Server link with automatic reconnection.
        supervise_task(
            "uplink",
            &fatal_tx,
            tokio::spawn(crate::uplink::run(
                config.server_url.clone(),
                raw_rx,
                message_tx,
                config.reconnect_initial(),
                config.reconnect_max(),
            )),
        );

        // Reassembly + filtering of the directions to forward.
        supervise_task(
            "reassembly",
            &fatal_tx,
            tokio::spawn(reassemble_loop(segment_rx, raw_tx, config.forward.clone())),
        );

        // Click jobs -> the game window, through the configured backend.
        #[cfg(all(windows, feature = "actuator"))]
        {
            use crate::actuator::run_executor;
            use crate::actuator::win::{MessageSurface, WinSurface};
            use crate::config::ActuatorBackend;
            let dry_run = actuator.mode == Mode::DryRun;
            let task = match config.actuator.backend {
                ActuatorBackend::Input => tokio::spawn(run_executor(
                    WinSurface,
                    job_rx,
                    gate.clone(),
                    actuator.epoch.clone(),
                    journal.clone(),
                    commands.clone(),
                    dry_run,
                )),
                ActuatorBackend::Message => tokio::spawn(run_executor(
                    MessageSurface::default(),
                    job_rx,
                    gate.clone(),
                    actuator.epoch.clone(),
                    journal.clone(),
                    commands.clone(),
                    dry_run,
                )),
            };
            supervise_task("actuator", &fatal_tx, task);
        }
        // Without an input backend the mode is Off and nothing ever submits.
        #[cfg(not(all(windows, feature = "actuator")))]
        drop(job_rx);

        // Keyboard input, decoupled from the session loop through the channel.
        supervise_task("stdin", &fatal_tx, tokio::spawn(stdin_loop(commands)));

        info!(server = %config.server_url, "relay started — idle, `start` arms the watch");
        print_controls();

        let fatal = session_loop(
            &controller,
            &gate,
            &journal,
            &actuator,
            command_rx,
            message_rx,
            fatal_rx,
        )
        .await;
        info!("relay stopped");
        // A supervised task dying is a failure, not a clean end: the banner and
        // the exit code must say so instead of "session ended".
        match fatal {
            Some(error) => Err(crate::Error::Fatal(error)),
            None => Ok(()),
        }
    }
}

/// Watches a worker task and reports a *panic* as a fatal message. A normal
/// return is not reported: a worker only ends when the pipeline is already
/// shutting down, and the true cause (a capture failure, Ctrl+C) surfaces on
/// its own — reporting the follow-on exit would just be noise.
fn supervise_task(
    name: &'static str,
    fatal: &mpsc::Sender<String>,
    handle: tokio::task::JoinHandle<()>,
) {
    let fatal = fatal.clone();
    tokio::spawn(async move {
        if let Err(err) = handle.await
            && err.is_panic()
        {
            let _ = fatal.send(format!("{name} task panicked")).await;
        }
    });
}

/// Awaits the session future (spawned so a panic is caught, not propagated)
/// and translates its end into a banner line plus a failed flag.
///
/// The gate is forced off on every path: a panicking session never reaches
/// the loop's own teardown, and capture must not keep streaming game traffic
/// under a crash banner. Idempotent after a clean teardown.
pub async fn supervise(
    session: impl Future<Output = Result<()>> + Send + 'static,
    gate: WatchGate,
) -> (String, bool) {
    let outcome = tokio::spawn(session).await;
    gate.set(false);
    match outcome {
        Ok(Ok(())) => (
            "session ended — restart the app to reconnect".to_owned(),
            false,
        ),
        Ok(Err(err)) => (format!("session error: {err}"), true),
        Err(panic) => (format!("session crashed: {panic}"), true),
    }
}

/// Consumes capture events, reassembles, forwards the ordered stream.
async fn reassemble_loop(
    mut events: mpsc::Receiver<CaptureEvent>,
    raw_tx: mpsc::Sender<Vec<u8>>,
    forward: ForwardConfig,
) {
    let mut reassembler = Reassembler::new();
    while let Some(event) = events.recv().await {
        let segment = match event {
            CaptureEvent::Resync => {
                reassembler.clear();
                continue;
            }
            CaptureEvent::Segment(segment) => segment,
        };
        if !should_forward(segment.direction, &forward) {
            continue;
        }
        let ordered = reassembler.push(&segment);
        if ordered.is_empty() {
            continue;
        }
        if raw_tx.send(ordered).await.is_err() {
            break; // uplink gone.
        }
    }
}

fn should_forward(direction: Direction, forward: &ForwardConfig) -> bool {
    match direction {
        Direction::ServerToClient => forward.server_to_client,
        Direction::ClientToServer => forward.client_to_server,
    }
}

/// Capture loop (synchronous context). Stops when the pipeline closes.
///
/// A recv error ends the loop AND is reported through `fatal`: tracing is
/// inert in the windowed build, so the session loop must journal the failure
/// and turn it into an error outcome the player can see.
fn capture_loop(
    mut source: Box<dyn PacketSource>,
    tx: mpsc::Sender<CaptureEvent>,
    gate: WatchGate,
    fatal: mpsc::Sender<String>,
) {
    let mut was_enabled = gate.is_enabled();
    loop {
        let segment = match source.next_segment() {
            Ok(segment) => segment,
            Err(err) => {
                error!(error = %err, "capture interrupted");
                let _ = fatal.blocking_send(format!("capture: {err}"));
                break;
            }
        };

        let enabled = gate.is_enabled();
        // Off -> on transition: request a resync before emitting, otherwise the
        // reassembler treats the sequence jump as an unfillable gap and never
        // delivers anything again.
        if enabled && !was_enabled && tx.blocking_send(CaptureEvent::Resync).is_err() {
            break;
        }
        was_enabled = enabled;

        if !enabled {
            continue; // Shop Watch off: emit nothing.
        }
        if tx.blocking_send(CaptureEvent::Segment(segment)).is_err() {
            break;
        }
    }
}

/// Reads stdin lines and forwards them as [`Command`]s; the session loop never
/// touches stdin.
async fn stdin_loop(commands: mpsc::Sender<Command>) {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match parse_command(&line) {
            Some(command) => {
                if commands.send(command).await.is_err() {
                    break; // session loop gone.
                }
            }
            None => println!(
                ">> unknown command: {:?} (start, stop, enter = toggle)",
                line.trim()
            ),
        }
    }
}

fn parse_command(line: &str) -> Option<Command> {
    match line.trim().to_ascii_lowercase().as_str() {
        "" | "t" | "toggle" => Some(Command::Toggle),
        "on" | "start" => Some(Command::Start),
        "off" | "stop" => Some(Command::Stop),
        _ => None,
    }
}

#[cfg(all(windows, feature = "windivert-backend"))]
fn build_source(config: &Config) -> Result<Box<dyn PacketSource>> {
    use crate::capture::WinDivertSource;
    let filter = config.capture_filter();
    info!(filter = %filter, "opening WinDivert capture (admin required)");
    let source = WinDivertSource::open(&filter, config.game_port, config.capture.buffer_size)?;
    Ok(Box::new(source))
}

#[cfg(not(all(windows, feature = "windivert-backend")))]
fn build_source(_config: &Config) -> Result<Box<dyn PacketSource>> {
    Err(crate::Error::Capture(
        "no capture backend compiled — enable the `windivert-backend` feature on Windows"
            .to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::control::Status;

    #[tokio::test]
    async fn run_refuses_unrestricted_filter() {
        let err = run(Config::default()).await.expect_err("must refuse");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn setup_starts_idle_with_gate_off() {
        let (_session, handles) = setup(Config::default());
        assert_eq!(handles.controller.lock().unwrap().status(), Status::Idle);
        assert!(!handles.gate.is_enabled());
        assert!(handles.journal.entries().is_empty());
        // The command channel is wired before the fallible pipeline runs.
        handles
            .commands
            .try_send(Command::Toggle)
            .expect("channel open");
    }

    /// Without an input backend the mode is Off — advice only, so watchdog
    /// deadlines would halt a session nobody clicks for.
    #[cfg(not(all(windows, feature = "actuator")))]
    #[test]
    fn setup_enables_recovery_only_when_live() {
        let (_session, handles) = setup(Config::default());
        assert!(!handles.controller.lock().unwrap().recovery_enabled());
    }

    /// Live clicking arms the watchdog; a dry run produces no wire feedback
    /// and must keep it dark.
    #[cfg(all(windows, feature = "actuator"))]
    #[test]
    fn setup_enables_recovery_only_when_live() {
        let (_session, handles) = setup(Config::default());
        assert!(handles.controller.lock().unwrap().recovery_enabled());

        let mut config = Config::default();
        config.actuator.dry_run = true;
        let (_session, handles) = setup(config);
        assert!(!handles.controller.lock().unwrap().recovery_enabled());
    }

    async fn panicking_session() -> crate::Result<()> {
        panic!("boom")
    }

    #[tokio::test]
    async fn supervise_task_reports_a_panic() {
        let (fatal_tx, mut fatal_rx) = mpsc::channel::<String>(4);
        supervise_task("uplink", &fatal_tx, tokio::spawn(async { panic!("boom") }));
        assert_eq!(
            fatal_rx.recv().await.as_deref(),
            Some("uplink task panicked")
        );
    }

    #[tokio::test]
    async fn supervise_task_ignores_a_clean_exit() {
        // A worker returning normally is the shutdown cascade, not a failure:
        // it must not surface as a fatal message.
        let (fatal_tx, mut fatal_rx) = mpsc::channel::<String>(4);
        supervise_task("uplink", &fatal_tx, tokio::spawn(async {}));
        drop(fatal_tx);
        assert_eq!(fatal_rx.recv().await, None);
    }

    #[tokio::test]
    async fn supervise_forces_gate_off_after_a_panic() {
        let gate = WatchGate::new(true);
        let (outcome, failed) = supervise(panicking_session(), gate.clone()).await;
        assert!(!gate.is_enabled());
        assert!(failed);
        assert!(outcome.contains("session crashed"));
    }

    #[tokio::test]
    async fn supervise_reports_clean_end_without_failure() {
        let gate = WatchGate::new(true);
        let (outcome, failed) = supervise(async { Ok(()) }, gate.clone()).await;
        assert!(!gate.is_enabled());
        assert!(!failed);
        assert!(outcome.contains("session ended"));
    }

    #[test]
    fn parse_command_maps_aliases() {
        assert_eq!(parse_command("start"), Some(Command::Start));
        assert_eq!(parse_command("on"), Some(Command::Start));
        assert_eq!(parse_command("stop"), Some(Command::Stop));
        assert_eq!(parse_command("off"), Some(Command::Stop));
        assert_eq!(parse_command("toggle"), Some(Command::Toggle));
        assert_eq!(parse_command("t"), Some(Command::Toggle));
        assert_eq!(parse_command(""), Some(Command::Toggle));
    }

    #[test]
    fn parse_command_trims_and_ignores_case() {
        assert_eq!(parse_command("  START \t"), Some(Command::Start));
        assert_eq!(parse_command("Stop"), Some(Command::Stop));
    }

    #[test]
    fn parse_command_rejects_unknown() {
        assert_eq!(parse_command("refresh"), None);
        assert_eq!(parse_command("sta rt"), None);
        // The skip command is gone: buying (or a fresh shop) is the only
        // way out of a pause.
        assert_eq!(parse_command("resume"), None);
        assert_eq!(parse_command("r"), None);
    }
}
