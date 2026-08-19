//! Orchestration: capture -> reassembly -> gate -> uplink -> controller -> display.
//!
//! The root holds the *wiring*: the session state handed out before any fallible
//! work runs, the channels that join the five concerns below, and the two entry
//! points `main.rs` calls. Each submodule owns one of those concerns, reaching
//! each other only through the channel ends and `Arc` handles created in
//! [`Session::run`] — there is no shared `&mut` state between them.
//!
//! - [`pressure`] — the vocabulary the two pumps share (`CaptureEvent`, the
//!   resync marker protocol).
//! - [`ingest`] — the blocking capture loop.
//! - [`reassembly`] — the post-resync anchor window and the forwarding ladder.
//! - [`workers`] — who owns the threads and tasks, and teardown order.
//! - [`console`] — stdin lines in, [`Command`]s out.
//! - `session` — the session loop itself, predates this split and keeps its
//!   own structure (the only place in the crate that nests two locks,
//!   `controller` -> `timings`).

mod console;
#[cfg(test)]
mod fixtures;
mod ingest;
mod pressure;
mod reassembly;
mod session;
mod workers;

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use futures_util::FutureExt;
use tokio::sync::{mpsc, watch};
use tracing::info;

use crate::actuator::{ActuatorHandle, Mode, SnapshotEpoch, plan};
use crate::capture::CaptureSource;
use crate::domain::control::{Controller, Limits};
use crate::domain::filter::Filter;
use crate::journal::EventLog;
use crate::render::print_controls;
use crate::stream::{BudgetedChunk, PipelineBudget};
use crate::uplink::UplinkEvent;
use crate::watch::WatchGate;
use crate::{Config, Result};

use console::stdin_loop;
use pressure::{CAPTURE_EVENT_QUEUE, CaptureEvent, PressureResync};
use reassembly::reassemble_loop_with_pressure;
use session::session_loop;
use workers::{SessionWorkers, spawn_capture_with_budget};

/// Reassembled chunks awaiting the uplink, and inbound server messages
/// awaiting the session loop. Both stages are byte-capped by
/// [`PipelineBudget`]; the slot count only bounds how far a producer runs
/// ahead.
const PIPELINE_QUEUE: usize = 256;

/// Fatal-failure reports. Several producers, one consumer, first message
/// wins — the depth exists so a racing second report can't block the task
/// that's already unwinding.
const FATAL_QUEUE: usize = 4;

/// Player commands awaiting the session loop. Safety stops do not ride this
/// queue (see [`Command`]), so saturation costs latency, never a missed stop.
const COMMAND_QUEUE: usize = 16;

/// Click jobs awaiting the actuator. Deliberately shallow: a deep queue would
/// let clicks planned against a dead shop pile up behind the epoch check.
const JOB_QUEUE: usize = 8;

/// A non-safety command into the session loop, decoupled from its source: the
/// stdin task and the GUI push the same values through the same channel
/// (stdin never produces the `Set*` variants).
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

/// Cooperative stop for a session running on a detached task.
///
/// The GUI closes its window on the OS main thread while the pipeline lives on
/// the runtime; without this the process would exit and skip teardown, leaving
/// an orphaned live capture session in the driver. [`Command::Stop`] only
/// disarms the hunt — it never leaves the session loop.
///
/// Cloneable (the sender is shared) so the window and the session can each
/// hold one.
#[derive(Clone)]
pub struct ShutdownSignal(Arc<watch::Sender<bool>>);

impl ShutdownSignal {
    fn new() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }

    /// Asks the session loop and every worker to wind down. Idempotent.
    pub fn request(&self) {
        self.0.send_replace(true);
    }

    fn subscribe(&self) -> watch::Receiver<bool> {
        self.0.subscribe()
    }
}

/// The owned half of [`setup`]: everything the relay pipeline consumes.
pub struct Session {
    config: Config,
    handles: SessionHandles,
    command_rx: mpsc::Receiver<Command>,
    actuator: ActuatorHandle,
    job_rx: mpsc::Receiver<plan::Job>,
    shutdown: ShutdownSignal,
}

/// Builds the shared session state and hands out clones before any fallible
/// work runs: a view keeps live handles even when [`Session::run`] fails
/// later (bad filter, no capture backend). The third value is the cooperative
/// stop — see [`ShutdownSignal`].
pub fn setup(config: Config) -> (Session, SessionHandles, ShutdownSignal) {
    // Session starts Idle; the player arms it with `start`.
    let gate = WatchGate::new(false);
    let journal = EventLog::default();
    let (command_tx, command_rx) = mpsc::channel::<Command>(COMMAND_QUEUE);
    let mut controller = Controller::new(config.filter.clone(), config.limits);
    if actuator_mode(&config) == Mode::Live {
        // Only real clicking gets watchdog deadlines: Off and DryRun produce
        // no wire feedback, so a deadline would self-halt both.
        controller.enable_recovery();
    }
    let controller = Arc::new(Mutex::new(controller));
    let handles = SessionHandles {
        controller,
        commands: command_tx,
        gate,
        journal,
    };
    let (job_tx, job_rx) = mpsc::channel::<plan::Job>(JOB_QUEUE);
    let timings = Arc::new(Mutex::new(config.actuator.timings));
    let actuator = ActuatorHandle::new(
        actuator_mode(&config),
        SnapshotEpoch::default(),
        job_tx,
        timings,
    );
    let shutdown = ShutdownSignal::new();
    let session = Session {
        config,
        handles: handles.clone(),
        command_rx,
        actuator,
        job_rx,
        shutdown: shutdown.clone(),
    };
    (session, handles, shutdown)
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
///
/// # Errors
///
/// [`crate::Error::Config`] when `[filter]` names no criteria — the console has
/// no editor, so an unrestricted filter can only be fixed in `config.toml` and
/// failing fast beats booting a watch that can never arm. Otherwise whatever
/// [`Session::run`] returns.
pub async fn run(config: Config) -> Result<()> {
    // The GUI path (setup + `Session::run`) boots instead and refuses arming
    // until a filter is set.
    if config.filter.is_unrestricted() {
        return Err(crate::Error::Config(
            "no [filter] criteria in config.toml — define what to hunt (see config.example.toml)"
                .to_owned(),
        ));
    }
    let (session, _handles, _shutdown) = setup(config);
    session.run().await
}

impl Session {
    /// Runs the relay and blocks until shutdown (Ctrl+C or end of stream).
    ///
    /// # Errors
    ///
    /// [`crate::Error::Capture`] when no capture backend can be opened (see
    /// `build_source`) or when the capture thread cannot be started, and
    /// [`crate::Error::Fatal`] carrying the session's own fatal — the first
    /// message the session loop froze off the fatal channel, already journaled
    /// and already self-describing. A clean teardown, including a player stop,
    /// is `Ok(())`.
    pub async fn run(self) -> Result<()> {
        let Self {
            config,
            handles,
            command_rx,
            actuator,
            job_rx,
            shutdown,
        } = self;
        let SessionHandles {
            controller,
            commands,
            gate,
            journal,
        } = handles;
        let budget = PipelineBudget::new();
        let pressure_resync = PressureResync::default();
        let (segment_tx, segment_rx) = mpsc::channel::<CaptureEvent>(CAPTURE_EVENT_QUEUE);
        let (raw_tx, raw_rx) = mpsc::channel::<BudgetedChunk>(PIPELINE_QUEUE);
        let (message_tx, message_rx) = mpsc::channel::<UplinkEvent>(PIPELINE_QUEUE);
        // One fatal-failure channel, several producers (see FATAL_QUEUE).
        let (fatal_tx, fatal_rx) = mpsc::channel::<String>(FATAL_QUEUE);
        // Every receiver is taken before the signal moves into the worker set;
        // a window close reaches the session loop and the workers alike.
        let shutdown_rx = shutdown.subscribe();
        let loop_shutdown_rx = shutdown.subscribe();

        // Blocking capture receiver on a dedicated thread, wrapped in
        // catch_unwind so a panic surfaces as a fatal message instead of a
        // silently dropped sender (which reads as a clean "session ended").
        let source = build_source(&config)?;
        let capture = spawn_capture_with_budget(
            source,
            segment_tx,
            gate.clone(),
            shutdown_rx.clone(),
            fatal_tx.clone(),
            budget.clone(),
            pressure_resync.clone(),
        )?;
        let mut workers = SessionWorkers::new(shutdown, capture);

        // Server link with automatic reconnection.
        workers.spawn(
            "uplink",
            &fatal_tx,
            crate::uplink::run(
                // The whole `ServerUrl`, not the dial string: the redacted form
                // travels with it, and the credential-bearing spelling is
                // reachable only through `as_str()`, at the one line that dials.
                config.server_url.clone(),
                raw_rx,
                message_tx,
                config.reconnect_initial(),
                config.reconnect_max(),
                // Races against the handshake, the connected pump and the
                // backoff (previously reached only by `abort` after the grace
                // deadline).
                shutdown_rx.clone(),
            ),
        );

        // Reassembly of the captured server-to-client stream.
        workers.spawn(
            "reassembly",
            &fatal_tx,
            reassemble_loop_with_pressure(segment_rx, raw_tx, pressure_resync),
        );

        // Click jobs -> the game window, through the configured backend.
        #[cfg(all(windows, feature = "actuator"))]
        {
            use crate::actuator::run_executor;
            use crate::actuator::win::{MessageSurface, WinSurface};
            use crate::config::ActuatorBackend;
            let dry_run = actuator.mode == Mode::DryRun;
            match config.actuator.backend {
                ActuatorBackend::Input => workers.spawn(
                    "actuator",
                    &fatal_tx,
                    run_executor(
                        WinSurface::default(),
                        job_rx,
                        gate.clone(),
                        actuator.epoch.clone(),
                        journal.clone(),
                        dry_run,
                    ),
                ),
                ActuatorBackend::Message => workers.spawn(
                    "actuator",
                    &fatal_tx,
                    run_executor(
                        MessageSurface::default(),
                        job_rx,
                        gate.clone(),
                        actuator.epoch.clone(),
                        journal.clone(),
                        dry_run,
                    ),
                ),
            }
        }
        // Without an input backend the mode is Off and nothing ever submits.
        #[cfg(not(all(windows, feature = "actuator")))]
        drop(job_rx);

        // Keyboard input, decoupled from the session loop through the channel.
        workers.spawn(
            "stdin",
            &fatal_tx,
            stdin_loop(commands, shutdown_rx, journal.clone()),
        );
        // Only the capture thread and owned task wrappers remain fatal
        // producers; channel closure can therefore still be observed.
        drop(fatal_tx);

        info!(
            // Spelled out rather than left to `Display` (which redacts too), so
            // an auditor reading log sites sees the redaction at the site.
            server = config.server_url.redacted(),
            "relay started — idle, `start` arms the watch"
        );
        print_controls();

        let loop_outcome = AssertUnwindSafe(session_loop(
            &controller,
            &gate,
            &journal,
            &actuator,
            command_rx,
            message_rx,
            fatal_rx,
            loop_shutdown_rx,
        ))
        .catch_unwind()
        .await;

        // Freeze the outcome before any cancellation or join result can occur.
        workers.shutdown(&gate, actuator).await;
        info!("relay stopped");
        let primary_failure = match loop_outcome {
            Ok(primary_failure) => primary_failure,
            Err(panic) => std::panic::resume_unwind(panic),
        };
        match primary_failure {
            Some(error) => Err(crate::Error::Fatal(error)),
            None => Ok(()),
        }
    }
}

/// Awaits the session future (spawned so a panic is caught, not propagated)
/// and translates its end into a banner line plus a failed flag.
///
/// The gate is forced off on every path: a panicking session skips its own
/// teardown, and capture must not keep streaming under a crash banner.
/// Idempotent after a clean teardown.
///
/// The task lives in a single-element [`tokio::task::JoinSet`], not a bare
/// `JoinHandle`: dropping a handle detaches, so a cancelled `supervise` used
/// to leave the session running unheld and `gate.set(false)` never reached —
/// capture still forwarding, the actuator still clicking, under a session
/// that is officially gone. `JoinSet` aborts on drop instead.
pub async fn supervise(
    session: impl Future<Output = Result<()>> + Send + 'static,
    gate: WatchGate,
) -> (String, bool) {
    let mut set = tokio::task::JoinSet::new();
    set.spawn(session);
    let outcome = set.join_next().await;
    gate.set(false);
    match outcome {
        Some(Ok(Ok(()))) => (
            "session ended — restart the app to reconnect".to_owned(),
            false,
        ),
        Some(Ok(Err(err))) => (format!("session error: {}", err.report()), true),
        Some(Err(panic)) => (format!("session crashed: {panic}"), true),
        // Unreachable: the set holds exactly the task spawned above. Reported as
        // a failure, not panicked on: this function's whole job is turning a
        // dead session into a line the player can read.
        None => (
            "session crashed: the session task vanished".to_owned(),
            true,
        ),
    }
}

/// A line that names no command, carrying the trimmed input so the message can
/// quote it.
///
/// Its `Display` carries the alias list, kept beside the table that
/// produces it rather than spelled out again at the call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseCommandError(String);

impl std::fmt::Display for ParseCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown command: {:?} (start, stop, enter = toggle)",
            self.0
        )
    }
}

impl std::error::Error for ParseCommandError {}

/// Parses the *typeable* subset of [`Command`]. The three `Set*` variants
/// carry live retune payloads (a [`Filter`], [`Limits`], [`plan::Timings`])
/// that no console line can express; the GUI constructs those directly, and
/// the missing match arms below document that.
impl std::str::FromStr for Command {
    type Err = ParseCommandError;

    fn from_str(line: &str) -> std::result::Result<Self, Self::Err> {
        let trimmed = line.trim();
        match trimmed.to_ascii_lowercase().as_str() {
            "" | "t" | "toggle" => Ok(Self::Toggle),
            "on" | "start" => Ok(Self::Start),
            "off" | "stop" => Ok(Self::Stop),
            _ => Err(ParseCommandError(trimmed.to_owned())),
        }
    }
}

/// Opens the compiled capture backend. Two arms, so every feature combination
/// builds and exactly one applies:
///
/// - **Npcap**, when `pcap-backend` is on — the shipped default. It taps every
///   adapter through `wpcap.dll` in this process: no driver of ours to load,
///   no second process, no UAC prompt of its own. The process *is* elevated
///   (manifested `requireAdministrator`), but for the actuator, not this —
///   see `build.rs`.
/// - **No backend** — an error the caller can show, never a panic; the
///   `--no-default-features` build, where the rest of the pipeline still
///   compiles and tests.
///
/// Either way what arrives is an IP-layer copy: `PcapSource` strips each
/// adapter's link framing before handing anything up, so it keeps working on
/// a Wi-Fi adapter, where a raw NIC tap would deliver 802.11 frames this
/// crate does not decode. Blocking, and quick: device enumeration plus one
/// open and one filter compile per adapter, called from `Session::run` with
/// nothing in it waiting on a human.
#[cfg(all(windows, feature = "pcap-backend"))]
fn build_source(config: &Config) -> Result<CaptureSource> {
    use crate::capture::PcapSource;
    info!(
        port = config.game_port,
        "opening the Npcap tap on every adapter (the tap itself needs no privilege)"
    );
    let (source, stop) = PcapSource::open(config.game_port)?;
    Ok(CaptureSource::new(source, stop))
}

#[cfg(not(all(windows, feature = "pcap-backend")))]
fn build_source(_config: &Config) -> Result<CaptureSource> {
    Err(crate::Error::Capture(
        "no capture backend compiled — enable `pcap-backend` (the default) on Windows".to_owned(),
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
        let (_session, handles, _shutdown) = setup(Config::default());
        assert_eq!(handles.controller.lock().unwrap().status(), Status::Idle);
        assert!(!handles.gate.is_enabled());
        assert!(handles.journal.to_entries().is_empty());
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
        let (_session, handles, _shutdown) = setup(Config::default());
        assert!(!handles.controller.lock().unwrap().is_recovery_enabled());
    }

    /// Live clicking arms the watchdog; a dry run produces no wire feedback
    /// and must keep it dark.
    #[cfg(all(windows, feature = "actuator"))]
    #[test]
    fn setup_enables_recovery_only_when_live() {
        let (_session, handles, _shutdown) = setup(Config::default());
        assert!(handles.controller.lock().unwrap().is_recovery_enabled());

        let mut config = Config::default();
        config.actuator.dry_run = true;
        let (_session, handles, _shutdown) = setup(config);
        assert!(!handles.controller.lock().unwrap().is_recovery_enabled());
    }

    async fn panicking_session() -> Result<()> {
        panic!("boom")
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

    // Redaction now lives in `config::ServerUrl` with its own tests; the
    // duplicate here is gone.

    #[test]
    fn parse_command_maps_aliases() {
        assert_eq!("start".parse(), Ok(Command::Start));
        assert_eq!("on".parse(), Ok(Command::Start));
        assert_eq!("stop".parse(), Ok(Command::Stop));
        assert_eq!("off".parse(), Ok(Command::Stop));
        assert_eq!("toggle".parse(), Ok(Command::Toggle));
        assert_eq!("t".parse(), Ok(Command::Toggle));
        assert_eq!("".parse(), Ok(Command::Toggle));
    }

    #[test]
    fn parse_command_trims_and_ignores_case() {
        assert_eq!("  START \t".parse(), Ok(Command::Start));
        assert_eq!("Stop".parse(), Ok(Command::Stop));
    }

    #[test]
    fn parse_command_rejects_unknown() {
        assert!("refresh".parse::<Command>().is_err());
        assert!("sta rt".parse::<Command>().is_err());
        // The skip command is gone: buying (or a fresh shop) is the only
        // way out of a pause.
        assert!("resume".parse::<Command>().is_err());
        assert!("r".parse::<Command>().is_err());
    }

    #[test]
    fn parse_command_error_quotes_the_trimmed_input_and_lists_the_aliases() {
        let err = "  Refresh \t"
            .parse::<Command>()
            .expect_err("not a command");
        assert_eq!(
            err.to_string(),
            "unknown command: \"Refresh\" (start, stop, enter = toggle)"
        );
    }
}
