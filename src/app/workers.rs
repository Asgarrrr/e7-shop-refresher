//! Worker supervision and teardown: who owns the capture thread and the four
//! Tokio tasks, and in what order they are wound down.
//!
//! Holds *handles*, never payloads — it never looks inside a [`CaptureEvent`].

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use futures_util::FutureExt;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tracing::{Instrument as _, error};

use crate::Result;
use crate::actuator::ActuatorHandle;
use crate::capture::{CaptureSource, CaptureStop};
use crate::stream::PipelineBudget;
use crate::watch::WatchGate;

use super::ShutdownSignal;
use super::ingest::capture_loop_budgeted;
use super::pressure::{CaptureEvent, PressureResync};

/// One deadline for the whole Tokio worker set. Cooperative pipeline closure
/// normally finishes immediately; this is only the cancellation-safe fallback.
const WORKER_SHUTDOWN_GRACE: Duration = Duration::from_millis(250);

pub(super) struct CaptureWorker {
    stop: Box<dyn CaptureStop>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CaptureWorker {
    /// Wakes and joins the OS thread synchronously. A timeout here would only
    /// detach an unabortable thread, so shutdown capability is the finite join.
    fn stop_and_join(&mut self) {
        self.stop.stop();
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

pub(super) struct SessionWorkers {
    shutdown: ShutdownSignal,
    capture: CaptureWorker,
    tasks: Vec<TokioWorker>,
}

impl SessionWorkers {
    pub(super) fn new(shutdown: ShutdownSignal, capture: CaptureWorker) -> Self {
        Self {
            shutdown,
            capture,
            tasks: Vec::new(),
        }
    }

    /// The panic catcher is the owned worker itself: no detached supervisor
    /// holds a second child handle. The span is entered here because four
    /// tasks interleave into one log file and all need `name` to correlate.
    pub(super) fn spawn(
        &mut self,
        name: &'static str,
        fatal: &mpsc::Sender<String>,
        future: impl Future<Output = ()> + Send + 'static,
    ) {
        let fatal = fatal.clone();
        let handle = tokio::spawn(
            async move {
                if AssertUnwindSafe(future).catch_unwind().await.is_err() {
                    let _ = fatal.send(format!("{name} task panicked")).await;
                }
            }
            .instrument(tracing::info_span!("worker", name)),
        );
        self.tasks.push(TokioWorker {
            name,
            handle: Some(handle),
            aborted: false,
        });
    }

    /// Producer-to-consumer teardown: gate/signal, actuator producer close,
    /// capture wake+join, pipeline EOF, then one grace deadline for all tasks.
    pub(super) async fn shutdown(mut self, gate: &WatchGate, actuator: ActuatorHandle) {
        gate.set(false);
        self.shutdown.request();
        drop(actuator);

        // `stop_and_join` parks its caller in `Thread::join`; on a runtime
        // worker that denies the scheduler the very tasks it is waiting on —
        // a deadlock at one worker thread.
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

        // The single deadline elapsed: abort every unfinished task before
        // awaiting any of them, so the aborts overlap.
        for worker in &mut self.tasks {
            if let Some(handle) = worker.handle.as_ref()
                && !handle.is_finished()
            {
                handle.abort();
                worker.aborted = true;
            }
        }
        // A *second* deadline, not a bare await: `abort()` only lands at an
        // await point, the actuator worker sits in `block_in_place`, which has
        // none, and a `SetWindowPos` aimed at a frozen Epic Seven never returns
        // (`actuator::win`) — so an unbounded await parks teardown, and with it
        // `runtime.block_on`, past a second Ctrl+C tokio already swallowed.
        // Detaching on timeout is the honest outcome: a thread stuck in a Win32
        // call cannot be reclaimed from here.
        let abandon_at = Instant::now() + WORKER_SHUTDOWN_GRACE;
        for worker in &mut self.tasks {
            if let Some(handle) = worker.handle.take() {
                match tokio::time::timeout_at(abandon_at, handle).await {
                    Ok(result) => report_join(worker.name, worker.aborted, result),
                    Err(_) => error!(
                        worker = worker.name,
                        "worker did not exit after abort — detaching it and finishing teardown"
                    ),
                }
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
        error!(worker = name, error = ?err, "worker join failed during teardown");
    }
}

pub(super) fn spawn_capture_with_budget(
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
        })
        // Not `?`: the blanket `From<io::Error>` would surface an exhausted
        // thread limit as a bare "i/o: <os error>", naming nothing.
        .map_err(|source| {
            crate::Error::Capture(format!("starting the capture thread: {source}"))
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

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};

    use super::*;
    use crate::actuator::{Mode, SnapshotEpoch, plan};
    use crate::capture::{PacketSource, Segment};

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
        fn next_segment(&mut self) -> Result<Segment> {
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
        fn stop(&mut self) {
            let (lock, wake) = &*self.state;
            let mut state = lock.lock().expect("blocking capture mutex poisoned");
            state.stopped = true;
            wake.notify_all();
        }
    }

    struct NoopStop;

    impl CaptureStop for NoopStop {
        fn stop(&mut self) {}
    }

    struct ImmediateErrorSource(&'static str);

    impl PacketSource for ImmediateErrorSource {
        fn next_segment(&mut self) -> Result<Segment> {
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
            "network capture: receive failed"
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
        // Freeze the first cause before allowing the racing panic, so teardown
        // cannot overwrite it.
        let primary = fatal_rx.recv().await.unwrap();
        let mut workers = SessionWorkers::new(shutdown_tx, capture);
        workers.spawn("uplink", &fatal_tx, async { panic!("nearby panic") });
        let (actuator, _job_rx) = fake_actuator();
        drop(fatal_tx);

        workers.shutdown(&WatchGate::new(false), actuator).await;

        assert_eq!(primary, "network capture: first");
        assert_eq!(
            fatal_rx.try_recv().unwrap(),
            "uplink task panicked",
            "the secondary panic remains diagnostic only"
        );
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
}
