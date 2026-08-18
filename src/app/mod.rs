//! Orchestration: capture -> reassembly -> gate -> uplink -> controller -> display.

mod session;

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::actuator::{ActuatorHandle, Mode, SnapshotEpoch, plan};
#[cfg(test)]
use crate::capture::Segment;
use crate::capture::{CaptureSource, CaptureStop, Direction, PacketSource};
use crate::config::ForwardConfig;
use crate::domain::control::{Controller, Limits};
use crate::domain::filter::Filter;
use crate::journal::EventLog;
use crate::render::print_controls;
use crate::stream::{
    BudgetedChunk, BudgetedSegment, InitialBurst, PipelineBudget, Reassembler, ReassemblyOutcome,
};
use crate::uplink::UplinkEvent;
use crate::watch::WatchGate;
use crate::{Config, Result};

use session::session_loop;

/// One deadline for the whole Tokio worker set. Cooperative pipeline closure
/// normally finishes immediately; this is only the cancellation-safe fallback.
const WORKER_SHUTDOWN_GRACE: Duration = Duration::from_millis(250);

/// How many admitted segments between two "capture progress" lines. The
/// *absence* of that line in a long log is the diagnostic: the backend is
/// reporting nothing on the configured port.
const CAPTURE_PROGRESS_EVERY: u64 = 1000;

/// Conservative one-shot allowance for reordered predecessors immediately
/// after capture resumes. Ten milliseconds is the documented hard cap: it
/// bounds latency even though no server-side timing evidence is available.
const INITIAL_ANCHOR_WINDOW: Duration = Duration::from_millis(10);

/// Event flowing from the capture thread to reassembly.
enum CaptureEvent {
    /// A byte-admitted TCP segment to reassemble.
    Budgeted(BudgetedSegment),
    /// Test-only compatibility path; production admits before enqueueing.
    #[cfg(test)]
    Segment(Segment),
    /// Shop Watch was just re-enabled after a pause: the reassembler must
    /// re-anchor a fresh origin (the bytes during the pause are lost).
    Resync,
    /// Byte pressure invalidated continuity. Unlike a player resync, this is
    /// counted and deduplicated by [`PressureResync`].
    PressureResync,
}

const RESYNC_ACK: u8 = 0;
const RESYNC_PENDING: u8 = 1;
const RESYNC_ENQUEUED: u8 = 2;

/// Lossless single-producer pressure marker protocol. A full metadata queue
/// leaves the request Pending; capture retries before admitting later bytes.
#[derive(Clone, Default)]
struct PressureResync(Arc<AtomicU8>);

impl PressureResync {
    fn request(&self, budget: &PipelineBudget) -> bool {
        if self
            .0
            .compare_exchange(
                RESYNC_ACK,
                RESYNC_PENDING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            budget.record_resync();
            true
        } else {
            false
        }
    }

    fn try_enqueue(&self, tx: &mpsc::Sender<CaptureEvent>) -> bool {
        if self
            .0
            .compare_exchange(
                RESYNC_PENDING,
                RESYNC_ENQUEUED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return self.0.load(Ordering::Acquire) == RESYNC_ENQUEUED;
        }
        match tx.try_send(CaptureEvent::PressureResync) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                let changed = self.0.compare_exchange(
                    RESYNC_ENQUEUED,
                    RESYNC_PENDING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                debug_assert!(changed.is_ok());
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.0.store(RESYNC_ACK, Ordering::Release);
                false
            }
        }
    }

    fn blocks_segments(&self) -> bool {
        self.0.load(Ordering::Acquire) != RESYNC_ACK
    }

    fn acknowledge(&self) {
        let previous = self.0.swap(RESYNC_ACK, Ordering::AcqRel);
        debug_assert_eq!(previous, RESYNC_ENQUEUED);
    }
}

enum AnchorState {
    /// Normal forwarding, including process startup before the first Resync.
    Steady,
    /// A Resync occurred, but no segment allowed by `ForwardConfig` has arrived.
    AwaitingFirst,
    /// The one bounded post-resync burst is waiting for predecessors.
    Buffering {
        burst: InitialBurst,
        deadline: Instant,
    },
}

/// A non-safety command into the session loop, decoupled from its source: the
/// stdin task and the GUI push the same values through the same channel (stdin
/// never produces the `Set*` variants). Safety stops use [`WatchGate`]'s
/// durable halt latch instead of this bounded queue.
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

/// Cooperative stop for a session that runs on a detached task.
///
/// The GUI build closes its window on the OS main thread while the pipeline
/// lives on the runtime; without this the process would simply exit and skip
/// every teardown — leaving an orphaned live capture session in the driver.
/// [`Command::Stop`] is not a substitute: it only disarms the hunt, it never
/// leaves the session loop.
///
/// Cloneable (the sender is shared, not duplicated) so the owner of the window
/// and the session can each hold one.
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
/// later (bad filter, no capture backend).
///
/// The third value is the cooperative stop: whoever outlives the session task
/// (the window, in the GUI build) must be able to ask for teardown.
pub fn setup(config: Config) -> (Session, SessionHandles, ShutdownSignal) {
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

/// The URL reduced to `scheme://host[:port]` — neither userinfo nor query,
/// which can carry a credential and must never reach a log the player is
/// asked to send us. `Config::validate` accepts any `wss://` URL without
/// inspecting either, so the redaction lives here rather than at parse time.
fn redacted_server_url(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    format!("{scheme}://{host}")
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
    let (session, _handles, _shutdown) = setup(config);
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
        let (segment_tx, segment_rx) = mpsc::channel::<CaptureEvent>(512);
        let (raw_tx, raw_rx) = mpsc::channel::<BudgetedChunk>(256);
        let (message_tx, message_rx) = mpsc::channel::<UplinkEvent>(256);
        // One fatal-failure channel, several producers. The session loop
        // freezes the first message before teardown starts.
        let (fatal_tx, fatal_rx) = mpsc::channel::<String>(4);
        // Every receiver is taken before the signal moves into the worker set;
        // a window close reaches the session loop and the workers alike.
        let shutdown_rx = shutdown.subscribe();
        let loop_shutdown_rx = shutdown.subscribe();

        // Blocking capture receiver on a dedicated thread.
        // Wrapped in catch_unwind so a panic surfaces as a fatal message instead
        // of a silently dropped sender (which reads as a clean "session ended").
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
                config.server_url.clone(),
                raw_rx,
                message_tx,
                config.reconnect_initial(),
                config.reconnect_max(),
            ),
        );

        // Reassembly + filtering of the directions to forward.
        workers.spawn(
            "reassembly",
            &fatal_tx,
            reassemble_loop_with_pressure(
                segment_rx,
                raw_tx,
                config.forward.clone(),
                pressure_resync,
            ),
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
        workers.spawn("stdin", &fatal_tx, stdin_loop(commands, shutdown_rx));
        // Only the capture thread and owned task wrappers remain fatal
        // producers; channel closure can therefore still be observed.
        drop(fatal_tx);

        info!(
            server = %redacted_server_url(&config.server_url),
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

        // Freeze the selected outcome before any cancellation or join result
        // can occur. Teardown diagnostics are secondary by construction.
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

struct CaptureWorker {
    stop: Box<dyn CaptureStop>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CaptureWorker {
    /// Wakes and joins the OS thread synchronously. A timeout here would only
    /// detach an unabortable thread, so shutdown capability is the finite join.
    fn stop_and_join(&mut self) {
        if let Err(err) = self.stop.stop() {
            error!(error = %err, "capture stop failed during teardown");
        }
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            error!("capture thread join failed during teardown");
        }
    }
}

struct TokioWorker {
    name: &'static str,
    handle: Option<tokio::task::JoinHandle<()>>,
    aborted: bool,
}

struct SessionWorkers {
    shutdown: ShutdownSignal,
    capture: CaptureWorker,
    tasks: Vec<TokioWorker>,
}

impl SessionWorkers {
    fn new(shutdown: ShutdownSignal, capture: CaptureWorker) -> Self {
        Self {
            shutdown,
            capture,
            tasks: Vec::new(),
        }
    }

    /// The panic catcher is the owned worker itself: there is no detached
    /// supervisor holding a second child handle.
    fn spawn(
        &mut self,
        name: &'static str,
        fatal: &mpsc::Sender<String>,
        future: impl Future<Output = ()> + Send + 'static,
    ) {
        let fatal = fatal.clone();
        let handle = tokio::spawn(async move {
            if AssertUnwindSafe(future).catch_unwind().await.is_err() {
                let _ = fatal.send(format!("{name} task panicked")).await;
            }
        });
        self.tasks.push(TokioWorker {
            name,
            handle: Some(handle),
            aborted: false,
        });
    }

    /// Producer-to-consumer teardown: gate/signal, actuator producer close,
    /// capture wake+join, pipeline EOF, then one grace deadline for all tasks.
    async fn shutdown(mut self, gate: &WatchGate, actuator: ActuatorHandle) {
        gate.set(false);
        self.shutdown.request();
        drop(actuator);

        // `stop_and_join` parks its caller in `Thread::join`. Doing that on a
        // runtime worker would deny the scheduler the very tasks whose exit is
        // being waited on — with a single worker, that is a deadlock. The
        // blocking pool exists to be parked.
        let mut capture = self.capture;
        if tokio::task::spawn_blocking(move || capture.stop_and_join())
            .await
            .is_err()
        {
            error!("capture teardown task failed during teardown");
        }

        let deadline = Instant::now() + WORKER_SHUTDOWN_GRACE;
        for worker in &mut self.tasks {
            let Some(handle) = worker.handle.as_mut() else {
                continue;
            };
            match tokio::time::timeout_at(deadline, handle).await {
                Ok(result) => {
                    report_join(worker.name, false, result);
                    worker.handle = None;
                }
                Err(_) => break,
            }
        }

        // The single deadline elapsed. Abort every unfinished task first, then
        // await every handle (including cancelled ones) before returning.
        for worker in &mut self.tasks {
            if let Some(handle) = worker.handle.as_ref()
                && !handle.is_finished()
            {
                handle.abort();
                worker.aborted = true;
            }
        }
        for worker in &mut self.tasks {
            if let Some(handle) = worker.handle.take() {
                report_join(worker.name, worker.aborted, handle.await);
            }
        }
    }
}

fn report_join(
    name: &'static str,
    aborted: bool,
    result: std::result::Result<(), tokio::task::JoinError>,
) {
    if let Err(err) = result
        && !(aborted && err.is_cancelled())
    {
        error!(worker = name, error = %err, "worker join failed during teardown");
    }
}

fn spawn_capture_with_budget(
    source: CaptureSource,
    tx: mpsc::Sender<CaptureEvent>,
    gate: WatchGate,
    shutdown: watch::Receiver<bool>,
    fatal: mpsc::Sender<String>,
    budget: PipelineBudget,
    pressure_resync: PressureResync,
) -> Result<CaptureWorker> {
    let CaptureSource { packets, stop } = source;
    let thread = std::thread::Builder::new()
        .name("capture".to_owned())
        .spawn(move || {
            let panic_fatal = fatal.clone();
            let run = AssertUnwindSafe(|| {
                capture_loop_budgeted(packets, tx, gate, shutdown, fatal, budget, pressure_resync)
            });
            if std::panic::catch_unwind(run).is_err() {
                let _ = panic_fatal.blocking_send("capture thread panicked".to_owned());
            }
        })?;
    Ok(CaptureWorker {
        stop,
        thread: Some(thread),
    })
}

#[cfg(test)]
fn spawn_capture(
    source: CaptureSource,
    tx: mpsc::Sender<CaptureEvent>,
    gate: WatchGate,
    shutdown: watch::Receiver<bool>,
    fatal: mpsc::Sender<String>,
) -> Result<CaptureWorker> {
    spawn_capture_with_budget(
        source,
        tx,
        gate,
        shutdown,
        fatal,
        PipelineBudget::new(),
        PressureResync::default(),
    )
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
#[cfg(test)]
async fn reassemble_loop(
    events: mpsc::Receiver<CaptureEvent>,
    raw_tx: mpsc::Sender<BudgetedChunk>,
    forward: ForwardConfig,
) {
    reassemble_loop_with_pressure(events, raw_tx, forward, PressureResync::default()).await;
}

async fn reassemble_loop_with_pressure(
    mut events: mpsc::Receiver<CaptureEvent>,
    raw_tx: mpsc::Sender<BudgetedChunk>,
    forward: ForwardConfig,
    pressure_resync: PressureResync,
) {
    let mut reassembler = Reassembler::new();
    let mut anchor = AnchorState::Steady;
    loop {
        let event = if let AnchorState::Buffering { deadline, .. } = &anchor {
            let deadline = *deadline;
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => {
                    if !flush_anchor(&mut anchor, &mut reassembler, &raw_tx).await {
                        break;
                    }
                    continue;
                }
                _ = raw_tx.closed() => {
                    let _ = flush_anchor(&mut anchor, &mut reassembler, &raw_tx).await;
                    break;
                }
                event = events.recv() => event,
            }
        } else {
            events.recv().await
        };

        let Some(event) = event else {
            let _ = flush_anchor(&mut anchor, &mut reassembler, &raw_tx).await;
            break;
        };

        let segment = match event {
            CaptureEvent::Resync => {
                // A newer epoch invalidates bytes that were never committed.
                // The next allowed segment starts a fresh one-shot window.
                reassembler.clear();
                anchor = AnchorState::AwaitingFirst;
                continue;
            }
            CaptureEvent::PressureResync => {
                reassembler.clear();
                anchor = AnchorState::AwaitingFirst;
                pressure_resync.acknowledge();
                continue;
            }
            CaptureEvent::Budgeted(segment) => segment,
            #[cfg(test)]
            CaptureEvent::Segment(segment) => PipelineBudget::new()
                .admit_capture(segment)
                .expect("test segment fits capture quota"),
        };
        if !should_forward(segment.direction, &forward) {
            continue;
        }

        // Plan 008 remains authoritative: never hold a SYN behind the anchor
        // deadline. Commit any older burst first, then let `Reassembler`
        // classify/reset the connection incarnation immediately.
        if segment.syn {
            if !flush_anchor(&mut anchor, &mut reassembler, &raw_tx).await {
                break;
            }
            anchor = AnchorState::Steady;
            match forward_segment(&mut reassembler, segment, &raw_tx).await {
                ForwardStatus::Open => {}
                ForwardStatus::Pressure => anchor = AnchorState::AwaitingFirst,
                ForwardStatus::Closed => break,
            }
            continue;
        }

        match &mut anchor {
            AnchorState::Steady => {
                match forward_segment(&mut reassembler, segment, &raw_tx).await {
                    ForwardStatus::Open => {}
                    ForwardStatus::Pressure => anchor = AnchorState::AwaitingFirst,
                    ForwardStatus::Closed => break,
                }
            }
            AnchorState::AwaitingFirst => {
                let mut burst = InitialBurst::new();
                if burst.would_exceed(&segment) {
                    anchor = AnchorState::Steady;
                    match forward_segment(&mut reassembler, segment, &raw_tx).await {
                        ForwardStatus::Open => {}
                        ForwardStatus::Pressure => anchor = AnchorState::AwaitingFirst,
                        ForwardStatus::Closed => break,
                    }
                    continue;
                }
                burst.push(segment);
                if burst.is_at_limit() {
                    anchor = AnchorState::Buffering {
                        burst,
                        deadline: Instant::now(),
                    };
                    if !flush_anchor(&mut anchor, &mut reassembler, &raw_tx).await {
                        break;
                    }
                } else {
                    anchor = AnchorState::Buffering {
                        burst,
                        deadline: Instant::now() + INITIAL_ANCHOR_WINDOW,
                    };
                }
            }
            AnchorState::Buffering { burst, .. } => {
                if burst.would_exceed(&segment) {
                    if !flush_anchor(&mut anchor, &mut reassembler, &raw_tx).await {
                        break;
                    }
                    match forward_segment(&mut reassembler, segment, &raw_tx).await {
                        ForwardStatus::Open => {}
                        ForwardStatus::Pressure => anchor = AnchorState::AwaitingFirst,
                        ForwardStatus::Closed => break,
                    }
                } else {
                    burst.push(segment);
                    if burst.is_at_limit()
                        && !flush_anchor(&mut anchor, &mut reassembler, &raw_tx).await
                    {
                        break;
                    }
                }
            }
        }
    }
}

async fn flush_anchor(
    anchor: &mut AnchorState,
    reassembler: &mut Reassembler,
    raw_tx: &mpsc::Sender<BudgetedChunk>,
) -> bool {
    let AnchorState::Buffering { burst, .. } = std::mem::replace(anchor, AnchorState::Steady)
    else {
        return true;
    };
    for segment in burst.into_ordered() {
        let status = match reassembler.push_budgeted(segment) {
            ReassemblyOutcome::Chunks(chunks) => forward_chunks(chunks, reassembler, raw_tx).await,
            ReassemblyOutcome::Pressure => ForwardStatus::Pressure,
        };
        match status {
            ForwardStatus::Open => {}
            // Both forms of pressure abandon the rest of the burst: its bytes
            // belong to an origin that no longer exists.
            ForwardStatus::Pressure => {
                *anchor = AnchorState::AwaitingFirst;
                return true;
            }
            ForwardStatus::Closed => return false,
        }
    }
    true
}

async fn forward_segment(
    reassembler: &mut Reassembler,
    segment: BudgetedSegment,
    raw_tx: &mpsc::Sender<BudgetedChunk>,
) -> ForwardStatus {
    match reassembler.push_budgeted(segment) {
        ReassemblyOutcome::Chunks(chunks) => forward_chunks(chunks, reassembler, raw_tx).await,
        ReassemblyOutcome::Pressure => ForwardStatus::Pressure,
    }
}

enum ForwardStatus {
    Open,
    Pressure,
    Closed,
}

/// Moves reassembled chunks into the outbound stage.
///
/// A chunk larger than the whole outbound quota can never be retagged, so it
/// is dropped. That is a hole in the forwarded byte stream, not a closed
/// pipeline: reassembly state is cleared so the next segment re-anchors,
/// exactly as under pending-byte pressure. Reporting it as a closed downstream
/// used to tear the session down and call it a clean end.
async fn forward_chunks(
    chunks: Vec<BudgetedChunk>,
    reassembler: &mut Reassembler,
    raw_tx: &mpsc::Sender<BudgetedChunk>,
) -> ForwardStatus {
    // WebSocket frame boundaries are deliberately non-semantic: reassembly
    // has always produced different batches for in-order arrivals and gap
    // fills. Only the concatenated byte order is the protocol contract.
    for chunk in chunks {
        let retag = chunk.retag_outbound();
        tokio::pin!(retag);
        let chunk = tokio::select! {
            _ = raw_tx.closed() => return ForwardStatus::Closed,
            chunk = &mut retag => match chunk {
                Ok(chunk) => chunk,
                Err(oversized) => {
                    let bytes = oversized.capacity();
                    reassembler.clear();
                    oversized.record_drop();
                    warn!(
                        bytes,
                        "reassembled chunk exceeds the outbound quota; dropped, re-anchoring"
                    );
                    return ForwardStatus::Pressure;
                }
            },
        };
        if raw_tx.send(chunk).await.is_err() {
            return ForwardStatus::Closed;
        }
    }
    ForwardStatus::Open
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
fn capture_loop_budgeted(
    mut source: Box<dyn PacketSource>,
    tx: mpsc::Sender<CaptureEvent>,
    gate: WatchGate,
    shutdown: watch::Receiver<bool>,
    fatal: mpsc::Sender<String>,
    budget: PipelineBudget,
    pressure_resync: PressureResync,
) {
    let mut was_enabled = gate.is_enabled();
    let mut pending_player_resync = false;
    let mut admitted_segments: u64 = 0;
    loop {
        let segment = match source.next_segment() {
            Ok(segment) => segment,
            Err(err) => {
                if !*shutdown.borrow() {
                    error!(error = %err, "capture interrupted");
                    let _ = fatal.blocking_send(format!("capture: {err}"));
                }
                break;
            }
        };

        // shutdown_recv may first yield a packet already queued in the driver.
        // Discard it and drop the source rather than forwarding after teardown.
        if *shutdown.borrow() {
            break;
        }

        let enabled = gate.is_enabled();
        // Off -> on transition: the reassembler must re-anchor before any later
        // byte reaches it, otherwise it treats the sequence jump as an
        // unfillable gap and never delivers anything again. The marker is
        // retried on later iterations instead of blocking for queue space:
        // parking this thread would back up the backend's own callback queue,
        // which costs captured packets.
        if enabled && !was_enabled {
            pending_player_resync = true;
        }
        was_enabled = enabled;

        if !enabled {
            continue; // Shop Watch off: emit nothing.
        }
        if tx.is_closed() {
            break;
        }

        let capacity = segment.payload.capacity();
        if pending_player_resync {
            match tx.try_send(CaptureEvent::Resync) {
                Ok(()) => pending_player_resync = false,
                // Every byte ahead of the marker belongs to the epoch the
                // resync discards anyway, so dropping it loses nothing.
                Err(mpsc::error::TrySendError::Full(_)) => {
                    budget.record_drop(capacity);
                    continue;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => break,
            }
        }

        // A backend-side loss breaks byte continuity exactly as byte pressure
        // does, so it reuses the counted, lossless resync protocol rather than
        // leaving reassembly to stall on a gap no retransmission can fill.
        if source.take_capture_loss() && pressure_resync.request(&budget) {
            let stats = budget.snapshot();
            warn!(
                dropped_segments = stats.dropped_segments,
                dropped_bytes = stats.dropped_bytes,
                resyncs = stats.resyncs,
                "capture backend lost packets; dropping until resync acknowledgement"
            );
        }
        if pressure_resync.blocks_segments() {
            budget.record_drop(capacity);
            pressure_resync.try_enqueue(&tx);
            continue;
        }

        let segment = match budget.admit_capture(segment) {
            Ok(segment) => segment,
            Err(segment) => {
                budget.record_drop(segment.payload.capacity());
                if pressure_resync.request(&budget) {
                    let stats = budget.snapshot();
                    warn!(
                        current_total = stats.current_total,
                        capture_bytes = stats.current_capture,
                        pending_bytes = stats.current_reassembly,
                        outbound_bytes = stats.current_outbound,
                        dropped_segments = stats.dropped_segments,
                        dropped_bytes = stats.dropped_bytes,
                        resyncs = stats.resyncs,
                        "capture pipeline byte pressure; dropping until resync acknowledgement"
                    );
                }
                pressure_resync.try_enqueue(&tx);
                continue;
            }
        };
        match tx.try_send(CaptureEvent::Budgeted(segment)) {
            Ok(()) => {
                admitted_segments += 1;
                if admitted_segments.is_multiple_of(CAPTURE_PROGRESS_EVERY) {
                    let stats = budget.snapshot();
                    debug!(
                        segments = admitted_segments,
                        dropped_segments = stats.dropped_segments,
                        dropped_bytes = stats.dropped_bytes,
                        resyncs = stats.resyncs,
                        "capture progress"
                    );
                }
            }
            Err(mpsc::error::TrySendError::Full(CaptureEvent::Budgeted(segment))) => {
                budget.record_drop(segment.capacity());
                if pressure_resync.request(&budget) {
                    let stats = budget.snapshot();
                    warn!(
                        current_total = stats.current_total,
                        capture_bytes = stats.current_capture,
                        dropped_segments = stats.dropped_segments,
                        dropped_bytes = stats.dropped_bytes,
                        resyncs = stats.resyncs,
                        "capture metadata queue full; dropping until resync acknowledgement"
                    );
                }
                drop(segment);
                pressure_resync.try_enqueue(&tx);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => break,
            Err(mpsc::error::TrySendError::Full(_)) => unreachable!(
                "try_send returns the value that was sent, always CaptureEvent::Budgeted"
            ),
        }
    }
}

#[cfg(test)]
fn capture_loop(
    source: Box<dyn PacketSource>,
    tx: mpsc::Sender<CaptureEvent>,
    gate: WatchGate,
    shutdown: watch::Receiver<bool>,
    fatal: mpsc::Sender<String>,
) {
    capture_loop_budgeted(
        source,
        tx,
        gate,
        shutdown,
        fatal,
        PipelineBudget::new(),
        PressureResync::default(),
    );
}

/// Reads stdin lines and forwards them as [`Command`]s; the session loop never
/// touches stdin.
async fn stdin_loop(commands: mpsc::Sender<Command>, shutdown: watch::Receiver<bool>) {
    input_loop(BufReader::new(tokio::io::stdin()), commands, shutdown).await;
}

/// Input-independent select core, injectable in tests so a pending read can be
/// cancelled without touching process stdin.
async fn input_loop(
    input: impl AsyncBufRead + Unpin,
    commands: mpsc::Sender<Command>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut lines = input.lines();
    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            line = lines.next_line() => match line {
                Ok(Some(line)) => match parse_command(&line) {
                    Some(command) => {
                        if commands.send(command).await.is_err() {
                            break; // session loop gone.
                        }
                    }
                    None => println!(
                        ">> unknown command: {:?} (start, stop, enter = toggle)",
                        line.trim()
                    ),
                },
                Ok(None) | Err(_) => break,
            },
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

/// Opens the compiled capture backend.
///
/// Two arms, so both feature combinations build:
///
/// - **WinDivert, through the elevated broker**, when its feature is on. This
///   process never touches the driver: it launches a second copy of this exe
///   with an administrator token and reads raw IP packets back off a named pipe,
///   so the parser, the reassembler, the TLS client and the UI all stay at
///   medium integrity. The packets are IP-layer copies, so the pipeline is
///   indifferent to the link layer beneath — the reason it keeps working on a
///   WiFi adapter, where a NIC-level tap delivers 802.11 frames this crate does
///   not decode.
/// - **No backend** — an error the caller can show, never a panic. This is the
///   `--no-default-features` build, where the rest of the pipeline still
///   compiles and tests.
///
/// Blocking on purpose, and called from `Session::run` on a runtime worker: a
/// UAC prompt sits in the middle of it. `spawn_elevated_broker` keeps that off
/// the runtime's back (see its `blocking` helper), which is why this stays a
/// plain synchronous call.
#[cfg(all(windows, feature = "windivert-backend"))]
fn build_source(config: &Config) -> Result<CaptureSource> {
    use crate::capture::{PipeSource, spawn_elevated_broker};
    info!(
        port = config.game_port,
        "starting the elevated capture helper (Windows will ask for approval)"
    );
    let (reader, stop) = spawn_elevated_broker(config.game_port)?;
    Ok(CaptureSource::new(
        PipeSource::new(reader, config.game_port),
        stop,
    ))
}

#[cfg(not(all(windows, feature = "windivert-backend")))]
fn build_source(_config: &Config) -> Result<CaptureSource> {
    Err(crate::Error::Capture(
        "no capture backend compiled — enable `windivert-backend` (default) on Windows".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::capture::FlowKey;
    use crate::domain::control::Status;

    fn initial_anchor_segment(seq: u32, payload: &[u8]) -> Segment {
        initial_anchor_segment_in(
            FlowKey {
                client: SocketAddr::from((Ipv4Addr::new(192, 168, 1, 10), 51000)),
                server: SocketAddr::from((Ipv4Addr::new(104, 116, 20, 111), 3333)),
            },
            Direction::ServerToClient,
            seq,
            false,
            payload,
        )
    }

    fn initial_anchor_segment_in(
        flow: FlowKey,
        direction: Direction,
        seq: u32,
        syn: bool,
        payload: &[u8],
    ) -> Segment {
        Segment {
            flow,
            direction,
            seq,
            syn,
            payload: Vec::from(payload),
        }
    }

    struct EnableOnFirstSegment {
        gate: WatchGate,
        segment: Option<Segment>,
    }

    impl PacketSource for EnableOnFirstSegment {
        fn next_segment(&mut self) -> crate::Result<Segment> {
            if let Some(segment) = self.segment.take() {
                self.gate.set(true);
                return Ok(segment);
            }
            Err(crate::Error::Capture(
                "characterization complete".to_owned(),
            ))
        }
    }

    /// Reports one backend-side packet loss, then delivers its only segment.
    struct LosingSource {
        segment: Option<Segment>,
        lost: bool,
    }

    impl PacketSource for LosingSource {
        fn next_segment(&mut self) -> crate::Result<Segment> {
            self.segment
                .take()
                .ok_or_else(|| crate::Error::Capture("characterization complete".to_owned()))
        }

        fn take_capture_loss(&mut self) -> bool {
            std::mem::take(&mut self.lost)
        }
    }

    #[derive(Default)]
    struct BlockingCaptureState {
        entered: bool,
        stopped: bool,
    }

    type SharedBlockingCapture = Arc<(Mutex<BlockingCaptureState>, Condvar)>;

    struct BlockingSource {
        state: SharedBlockingCapture,
        _live: LiveGuard,
        events: Option<Arc<Mutex<Vec<&'static str>>>>,
    }

    impl PacketSource for BlockingSource {
        fn next_segment(&mut self) -> crate::Result<Segment> {
            let (lock, wake) = &*self.state;
            let mut state = lock.lock().expect("blocking capture mutex poisoned");
            state.entered = true;
            wake.notify_all();
            while !state.stopped {
                state = wake
                    .wait(state)
                    .expect("blocking capture mutex poisoned while waiting");
            }
            if let Some(events) = &self.events {
                events.lock().unwrap().push("capture");
            }
            Err(crate::Error::Capture("fake receive stopped".to_owned()))
        }
    }

    struct BlockingStop {
        state: SharedBlockingCapture,
    }

    impl CaptureStop for BlockingStop {
        fn stop(&mut self) -> crate::Result<()> {
            let (lock, wake) = &*self.state;
            let mut state = lock.lock().expect("blocking capture mutex poisoned");
            state.stopped = true;
            wake.notify_all();
            Ok(())
        }
    }

    struct NoopStop;

    impl CaptureStop for NoopStop {
        fn stop(&mut self) -> crate::Result<()> {
            Ok(())
        }
    }

    struct ImmediateErrorSource(&'static str);

    impl PacketSource for ImmediateErrorSource {
        fn next_segment(&mut self) -> crate::Result<Segment> {
            Err(crate::Error::Capture(self.0.to_owned()))
        }
    }

    struct LiveGuard(Arc<AtomicUsize>);

    impl LiveGuard {
        fn new(live: &Arc<AtomicUsize>) -> Self {
            live.fetch_add(1, Ordering::SeqCst);
            Self(live.clone())
        }
    }

    impl Drop for LiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn blocking_capture(
        events: Option<Arc<Mutex<Vec<&'static str>>>>,
    ) -> (CaptureSource, SharedBlockingCapture, Arc<AtomicUsize>) {
        let state = Arc::new((Mutex::new(BlockingCaptureState::default()), Condvar::new()));
        let live = Arc::new(AtomicUsize::new(0));
        let source = BlockingSource {
            state: state.clone(),
            _live: LiveGuard::new(&live),
            events,
        };
        let stop = BlockingStop {
            state: state.clone(),
        };
        (CaptureSource::new(source, stop), state, live)
    }

    fn wait_until_capture_blocks(state: &SharedBlockingCapture) {
        let (lock, wake) = &**state;
        let mut state = lock.lock().expect("blocking capture mutex poisoned");
        while !state.entered {
            state = wake
                .wait(state)
                .expect("blocking capture mutex poisoned while waiting");
        }
    }

    fn finished_capture_worker() -> CaptureWorker {
        CaptureWorker {
            stop: Box::new(NoopStop),
            thread: Some(std::thread::spawn(|| {})),
        }
    }

    fn fake_actuator() -> (ActuatorHandle, mpsc::Receiver<plan::Job>) {
        let (jobs, job_rx) = mpsc::channel(1);
        (
            ActuatorHandle::new(
                Mode::Off,
                SnapshotEpoch::default(),
                jobs,
                Arc::new(Mutex::new(plan::Timings::default())),
            ),
            job_rx,
        )
    }

    async fn recv_exact(rx: &mut mpsc::Receiver<BudgetedChunk>, expected_len: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        while bytes.len() < expected_len {
            let chunk = rx.recv().await.expect("outbound channel closed early");
            bytes.extend_from_slice(chunk.as_slice());
        }
        bytes
    }

    fn segment_with_capacity(seq: u32, len: usize, capacity: usize) -> Segment {
        let mut payload = Vec::with_capacity(capacity);
        payload.resize(len, b'X');
        let mut segment = initial_anchor_segment(seq, &[]);
        segment.payload = payload;
        segment
    }

    #[test]
    fn capture_pressure_counts_bytes_and_queues_one_resync() {
        let budget = PipelineBudget::with_test_limits(8, 8, 8, 8);
        let pressure = PressureResync::default();
        let (tx, mut rx) = mpsc::channel(512);
        for _ in 0..512 {
            tx.try_send(CaptureEvent::Resync).unwrap();
        }

        let rejected = match budget.admit_capture(segment_with_capacity(1000, 1, 16)) {
            Ok(_) => panic!("oversized segment unexpectedly admitted"),
            Err(segment) => segment,
        };
        budget.record_drop(rejected.payload.capacity());
        assert!(pressure.request(&budget));
        assert!(!pressure.try_enqueue(&tx));
        assert!(pressure.blocks_segments());
        assert_eq!(budget.snapshot().dropped_segments, 1);
        assert_eq!(budget.snapshot().dropped_bytes, 16);
        assert_eq!(budget.snapshot().resyncs, 1);

        assert!(matches!(rx.try_recv(), Ok(CaptureEvent::Resync)));
        assert!(pressure.try_enqueue(&tx));
        assert!(!pressure.request(&budget));
        for _ in 0..511 {
            assert!(matches!(rx.try_recv(), Ok(CaptureEvent::Resync)));
        }
        assert!(matches!(rx.try_recv(), Ok(CaptureEvent::PressureResync)));
        pressure.acknowledge();
        assert!(!pressure.blocks_segments());
        assert_eq!(budget.snapshot().resyncs, 1);
    }

    #[tokio::test]
    async fn stalled_outbound_never_exceeds_pipeline_budget() {
        let budget = PipelineBudget::with_test_limits(128, 128, 128, 64);
        let mut reassembler = Reassembler::new();
        let mut chunks = match reassembler.push_budgeted(
            budget
                .admit_capture(segment_with_capacity(1000, 1, 64))
                .unwrap(),
        ) {
            ReassemblyOutcome::Chunks(chunks) => chunks,
            ReassemblyOutcome::Pressure => panic!("unexpected pressure"),
        };
        chunks.extend(
            match reassembler.push_budgeted(
                budget
                    .admit_capture(segment_with_capacity(1001, 1, 64))
                    .unwrap(),
            ) {
                ReassemblyOutcome::Chunks(chunks) => chunks,
                ReassemblyOutcome::Pressure => panic!("unexpected pressure"),
            },
        );
        let (tx, mut rx) = mpsc::channel(1);
        // The reassembler travels with the task: `forward_chunks` may have to
        // clear it, and the test still needs it afterwards to release leases.
        let task = tokio::spawn(async move {
            let status = forward_chunks(chunks, &mut reassembler, &tx).await;
            (status, reassembler)
        });
        tokio::task::yield_now().await;
        let stats = budget.snapshot();
        assert_eq!(stats.current_total, 128);
        assert_eq!(stats.current_outbound, 64);
        assert!(stats.current_total <= 128);

        let first = rx.recv().await.unwrap();
        drop(first);
        let (status, reassembler) = task.await.unwrap();
        assert!(matches!(status, ForwardStatus::Open));
        let second = rx.recv().await.unwrap();
        assert!(budget.snapshot().current_total <= 128);
        drop(second);
        drop(reassembler);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn steady_pending_pressure_rearms_the_initial_anchor_window() {
        let budget = PipelineBudget::with_test_limits(256, 256, 8, 256);
        let (event_tx, event_rx) = mpsc::channel(8);
        let (raw_tx, mut raw_rx) = mpsc::channel(8);
        let task = tokio::spawn(reassemble_loop_with_pressure(
            event_rx,
            raw_tx,
            ForwardConfig::default(),
            PressureResync::default(),
        ));

        event_tx
            .send(CaptureEvent::Budgeted(
                budget
                    .admit_capture(segment_with_capacity(1000, 1, 8))
                    .unwrap(),
            ))
            .await
            .unwrap();
        drop(raw_rx.recv().await.unwrap());

        event_tx
            .send(CaptureEvent::Budgeted(
                budget
                    .admit_capture(segment_with_capacity(2000, 1, 16))
                    .unwrap(),
            ))
            .await
            .unwrap();
        event_tx
            .send(CaptureEvent::Budgeted(
                budget
                    .admit_capture(segment_with_capacity(9000, 1, 8))
                    .unwrap(),
            ))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert!(matches!(
            raw_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(budget.snapshot().current_capture, 8);

        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;
        assert_eq!(recv_exact(&mut raw_rx, 1).await, b"X");
        drop(event_tx);
        task.await.unwrap();
        drop(raw_rx);
        assert_eq!(budget.snapshot().current_total, 0);
        assert_eq!(budget.snapshot().resyncs, 1);
    }

    #[test]
    fn initial_anchor_off_to_on_enqueues_resync_before_triggering_segment() {
        let gate = WatchGate::new(false);
        let triggering = initial_anchor_segment(1000, b"AB");
        let source = EnableOnFirstSegment {
            gate: gate.clone(),
            segment: Some(triggering.clone()),
        };
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let (fatal_tx, _fatal_rx) = mpsc::channel(1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);

        capture_loop(Box::new(source), event_tx, gate, shutdown_rx, fatal_tx);

        assert!(matches!(event_rx.try_recv(), Ok(CaptureEvent::Resync)));
        let emitted = event_rx.try_recv().expect("triggering segment follows");
        match emitted {
            CaptureEvent::Budgeted(segment) => {
                assert_eq!(segment.seq, triggering.seq);
                assert_eq!(segment.payload(), triggering.payload);
            }
            CaptureEvent::Segment(_) | CaptureEvent::Resync | CaptureEvent::PressureResync => {
                panic!("expected triggering segment after resync")
            }
        }
    }

    #[test]
    fn off_to_on_resync_drops_bytes_rather_than_parking_the_capture_thread() {
        let gate = WatchGate::new(false);
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let (fatal_tx, _fatal_rx) = mpsc::channel(1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let budget = PipelineBudget::new();
        // Occupy the only slot the resync marker could take. A blocking send
        // here used to park the capture thread, which backs up the backend's
        // own callback queue until it starts losing packets.
        event_tx.try_send(CaptureEvent::PressureResync).unwrap();
        let source = EnableOnFirstSegment {
            gate: gate.clone(),
            segment: Some(initial_anchor_segment(1000, b"AB")),
        };

        capture_loop_budgeted(
            Box::new(source),
            event_tx,
            gate,
            shutdown_rx,
            fatal_tx,
            budget.clone(),
            PressureResync::default(),
        );

        // Returning at all is the point: the segment ahead of the un-enqueued
        // marker is dropped and counted, never forwarded past a stale anchor.
        assert_eq!(budget.snapshot().dropped_segments, 1);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(CaptureEvent::PressureResync)
        ));
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn a_backend_packet_loss_resyncs_instead_of_ending_the_session() {
        let gate = WatchGate::new(true);
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let (fatal_tx, mut fatal_rx) = mpsc::channel(1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let budget = PipelineBudget::new();
        let source = LosingSource {
            segment: Some(initial_anchor_segment(1000, b"AB")),
            lost: true,
        };

        capture_loop_budgeted(
            Box::new(source),
            event_tx,
            gate,
            shutdown_rx,
            fatal_tx,
            budget.clone(),
            PressureResync::default(),
        );

        // The lost bytes cost one segment and a re-anchor, not the session.
        assert!(matches!(
            event_rx.try_recv(),
            Ok(CaptureEvent::PressureResync)
        ));
        let stats = budget.snapshot();
        assert_eq!(stats.resyncs, 1);
        assert_eq!(stats.dropped_segments, 1);
        // The only fatal is the source running out of characterization data.
        assert_eq!(
            fatal_rx.try_recv().unwrap(),
            "capture: network capture: characterization complete"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_first_post_resync_segment_waits_once_then_forwards() {
        let (event_tx, event_rx) = mpsc::channel(4);
        let (raw_tx, mut raw_rx) = mpsc::channel(1);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx, ForwardConfig::default()));

        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(1000, b"AB")))
            .await
            .unwrap();

        tokio::task::yield_now().await;
        assert!(matches!(
            raw_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;
        assert_eq!(recv_exact(&mut raw_rx, 2).await, b"AB");
        drop(event_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_all_six_permutations_preserve_order_across_chunks() {
        let segments = [
            initial_anchor_segment(1000, b"AB"),
            initial_anchor_segment(1002, b"CD"),
            initial_anchor_segment(1004, b"EF"),
        ];
        for permutation in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let (event_tx, event_rx) = mpsc::channel(4);
            let (raw_tx, mut raw_rx) = mpsc::channel(1);
            let task = tokio::spawn(reassemble_loop(event_rx, raw_tx, ForwardConfig::default()));
            event_tx.send(CaptureEvent::Resync).await.unwrap();
            for index in permutation {
                event_tx
                    .send(CaptureEvent::Segment(segments[index].clone()))
                    .await
                    .unwrap();
            }

            tokio::task::yield_now().await;
            assert!(matches!(
                raw_rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
            tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;
            assert_eq!(
                recv_exact(&mut raw_rx, 6).await,
                b"ABCDEF",
                "arrival permutation {permutation:?}"
            );
            drop(event_tx);
            task.await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_exact_segment_limit_flushes_immediately() {
        let (event_tx, event_rx) = mpsc::channel(256);
        let (raw_tx, mut raw_rx) = mpsc::channel(1);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx, ForwardConfig::default()));
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        for index in 0..128u32 {
            event_tx
                .send(CaptureEvent::Segment(initial_anchor_segment(
                    1000 + index,
                    b"X",
                )))
                .await
                .unwrap();
        }

        assert_eq!(recv_exact(&mut raw_rx, 128).await, vec![b'X'; 128]);
        drop(event_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_exact_byte_limit_flushes_immediately() {
        let (event_tx, event_rx) = mpsc::channel(2);
        let (raw_tx, mut raw_rx) = mpsc::channel(1);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx, ForwardConfig::default()));
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(
                1000,
                &vec![b'X'; crate::stream::INITIAL_ANCHOR_MAX_BYTES],
            )))
            .await
            .unwrap();

        assert_eq!(
            raw_rx.recv().await.map(|bytes| bytes.len()),
            Some(crate::stream::INITIAL_ANCHOR_MAX_BYTES)
        );
        drop(event_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_byte_overflow_flushes_before_processing_next_segment() {
        let (event_tx, event_rx) = mpsc::channel(3);
        let (raw_tx, mut raw_rx) = mpsc::channel(2);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx, ForwardConfig::default()));
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(
                1000,
                &vec![b'A'; crate::stream::INITIAL_ANCHOR_MAX_BYTES - 1],
            )))
            .await
            .unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(
                1000u32.wrapping_add((crate::stream::INITIAL_ANCHOR_MAX_BYTES - 1) as u32),
                b"BC",
            )))
            .await
            .unwrap();

        assert_eq!(
            raw_rx.recv().await.map(|bytes| bytes.len()),
            Some(crate::stream::INITIAL_ANCHOR_MAX_BYTES - 1)
        );
        assert_eq!(recv_exact(&mut raw_rx, 2).await, b"BC");
        drop(event_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_channel_close_flushes_pending_burst() {
        let (event_tx, event_rx) = mpsc::channel(2);
        let (raw_tx, mut raw_rx) = mpsc::channel(1);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx, ForwardConfig::default()));
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(1000, b"AB")))
            .await
            .unwrap();
        drop(event_tx);

        assert_eq!(recv_exact(&mut raw_rx, 2).await, b"AB");
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_downstream_close_does_not_wait_for_deadline() {
        let (event_tx, event_rx) = mpsc::channel(2);
        let (raw_tx, raw_rx) = mpsc::channel(1);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx, ForwardConfig::default()));
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(1000, b"AB")))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        drop(raw_rx);

        task.await.unwrap();
        drop(event_tx);
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_new_resync_discards_and_rearms_pending_epoch() {
        let (event_tx, event_rx) = mpsc::channel(4);
        let (raw_tx, mut raw_rx) = mpsc::channel(1);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx, ForwardConfig::default()));
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(1000, b"AB")))
            .await
            .unwrap();
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(9000, b"XY")))
            .await
            .unwrap();

        tokio::task::yield_now().await;
        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;
        assert_eq!(recv_exact(&mut raw_rx, 2).await, b"XY");
        drop(event_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_returns_to_immediate_steady_state_after_one_flush() {
        let (event_tx, event_rx) = mpsc::channel(3);
        let (raw_tx, mut raw_rx) = mpsc::channel(2);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx, ForwardConfig::default()));
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(1000, b"AB")))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;
        assert_eq!(recv_exact(&mut raw_rx, 2).await, b"AB");

        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(1002, b"CD")))
            .await
            .unwrap();
        assert_eq!(recv_exact(&mut raw_rx, 2).await, b"CD");
        drop(event_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_filters_before_starting_timer_or_counting_budget() {
        let (event_tx, event_rx) = mpsc::channel(4);
        let (raw_tx, mut raw_rx) = mpsc::channel(1);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx, ForwardConfig::default()));
        let base = initial_anchor_segment(1000, b"ignored");
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment_in(
                base.flow,
                Direction::ClientToServer,
                2000,
                false,
                b"ignored",
            )))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;
        assert!(matches!(
            raw_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(1000, b"AB")))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert!(matches!(
            raw_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;
        assert_eq!(recv_exact(&mut raw_rx, 2).await, b"AB");
        drop(event_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_isolates_flow_and_direction_while_preserving_slots() {
        let (event_tx, event_rx) = mpsc::channel(6);
        let (raw_tx, mut raw_rx) = mpsc::channel(1);
        let task = tokio::spawn(reassemble_loop(
            event_rx,
            raw_tx,
            ForwardConfig {
                server_to_client: true,
                client_to_server: true,
            },
        ));
        let first = initial_anchor_segment(1000, b"AB").flow;
        let second = FlowKey {
            client: SocketAddr::from((Ipv4Addr::new(192, 168, 1, 10), 52000)),
            server: first.server,
        };
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        for segment in [
            initial_anchor_segment_in(first, Direction::ServerToClient, 1002, false, b"CD"),
            initial_anchor_segment_in(second, Direction::ClientToServer, 2002, false, b"WX"),
            initial_anchor_segment_in(first, Direction::ServerToClient, 1000, false, b"AB"),
            initial_anchor_segment_in(second, Direction::ClientToServer, 2000, false, b"UV"),
        ] {
            event_tx.send(CaptureEvent::Segment(segment)).await.unwrap();
        }
        tokio::task::yield_now().await;
        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;

        assert_eq!(recv_exact(&mut raw_rx, 8).await, b"ABUVCDWX");
        drop(event_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_wrap_and_overlap_are_reassembled_after_burst_ordering() {
        let (event_tx, event_rx) = mpsc::channel(5);
        let (raw_tx, mut raw_rx) = mpsc::channel(1);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx, ForwardConfig::default()));
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        for segment in [
            initial_anchor_segment(0, b"CDEF"),
            initial_anchor_segment(2, b"EFGH"),
            initial_anchor_segment(u32::MAX - 1, b"ABCD"),
        ] {
            event_tx.send(CaptureEvent::Segment(segment)).await.unwrap();
        }
        tokio::task::yield_now().await;
        tokio::time::advance(INITIAL_ANCHOR_WINDOW).await;

        assert_eq!(recv_exact(&mut raw_rx, 8).await, b"ABCDEFGH");
        drop(event_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_syn_is_never_delayed_and_new_flows_need_no_global_window() {
        let (event_tx, event_rx) = mpsc::channel(6);
        let (raw_tx, mut raw_rx) = mpsc::channel(2);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx, ForwardConfig::default()));
        let first = initial_anchor_segment(1000, b"AB").flow;
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment_in(
                first,
                Direction::ServerToClient,
                999,
                true,
                b"",
            )))
            .await
            .unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment(1000, b"AB")))
            .await
            .unwrap();
        assert_eq!(recv_exact(&mut raw_rx, 2).await, b"AB");

        let second = FlowKey {
            client: SocketAddr::from((Ipv4Addr::new(192, 168, 1, 10), 52000)),
            server: first.server,
        };
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment_in(
                second,
                Direction::ServerToClient,
                4999,
                true,
                b"",
            )))
            .await
            .unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment_in(
                second,
                Direction::ServerToClient,
                5000,
                false,
                b"XY",
            )))
            .await
            .unwrap();
        assert_eq!(recv_exact(&mut raw_rx, 2).await, b"XY");
        drop(event_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn initial_anchor_new_syn_flushes_pending_burst_then_resets_immediately() {
        let (event_tx, event_rx) = mpsc::channel(5);
        let (raw_tx, mut raw_rx) = mpsc::channel(2);
        let task = tokio::spawn(reassemble_loop(event_rx, raw_tx, ForwardConfig::default()));
        let flow = initial_anchor_segment(1000, b"old").flow;
        event_tx.send(CaptureEvent::Resync).await.unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment_in(
                flow,
                Direction::ServerToClient,
                1000,
                false,
                b"old",
            )))
            .await
            .unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment_in(
                flow,
                Direction::ServerToClient,
                8999,
                true,
                b"",
            )))
            .await
            .unwrap();
        event_tx
            .send(CaptureEvent::Segment(initial_anchor_segment_in(
                flow,
                Direction::ServerToClient,
                9000,
                false,
                b"new",
            )))
            .await
            .unwrap();

        assert_eq!(recv_exact(&mut raw_rx, 3).await, b"old");
        assert_eq!(recv_exact(&mut raw_rx, 3).await, b"new");
        drop(event_tx);
        task.await.unwrap();
    }

    #[test]
    fn worker_shutdown_capture_wakes_blocking_receive_and_joins() {
        let (source, state, live) = blocking_capture(None);
        let (event_tx, _event_rx) = mpsc::channel(1);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (fatal_tx, mut fatal_rx) = mpsc::channel(1);
        let mut capture = spawn_capture(
            source,
            event_tx,
            WatchGate::new(false),
            shutdown_rx,
            fatal_tx,
        )
        .unwrap();
        wait_until_capture_blocks(&state);

        shutdown_tx.send_replace(true);
        capture.stop_and_join();

        assert!(state.0.lock().unwrap().stopped);
        assert_eq!(live.load(Ordering::SeqCst), 0, "capture source was joined");
        assert!(matches!(
            fatal_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn worker_shutdown_capture_error_is_fatal_before_shutdown() {
        let source = CaptureSource::new(ImmediateErrorSource("receive failed"), NoopStop);
        let (event_tx, _event_rx) = mpsc::channel(1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (fatal_tx, mut fatal_rx) = mpsc::channel(1);
        let mut capture = spawn_capture(
            source,
            event_tx,
            WatchGate::new(false),
            shutdown_rx,
            fatal_tx,
        )
        .unwrap();

        capture.stop_and_join();

        assert_eq!(
            fatal_rx.try_recv().unwrap(),
            "capture: network capture: receive failed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn worker_shutdown_clean_pipeline_closes_in_producer_order() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (source, state, capture_live) = blocking_capture(Some(events.clone()));
        let (event_tx, mut event_rx) = mpsc::channel::<CaptureEvent>(1);
        let shutdown_tx = ShutdownSignal::new();
        let shutdown_rx = shutdown_tx.subscribe();
        let (fatal_tx, mut fatal_rx) = mpsc::channel(4);
        let capture = spawn_capture(
            source,
            event_tx,
            WatchGate::new(true),
            shutdown_rx.clone(),
            fatal_tx.clone(),
        )
        .unwrap();
        wait_until_capture_blocks(&state);
        let mut workers = SessionWorkers::new(shutdown_tx, capture);
        let task_live = Arc::new(AtomicUsize::new(0));
        let (raw_tx, mut raw_rx) = mpsc::channel::<Vec<u8>>(1);

        let reassembly_events = events.clone();
        let reassembly_guard = LiveGuard::new(&task_live);
        workers.spawn("reassembly", &fatal_tx, async move {
            let _guard = reassembly_guard;
            while event_rx.recv().await.is_some() {}
            reassembly_events.lock().unwrap().push("reassembly");
            drop(raw_tx);
        });

        let uplink_events = events.clone();
        let uplink_guard = LiveGuard::new(&task_live);
        workers.spawn("uplink", &fatal_tx, async move {
            let _guard = uplink_guard;
            while raw_rx.recv().await.is_some() {}
            uplink_events.lock().unwrap().push("uplink");
        });

        let (actuator, mut job_rx) = fake_actuator();
        let actuator_events = events.clone();
        let actuator_guard = LiveGuard::new(&task_live);
        workers.spawn("actuator", &fatal_tx, async move {
            let _guard = actuator_guard;
            while job_rx.recv().await.is_some() {}
            actuator_events.lock().unwrap().push("actuator");
        });

        let stdin_events = events.clone();
        let stdin_guard = LiveGuard::new(&task_live);
        workers.spawn("stdin", &fatal_tx, async move {
            let _guard = stdin_guard;
            let mut shutdown = shutdown_rx;
            if !*shutdown.borrow() {
                let _ = shutdown.changed().await;
            }
            stdin_events.lock().unwrap().push("stdin");
        });
        drop(fatal_tx);
        let gate = WatchGate::new(true);

        workers.shutdown(&gate, actuator).await;

        let events = events.lock().unwrap();
        let capture = events.iter().position(|event| *event == "capture").unwrap();
        let reassembly = events
            .iter()
            .position(|event| *event == "reassembly")
            .unwrap();
        let uplink = events.iter().position(|event| *event == "uplink").unwrap();
        assert!(capture < reassembly && reassembly < uplink, "{events:?}");
        assert!(events.contains(&"actuator"));
        assert!(events.contains(&"stdin"));
        assert!(!gate.is_enabled());
        assert_eq!(capture_live.load(Ordering::SeqCst), 0);
        assert_eq!(task_live.load(Ordering::SeqCst), 0);
        assert!(matches!(
            fatal_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn worker_shutdown_cooperative_task_exits_during_grace_and_is_joined() {
        let shutdown_tx = ShutdownSignal::new();
        let mut shutdown_rx = shutdown_tx.subscribe();
        let mut workers = SessionWorkers::new(shutdown_tx, finished_capture_worker());
        let (fatal_tx, _fatal_rx) = mpsc::channel(1);
        let live = Arc::new(AtomicUsize::new(0));
        let guard = LiveGuard::new(&live);
        workers.spawn("cooperative", &fatal_tx, async move {
            let _guard = guard;
            if !*shutdown_rx.borrow() {
                let _ = shutdown_rx.changed().await;
            }
        });
        let (actuator, _job_rx) = fake_actuator();
        let before = Instant::now();

        workers.shutdown(&WatchGate::new(true), actuator).await;

        assert!(Instant::now().duration_since(before) < WORKER_SHUTDOWN_GRACE);
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn worker_shutdown_pending_tasks_share_deadline_and_abort_is_awaited() {
        let shutdown_tx = ShutdownSignal::new();
        let mut workers = SessionWorkers::new(shutdown_tx, finished_capture_worker());
        let (fatal_tx, _fatal_rx) = mpsc::channel(1);
        let live = Arc::new(AtomicUsize::new(0));
        for name in ["pending-one", "pending-two"] {
            let guard = LiveGuard::new(&live);
            workers.spawn(name, &fatal_tx, async move {
                let _guard = guard;
                pending::<()>().await;
            });
        }
        let (actuator, _job_rx) = fake_actuator();
        let before = Instant::now();

        workers.shutdown(&WatchGate::new(true), actuator).await;

        assert_eq!(
            Instant::now().duration_since(before),
            WORKER_SHUTDOWN_GRACE,
            "the deadline is shared rather than restarted per task"
        );
        assert_eq!(live.load(Ordering::SeqCst), 0, "aborts were awaited");
    }

    #[tokio::test(start_paused = true)]
    async fn worker_shutdown_first_fatal_remains_primary_during_task_panic() {
        let source = CaptureSource::new(ImmediateErrorSource("first"), NoopStop);
        let (event_tx, _event_rx) = mpsc::channel(1);
        let shutdown_tx = ShutdownSignal::new();
        let shutdown_rx = shutdown_tx.subscribe();
        let (fatal_tx, mut fatal_rx) = mpsc::channel(4);
        let capture = spawn_capture(
            source,
            event_tx,
            WatchGate::new(false),
            shutdown_rx,
            fatal_tx.clone(),
        )
        .unwrap();
        // Select and freeze the first cause before allowing the racing panic;
        // this is deterministic and makes teardown unable to overwrite it.
        let primary = fatal_rx.recv().await.unwrap();
        let mut workers = SessionWorkers::new(shutdown_tx, capture);
        workers.spawn("uplink", &fatal_tx, async { panic!("nearby panic") });
        let (actuator, _job_rx) = fake_actuator();
        drop(fatal_tx);

        workers.shutdown(&WatchGate::new(false), actuator).await;

        assert_eq!(primary, "capture: network capture: first");
        assert_eq!(
            fatal_rx.try_recv().unwrap(),
            "uplink task panicked",
            "the secondary panic remains diagnostic only"
        );
    }

    #[tokio::test]
    async fn worker_shutdown_pending_stdin_read_exits_on_signal() {
        let (reader, _writer) = tokio::io::duplex(64);
        let (commands, mut command_rx) = mpsc::channel(1);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(input_loop(BufReader::new(reader), commands, shutdown_rx));
        tokio::task::yield_now().await;

        shutdown_tx.send_replace(true);
        task.await.unwrap();

        assert!(matches!(
            command_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

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
        let (_session, handles, _shutdown) = setup(Config::default());
        assert!(!handles.controller.lock().unwrap().recovery_enabled());
    }

    /// Live clicking arms the watchdog; a dry run produces no wire feedback
    /// and must keep it dark.
    #[cfg(all(windows, feature = "actuator"))]
    #[test]
    fn setup_enables_recovery_only_when_live() {
        let (_session, handles, _shutdown) = setup(Config::default());
        assert!(handles.controller.lock().unwrap().recovery_enabled());

        let mut config = Config::default();
        config.actuator.dry_run = true;
        let (_session, handles, _shutdown) = setup(config);
        assert!(!handles.controller.lock().unwrap().recovery_enabled());
    }

    async fn panicking_session() -> crate::Result<()> {
        panic!("boom")
    }

    #[tokio::test]
    async fn worker_shutdown_named_task_reports_a_panic() {
        let shutdown_tx = ShutdownSignal::new();
        let mut workers = SessionWorkers::new(shutdown_tx, finished_capture_worker());
        let (fatal_tx, mut fatal_rx) = mpsc::channel::<String>(4);
        workers.spawn("uplink", &fatal_tx, async { panic!("boom") });
        let (actuator, _job_rx) = fake_actuator();

        assert_eq!(
            fatal_rx.recv().await.as_deref(),
            Some("uplink task panicked")
        );
        workers.shutdown(&WatchGate::new(false), actuator).await;
    }

    #[tokio::test]
    async fn worker_shutdown_named_task_ignores_a_clean_exit() {
        let shutdown_tx = ShutdownSignal::new();
        let mut workers = SessionWorkers::new(shutdown_tx, finished_capture_worker());
        let (fatal_tx, mut fatal_rx) = mpsc::channel::<String>(4);
        workers.spawn("uplink", &fatal_tx, async {});
        let (actuator, _job_rx) = fake_actuator();
        drop(fatal_tx);

        workers.shutdown(&WatchGate::new(false), actuator).await;
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

    /// A logged URL is a URL the player is asked to send us: userinfo and
    /// query string must never survive it.
    #[test]
    fn redacted_server_url_keeps_only_scheme_and_host() {
        assert_eq!(
            redacted_server_url("wss://ingest.arkyve.dev/refresh-shop"),
            "wss://ingest.arkyve.dev"
        );
        assert_eq!(
            redacted_server_url("wss://token:secret@ingest.arkyve.dev:8443/path?key=abc"),
            "wss://ingest.arkyve.dev:8443"
        );
        assert_eq!(
            redacted_server_url("ws://127.0.0.1:9000/?key=abc#frag"),
            "ws://127.0.0.1:9000"
        );
        // Not a URL at all: nothing to leak, nothing to invent.
        assert_eq!(redacted_server_url("garbage"), "://garbage");
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
