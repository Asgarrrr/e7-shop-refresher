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
    /// Wakes and joins the OS thread synchronously. A timeout *here* would only
    /// detach an unabortable thread, so the join stays untimed and the bound
    /// lives one level up, in the process: both `run_mode` arms hand the whole
    /// session to `teardown_failed(.., TEARDOWN_GRACE)` (`src/main.rs`), which
    /// aborts the wait and logs the detach when the grace expires.
    ///
    /// It is not "the finite join": when `stop()` fails to wake the thread this
    /// never returns, and that process-level grace is the only thing that ends
    /// the wait — held by
    /// `worker_shutdown_stalls_when_stop_does_not_wake_the_capture_thread`
    /// below.
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

/// Raises this thread's own scheduling priority above the process default.
///
/// The thread `spawn_capture_with_budget` spawns is the one that drains
/// `capture::pcap`'s frame funnel. At `NORMAL` priority it competes for CPU
/// time on equal footing with every other thread on the machine, including an
/// unrelated process's video decode — lose that race for long enough and the
/// funnel fills, a frame is dropped and `ResyncCause::CaptureFunnel` fires, on
/// a machine whose network and driver are both fine. That is not hypothetical:
/// it is the session `capture::pcap::FRAME_QUEUE_BYTES` cites, where a video
/// was playing beside the game. `HIGHEST` is two steps above `NORMAL` within
/// the process's own priority *class* (still `NORMAL_PRIORITY_CLASS`, not
/// `REALTIME_PRIORITY_CLASS`), so it needs no privilege beyond what an
/// unelevated process already has.
///
/// This and the size of the funnel are not alternatives, and neither is
/// sufficient. Priority shortens the stall; the funnel's byte bound (4 MiB,
/// roughly three thousand ordinary frames, against the sixteen slots that
/// overflowed) survives the stall that happens anyway. Neither has been
/// measured to remove `ResyncCause::CaptureFunnel` on its own — what was
/// measured is the overflow both are aimed at.
///
/// This is a real trade, not a free win: Win32 scheduling preempts strictly by
/// priority, so this thread now also outranks every other `NORMAL` thread in
/// *this* process — the egui/winit UI thread and the tokio relay-workers that
/// read what this one forwards downstream included. What keeps that
/// acceptable is the loop this guards (`capture_loop_budgeted`): it blocks on
/// `recv_timeout` between packets rather than spinning, so the windows where
/// it actually holds a core are short, not sustained.
#[cfg(windows)]
fn raise_capture_thread_priority() {
    // Imported here and not at the top of the module: this is the only `warn!`
    // in the file and it is Windows-only, so a module-level import is an unused
    // one on every dev lane.
    use tracing::warn;
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
    };

    // SAFETY: a pseudo-handle valid for the calling thread's whole lifetime,
    // never needing `CloseHandle`.
    let this_thread = unsafe { GetCurrentThread() };
    // SAFETY: `this_thread` is a valid handle to the thread making this call,
    // so `SetThreadPriority` only affects the thread that owns it.
    if unsafe { SetThreadPriority(this_thread, THREAD_PRIORITY_HIGHEST) } == 0 {
        warn!("could not raise the capture thread's priority; scheduling stays default");
    }
}

/// No thread-priority API is reached off Windows; this backend is Windows-only
/// (see `pcap-backend`'s `libloading` dependency), so nothing here is ever
/// exercised, but `spawn_capture_with_budget` still compiles for the dev
/// lanes that build this module cross-platform.
#[cfg(not(windows))]
fn raise_capture_thread_priority() {}

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
            raise_capture_thread_priority();
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
            // Released before `events` is taken: nothing past the wait reads
            // the capture state, and holding it here would make this the one
            // place that nests state-then-events — an order the four workers
            // below, which only ever take `events` alone, never establish.
            drop(state);
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
            // Notified after the release, unlike the `entered` half above where
            // the guard has to survive into the wait loop. No wakeup is lost
            // either way: a waiter either still holds the lock, and will see the
            // flag on its own re-check, or is already inside `wait` and cannot
            // miss the notify.
            drop(state);
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

    /// A capture whose `stop()` does not wake the parked receive — the pcap
    /// failure `stop_and_join` carries no bound of its own against. The shared
    /// state comes back so the caller can release the thread itself once it has
    /// measured the stall.
    fn unwakeable_capture() -> (CaptureSource, SharedBlockingCapture) {
        let state = Arc::new((Mutex::new(BlockingCaptureState::default()), Condvar::new()));
        let live = Arc::new(AtomicUsize::new(0));
        let source = BlockingSource {
            state: state.clone(),
            _live: LiveGuard::new(&live),
            events: None,
        };
        (CaptureSource::new(source, NoopStop), state)
    }

    /// Releases a capture thread parked by [`unwakeable_capture`] on the way
    /// out, an assertion failure included: the blocking task
    /// `SessionWorkers::shutdown` leaves behind outlives the abort, and the
    /// test runtime's drop waits for the blocking pool — so without this a
    /// failing assertion would hang the test binary instead of reporting.
    struct ReleaseOnDrop(SharedBlockingCapture);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            let (lock, wake) = &*self.0;
            crate::sync::lock_ignoring_poison(lock).stopped = true;
            wake.notify_all();
        }
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
                Arc::new(Mutex::new(crate::actuator::ClickMode::default())),
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

    /// `raise_capture_thread_priority`'s effect is OS scheduler state, not a
    /// value this crate owns — the only honest check is asking Windows what it
    /// actually did, the way `install.rs`'s Windows-only unsafe surface is
    /// verified elsewhere in this crate.
    #[cfg(windows)]
    #[test]
    fn worker_capture_thread_runs_at_highest_priority() {
        use std::os::windows::io::AsRawHandle;

        use windows_sys::Win32::System::Threading::{GetThreadPriority, THREAD_PRIORITY_HIGHEST};

        let (source, state, _live) = blocking_capture(None);
        let (event_tx, _event_rx) = mpsc::channel(1);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (fatal_tx, _fatal_rx) = mpsc::channel(1);
        let mut capture = spawn_capture(
            source,
            event_tx,
            WatchGate::new(false),
            shutdown_rx,
            fatal_tx,
        )
        .unwrap();
        // `entered` is only set once `next_segment` runs, which is ordered
        // after `raise_capture_thread_priority` on the same thread — so
        // observing it here means the priority call already landed.
        wait_until_capture_blocks(&state);

        let handle = capture.thread.as_ref().unwrap().as_raw_handle();
        // SAFETY: `handle` names the capture thread this function's own
        // `capture` variable is still joining on, so it is a live, valid
        // thread handle for the whole call.
        let priority = unsafe { GetThreadPriority(handle.cast()) };
        assert_eq!(
            priority, THREAD_PRIORITY_HIGHEST,
            "capture thread must win scheduling races against unrelated CPU load"
        );

        capture.stop_and_join();
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

        // One guard for the whole block: the three positions are compared
        // against one another, so they have to index the same vector, and the
        // `{events:?}` that reports the failure has to be that vector too.
        // Nothing contends for it here — `shutdown` returned, so every task
        // that pushes into it has already been joined.
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

    /// The premise `stop_and_join`'s comment rests on, and the reason the
    /// process-level grace exists: a `stop()` that does not wake the capture
    /// thread leaves `SessionWorkers::shutdown` with no deadline of its own,
    /// and only `teardown_failed` ends the wait. Give this session's own
    /// teardown an internal bound and this fails, because `teardown_failed`
    /// would then report a session that finished rather than one abandoned.
    ///
    /// A real clock and a short grace, unlike its neighbours: under
    /// `start_paused` the runtime never auto-advances while the `spawn_blocking`
    /// that `SessionWorkers::shutdown` parks the capture join in is still in
    /// flight, so the deadline is never reached — measured, this test ran past
    /// libtest's "has been running for over 60 seconds" notice and had to be
    /// killed. Nothing below asserts *how long* the wait was, only that it had
    /// to be cut short, so the grace is sized for the test rather than for the
    /// process.
    #[tokio::test]
    async fn worker_shutdown_stalls_when_stop_does_not_wake_the_capture_thread() {
        let (source, state) = unwakeable_capture();
        let _release = ReleaseOnDrop(state.clone());
        let (event_tx, _event_rx) = mpsc::channel(1);
        let shutdown_tx = ShutdownSignal::new();
        let shutdown_rx = shutdown_tx.subscribe();
        let (fatal_tx, _fatal_rx) = mpsc::channel(1);
        let capture = spawn_capture(
            source,
            event_tx,
            WatchGate::new(false),
            shutdown_rx,
            fatal_tx,
        )
        .unwrap();
        wait_until_capture_blocks(&state);
        let workers = SessionWorkers::new(shutdown_tx, capture);

        let teardown = tokio::spawn(async move {
            let (actuator, _job_rx) = fake_actuator();
            workers.shutdown(&WatchGate::new(true), actuator).await;
        });

        // The call both of `main.rs`'s `run_mode` arms make on the whole
        // session, standing in for its `TEARDOWN_GRACE`.
        assert!(
            crate::teardown_failed(teardown, Duration::from_millis(200)).await,
            "a capture stop that never wakes the thread must be reported as an abandoned session"
        );
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
