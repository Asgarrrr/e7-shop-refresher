//! Orchestration: capture -> reassembly -> gate -> uplink -> controller -> display.
//!
//! The root holds the *wiring*. Submodules reach each other only through the
//! channel ends and `Arc` handles created in [`Session::run`] — no shared
//! `&mut` state. `session` predates that split, and is the only place in the
//! crate that nests two locks (`controller` -> `timings`).

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

use crate::actuator::{ActuatorHandle, ClickMode, Mode, SnapshotEpoch, plan};
use crate::capture::{CaptureHealth, CaptureSource};
use crate::domain::control::{Controller, Limits};
use crate::domain::filter::Filter;
use crate::journal::EventLog;
use crate::render::print_controls;
use crate::stream::{BudgetedChunk, PipelineBudget, RunBaselineCell};
use crate::uplink::{UplinkEvent, VocabularyCell};
use crate::watch::WatchGate;
use crate::{Config, Result};

use console::stdin_loop;
use pressure::{CAPTURE_EVENT_QUEUE, CaptureEvent, PressureResync};
use reassembly::reassemble_loop_with_pressure;
use session::{SessionGate, session_loop};
use workers::{SessionWorkers, spawn_capture_with_budget};

/// Depth of both pipeline channels, which are backpressured differently.
/// `raw_tx` carries [`BudgetedChunk`], whose [`PipelineBudget`] lease caps the
/// bytes in flight whatever the depth, so there the slot count only bounds how
/// far reassembly runs ahead of the uplink. `message_tx` carries
/// [`UplinkEvent`], which holds no lease: these slots are its only
/// backpressure, and each one can hold a whole decoded message. What bounds an
/// inbound message is tungstenite's default 64 MiB ceiling — `connect_async` is
/// called with no `WebSocketConfig` — not the 32 MiB pipeline budget. Raising
/// this number raises that second worst case with it.
const PIPELINE_QUEUE: usize = 256;

/// First message wins; the depth exists so a racing second report cannot block
/// the task that is already unwinding.
const FATAL_QUEUE: usize = 4;

/// Safety stops do not ride this queue (see [`Command`]), so saturation costs
/// latency, never a missed stop.
const COMMAND_QUEUE: usize = 16;

/// Deliberately shallow: a deep queue would let clicks planned against a dead
/// shop pile up behind the epoch check.
const JOB_QUEUE: usize = 8;

/// A non-safety command, decoupled from its source: the stdin task and the GUI
/// push the same values through the same channel.
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
    /// Live rehearsal/backend switch; applies to the next job the executor
    /// dequeues.
    ///
    /// One variant carrying both, not two: they are applied together and a
    /// single Apply must not be able to land in halves. See [`ClickMode`].
    SetClickMode(ClickMode),
}

/// Cheap clones of the shared session state, for a view (the GUI) running
/// beside the session loop: the controller is read under short locks.
#[derive(Clone)]
pub struct SessionHandles {
    pub controller: Arc<Mutex<Controller>>,
    pub commands: mpsc::Sender<Command>,
    pub gate: WatchGate,
    pub journal: EventLog,
    /// Byte accounting for the pipeline: `budget.snapshot()` takes a short
    /// lock that never crosses a frame boundary, the same rule `controller`
    /// above already follows.
    pub(crate) budget: PipelineBudget,
    /// The zero the capture readout counts the current run from. Written by the
    /// session loop on the edge that opens the gate, never by the frame that
    /// reads it — see `stream::RunBaselineCell` for the blind spot that costs.
    pub(crate) run_baseline: RunBaselineCell,
    /// Live capture counters — see [`CaptureHealth`] for why a clone of this
    /// is safe to read from the egui thread while [`Session::run`]'s capture
    /// pump increments the same atomics on its own thread. Constructed here,
    /// before the backend that will increment it exists, precisely so a
    /// clone can ride in this struct from the very first frame.
    pub(crate) capture_health: CaptureHealth,
    /// The filter choices the server pushed, for Setup to offer. Empty until a
    /// `catalog` message arrives, and empty for good against a server with no
    /// Catalog to read — the editor falls back to free text, which is what every
    /// build did before this existed.
    pub vocabulary: VocabularyCell,
}

impl SessionHandles {
    /// Builds handles for a caller outside this crate that drives the window
    /// over its own controller, gate, journal and command channel instead of
    /// a real [`Session`] — today, only `examples/ui_preview.rs`.
    ///
    /// `budget` and `capture_health` are not parameters: both are
    /// `pub(crate)`-constructed types, unreachable from outside this crate,
    /// and a preview runs no real capture pipeline to feed them anyway — they
    /// start fresh and empty, which reads as "no traffic yet" rather than a
    /// fault. See `CaptureCounters` and `ui::capture_health::diagnosis` for
    /// why that default is the honest one.
    pub fn for_preview(
        controller: Arc<Mutex<Controller>>,
        commands: mpsc::Sender<Command>,
        gate: WatchGate,
        journal: EventLog,
    ) -> Self {
        let budget = PipelineBudget::new();
        Self {
            controller,
            commands,
            gate,
            journal,
            run_baseline: RunBaselineCell::new(budget.clone()),
            budget,
            capture_health: CaptureHealth::default(),
            vocabulary: VocabularyCell::new(),
        }
    }
}

/// Cooperative stop for a session running on a detached task. The GUI closes
/// its window on the OS main thread while the pipeline lives on the runtime;
/// without this the process exits skipping teardown, leaving an orphaned live
/// capture session in the driver. [`Command::Stop`] only disarms the hunt.
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

/// Hands out clones before any fallible work runs, so a view keeps live handles
/// even when [`Session::run`] fails later (bad filter, no capture backend).
pub fn setup(config: Config) -> (Session, SessionHandles, ShutdownSignal) {
    // Session starts Idle; the player arms it with `start`.
    let gate = WatchGate::new(false);
    let journal = EventLog::default();
    let (command_tx, command_rx) = mpsc::channel::<Command>(COMMAND_QUEUE);
    let mut controller = Controller::new(config.filter.clone(), config.limits);
    if actuator_mode(&config) == Mode::Live {
        // Only real clicking gets watchdog deadlines: Off and DryRun produce
        // no wire feedback, so a deadline would self-halt both.
        controller.set_recovery(true);
    }
    let controller = Arc::new(Mutex::new(controller));
    // Both created here, ahead of `Session::run`, so a clone can ride in
    // `SessionHandles` from the window's first frame — `run` hands the
    // originals to the pipeline instead of making fresh ones, exactly as
    // `controller` above already does.
    let budget = PipelineBudget::new();
    let capture_health = CaptureHealth::default();
    let handles = SessionHandles {
        controller,
        commands: command_tx,
        gate,
        journal,
        run_baseline: RunBaselineCell::new(budget.clone()),
        budget,
        capture_health,
        vocabulary: VocabularyCell::new(),
    };
    let (job_tx, job_rx) = mpsc::channel::<plan::Job>(JOB_QUEUE);
    let timings = Arc::new(Mutex::new(config.actuator.timings));
    // Seeded from the file, then owned by the player: `Command::SetClickMode`
    // writes it and the executor snapshots it once per job.
    let click_mode = Arc::new(Mutex::new(ClickMode {
        dry_run: config.actuator.dry_run,
        backend: config.actuator.backend,
    }));
    let actuator = ActuatorHandle::new(
        actuator_mode(&config),
        SnapshotEpoch::default(),
        job_tx,
        timings,
        click_mode,
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

/// The backend as a trait object, for [`Session::run`]'s single spawn and for
/// the swap it hands the executor.
///
/// `Send` in the bound because the executor is spawned onto a work-stealing
/// runtime and the surface travels with it — the same requirement the concrete
/// types met implicitly before they were boxed.
#[cfg(all(windows, feature = "actuator"))]
fn build_surface(
    backend: crate::actuator::ActuatorBackend,
) -> Box<dyn crate::actuator::Surface<Window = crate::actuator::win::Target> + Send> {
    use crate::actuator::ActuatorBackend;
    use crate::actuator::win::{MessageSurface, WinSurface};
    match backend {
        ActuatorBackend::Input => Box::new(WinSurface::default()),
        ActuatorBackend::Message => Box::new(MessageSurface::default()),
    }
}

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
/// no editor, so failing fast beats booting a watch that can never arm.
/// Otherwise whatever [`Session::run`] returns.
pub async fn run(config: Config) -> Result<()> {
    // The GUI path boots instead, and refuses arming until a filter is set.
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
    /// [`crate::Error::Capture`] when no capture backend can be opened or the
    /// capture thread cannot start, and [`crate::Error::Fatal`] carrying the
    /// first message the loop froze off the fatal channel, already journaled
    /// and self-describing. A clean teardown, player stop included, is `Ok(())`.
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
            run_baseline,
            budget,
            capture_health,
            vocabulary,
        } = handles;
        let pressure_resync = PressureResync::default();
        let (segment_tx, segment_rx) = mpsc::channel::<CaptureEvent>(CAPTURE_EVENT_QUEUE);
        let (raw_tx, raw_rx) = mpsc::channel::<BudgetedChunk>(PIPELINE_QUEUE);
        let (message_tx, message_rx) = mpsc::channel::<UplinkEvent>(PIPELINE_QUEUE);
        let (fatal_tx, fatal_rx) = mpsc::channel::<String>(FATAL_QUEUE);
        // Every receiver is taken before the signal moves into the worker set;
        // a window close reaches the session loop and the workers alike.
        let shutdown_rx = shutdown.subscribe();
        let loop_shutdown_rx = shutdown.subscribe();

        // Wrapped in catch_unwind so a panic surfaces as a fatal message rather
        // than a dropped sender, which reads as a clean "session ended".
        let source = build_source(&config, capture_health)?;
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

        workers.spawn(
            "uplink",
            &fatal_tx,
            crate::uplink::run(
                // The whole `ServerUrl`, so the redacted form travels with it
                // and the credential-bearing spelling stays behind `as_str()`.
                config.server_url.clone(),
                raw_rx,
                message_tx,
                config.reconnect_initial(),
                config.reconnect_max(),
                // Races against the handshake, the connected pump and the
                // backoff — previously reached only by `abort`.
                shutdown_rx.clone(),
            ),
        );

        workers.spawn(
            "reassembly",
            &fatal_tx,
            reassemble_loop_with_pressure(segment_rx, raw_tx, pressure_resync),
        );

        #[cfg(all(windows, feature = "actuator"))]
        {
            use crate::actuator::win::Target;
            use crate::actuator::{Surface, run_executor};
            // One spawn over a boxed backend, where there used to be two arms
            // choosing a concrete one for the session's life. Both shipped
            // backends declare `type Window = Target`, so they unify under this
            // object and the executor can swap them between jobs.
            let surface = build_surface(actuator.click_mode().backend);
            workers.spawn(
                "actuator",
                &fatal_tx,
                run_executor(
                    surface,
                    job_rx,
                    gate.clone(),
                    actuator.epoch.clone(),
                    journal.clone(),
                    actuator.click_mode_cell(),
                    // Replaces the whole backend rather than reconfiguring one:
                    // the old surface has already released, and a fresh
                    // `Default` is what the two arms above used to build.
                    |backend, held: &mut Box<dyn Surface<Window = Target> + Send>| {
                        *held = build_surface(backend);
                    },
                ),
            );
        }
        // Without an input backend the mode is Off and nothing ever submits.
        #[cfg(not(all(windows, feature = "actuator")))]
        drop(job_rx);

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

        // The loop's own view of the gate: arming it and publishing the run
        // baseline are one act, and only this loop performs it. The bare
        // `WatchGate` stays in scope for the workers and the teardown, which only
        // ever shut it.
        let session_gate = SessionGate::new(gate.clone(), run_baseline);
        let loop_outcome = AssertUnwindSafe(session_loop(
            &controller,
            &session_gate,
            &journal,
            &actuator,
            &vocabulary,
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
/// A single-element [`tokio::task::JoinSet`], not a bare `JoinHandle`: dropping
/// a handle detaches, so a cancelled `supervise` left the session running with
/// `gate.set(false)` never reached. `JoinSet` aborts on drop.
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
        // Unreachable, but reported rather than panicked on: this function's
        // whole job is turning a dead session into a line the player can read.
        None => (
            "session crashed: the session task vanished".to_owned(),
            true,
        ),
    }
}

/// Carries the trimmed input so the message can quote it. Its `Display` lists
/// the aliases, kept beside the table that produces them.
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

/// Parses the *typeable* subset of [`Command`]. The three `Set*` variants carry
/// payloads no console line can express, so the GUI constructs those directly —
/// which is what the missing match arms below mean.
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
/// - **Npcap**, when `pcap-backend` is on — the shipped default, tapping every
///   adapter through `wpcap.dll` in this process: no driver of ours, no second
///   process, no UAC prompt of its own. The process *is* elevated, but for the
///   actuator, not this — see `build.rs`.
/// - **No backend** — an error the caller can show, never a panic; the
///   `--no-default-features` build.
///
/// Either way what arrives is an IP-layer copy: `PcapSource` strips each
/// adapter's link framing, so it keeps working on Wi-Fi, where a raw NIC tap
/// would deliver 802.11 frames this crate does not decode. Blocking, but quick
/// — nothing in it waits on a human.
///
/// `health` rides along rather than being constructed here: `setup` made it
/// before either arm below could run, so a clone already sits in
/// `SessionHandles` for the window by the time this returns — the no-backend
/// arm still takes it, unused, so both signatures match.
#[cfg(all(windows, feature = "pcap-backend"))]
fn build_source(config: &Config, health: CaptureHealth) -> Result<CaptureSource> {
    use crate::capture::PcapSource;
    info!(
        port = config.game_port,
        "opening the Npcap tap on every adapter (the tap itself needs no privilege)"
    );
    let (source, stop) = PcapSource::open(config.game_port, health)?;
    Ok(CaptureSource::new(source, stop))
}

#[cfg(not(all(windows, feature = "pcap-backend")))]
fn build_source(_config: &Config, _health: CaptureHealth) -> Result<CaptureSource> {
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
