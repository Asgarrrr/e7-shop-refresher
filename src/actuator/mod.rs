//! The actuator: turns controller decisions into input driven into the game
//! window. [`plan`] is the pure half (zones, transform, timed job builders);
//! the executor below replays a job's steps against a [`Surface`], dropping
//! the job the moment the world changes underneath it.

pub mod plan;
#[cfg(all(windows, feature = "actuator"))]
mod shield;
#[cfg(all(windows, feature = "actuator"))]
pub mod win;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::journal::EventLog;
use crate::watch::{HaltSource, WatchGate};

use plan::{Input, Job, Timings};

/// Generation counter of the shop state, bumped on every shop message. A job
/// carries the epoch it was planned against and the executor refuses to act
/// on any other: clicks aimed at a shop that no longer exists must die, not
/// land.
#[derive(Clone, Default)]
pub struct SnapshotEpoch(Arc<AtomicU64>);

impl SnapshotEpoch {
    pub fn bump(&self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }

    #[must_use]
    pub fn current(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// No input backend armed: decisions render as advice to the player.
    Off,
    /// Plans and journals the clicks (screen coords + waits), sends nothing.
    DryRun,
    /// Sends real input to the game window.
    Live,
}

/// The session's grip on the executor: submit jobs, bump the epoch, read and
/// retune the player's extra waits.
#[derive(Clone)]
pub struct ActuatorHandle {
    pub mode: Mode,
    pub epoch: SnapshotEpoch,
    jobs: mpsc::Sender<Job>,
    /// Shared with [`setup`]'s live-edit path: the session thread swaps this
    /// on a `SetTimings` command and reads it when building each job. Jobs
    /// bake the resolved waits at submit time, so the executor never touches
    /// it.
    timings: Arc<Mutex<Timings>>,
}

impl ActuatorHandle {
    pub fn new(
        mode: Mode,
        epoch: SnapshotEpoch,
        jobs: mpsc::Sender<Job>,
        timings: Arc<Mutex<Timings>>,
    ) -> Self {
        Self {
            mode,
            epoch,
            jobs,
            timings,
        }
    }

    /// Queues a job for the executor, naming *why* it was lost when it was —
    /// the caller journals the drop, a lost click must not be silent.
    #[must_use = "a rejected job means a lost click — journal the drop"]
    pub fn submit(&self, job: Job) -> Result<(), SubmitError> {
        match self.jobs.try_send(job) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(SubmitError::QueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(SubmitError::ExecutorGone),
        }
    }

    /// The extra waits to bake into the next job, copied out from under the
    /// lock (never held across a plan build).
    #[must_use]
    pub fn timings(&self) -> Timings {
        *self
            .timings
            .lock()
            .expect("actuator timings mutex poisoned")
    }

    /// Swaps in the player's retuned waits; the next queued job uses them.
    pub fn set_timings(&self, timings: Timings) {
        *self
            .timings
            .lock()
            .expect("actuator timings mutex poisoned") = timings;
    }
}

/// Why a job never reached the executor.
///
/// Both are lost clicks, but they need opposite advice, so they must not
/// collapse back into one flag: a full queue is transient back-pressure the
/// next tick clears on its own, while a closed channel means nobody is at the
/// other end and no amount of waiting will help. Journaling the first when it
/// is really the second sends the player looking for a slow actuator that does
/// not exist — that mistake already cost one full investigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    /// The executor is alive but behind: the bounded queue is at capacity.
    QueueFull,
    /// The receiving end is gone, so nothing will ever run this job. Since a
    /// fatal no longer ends [`run_executor`], this is reachable only once the
    /// session is tearing its workers down.
    ExecutorGone,
}

/// How a surface failure must be handled. Classified at the error's birth
/// site (the backend knows what broke), never blanket-mapped per trait
/// method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceError {
    /// The world moved under the job (window dragged, resized, minimized):
    /// abort the remainder, keep the loop — the next job's `acquire()`
    /// re-reads a fresh rect, so the watchdog's retry self-heals it.
    Recoverable(String),
    /// Acting again would be blind (window gone, shield unraisable): the
    /// executor halts the watch — gate off, cause latched — and drops every
    /// further job before touching the surface, until the player fixes the
    /// cause and re-arms.
    Fatal(String),
}

/// The input backend the executor drives: real input on Windows, a recorder
/// in tests.
///
/// Every method may park its thread for the length of an input (Win32
/// syscalls plus the deliberate settle and hold beats), so the executor only
/// ever calls them through `blocking`.
pub trait Surface {
    /// Locates the game window, returning its client area — whether it is
    /// brought to the foreground is backend-specific.
    fn acquire(&mut self) -> Result<plan::ClientRect, SurfaceError>;
    /// One left click at a screen point, held `press_ms`.
    ///
    /// Preconditioned on a successful [`acquire`](Surface::acquire) since the
    /// last [`release`](Surface::release): the point is meaningless without
    /// the client rect that produced it. Implementations must answer a
    /// violation with [`SurfaceError::Fatal`], never a panic — the executor
    /// runs inside a supervised task, so a panic ends the whole session while
    /// `Fatal` halts the watch with a reason the player can read.
    fn click(&mut self, at: (i32, i32), press_ms: u64) -> Result<(), SurfaceError>;
    /// Wheel notches at a screen point. Same precondition and same
    /// fail-closed rule as [`click`](Surface::click).
    fn scroll(&mut self, at: (i32, i32), notches: i32) -> Result<(), SurfaceError>;
    /// Job over, completed or aborted: undo whatever the inputs set up.
    /// Implementations must make this idempotent and non-panicking because
    /// it runs from a destructor.
    fn release(&mut self) {}
}

/// Runs one blocking [`Surface`] call without starving the runtime.
///
/// Every surface method parks its thread: the Win32 backends sleep through
/// the cursor-settle, button-hold, focus-settle and shield-drain beats, wait
/// on the shield thread's setup handshake, and call `SetForegroundWindow` /
/// `SendInput` / `FindWindowW` synchronously — 120-170 ms for a single click,
/// and a multi-slot buy is a dozen of them back to back. Left plain on a
/// runtime worker that stalls the reassembly task long enough for the capture
/// channels to overflow, and the stream re-anchors in the middle of a
/// purchase — precisely when the `purchase` echo is due. It also outlasts
/// shutdown's grace deadline, because `JoinHandle::abort` cannot interrupt a
/// thread sitting in `std::thread::sleep`.
///
/// `block_in_place` hands the worker's other tasks to a sibling thread for the
/// duration. It panics anywhere but the multi-thread runtime, hence the
/// flavor probe: the executor's own tests drive it on the current-thread
/// runtime with the clock paused (`start_paused` and `multi_thread` are
/// mutually exclusive), and the guard tests call it with no runtime at all.
fn blocking<T>(call: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(call),
        _ => call(),
    }
}

/// Owns cleanup for one successfully acquired job.
struct SurfaceJobGuard<'a, S: Surface> {
    surface: Option<&'a mut S>,
}

impl<'a, S: Surface> SurfaceJobGuard<'a, S> {
    fn new(surface: &'a mut S) -> Self {
        Self {
            surface: Some(surface),
        }
    }

    fn click(&mut self, at: (i32, i32), press_ms: u64) -> Result<(), SurfaceError> {
        let surface = self
            .surface
            .as_deref_mut()
            .expect("active surface job guard");
        blocking(|| surface.click(at, press_ms))
    }

    fn scroll(&mut self, at: (i32, i32), notches: i32) -> Result<(), SurfaceError> {
        let surface = self
            .surface
            .as_deref_mut()
            .expect("active surface job guard");
        blocking(|| surface.scroll(at, notches))
    }

    fn release_once(&mut self) {
        if let Some(surface) = self.surface.take() {
            // Cleanup posts and hides too: blocking all the same, and this
            // also runs from `Drop` on the runtime worker.
            blocking(|| surface.release());
        }
    }
}

impl<S: Surface> Drop for SurfaceJobGuard<'_, S> {
    fn drop(&mut self) {
        self.release_once();
    }
}

/// Replays queued jobs step by step. Before every act it re-checks the gate
/// and the epoch: a stop or a fresh shop mid-job aborts the remaining steps —
/// never click blind. With `dry_run` the resolved screen input is journaled
/// instead of sent.
///
/// A fatal fault ends the job, never the task, and that is not a weakening of
/// "an actuator that cannot act safely stops acting":
///
/// - [`fail`] calls `WatchGate::request_halt`, which disables the gate
///   *synchronously* and latches the cause so nothing can re-arm behind the
///   player's back.
/// - [`drop_reason`] re-reads that gate at the top of every job and again
///   before every step, and answers `"the watch is off"` while it is down.
/// - So from the fatal onwards every job is dropped before `acquire` is even
///   called: not one input can be delivered. The loop stops acting; only the
///   task survives. It also means `fail` cannot spam — nothing reaches a
///   surface call again until the player re-arms.
///
/// The task has to survive because it is spawned exactly once per session and
/// nothing respawns it. Returning would drop the job receiver, and every later
/// submit would then fail against a channel nobody reads — while `Start` from
/// `Status::Stopped` happily re-arms the gate and the whole session goes on
/// *looking* alive. Staying in the loop makes the advice the halt prints
/// ("relaunch Epic Seven without administrator rights") actually work: the
/// player fixes the cause, presses Start, the next job re-runs `acquire`, the
/// preflight re-probes, and the actuator recovers with no process restart.
pub async fn run_executor(
    mut surface: impl Surface,
    mut jobs: mpsc::Receiver<Job>,
    gate: WatchGate,
    epoch: SnapshotEpoch,
    journal: EventLog,
    dry_run: bool,
) {
    while let Some(job) = jobs.recv().await {
        if let Some(reason) = drop_reason(&job, &epoch, &gate) {
            // Dropping is correct; dropping silently is not — the submit
            // side already journaled the promised click.
            journal.emit(&[format!(">> actuator: {reason} — dropped planned clicks")]);
            continue;
        }
        // `acquire` finds the window, may steal the foreground and sleeps out
        // the focus settle: blocking like every other surface call.
        let rect = match blocking(|| surface.acquire()) {
            Ok(rect) => rect,
            Err(SurfaceError::Recoverable(reason)) => {
                // Nothing engaged, nothing landed: drop the job and let the
                // watchdog turn the silence into a retry.
                abort(&journal, &reason);
                continue;
            }
            Err(SurfaceError::Fatal(reason)) => {
                // Nothing was acquired, so there is nothing to release. The
                // gate is down now: every job behind this one is dropped at
                // the top of the loop until the player re-arms.
                fail(&journal, &gate, &reason);
                continue;
            }
        };
        let mut surface = SurfaceJobGuard::new(&mut surface);
        // A minimized window acquires with an empty client area: same fault
        // as minimized mid-job, same recoverable abort.
        if rect.width <= 0 || rect.height <= 0 {
            abort(
                &journal,
                &format!("degenerate client area {}×{}", rect.width, rect.height),
            );
            continue;
        }
        for step in &job.steps {
            tokio::time::sleep(Duration::from_millis(step.wait_ms)).await;
            if let Some(reason) = drop_reason(&job, &epoch, &gate) {
                abort(&journal, reason);
                break;
            }
            let at = match plan::to_screen(rect, step.input.at()) {
                Ok(at) => at,
                Err(reason) => {
                    // Abandon the remaining steps, keep the task: the guard
                    // still releases on the way out of this iteration.
                    fail(&journal, &gate, &reason);
                    break;
                }
            };
            let delivered = match step.input {
                Input::Click { press_ms, .. } => {
                    if dry_run {
                        journal.emit(&[format!(
                            ">> dry-run: click ({}, {}) after {} ms, hold {press_ms} ms",
                            at.0, at.1, step.wait_ms
                        )]);
                        Ok(())
                    } else {
                        surface.click(at, press_ms)
                    }
                }
                Input::Scroll { notches, .. } => {
                    if dry_run {
                        journal.emit(&[format!(
                            ">> dry-run: scroll {notches} at ({}, {}) after {} ms",
                            at.0, at.1, step.wait_ms
                        )]);
                        Ok(())
                    } else {
                        surface.scroll(at, notches)
                    }
                }
            };
            if let Err(error) = delivered {
                match error {
                    SurfaceError::Recoverable(reason) => {
                        // Landed inputs stay landed; the watchdog's retry
                        // re-acquires a fresh rect, so a dragged window
                        // self-heals instead of halting the loop.
                        abort(&journal, &reason);
                    }
                    SurfaceError::Fatal(reason) => {
                        // Same as above: landed inputs stay landed, the rest
                        // of the job dies with the gate, the task lives on.
                        fail(&journal, &gate, &reason);
                    }
                }
                break;
            }
        }
    }
}

/// Why a job (or its remainder) must not act: a newer shop invalidated the
/// planned coordinates, or the watch is off. `None` means clear to act.
#[must_use]
fn drop_reason(job: &Job, epoch: &SnapshotEpoch, gate: &WatchGate) -> Option<&'static str> {
    if job.epoch != epoch.current() {
        Some("the shop changed")
    } else if !gate.is_enabled() {
        Some("the watch is off")
    } else {
        None
    }
}

/// A recoverable fault ends the job, never the loop: journaled, then the
/// watchdog turns the silence into a retry.
fn abort(journal: &EventLog, reason: &str) {
    journal.emit(&[format!(">> actuator: {reason} — aborted remaining clicks")]);
}

/// An actuator that cannot act safely stops acting — with its own label,
/// never the player's.
///
/// `request_halt` disables the gate synchronously and latches the cause, so
/// every job after this one is refused by [`drop_reason`] before `acquire` is
/// reached: the loop is inert from here even though its task keeps running
/// (see [`run_executor`] for why the task must not die). That also makes this
/// self-limiting — no surface call happens again until the player
/// acknowledges the halt and re-arms, so a standing fault cannot flood the
/// journal with repeats.
fn fail(journal: &EventLog, gate: &WatchGate, reason: &str) {
    journal.emit(&[format!(">> actuator: {reason} — stopping the loop")]);
    gate.request_halt(HaltSource::ActuatorFailed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use plan::{ClientRect, Trigger};
    use std::sync::Mutex;

    #[tokio::test]
    async fn fail_disables_the_gate_and_latches_the_actuator_cause() {
        let journal = EventLog::default();
        let gate = WatchGate::new(true);
        fail(&journal, &gate, "window gone");
        assert!(!gate.is_enabled());
        assert_eq!(gate.halt_requested().await, HaltSource::ActuatorFailed);
        assert!(
            journal
                .entries()
                .iter()
                .any(|l| l.text.contains("window gone"))
        );
    }

    /// The whole point of the flavor probe: `block_in_place` panics off the
    /// multi-thread runtime, and both other contexts are real — the executor
    /// tests below run current-thread (paused clock), the guard tests run
    /// with no runtime at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_offloads_on_the_multi_thread_runtime() {
        assert_eq!(blocking(|| 7), 7);
    }

    #[tokio::test]
    async fn blocking_runs_inline_on_the_current_thread_runtime() {
        assert_eq!(blocking(|| 7), 7);
    }

    #[test]
    fn blocking_runs_inline_without_a_runtime() {
        assert_eq!(blocking(|| 7), 7);
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Recorded {
        Click((i32, i32), u64),
        Scroll((i32, i32), i32),
    }

    /// Records every input; `on_input` runs after each; `deny_after` fails
    /// the next input once `n` have landed (one-shot, so a later job can
    /// succeed).
    struct FakeSurface {
        rect: Result<ClientRect, SurfaceError>,
        events: Arc<Mutex<Vec<Recorded>>>,
        on_input: Box<dyn FnMut() + Send>,
        deny_after: Option<(usize, SurfaceError)>,
        releases: Arc<Mutex<usize>>,
    }

    impl FakeSurface {
        fn new(rect: Result<ClientRect, SurfaceError>) -> (Self, Arc<Mutex<Vec<Recorded>>>) {
            let events = Arc::new(Mutex::new(Vec::new()));
            let surface = Self {
                rect,
                events: events.clone(),
                on_input: Box::new(|| {}),
                deny_after: None,
                releases: Arc::new(Mutex::new(0)),
            };
            (surface, events)
        }

        fn deny(&mut self) -> Result<(), SurfaceError> {
            let due = self
                .deny_after
                .as_ref()
                .is_some_and(|(n, _)| self.events.lock().unwrap().len() >= *n);
            if due {
                return Err(self.deny_after.take().expect("just checked").1);
            }
            Ok(())
        }
    }

    impl Surface for FakeSurface {
        fn acquire(&mut self) -> Result<ClientRect, SurfaceError> {
            self.rect.clone()
        }

        fn click(&mut self, at: (i32, i32), press_ms: u64) -> Result<(), SurfaceError> {
            self.deny()?;
            self.events
                .lock()
                .unwrap()
                .push(Recorded::Click(at, press_ms));
            (self.on_input)();
            Ok(())
        }

        fn scroll(&mut self, at: (i32, i32), notches: i32) -> Result<(), SurfaceError> {
            self.deny()?;
            self.events
                .lock()
                .unwrap()
                .push(Recorded::Scroll(at, notches));
            (self.on_input)();
            Ok(())
        }

        fn release(&mut self) {
            *self.releases.lock().unwrap() += 1;
        }
    }

    #[test]
    fn surface_job_guard_releases_once_on_scope_exit() {
        let (mut surface, _) = FakeSurface::new(design_rect());
        let releases = surface.releases.clone();

        {
            let _guard = SurfaceJobGuard::new(&mut surface);
        }

        assert_eq!(*releases.lock().unwrap(), 1);
    }

    #[test]
    fn surface_job_guard_explicit_release_is_idempotent_with_drop() {
        let (mut surface, _) = FakeSurface::new(design_rect());
        let releases = surface.releases.clone();

        {
            let mut guard = SurfaceJobGuard::new(&mut surface);
            guard.release_once();
            guard.release_once();
        }

        assert_eq!(*releases.lock().unwrap(), 1);
    }

    #[test]
    fn surface_job_guard_releases_once_during_unwind() {
        let (mut surface, _) = FakeSurface::new(design_rect());
        let releases = surface.releases.clone();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = SurfaceJobGuard::new(&mut surface);
            panic!("test unwind");
        }));

        assert!(result.is_err());
        assert_eq!(*releases.lock().unwrap(), 1);
    }

    fn design_rect() -> Result<ClientRect, SurfaceError> {
        Ok(ClientRect {
            left: 0,
            top: 0,
            width: 1280,
            height: 720,
        })
    }

    struct Rig {
        job_tx: mpsc::Sender<Job>,
        job_rx: mpsc::Receiver<Job>,
        gate: WatchGate,
        epoch: SnapshotEpoch,
        journal: EventLog,
    }

    fn rig() -> Rig {
        let (job_tx, job_rx) = mpsc::channel(8);
        Rig {
            job_tx,
            job_rx,
            gate: WatchGate::new(true),
            epoch: SnapshotEpoch::default(),
            journal: EventLog::default(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn executor_skips_stale_epoch_jobs() {
        let rig = rig();
        let (surface, events) = FakeSurface::new(design_rect());
        let job = plan::refresh_job(
            Trigger::Refreshed,
            Timings::default(),
            rig.epoch.current(),
            1,
        );
        rig.epoch.bump(); // a newer shop arrived before the job started
        rig.job_tx.send(job).await.unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false).await;
        assert!(events.lock().unwrap().is_empty());
        // Dropped, but never silently: the submit side promised a click.
        assert!(journal.entries().iter().any(|line| {
            line.text
                .contains("the shop changed — dropped planned clicks")
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn executor_skips_jobs_while_gate_off() {
        let rig = rig();
        rig.gate.set(false);
        let (surface, events) = FakeSurface::new(design_rect());
        rig.job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                0,
                1,
            ))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false).await;
        assert!(events.lock().unwrap().is_empty());
        assert!(journal.entries().iter().any(|line| {
            line.text
                .contains("the watch is off — dropped planned clicks")
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn executor_aborts_mid_job_when_gate_turns_off() {
        let rig = rig();
        let (mut surface, events) = FakeSurface::new(design_rect());
        let releases = surface.releases.clone();
        let gate = rig.gate.clone();
        surface.on_input = Box::new(move || gate.set(false));
        rig.job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                0,
                1,
            ))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false).await;
        // Two steps planned, only the first landed — and the abort is
        // journaled.
        assert_eq!(events.lock().unwrap().len(), 1);
        assert_eq!(*releases.lock().unwrap(), 1);
        assert!(
            journal
                .entries()
                .iter()
                .any(|line| line.text.contains("aborted remaining clicks"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn executor_aborts_mid_job_on_epoch_bump() {
        let rig = rig();
        let (mut surface, events) = FakeSurface::new(design_rect());
        let releases = surface.releases.clone();
        let epoch = rig.epoch.clone();
        surface.on_input = Box::new(move || epoch.bump());
        rig.job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                0,
                1,
            ))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false).await;
        assert_eq!(events.lock().unwrap().len(), 1);
        assert_eq!(*releases.lock().unwrap(), 1);
        assert!(journal.entries().iter().any(|line| {
            line.text
                .contains("the shop changed — aborted remaining clicks")
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn executor_stops_the_loop_when_acquire_fails() {
        let rig = rig();
        let (surface, events) =
            FakeSurface::new(Err(SurfaceError::Fatal("game window not found".to_owned())));
        let releases = surface.releases.clone();
        rig.job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                0,
                1,
            ))
            .await
            .unwrap();
        // The fatal no longer ends the task, so the producer has to close the
        // channel for the loop to finish; the timeout still fails the test if
        // the fatal ever stops consuming.
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        let gate = rig.gate.clone();
        tokio::time::timeout(
            Duration::from_secs(10),
            run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false),
        )
        .await
        .expect("the executor must keep draining after a fatal acquire, then end on EOF");
        assert!(events.lock().unwrap().is_empty());
        assert!(!gate.is_enabled());
        assert_eq!(gate.halt_requested().await, HaltSource::ActuatorFailed);
        assert_eq!(*releases.lock().unwrap(), 0, "acquire engaged nothing");
        assert!(journal.entries().iter().any(|line| {
            line.text
                .contains("actuator: game window not found — stopping the loop")
        }));
    }

    /// The preflight's verdict has to *reach the player*, not just exist.
    ///
    /// `win::preflight_refusal` is what `Surface::acquire` answers when the game
    /// window is out of this process's reach; this is the rest of the trip —
    /// through `fail`, into the journal the window renders, at acquire time and
    /// before a single click is planned. Built from the real classifier rather
    /// than from a copy of its text, so a reworded diagnosis cannot pass here
    /// while shipping something else.
    #[cfg(all(windows, feature = "actuator"))]
    #[tokio::test(start_paused = true)]
    async fn a_refused_preflight_reaches_the_journal_naming_the_integrity_level() {
        use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;

        let refusal = win::preflight_refusal(&std::io::Error::from_raw_os_error(
            ERROR_ACCESS_DENIED as i32,
        ));
        let rig = rig();
        let (surface, events) = FakeSurface::new(Err(refusal));
        rig.job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                0,
                1,
            ))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        let gate = rig.gate.clone();
        tokio::time::timeout(
            Duration::from_secs(10),
            run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false),
        )
        .await
        .expect("a refused preflight must halt the watch, not wedge the executor");

        assert!(events.lock().unwrap().is_empty(), "nothing may be clicked");
        assert!(!gate.is_enabled());
        let line = journal
            .entries()
            .into_iter()
            .find(|line| line.text.contains("stopping the loop"))
            .expect("the halt must be journaled");
        assert!(
            line.text.contains("higher integrity level"),
            "{}",
            line.text
        );
        assert!(
            line.text
                .contains("relaunch Epic Seven without administrator rights"),
            "{}",
            line.text
        );
    }

    #[tokio::test(start_paused = true)]
    async fn executor_stops_the_loop_when_an_input_fails() {
        // A fatal input failure (e.g. the shield refused to raise) halts
        // with the actuator's own label — never a blind click or a silent
        // skip.
        let rig = rig();
        let (mut surface, events) = FakeSurface::new(design_rect());
        surface.deny_after = Some((
            0,
            SurfaceError::Fatal("could not raise the input shield".to_owned()),
        ));
        let releases = surface.releases.clone();
        rig.job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                0,
                1,
            ))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        let gate = rig.gate.clone();
        tokio::time::timeout(
            Duration::from_secs(10),
            run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false),
        )
        .await
        .expect("a fatal input must end the job, then the loop ends on EOF");
        assert!(events.lock().unwrap().is_empty());
        assert!(!gate.is_enabled());
        assert_eq!(gate.halt_requested().await, HaltSource::ActuatorFailed);
        assert_eq!(*releases.lock().unwrap(), 1);
        assert!(journal.entries().iter().any(|line| {
            line.text
                .contains("could not raise the input shield — stopping the loop")
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn executor_keeps_landed_inputs_and_stops_once_on_a_mid_job_failure() {
        // Three inputs land, the fourth fails fatally: landed inputs stay
        // recorded, exactly one halt goes out, the surface is still
        // released.
        //
        // The job queued behind it is the point of the second submit: the
        // executor now lives long enough to pick it up, and must drop it on
        // the downed gate rather than act on it.
        let rig = rig();
        let (mut surface, events) = FakeSurface::new(design_rect());
        surface.deny_after = Some((
            3,
            SurfaceError::Fatal("could not raise the input shield".to_owned()),
        ));
        let releases = surface.releases.clone();
        rig.job_tx
            .send(plan::buy_job(
                Trigger::ShopOpened,
                Timings::default(),
                0,
                &[0, 4],
                42,
            ))
            .await
            .unwrap();
        rig.job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                0,
                1,
            ))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        let gate = rig.gate.clone();
        tokio::time::timeout(
            Duration::from_secs(10),
            run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false),
        )
        .await
        .expect("a fatal mid-job input must drop the queued work, then end on EOF");
        assert_eq!(events.lock().unwrap().len(), 3);
        assert!(!gate.is_enabled());
        assert_eq!(gate.halt_requested().await, HaltSource::ActuatorFailed);
        // Only the first job ever acquired: the queued one was refused before
        // the surface was touched.
        assert_eq!(*releases.lock().unwrap(), 1);
        let lines = journal.entries();
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.text.contains("stopping the loop"))
                .count(),
            1,
            "exactly one halt — the downed gate keeps `fail` from re-firing"
        );
        assert!(lines.iter().any(|line| {
            line.text
                .contains("the watch is off — dropped planned clicks")
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn a_re_armed_watch_acts_again_after_a_fatal_without_a_process_restart() {
        // The whole reason the fatal paths stopped returning.
        //
        // The player is told to relaunch Epic Seven without administrator
        // rights and press Start again. That advice is only true if the
        // executor task outlived the halt: it is spawned once per session, so
        // a `return` here would leave the re-armed session submitting into a
        // channel nobody reads.
        let rig = rig();
        let (mut surface, events) = FakeSurface::new(design_rect());
        // One-shot, like the shield refusing to raise against an elevated
        // window: it fails the first input and never again, standing in for
        // the player fixing the cause between the two jobs.
        surface.deny_after = Some((
            0,
            SurfaceError::Fatal("could not raise the input shield".to_owned()),
        ));
        let releases = surface.releases.clone();
        let gate = rig.gate.clone();
        let journal = rig.journal.clone();
        // Moved, not cloned: this is the only producer, so dropping it below
        // is what ends the loop.
        let job_tx = rig.job_tx;
        let executor = tokio::spawn(run_executor(
            surface,
            rig.job_rx,
            rig.gate,
            rig.epoch,
            rig.journal,
            false,
        ));

        job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                0,
                1,
            ))
            .await
            .unwrap();
        assert_eq!(gate.halt_requested().await, HaltSource::ActuatorFailed);
        assert!(!gate.is_enabled());
        assert!(
            events.lock().unwrap().is_empty(),
            "the fatal must deliver nothing"
        );

        // What the session does once it has dispatched the halt and the player
        // presses Start again.
        gate.acknowledge_halt(HaltSource::ActuatorFailed);
        gate.set(true);
        assert!(
            gate.is_enabled(),
            "the acknowledged cause must let it re-arm"
        );

        job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                0,
                2,
            ))
            .await
            .unwrap();
        drop(job_tx);
        tokio::time::timeout(Duration::from_secs(10), executor)
            .await
            .expect("the executor must still be running to serve the second job")
            .expect("the executor task must not panic");

        // Both of the second job's steps landed, on a rect it re-acquired
        // itself, and nothing halted again.
        assert_eq!(events.lock().unwrap().len(), 2);
        assert_eq!(
            *releases.lock().unwrap(),
            2,
            "each job acquired and released"
        );
        assert!(gate.is_enabled(), "the recovered job must not re-halt");
        assert_eq!(
            journal
                .entries()
                .iter()
                .filter(|line| line.text.contains("stopping the loop"))
                .count(),
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn executor_aborts_without_halt_on_a_recoverable_input_failure() {
        // The window moved mid-job: landed inputs stay, the remainder is
        // aborted without stopping the loop — the watchdog's retry
        // re-acquires a fresh rect.
        let rig = rig();
        let (mut surface, events) = FakeSurface::new(design_rect());
        surface.deny_after = Some((
            3,
            SurfaceError::Recoverable("the game window moved or resized mid-job".to_owned()),
        ));
        let releases = surface.releases.clone();
        let gate = rig.gate.clone();
        rig.job_tx
            .send(plan::buy_job(
                Trigger::ShopOpened,
                Timings::default(),
                0,
                &[0, 4],
                42,
            ))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false).await;
        assert_eq!(events.lock().unwrap().len(), 3);
        assert!(gate.is_enabled(), "no halt for a recoverable abort");
        assert_eq!(*releases.lock().unwrap(), 1);
        assert!(journal.entries().iter().any(|line| {
            line.text
                .contains("the game window moved or resized mid-job — aborted remaining clicks")
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn executor_serves_the_next_job_after_a_recoverable_abort() {
        // A recoverable abort ends one job, not the loop: the next job runs
        // against a freshly acquired rect.
        let rig = rig();
        let (mut surface, events) = FakeSurface::new(design_rect());
        surface.deny_after = Some((
            1,
            SurfaceError::Recoverable("the game window moved or resized mid-job".to_owned()),
        ));
        let releases = surface.releases.clone();
        rig.job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                0,
                1,
            ))
            .await
            .unwrap();
        rig.job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                0,
                2,
            ))
            .await
            .unwrap();
        drop(rig.job_tx);
        let gate = rig.gate.clone();
        run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false).await;
        // First job: one landed click, then the abort. Second job: both.
        assert_eq!(events.lock().unwrap().len(), 3);
        assert!(gate.is_enabled());
        assert_eq!(*releases.lock().unwrap(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn executor_aborts_without_halt_on_a_minimized_acquire() {
        // A minimized window acquires with an empty client area: same fault
        // as minimized mid-job, same recoverable abort — the loop halts only
        // if the watchdog's retries stay broken.
        let rig = rig();
        let (surface, events) = FakeSurface::new(Ok(ClientRect {
            left: 0,
            top: 0,
            width: 0,
            height: 0,
        }));
        let releases = surface.releases.clone();
        rig.job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                0,
                1,
            ))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        let gate = rig.gate.clone();
        run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false).await;
        assert!(events.lock().unwrap().is_empty());
        assert!(gate.is_enabled());
        assert_eq!(*releases.lock().unwrap(), 1);
        assert!(journal.entries().iter().any(|line| {
            line.text
                .contains("degenerate client area 0×0 — aborted remaining clicks")
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn executor_releases_the_surface_after_every_acquired_job() {
        // Every acquired job releases; a job dropped before acquire never
        // engaged anything.
        let rig = rig();
        let (mut surface, _events) = FakeSurface::new(design_rect());
        let releases = surface.releases.clone();
        let gate = rig.gate.clone();
        surface.on_input = Box::new(move || gate.set(false));
        rig.job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                0,
                1,
            ))
            .await
            .unwrap();
        rig.job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                0,
                2,
            ))
            .await
            .unwrap();
        drop(rig.job_tx);
        run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false).await;
        assert_eq!(*releases.lock().unwrap(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn executor_stops_the_loop_on_a_narrow_window() {
        let rig = rig();
        let (surface, events) = FakeSurface::new(Ok(ClientRect {
            left: 0,
            top: 0,
            width: 1280,
            height: 800,
        }));
        let releases = surface.releases.clone();
        rig.job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                0,
                1,
            ))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        let gate = rig.gate.clone();
        tokio::time::timeout(
            Duration::from_secs(10),
            run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false),
        )
        .await
        .expect("a fatal coordinate conversion must end the job, then the loop ends on EOF");
        assert!(events.lock().unwrap().is_empty());
        assert!(!gate.is_enabled());
        assert_eq!(gate.halt_requested().await, HaltSource::ActuatorFailed);
        assert_eq!(*releases.lock().unwrap(), 1);
        assert!(
            journal
                .entries()
                .iter()
                .any(|line| line.text.contains("narrower than 16:9"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn executor_dry_run_journals_without_input() {
        let rig = rig();
        let (surface, events) = FakeSurface::new(design_rect());
        let releases = surface.releases.clone();
        rig.job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                0,
                1,
            ))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, true).await;
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(*releases.lock().unwrap(), 1);
        let lines = journal.entries();
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.text.contains("dry-run: click"))
                .count(),
            2
        );
    }

    #[tokio::test(start_paused = true)]
    async fn executor_replays_steps_in_order_at_screen_coords() {
        let rig = rig();
        let rect = ClientRect {
            left: 10,
            top: 20,
            width: 1920,
            height: 1080,
        };
        let (surface, events) = FakeSurface::new(Ok(rect));
        let releases = surface.releases.clone();
        let job = plan::buy_job(Trigger::ShopOpened, Timings::default(), 0, &[1], 42);
        let expected: Vec<Recorded> = job
            .steps
            .iter()
            .map(|step| {
                let at = plan::to_screen(rect, step.input.at()).unwrap();
                match step.input {
                    Input::Click { press_ms, .. } => Recorded::Click(at, press_ms),
                    Input::Scroll { notches, .. } => Recorded::Scroll(at, notches),
                }
            })
            .collect();
        rig.job_tx.send(job).await.unwrap();
        drop(rig.job_tx);
        run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false).await;
        assert_eq!(*events.lock().unwrap(), expected);
        assert_eq!(*releases.lock().unwrap(), 1);
    }
}
