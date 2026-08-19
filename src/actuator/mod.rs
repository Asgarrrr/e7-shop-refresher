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

use plan::{Epoch, Input, Job, ScreenError, Timings};

/// The actuator's poison-tolerant lock: a panic on some other thread must not
/// turn every later click into a fatal. The obligation [`crate::sync`] puts on
/// its callers is discharged here by the shape of what is guarded — `Timings` is
/// `Copy` and copied straight out, and the shield's slot is a plain handle, so
/// neither can be caught half-written.
use crate::sync::lock_ignoring_poison as lock;

/// Generation counter of the shop state, bumped on every shop message. A job
/// carries the epoch it was planned against and the executor refuses to act
/// on any other: clicks aimed at a shop that no longer exists must die, not
/// land.
///
/// The ordering is `Relaxed` on purpose, and it is not a weakening: the epoch is
/// only ever compared for equality, and nothing is published *through* it. The
/// value reaches the executor baked into a `Job` travelling over an `mpsc`
/// channel, whose `send`/`recv` is what creates the happens-before edge; the
/// snapshot itself arrives under the controller's own mutex. A stronger load
/// would not be a fresher one.
#[derive(Clone, Default)]
pub struct SnapshotEpoch(Arc<AtomicU64>);

impl SnapshotEpoch {
    pub fn bump(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    /// The generation to plan against, as an [`Epoch`] rather than a bare `u64`:
    /// every job builder takes it beside a millisecond seed, and the two are not
    /// interchangeable.
    #[must_use]
    pub fn current(&self) -> Epoch {
        Epoch(self.0.load(Ordering::Relaxed))
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
    /// Shared with [`crate::app::setup`]'s live-edit path: the session thread swaps this
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
    ///
    /// # Errors
    ///
    /// [`SubmitError::QueueFull`] when the executor is alive but behind, which
    /// the next tick clears on its own, and [`SubmitError::ExecutorGone`] when
    /// nobody is reading any more. The two need opposite advice, which is why
    /// they are not one flag — see [`SubmitError`].
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
    ///
    /// Poison-tolerant (see [`lock`]): this is called while building *every*
    /// queued job, and panicking here would take the session down over an
    /// unrelated fault.
    #[must_use]
    pub fn timings(&self) -> Timings {
        *lock(&self.timings)
    }

    /// Swaps in the player's retuned waits; the next queued job uses them.
    /// Poison-tolerant for the same reason as [`timings`](Self::timings).
    pub fn set_timings(&self, timings: Timings) {
        *lock(&self.timings) = timings;
    }
}

/// Why a job never reached the executor.
///
/// Both are lost clicks, but they need opposite advice, so they must not
/// collapse back into one flag: a full queue is transient back-pressure the
/// next tick clears on its own, while a closed channel means nobody is at the
/// other end and no amount of waiting will help. Journaling the first when it
/// is really the second sends the player looking for a slow actuator that does
/// not exist.
///
/// The `Display` texts are the neutral one-liners a log or a crash chain wants.
/// The journal deliberately says something longer and different at each of the
/// two call sites, because there the *advice* is the point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    /// The executor is alive but behind: the bounded queue is at capacity.
    #[error("the actuator queue is full — the executor is behind")]
    QueueFull,
    /// The receiving end is gone, so nothing will ever run this job. Since a
    /// fatal no longer ends [`run_executor`], this is reachable only once the
    /// session is tearing its workers down.
    #[error("the actuator executor is gone — nothing will run this job")]
    ExecutorGone,
}

/// How a surface failure must be handled. Classified at the error's birth
/// site (the backend knows what broke), never blanket-mapped per trait
/// method.
///
/// Both payloads are already operator-facing text assembled so a human can read
/// it, so `Display` is `{0}` verbatim: that is what lets an actuator failure
/// appear in a `error = %err` field or a crash chain at all, instead of only as
/// whatever prose one match arm happened to build.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SurfaceError {
    /// The world moved under the job (window dragged, resized, minimized):
    /// abort the remainder, keep the loop — the next job's `acquire()`
    /// re-reads a fresh rect, so the watchdog's retry self-heals it.
    #[error("{0}")]
    Recoverable(String),
    /// Acting again would be blind (window gone, shield unraisable): the
    /// executor halts the watch — gate off, cause latched — and drops every
    /// further job before touching the surface, until the player fixes the
    /// cause and re-arms.
    #[error("{0}")]
    Fatal(String),
}

/// The input backend the executor drives: real input on Windows, a recorder
/// in tests.
///
/// Every method may park its thread for the length of an input (Win32
/// syscalls plus the deliberate settle and hold beats), so the executor only
/// ever calls them through `blocking`.
///
/// # Why the window is a parameter rather than a field
///
/// This invariant (`api-004`) used to be enforced at run time in three separate
/// places: an `Option<Target>` field and a `target()` guard inside each Windows
/// backend, plus an `.expect("active surface job guard")` in the executor's own
/// guard — three chances to get the next backend wrong, and the backends' own
/// copies duplicated state the executor already owned (it takes the
/// [`plan::ClientRect`] out of `acquire` and carries it through the whole job
/// anyway).
///
/// Instead `acquire` hands back a [`Window`](Surface::Window) — opaque,
/// backend-owned, whatever the backend needs to act (the Win32 backends put the
/// `HWND` and the measured client rect in it) — and every input method takes
/// one. There is no state to forget to set, no guard to forget to write, and
/// "input without an acquire" is not a value that can be built. What stays
/// fail-closed is everything the *world* can break: the window died, moved, or
/// refuses input. That is what [`SurfaceError`] is for, and the backends still
/// re-verify on every single event.
pub trait Surface {
    /// A backend's proof that it acquired the game window, and everything it
    /// needs to act on it.
    ///
    /// Opaque on purpose: the executor only routes it from `acquire` to the
    /// input calls and finally to `release`, and never looks inside. A backend
    /// with nothing to carry uses `()`.
    type Window;

    /// Locates the game window, returning its client area — whether it is
    /// brought to the foreground is backend-specific.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::Recoverable`] when the window is alive but not usable
    /// right now, so the *next* `acquire` may well succeed: [`run_executor`]
    /// drops this one job and the watchdog's retry heals it.
    /// [`SurfaceError::Fatal`] when acting would be blind — no window carrying
    /// the game's title, a client rect Windows refuses to read at all, a process
    /// DPI awareness that would place every click at the wrong scale, or a window
    /// at a higher integrity level than this process. There the executor halts
    /// the watch and the payload is the line the player reads.
    ///
    /// Either way nothing was engaged, so a failed `acquire` leaves no
    /// [`release`](Surface::release) owing — which is why the executor only
    /// builds its cleanup guard on the `Ok` path.
    fn acquire(&mut self) -> Result<(Self::Window, plan::ClientRect), SurfaceError>;

    /// The same window and the same client area, for a job that will not send
    /// anything: [`Mode::DryRun`].
    ///
    /// Required rather than defaulted to `acquire`, because a default is exactly
    /// how this went wrong. The simulation path called `acquire`, and for the
    /// `input` backend that means a real `SetForegroundWindow` — so turning on
    /// the mode a cautious player turns on *first* yanked Epic Seven in front of
    /// whatever they were doing, on every tick, while sending no input. A dry
    /// run that reorders the desktop is not a dry run. Making this a decision
    /// each backend has to write down is what stops the next one inheriting the
    /// same surprise.
    ///
    /// Measuring is still allowed, and wanted. The rect is what resolves the
    /// journal's screen coordinates, so without it the dry run stops answering
    /// the question it exists for; a reachability preflight that provably
    /// changes nothing (see `win::probe_window_reachable`) is likewise worth
    /// keeping, since "the real run would be refused by UIPI" is the single most
    /// useful thing a rehearsal can report. The line is *engaging*, not
    /// *looking*.
    ///
    /// # Errors
    ///
    /// The same classification as [`acquire`](Surface::acquire), and the same
    /// consequences: a dry run reports the faults a live run would hit, which is
    /// the point of rehearsing.
    fn measure(&mut self) -> Result<(Self::Window, plan::ClientRect), SurfaceError>;

    /// One left click at a screen point, held `press_ms`.
    ///
    /// `window` is what [`acquire`](Surface::acquire) handed back, so the
    /// precondition the point depends on — that a client rect was measured and
    /// this is the window it was measured on — is carried by the argument rather
    /// than asserted. Implementations must still answer everything the world can
    /// break with a [`SurfaceError`], never a panic: the executor runs inside a
    /// supervised task, so a panic ends the whole session while `Fatal` halts the
    /// watch with a reason the player can read.
    fn click(
        &mut self,
        window: &Self::Window,
        at: (i32, i32),
        press_ms: u64,
    ) -> Result<(), SurfaceError>;
    /// Wheel notches at a screen point. Same contract and same fail-closed rule
    /// as [`click`](Surface::click).
    fn scroll(
        &mut self,
        window: &Self::Window,
        at: (i32, i32),
        notches: i32,
    ) -> Result<(), SurfaceError>;
    /// Job over, completed or aborted: undo whatever the inputs set up for
    /// `window`. Implementations must make this idempotent and non-panicking
    /// because it runs from a destructor.
    ///
    /// The window is borrowed rather than consumed so the executor's guard can
    /// own it outright instead of behind an `Option` — an `Option` there is what
    /// made the guard's own `expect` necessary, and this trait is meant to delete
    /// that kind of check, not relocate it.
    fn release(&mut self, window: &Self::Window) {
        let _ = window;
    }
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

/// Owns cleanup for one successfully acquired job, and the window that job
/// acquired.
///
/// Both fields are plain values rather than `Option`s: the guard is built only on
/// `acquire`'s `Ok` path, so an inactive guard is not a state it has (see
/// [`Surface`]'s "why the window is a parameter" for the `api-004` invariant
/// this removes the last runtime check of). Idempotence of the release is a
/// `bool`, which is the only thing the `Option` was actually tracking.
struct SurfaceJobGuard<'a, S: Surface> {
    surface: &'a mut S,
    window: S::Window,
    released: bool,
}

impl<'a, S: Surface> SurfaceJobGuard<'a, S> {
    fn new(surface: &'a mut S, window: S::Window) -> Self {
        Self {
            surface,
            window,
            released: false,
        }
    }

    fn click(&mut self, at: (i32, i32), press_ms: u64) -> Result<(), SurfaceError> {
        let Self {
            surface, window, ..
        } = self;
        blocking(|| surface.click(window, at, press_ms))
    }

    fn scroll(&mut self, at: (i32, i32), notches: i32) -> Result<(), SurfaceError> {
        let Self {
            surface, window, ..
        } = self;
        blocking(|| surface.scroll(window, at, notches))
    }

    fn release_once(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let Self {
            surface, window, ..
        } = self;
        // Cleanup posts and hides too: blocking all the same, and this
        // also runs from `Drop` on the runtime worker.
        blocking(|| surface.release(window));
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
/// submit would then fail against a channel nobody reads, even though `Start`
/// from `Status::Stopped` re-arms the gate and the session looks alive. Staying
/// in the loop is what lets the player act on the halt: fix the cause, press
/// Start, the next job re-runs `acquire`, and the actuator recovers with no
/// process restart. (The UIPI halt is the one exception whose fix *is*
/// restarting this app, since the integrity level is fixed at process start.)
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
        // the focus settle: blocking like every other surface call. A dry run
        // takes the door that only looks — the whole mode is a promise not to
        // touch the game, and `acquire` was breaking it before any click was
        // even planned.
        let (window, rect) = match blocking(|| {
            if dry_run {
                surface.measure()
            } else {
                surface.acquire()
            }
        }) {
            Ok(acquired) => acquired,
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
        let mut surface = SurfaceJobGuard::new(&mut surface, window);
        // Built here, between the guard and the first sleep, because both of its
        // refusals are properties of the rect `acquire` just measured — nothing
        // in the loop re-reads it, so asking per step asked an unchanging
        // question after having already waited out a step delay. A minimized
        // window used to burn 1.18 s, or up to 61 s with a configured range,
        // before abandoning a job that could never land a click. After the
        // guard, not before it: a successful `acquire` owes a `release` whatever
        // the rect turns out to be.
        let viewport = match plan::Viewport::of(rect) {
            Ok(viewport) => viewport,
            // A minimized window acquires with an empty client area, and the
            // next `acquire` reads a fresh one: recoverable, so drop this job
            // and let the watchdog's retry heal it. The classification is the
            // converter's `ScreenError`, not a `rect.width <= 0` test spelled
            // out again here.
            Err(error @ ScreenError::DegenerateRect { .. }) => {
                abort(&journal, &error.to_string());
                continue;
            }
            // Nothing the loop can heal: the player has to widen the window.
            // Abandon the job, keep the task — the guard releases as this
            // iteration unwinds.
            Err(error @ ScreenError::TooNarrow { .. }) => {
                fail(&journal, &gate, &error.to_string());
                continue;
            }
        };
        for step in &job.steps {
            tokio::time::sleep(Duration::from_millis(step.wait_ms)).await;
            if let Some(reason) = drop_reason(&job, &epoch, &gate) {
                abort(&journal, reason);
                break;
            }
            // Total: the two ways this transform has no answer were both settled
            // above, before a single millisecond was spent.
            let at = viewport.place(step.input.at());
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
    // `warn`, not `info`, for the same reason as a link-down: the job did not
    // do what the journal already promised it would, and the watchdog's retry
    // only makes sense to a reader who can still see this line.
    journal.emit_at(
        tracing::Level::WARN,
        &[format!(">> actuator: {reason} — aborted remaining clicks")],
    );
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
    // `error`, matching a session abort: this is one of the two lines that say
    // the product stopped doing its job. It is also self-limiting (see above),
    // so it cannot flood the file at this level.
    journal.emit_at(
        tracing::Level::ERROR,
        &[format!(">> actuator: {reason} — stopping the loop")],
    );
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
        assert_eq!(gate.next_halt().await, HaltSource::ActuatorFailed);
        assert!(
            journal
                .to_entries()
                .iter()
                .any(|l| l.text.contains("window gone"))
        );
    }

    /// `apply()` reaches `actuator.timings()` while holding the controller guard,
    /// so without [`lock`]'s poison tolerance a panic under the timings lock
    /// would poison that one too and every later dispatch would panic.
    #[test]
    fn a_poisoned_timings_mutex_still_serves_reads_and_writes() {
        let timings = Arc::new(Mutex::new(Timings::default()));
        let poisoner = Arc::clone(&timings);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poisoner.lock().expect("first lock is clean");
            panic!("some other thread's fault");
        }));
        assert!(unwound.is_err());
        assert!(timings.is_poisoned(), "the test needs a poisoned lock");

        let (job_tx, _job_rx) = mpsc::channel(1);
        let handle = ActuatorHandle::new(
            Mode::Live,
            SnapshotEpoch::default(),
            job_tx,
            Arc::clone(&timings),
        );
        // Neither of these may panic, and the write must still be observable.
        assert_eq!(handle.timings(), Timings::default());
        let retuned = plan::TimingPreset::Cautious.timings();
        handle.set_timings(retuned);
        assert_eq!(handle.timings(), retuned);
    }

    /// Exercises the `block_in_place` arm of [`blocking`]; the other two
    /// contexts it must also tolerate are covered below.
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

    /// A distinguishable stand-in for a real backend's window handle: one is
    /// minted per `acquire`, so a test can prove that every input and the release
    /// were handed *this job's* window and not a stale one.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct FakeWindow(u32);

    /// Records every input; `on_input` runs after each; `deny_after` fails
    /// the next input once `n` have landed (one-shot, so a later job can
    /// succeed).
    struct FakeSurface {
        rect: Result<ClientRect, SurfaceError>,
        events: Arc<Mutex<Vec<Recorded>>>,
        on_input: Box<dyn FnMut() + Send>,
        deny_after: Option<(usize, SurfaceError)>,
        releases: Arc<Mutex<usize>>,
        /// Handed out by either door, incrementing, so two jobs never share one.
        acquires: u32,
        /// Set by `acquire` and never by `measure`: the real `input` backend's
        /// `acquire` steals the foreground, so a test can ask whether a job took
        /// the door that acts on the desktop.
        engaged: Arc<Mutex<bool>>,
        /// Every window this surface was *given* back, in call order: one entry
        /// per `click`/`scroll`/`release`.
        windows: Arc<Mutex<Vec<FakeWindow>>>,
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
                acquires: 0,
                engaged: Arc::new(Mutex::new(false)),
                windows: Arc::new(Mutex::new(Vec::new())),
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

        /// Records which window the caller handed back. Deliberately *before*
        /// `deny`, so a refused input still proves it was aimed at the right one.
        fn saw(&mut self, window: &FakeWindow) {
            self.windows.lock().unwrap().push(*window);
        }
    }

    impl Surface for FakeSurface {
        type Window = FakeWindow;

        fn acquire(&mut self) -> Result<(FakeWindow, ClientRect), SurfaceError> {
            *self.engaged.lock().unwrap() = true;
            self.measure()
        }

        fn measure(&mut self) -> Result<(FakeWindow, ClientRect), SurfaceError> {
            let rect = self.rect.clone()?;
            self.acquires += 1;
            Ok((FakeWindow(self.acquires), rect))
        }

        fn click(
            &mut self,
            window: &FakeWindow,
            at: (i32, i32),
            press_ms: u64,
        ) -> Result<(), SurfaceError> {
            self.saw(window);
            self.deny()?;
            self.events
                .lock()
                .unwrap()
                .push(Recorded::Click(at, press_ms));
            (self.on_input)();
            Ok(())
        }

        fn scroll(
            &mut self,
            window: &FakeWindow,
            at: (i32, i32),
            notches: i32,
        ) -> Result<(), SurfaceError> {
            self.saw(window);
            self.deny()?;
            self.events
                .lock()
                .unwrap()
                .push(Recorded::Scroll(at, notches));
            (self.on_input)();
            Ok(())
        }

        fn release(&mut self, window: &FakeWindow) {
            self.saw(window);
            *self.releases.lock().unwrap() += 1;
        }
    }

    #[test]
    fn surface_job_guard_releases_once_on_scope_exit() {
        let (mut surface, _) = FakeSurface::new(design_rect());
        let releases = surface.releases.clone();

        {
            let _guard = SurfaceJobGuard::new(&mut surface, FakeWindow(1));
        }

        assert_eq!(*releases.lock().unwrap(), 1);
    }

    #[test]
    fn surface_job_guard_explicit_release_is_idempotent_with_drop() {
        let (mut surface, _) = FakeSurface::new(design_rect());
        let releases = surface.releases.clone();

        {
            let mut guard = SurfaceJobGuard::new(&mut surface, FakeWindow(1));
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
            let _guard = SurfaceJobGuard::new(&mut surface, FakeWindow(1));
            panic!("test unwind");
        }));

        assert!(result.is_err());
        assert_eq!(*releases.lock().unwrap(), 1);
    }

    /// Proves the `api-004` fix (see [`Surface`]): the window travels from
    /// `acquire` through every input to the release, and the executor cannot
    /// route a stale one because it does not keep one.
    ///
    /// Two jobs run back to back against one surface, each minting its own
    /// window. Job 1 is aborted by a recoverable refusal on its second input, so
    /// it is also the case where the old shape had a half-cleared
    /// `Option<Target>` to get wrong.
    #[tokio::test(start_paused = true)]
    async fn every_input_and_the_release_see_the_window_that_job_acquired() {
        let rig = rig();
        let (mut surface, events) = FakeSurface::new(design_rect());
        let windows = surface.windows.clone();
        let releases = surface.releases.clone();
        surface.deny_after = Some((
            1,
            SurfaceError::Recoverable("the game window moved or resized mid-job".to_owned()),
        ));
        for seed in [1, 2] {
            rig.job_tx
                .send(plan::refresh_job(
                    Trigger::Refreshed,
                    Timings::default(),
                    Epoch(0),
                    seed,
                ))
                .await
                .unwrap();
        }
        drop(rig.job_tx);
        run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false).await;

        // Job 1: one click lands, the second is refused, then the abort's
        // release. Job 2: both clicks and its own release.
        assert_eq!(events.lock().unwrap().len(), 3);
        assert_eq!(*releases.lock().unwrap(), 2);
        // Every call was handed *that* job's window, in order. A stale one would
        // show up here as a `1` after the first `2` — which is exactly what a
        // surface-held `Option<Target>` could produce and nothing checked.
        assert_eq!(
            *windows.lock().unwrap(),
            vec![
                FakeWindow(1),
                FakeWindow(1),
                FakeWindow(1),
                FakeWindow(2),
                FakeWindow(2),
                FakeWindow(2),
            ]
        );
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
        assert!(journal.to_entries().iter().any(|line| {
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
                Epoch(0),
                1,
            ))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false).await;
        assert!(events.lock().unwrap().is_empty());
        assert!(journal.to_entries().iter().any(|line| {
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
                Epoch(0),
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
                .to_entries()
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
                Epoch(0),
                1,
            ))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false).await;
        assert_eq!(events.lock().unwrap().len(), 1);
        assert_eq!(*releases.lock().unwrap(), 1);
        assert!(journal.to_entries().iter().any(|line| {
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
                Epoch(0),
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
        assert_eq!(gate.next_halt().await, HaltSource::ActuatorFailed);
        assert_eq!(*releases.lock().unwrap(), 0, "acquire engaged nothing");
        assert!(journal.to_entries().iter().any(|line| {
            line.text
                .contains("actuator: game window not found — stopping the loop")
        }));
    }

    /// The sibling arm of the one above, three lines apart in the source and
    /// the opposite verdict: an `acquire` that fails *recoverably* — the window
    /// vanished between two jobs — must drop the job and leave the watch armed
    /// so the watchdog's retry can heal it. Delete this and a mis-edit that
    /// routes it through `fail` turns a transient blip into a stopped hunt that
    /// blames the actuator; nothing else executes that arm
    /// (`executor_aborts_without_halt_on_a_minimized_acquire` acquires *fine*
    /// and is classified later, by `ScreenError::DegenerateRect`).
    #[tokio::test(start_paused = true)]
    async fn executor_aborts_without_halt_when_acquire_fails_recoverably() {
        let rig = rig();
        let (surface, events) = FakeSurface::new(Err(SurfaceError::Recoverable(
            "game window vanished".to_owned(),
        )));
        let releases = surface.releases.clone();
        rig.job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                Epoch(0),
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
        .expect("a recoverable acquire must end the job, then the loop ends on EOF");
        assert!(events.lock().unwrap().is_empty());
        assert!(
            gate.is_enabled(),
            "a transient blip must not halt the watch"
        );
        assert_eq!(*releases.lock().unwrap(), 0, "acquire engaged nothing");
        assert!(journal.to_entries().iter().any(|line| {
            line.text
                .contains("actuator: game window vanished — aborted remaining clicks")
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
                Epoch(0),
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
            .to_entries()
            .into_iter()
            .find(|line| line.text.contains("stopping the loop"))
            .expect("the halt must be journaled");
        assert!(
            line.text.contains("higher integrity level"),
            "{}",
            line.text
        );
        // The cause the player reads must be the real one (STOVE elevates the
        // game) and the action must be one they can perform on their side.
        assert!(line.text.contains("STOVE launcher"), "{}", line.text);
        assert!(
            line.text.contains("restart it as administrator"),
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
                Epoch(0),
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
        assert_eq!(gate.next_halt().await, HaltSource::ActuatorFailed);
        assert_eq!(*releases.lock().unwrap(), 1);
        assert!(journal.to_entries().iter().any(|line| {
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
                Epoch(0),
                &[plan::Row::new(0).unwrap(), plan::Row::new(4).unwrap()],
                42,
            ))
            .await
            .unwrap();
        rig.job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                Epoch(0),
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
        assert_eq!(gate.next_halt().await, HaltSource::ActuatorFailed);
        // Only the first job ever acquired: the queued one was refused before
        // the surface was touched.
        assert_eq!(*releases.lock().unwrap(), 1);
        let lines = journal.to_entries();
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
        // Proves [`run_executor`]'s task-survives-a-fatal invariant end to end:
        // re-arm, then a second job must still be served with no restart.
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
                Epoch(0),
                1,
            ))
            .await
            .unwrap();
        assert_eq!(gate.next_halt().await, HaltSource::ActuatorFailed);
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
                Epoch(0),
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
                .to_entries()
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
                Epoch(0),
                &[plan::Row::new(0).unwrap(), plan::Row::new(4).unwrap()],
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
        assert!(journal.to_entries().iter().any(|line| {
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
                Epoch(0),
                1,
            ))
            .await
            .unwrap();
        rig.job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                Epoch(0),
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
        //
        // The classification now comes from `ScreenError::DegenerateRect`, not
        // from a duplicate `rect.width <= 0` test the executor used to run
        // before the step loop. That is what this test guards: with a `String`
        // error, deleting the duplicate turned a transient minimize into a hard
        // halt with no compiler complaint.
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
                Epoch(0),
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
        assert!(journal.to_entries().iter().any(|line| {
            line.text
                .contains("degenerate client area 0×0 — aborted remaining clicks")
        }));
    }

    /// What the two verdict tests around this one do not pin: *when* the refusal
    /// lands. Both `ScreenError`s are properties of the rect `acquire` measured,
    /// so the answer is knowable before the job's first delay — but the only
    /// place the conversion ran was inside the step loop, one
    /// `sleep(step.wait_ms)` in. A minimized window paid that delay in full
    /// (1.18 s on the default timings, up to 61 s with a configured range) to
    /// reach a conclusion that was already true when the rect was read.
    ///
    /// Virtual time makes the cost exact rather than approximate: paused, the
    /// runtime auto-advances only over an awaited `sleep`, so a zero elapsed is
    /// proof that no step delay was waited out at all.
    #[tokio::test(start_paused = true)]
    async fn an_unmappable_window_is_refused_before_the_first_step_delay() {
        for rect in [
            // Minimized: recoverable, one job dropped.
            ClientRect {
                left: 0,
                top: 0,
                width: 0,
                height: 0,
            },
            // Narrower than 16:9: fatal, the watch halts.
            ClientRect {
                left: 0,
                top: 0,
                width: 1280,
                height: 800,
            },
        ] {
            let rig = rig();
            let (surface, events) = FakeSurface::new(Ok(rect));
            rig.job_tx
                .send(plan::buy_job(
                    Trigger::ShopOpened,
                    Timings::default(),
                    Epoch(0),
                    &[plan::Row::new(0).expect("row 0 is one of the six")],
                    42,
                ))
                .await
                .unwrap();
            drop(rig.job_tx);
            let started = tokio::time::Instant::now();
            run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false).await;
            assert_eq!(
                started.elapsed(),
                Duration::ZERO,
                "{rect:?} was unmappable before the job started; waiting a step delay to say so \
                 buys nothing"
            );
            assert!(events.lock().unwrap().is_empty());
        }
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
                Epoch(0),
                1,
            ))
            .await
            .unwrap();
        rig.job_tx
            .send(plan::refresh_job(
                Trigger::Refreshed,
                Timings::default(),
                Epoch(0),
                2,
            ))
            .await
            .unwrap();
        drop(rig.job_tx);
        run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, false).await;
        assert_eq!(*releases.lock().unwrap(), 1);
    }

    /// The other half of the classification: the same converter, the opposite
    /// verdict. A window narrower than 16:9 is not something the loop can heal,
    /// so it halts the watch — and the two arms are now told apart by
    /// `ScreenError`'s variant rather than by which check ran first.
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
                Epoch(0),
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
        assert_eq!(gate.next_halt().await, HaltSource::ActuatorFailed);
        assert_eq!(*releases.lock().unwrap(), 1);
        assert!(
            journal
                .to_entries()
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
                Epoch(0),
                1,
            ))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, true).await;
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(*releases.lock().unwrap(), 1);
        let lines = journal.to_entries();
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.text.contains("dry-run: click"))
                .count(),
            2
        );
    }

    /// The half of "dry run must not touch the game" the two tests around this
    /// one cannot see: they prove no *input* is sent, and `acquire` is not an
    /// input. On the `input` backend it is a real `SetForegroundWindow`, so the
    /// mode a cautious player enables first was reordering their desktop on every
    /// tick while sending nothing — and a `SetWindowPos` preflight had since been
    /// added to that same path. `Surface` now has two doors and the live job is
    /// what proves the engaging one still exists.
    #[tokio::test(start_paused = true)]
    async fn a_dry_run_takes_the_door_that_does_not_engage_the_window() {
        for dry_run in [true, false] {
            let rig = rig();
            let (surface, _events) = FakeSurface::new(design_rect());
            let engaged = surface.engaged.clone();
            rig.job_tx
                .send(plan::refresh_job(
                    Trigger::Refreshed,
                    Timings::default(),
                    Epoch(0),
                    1,
                ))
                .await
                .unwrap();
            drop(rig.job_tx);
            run_executor(
                surface,
                rig.job_rx,
                rig.gate,
                rig.epoch,
                rig.journal,
                dry_run,
            )
            .await;
            assert_eq!(
                *engaged.lock().unwrap(),
                !dry_run,
                "dry_run={dry_run}: only a job that will really click may engage the window"
            );
        }
    }

    /// The other half of "dry run must not touch the game": the test above
    /// submits a `refresh_job`, which is two clicks and no scroll, so the
    /// `Input::Scroll` arm of the dry-run branch was proved by nothing. A
    /// bottom-group row is what plans scrolls — one to the top, one to the
    /// bottom — and dry run is what a cautious player turns on first.
    #[tokio::test(start_paused = true)]
    async fn executor_dry_run_journals_a_scroll_without_touching_the_surface() {
        let rig = rig();
        let (surface, events) = FakeSurface::new(design_rect());
        let releases = surface.releases.clone();
        rig.job_tx
            .send(plan::buy_job(
                Trigger::ShopOpened,
                Timings::default(),
                Epoch(0),
                // First row past `LAST_TOP_ROW`, which is `plan`-private here.
                &[plan::Row::new(4).expect("row 4 is one of the six")],
                42,
            ))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(surface, rig.job_rx, rig.gate, rig.epoch, rig.journal, true).await;
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(*releases.lock().unwrap(), 1);
        let lines = journal.to_entries();
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.text.contains("dry-run: scroll"))
                .count(),
            2,
            "scroll to the top, then to the bottom group"
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
        let job = plan::buy_job(
            Trigger::ShopOpened,
            Timings::default(),
            Epoch(0),
            &[plan::Row::new(1).unwrap()],
            42,
        );
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
