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

/// The actuator's poison-tolerant lock: a panic on another thread must not turn
/// every later click into a fatal. [`crate::sync`]'s obligation on its callers is
/// discharged by what is guarded — `Timings` is `Copy` and copied straight out,
/// the shield's slot is a plain handle — so neither can be caught half-written.
use crate::sync::lock_ignoring_poison as lock;

/// Generation counter of the shop state, bumped on every shop message. A job
/// carries the epoch it was planned against and the executor refuses to act on
/// any other: clicks aimed at a shop that no longer exists must die, not land.
///
/// `Relaxed` is not a weakening: the epoch is only compared for equality and
/// nothing is published *through* it. It reaches the executor baked into a `Job`
/// on an `mpsc` channel, whose `send`/`recv` makes the happens-before edge.
#[derive(Clone, Default)]
pub struct SnapshotEpoch(Arc<AtomicU64>);

impl SnapshotEpoch {
    pub fn bump(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    /// An [`Epoch`] rather than a bare `u64`: every job builder takes it beside
    /// a millisecond seed, and the two are not interchangeable.
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

/// Input backend of the Windows build. Re-exported as
/// [`crate::config::ActuatorBackend`], which is where `config.toml` names it.
///
/// Owned here rather than in `config` because [`run_executor`] reads it once
/// per job to decide which [`Surface`] to drive, and this module imports
/// nothing from `config`. `Deserialize` because the config layer parses
/// straight into it — the arrangement [`plan::Timings`] already has.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActuatorBackend {
    /// `SendInput`: drives the real cursor and forces the game window to the
    /// foreground. Works whatever the engine reads input from — the fallback
    /// if a game update stops honoring posted messages.
    Input,
    /// `PostMessageW`: posts synthetic mouse messages to the window — no
    /// focus stolen, the player keeps the mouse. Live-validated against the
    /// game (refresh, buys, wheel scroll, unfocused window).
    #[default]
    Message,
}

/// What the executor should do with the *next* job it dequeues: whether to send
/// any input at all, and which Win32 path to send it through.
///
/// **One cell, not two atomics.** The pair is read together at the top of every
/// job, and two independent atomics could be observed half-updated — a job
/// running the old backend in the new rehearsal state, or the reverse. A single
/// Apply must not be able to land in halves, so the pair travels as one value
/// under one lock, exactly as [`Timings`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClickMode {
    /// Plan and journal the clicks, send nothing.
    pub dry_run: bool,
    pub backend: ActuatorBackend,
}

/// The session's grip on the executor: submit jobs, bump the epoch, read and
/// retune the player's extra waits and click mode.
#[derive(Clone)]
pub struct ActuatorHandle {
    /// What the actuator is compiled and configured to do at all — `Off` when
    /// no backend is built in. **Not** the rehearsal switch: that moved into
    /// [`ClickMode`] when it became live, because a `Copy` field on a cloned
    /// handle cannot carry a change to the executor.
    pub mode: Mode,
    pub epoch: SnapshotEpoch,
    jobs: mpsc::Sender<Job>,
    /// Shared with [`crate::app::setup`]'s live-edit path. Jobs bake the
    /// resolved waits at submit time, so the executor never touches it.
    timings: Arc<Mutex<Timings>>,
    /// Shared with the executor, which snapshots it once per job. See
    /// [`ClickMode`] for why it is one cell.
    click_mode: Arc<Mutex<ClickMode>>,
}

impl ActuatorHandle {
    pub fn new(
        mode: Mode,
        epoch: SnapshotEpoch,
        jobs: mpsc::Sender<Job>,
        timings: Arc<Mutex<Timings>>,
        click_mode: Arc<Mutex<ClickMode>>,
    ) -> Self {
        Self {
            mode,
            epoch,
            jobs,
            timings,
            click_mode,
        }
    }

    /// Queues a job for the executor, naming *why* it was lost when it was: the
    /// caller journals the drop, a lost click must not be silent.
    ///
    /// # Errors
    ///
    /// [`SubmitError::QueueFull`] when the executor is alive but behind, and
    /// [`SubmitError::ExecutorGone`] when nobody is reading any more — two
    /// variants because they need opposite advice, see [`SubmitError`].
    #[must_use = "a rejected job means a lost click — journal the drop"]
    pub fn submit(&self, job: Job) -> Result<(), SubmitError> {
        match self.jobs.try_send(job) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(SubmitError::QueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(SubmitError::ExecutorGone),
        }
    }

    /// The extra waits to bake into the next job, copied out from under the lock
    /// (never held across a plan build). Poison-tolerant (see [`lock`]): this
    /// runs while building *every* queued job, so panicking here would take the
    /// session down over an unrelated fault.
    #[must_use]
    pub fn timings(&self) -> Timings {
        *lock(&self.timings)
    }

    /// Swaps in the player's retuned waits; the next queued job uses them.
    /// Poison-tolerant for the same reason as [`timings`](Self::timings).
    pub fn set_timings(&self, timings: Timings) {
        *lock(&self.timings) = timings;
    }

    /// The mode the next *dequeued* job runs in.
    ///
    /// Note the difference from [`timings`](Self::timings), and it is the whole
    /// design: waits are baked in at **submit** time, so a job carries the
    /// timings it was planned with. The mode is read at **dequeue** time, so a
    /// job already sitting in the queue picks up a change made after it was
    /// planned. Both are "applies to the next job" from the player's side; only
    /// one of them can be baked in, because a job carries no surface.
    #[must_use]
    pub fn click_mode(&self) -> ClickMode {
        *lock(&self.click_mode)
    }

    /// Swaps in the player's rehearsal switch and backend choice. Takes effect
    /// on the next job the executor dequeues — never mid-job, see
    /// [`run_executor`].
    pub fn set_click_mode(&self, mode: ClickMode) {
        *lock(&self.click_mode) = mode;
    }

    /// The cell itself, for the one caller that must *watch* it rather than
    /// read it once: [`run_executor`], which snapshots it per job.
    ///
    /// Deliberately not `pub`-facing beyond that — everyone else goes through
    /// [`click_mode`](Self::click_mode) and [`set_click_mode`](Self::set_click_mode),
    /// so the lock is never held across anything.
    #[must_use]
    pub fn click_mode_cell(&self) -> Arc<Mutex<ClickMode>> {
        Arc::clone(&self.click_mode)
    }
}

/// Why a job never reached the executor. Do not collapse these back into one
/// flag: a full queue is transient back-pressure the next tick clears, a closed
/// channel means nobody is at the other end, and journaling the first when it is
/// really the second sends the player after a slow actuator that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    /// The executor is alive but behind: the bounded queue is at capacity.
    #[error("the actuator queue is full — the executor is behind")]
    QueueFull,
    /// The receiving end is gone. Since a fatal no longer ends [`run_executor`],
    /// this is reachable only once the session is tearing its workers down.
    #[error("the actuator executor is gone — nothing will run this job")]
    ExecutorGone,
}

/// How a surface failure must be handled. Classified at the error's birth site
/// (the backend knows what broke), never blanket-mapped per trait method.
///
/// Both payloads are already operator-facing text, so `Display` is `{0}`
/// verbatim: that is what lets an actuator failure appear in an `error = %err`
/// field or a crash chain at all.
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

/// The input backend the executor drives: real input on Windows, a recorder in
/// tests. Every method may park its thread for the length of an input, so the
/// executor only ever calls them through `blocking`.
///
/// Do not move the window back into the backend as a field. `acquire` hands one
/// out and every input method takes it, so "input without an acquire" is not a
/// value that can be built; as a field (`api-004`) it took three run-time checks
/// to say the same — an `Option<Target>` plus a `target()` guard in each Windows
/// backend, and an `.expect` in the executor's guard. What stays fail-closed is
/// only what the *world* can break, which is [`SurfaceError`]'s job.
pub trait Surface {
    /// A backend's proof that it acquired the game window, and everything it
    /// needs to act on it. Opaque: the executor only routes it from `acquire` to
    /// the input calls and finally to `release`. A backend with nothing to carry
    /// uses `()`.
    type Window;

    /// Locates the game window, returning its client area — whether it is
    /// brought to the foreground is backend-specific.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::Recoverable`] when the window is alive but not usable
    /// right now, so the *next* `acquire` may succeed. [`SurfaceError::Fatal`]
    /// when acting would be blind — no window carrying the game's title, a
    /// client rect Windows refuses to read, a process DPI awareness that would
    /// place every click at the wrong scale, or a window at a higher integrity
    /// level than this process — and the payload is the line the player reads.
    ///
    /// Either way nothing was engaged, so a failed `acquire` leaves no
    /// [`release`](Surface::release) owing: hence the cleanup guard being built
    /// only on the `Ok` path.
    fn acquire(&mut self) -> Result<(Self::Window, plan::ClientRect), SurfaceError>;

    /// The same window and the same client area, for a job that will not send
    /// anything: [`Mode::DryRun`].
    ///
    /// Do not default this to `acquire`: that default is how the simulation path
    /// came to call a real `SetForegroundWindow` on the `input` backend, so the
    /// mode a cautious player turns on *first* yanked Epic Seven in front of
    /// them every tick while sending no input. Each backend writing the decision
    /// down is what stops the next one inheriting the surprise.
    ///
    /// Measuring is still wanted: the rect resolves the journal's screen
    /// coordinates, and a preflight that provably changes nothing (see
    /// `win::probe_window_reachable`) is the most useful thing a rehearsal can
    /// report. The line is *engaging*, not *looking*.
    ///
    /// # Errors
    ///
    /// The same classification as [`acquire`](Surface::acquire): a dry run
    /// reports the faults a live run would hit.
    fn measure(&mut self) -> Result<(Self::Window, plan::ClientRect), SurfaceError>;

    /// One left click at a screen point, held `press_ms`.
    ///
    /// Implementations must answer everything the world can break with a
    /// [`SurfaceError`], never a panic: the executor runs in a supervised task,
    /// so a panic ends the session while `Fatal` halts the watch with a reason.
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
    /// `window`. Must be idempotent and non-panicking — it runs from a
    /// destructor — and the window is borrowed rather than consumed so the
    /// executor's guard can own it outright instead of behind the `Option` that
    /// made its `expect` necessary.
    fn release(&mut self, window: &Self::Window) {
        let _ = window;
    }
}

/// Lets a boxed trait object stand in for a concrete backend, so
/// [`run_executor`] can swap which one it drives between jobs while staying
/// generic.
///
/// Both shipped backends declare `type Window = Target` — one shared type in
/// `win::mod` — so `dyn Surface<Window = Target>` unifies them with no wrapper
/// enum. Written as a blanket impl over `W` rather than over that one type so
/// the test fake, whose `Window` is its own, is neither touched nor tempted
/// into the box.
impl<W> Surface for Box<dyn Surface<Window = W> + Send> {
    type Window = W;

    fn acquire(&mut self) -> Result<(Self::Window, plan::ClientRect), SurfaceError> {
        (**self).acquire()
    }

    fn measure(&mut self) -> Result<(Self::Window, plan::ClientRect), SurfaceError> {
        (**self).measure()
    }

    fn click(
        &mut self,
        window: &Self::Window,
        at: (i32, i32),
        press_ms: u64,
    ) -> Result<(), SurfaceError> {
        (**self).click(window, at, press_ms)
    }

    fn scroll(
        &mut self,
        window: &Self::Window,
        at: (i32, i32),
        notches: i32,
    ) -> Result<(), SurfaceError> {
        (**self).scroll(window, at, notches)
    }

    fn release(&mut self, window: &Self::Window) {
        (**self).release(window);
    }
}

/// Runs one blocking [`Surface`] call without starving the runtime.
///
/// Every surface method parks its thread — 120-170 ms for a single click, a
/// dozen of those back to back for a multi-slot buy. Left plain on a runtime
/// worker, that stalls the reassembly task until the capture channels overflow
/// and the stream re-anchors mid-purchase, precisely when the `purchase` echo is
/// due; it also outlasts shutdown's grace deadline, since `JoinHandle::abort`
/// cannot interrupt a thread sitting in `std::thread::sleep`.
///
/// `block_in_place` panics anywhere but the multi-thread runtime, hence the
/// flavor probe: the executor's own tests drive it on the current-thread runtime
/// with the clock paused (`start_paused` and `multi_thread` are mutually
/// exclusive), and the guard tests call it with no runtime at all.
fn blocking<T>(call: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(call),
        _ => call(),
    }
}

/// Owns cleanup for one successfully acquired job, and that job's window.
///
/// Plain values rather than `Option`s: the guard is built only on `acquire`'s
/// `Ok` path, so an inactive guard is not a state it has. Release idempotence is
/// the `bool`, which is all the `Option` was really tracking.
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
        // Cleanup posts and hides too: blocking all the same, and it also runs
        // from `Drop` on the runtime worker.
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
/// A fatal fault ends the job, never the task, and that still stops the actuator
/// acting: [`fail`] disables the gate *synchronously* and latches the cause, and
/// [`drop_reason`] re-reads it at the top of every job and before every step, so
/// from the fatal onwards jobs are dropped before `acquire` is even called —
/// which also keeps `fail` from spamming.
///
/// The task must survive because it is spawned once per session and nothing
/// respawns it: returning would drop the job receiver, and every later submit
/// would fail against a channel nobody reads, even though `Start` from
/// `Status::Stopped` re-arms the gate and the session looks alive. Staying in
/// the loop is what lets the player fix the cause, press Start, and recover with
/// no process restart. (The UIPI halt is the one exception whose fix *is*
/// restarting this app: the integrity level is fixed at process start.)
pub async fn run_executor<S: Surface>(
    mut surface: S,
    mut jobs: mpsc::Receiver<Job>,
    gate: WatchGate,
    epoch: SnapshotEpoch,
    journal: EventLog,
    click_mode: Arc<Mutex<ClickMode>>,
    mut reload: impl FnMut(ActuatorBackend, &mut S),
) {
    // The backend in hand. Compared against the snapshot below so `reload` runs
    // only on an actual change: rebuilding every job would throw away whatever
    // state a backend keeps across acquires for no reason.
    let mut driving = lock(&click_mode).backend;
    while let Some(job) = jobs.recv().await {
        // **Read once, here, and nowhere below.** `dry_run` used to be a
        // parameter, so its three reads inside a job could not disagree. Now it
        // can change under us, and a job that took the `measure` path and then
        // sent real input would be acting on a rect it never engaged — or the
        // reverse, stealing the foreground and then only journalling. Snapshot
        // above `drop_reason` so the whole job, drop line included, describes
        // one mode.
        //
        // Swapping the surface belongs here for a second reason: between jobs
        // `SurfaceJobGuard` has already run its `release`, so nothing is owed to
        // the backend being replaced.
        let mode = *lock(&click_mode);
        if mode.backend != driving {
            reload(mode.backend, &mut surface);
            driving = mode.backend;
        }
        let dry_run = mode.dry_run;
        if let Some(reason) = drop_reason(&job, &epoch, &gate) {
            // Dropping is correct; dropping silently is not — the submit
            // side already journaled the promised click.
            journal.emit(&[format!(">> actuator: {reason} — dropped planned clicks")]);
            continue;
        }
        // A dry run takes the door that only looks: the mode is a promise not to
        // touch the game, and `acquire` — which may steal the foreground — was
        // breaking it before any click was even planned.
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
                // Nothing was acquired, so nothing is owed a release. The gate is
                // down now, so every job behind this one is dropped at the top
                // of the loop until the player re-arms.
                fail(&journal, &gate, &reason);
                continue;
            }
        };
        let mut surface = SurfaceJobGuard::new(&mut surface, window);
        // Between the guard and the first sleep: both refusals are properties of
        // the rect `acquire` just measured, so asking per step waited out a step
        // delay (1.18 s by default, up to 61 s with a configured range) to
        // answer an unchanging question. After the guard, though: a successful
        // `acquire` owes a `release` whatever the rect turns out to be.
        let viewport = match plan::Viewport::of(rect) {
            Ok(viewport) => viewport,
            // A minimized window acquires with an empty client area, and the
            // next `acquire` reads a fresh one. Classified by the converter's
            // `ScreenError`, not by a `rect.width <= 0` test repeated here.
            Err(error @ ScreenError::DegenerateRect { .. }) => {
                abort(&journal, &error.to_string());
                continue;
            }
            // Nothing the loop can heal: the player has to widen the window.
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
    // `warn`, not `info`: the job did not do what the journal already promised,
    // and the watchdog's retry only makes sense to a reader who saw this line.
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
/// reached: the loop is inert from here even though its task keeps running (see
/// [`run_executor`] for why it must not die). That also makes this
/// self-limiting — no surface call happens again until the player acknowledges
/// the halt and re-arms — so a standing fault cannot flood the journal.
fn fail(journal: &EventLog, gate: &WatchGate, reason: &str) {
    // `error`, matching a session abort: one of the two lines that say the
    // product stopped doing its job, and self-limiting (see above), so it cannot
    // flood the file at this level.
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

    /// A mode cell the executor will read but nobody will write.
    fn mode_cell(mode: ClickMode) -> Arc<Mutex<ClickMode>> {
        Arc::new(Mutex::new(mode))
    }

    /// The default for every test that predates the live switch: real clicks,
    /// the shipped backend.
    fn live_mode() -> Arc<Mutex<ClickMode>> {
        mode_cell(ClickMode::default())
    }

    fn rehearsal_mode() -> Arc<Mutex<ClickMode>> {
        mode_cell(ClickMode {
            dry_run: true,
            ..ClickMode::default()
        })
    }

    /// The swap hook for tests that never change backend. Panics rather than
    /// no-ops: a test that reaches it has changed the backend without meaning
    /// to, and a silent no-op would hide that.
    fn no_reload<S>(_backend: ActuatorBackend, _surface: &mut S) {
        panic!("this test must not swap the backend");
    }

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

    /// `apply()` reaches `actuator.timings()` while holding the controller
    /// guard, so without [`lock`]'s poison tolerance a panic under the timings
    /// lock would poison that one too and every later dispatch would panic.
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
            Arc::new(Mutex::new(ClickMode::default())),
        );
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

    /// One is minted per `acquire`, so a test can prove that every input and the
    /// release were handed *this job's* window and not a stale one.
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
        /// Set by `acquire` and never by `measure`, so a test can ask whether a
        /// job took the door that acts on the desktop.
        engaged: Arc<Mutex<bool>>,
        /// Every window this surface was *given* back, in call order.
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

        /// Called *before* `deny`, so a refused input still proves it was aimed
        /// at the right window.
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
    /// route a stale one because it keeps none. Job 1 aborts on its second
    /// input — the case where the old shape had a half-cleared `Option<Target>`.
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
            rig.job_tx.send(refresh(seed)).await.unwrap();
        }
        rig.run(surface, live_mode()).await;

        // Job 1: one click lands, the second is refused, then the abort's
        // release. Job 2: both clicks and its own release.
        assert_eq!(events.lock().unwrap().len(), 3);
        assert_eq!(*releases.lock().unwrap(), 2);
        // A stale window would show up here as a `1` after the first `2` —
        // exactly what a surface-held `Option<Target>` could produce.
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

    /// The two-click refresh job these tests use as their generic unit of work.
    /// `seed` stays a caller argument: a test that queues two jobs tells them
    /// apart by it, and `every_input_and_the_release_see_the_window_that_job_acquired`
    /// reads the two windows in that order. `Epoch(0)` is what a fresh [`rig`]'s
    /// `SnapshotEpoch` reports, so a job built here is never stale by accident —
    /// the one test about staleness builds its job by hand from `rig.epoch`.
    fn refresh(seed: u64) -> Job {
        plan::refresh_job(Trigger::Refreshed, Timings::default(), Epoch(0), seed)
    }

    /// A buy job for `rows`, at the same fixed epoch as [`refresh`]. Which rows
    /// are planned is the whole subject of the tests that call this — a
    /// bottom-group row is what makes the plan contain scrolls — so the slice
    /// stays at the call site, including how each `Row` was built.
    fn buy(rows: &[plan::Row]) -> Job {
        plan::buy_job(Trigger::ShopOpened, Timings::default(), Epoch(0), rows, 42)
    }

    /// What a finished executor run leaves for the test to read. Returned
    /// rather than hand-cloned above every call: [`run_executor`] takes the
    /// journal and the gate by value, so each site used to clone whichever one
    /// it was about one line before handing both away.
    struct Ran {
        journal: EventLog,
        gate: WatchGate,
    }

    impl Rig {
        /// Closes the submit side, then drives the executor over exactly what is
        /// already queued so the loop ends on EOF rather than on a timer.
        ///
        /// Hides no verdict. `mode` is a caller argument because the rehearsal
        /// switch is what several of these tests are about; the surface, with
        /// whatever failure it was armed with, is built at the call site; and
        /// the backend-swap hook is [`no_reload`], which panics — a test that
        /// means to swap backends calls [`run_executor`] itself.
        async fn run<S: Surface>(self, surface: S, mode: Arc<Mutex<ClickMode>>) -> Ran {
            let ran = Ran {
                journal: self.journal.clone(),
                gate: self.gate.clone(),
            };
            drop(self.job_tx);
            run_executor(
                surface,
                self.job_rx,
                self.gate,
                self.epoch,
                self.journal,
                mode,
                no_reload,
            )
            .await;
            ran
        }

        /// [`Rig::run`] under a ceiling, for the tests whose subject is that the
        /// executor *keeps draining* after a fatal instead of wedging. The
        /// wedge is the failure they watch for, so `expectation` — the sentence
        /// a hang would print — stays at the call site, one per test.
        async fn run_bounded<S: Surface>(
            self,
            surface: S,
            mode: Arc<Mutex<ClickMode>>,
            expectation: &str,
        ) -> Ran {
            tokio::time::timeout(Duration::from_secs(10), self.run(surface, mode))
                .await
                .expect(expectation)
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
        let ran = rig.run(surface, live_mode()).await;
        assert!(events.lock().unwrap().is_empty());
        // Dropped, but never silently: the submit side promised a click.
        assert!(ran.journal.to_entries().iter().any(|line| {
            line.text
                .contains("the shop changed — dropped planned clicks")
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn executor_skips_jobs_while_gate_off() {
        let rig = rig();
        rig.gate.set(false);
        let (surface, events) = FakeSurface::new(design_rect());
        rig.job_tx.send(refresh(1)).await.unwrap();
        let ran = rig.run(surface, live_mode()).await;
        assert!(events.lock().unwrap().is_empty());
        assert!(ran.journal.to_entries().iter().any(|line| {
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
        rig.job_tx.send(refresh(1)).await.unwrap();
        let ran = rig.run(surface, live_mode()).await;
        // Two steps planned, only the first landed.
        assert_eq!(events.lock().unwrap().len(), 1);
        assert_eq!(*releases.lock().unwrap(), 1);
        assert!(
            ran.journal
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
        rig.job_tx.send(refresh(1)).await.unwrap();
        let ran = rig.run(surface, live_mode()).await;
        assert_eq!(events.lock().unwrap().len(), 1);
        assert_eq!(*releases.lock().unwrap(), 1);
        assert!(ran.journal.to_entries().iter().any(|line| {
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
        rig.job_tx.send(refresh(1)).await.unwrap();
        // The fatal no longer ends the task, so the producer has to close the
        // channel for the loop to finish; the ceiling fails the test if the
        // fatal ever stops consuming.
        let ran = rig
            .run_bounded(
                surface,
                live_mode(),
                "the executor must keep draining after a fatal acquire, then end on EOF",
            )
            .await;
        assert!(events.lock().unwrap().is_empty());
        assert!(!ran.gate.is_enabled());
        assert_eq!(ran.gate.next_halt().await, HaltSource::ActuatorFailed);
        assert_eq!(*releases.lock().unwrap(), 0, "acquire engaged nothing");
        assert!(ran.journal.to_entries().iter().any(|line| {
            line.text
                .contains("actuator: game window not found — stopping the loop")
        }));
    }

    /// The sibling arm of the one above, three lines apart in the source and the
    /// opposite verdict. Nothing else executes it —
    /// `executor_aborts_without_halt_on_a_minimized_acquire` acquires *fine* and
    /// is classified later by `ScreenError::DegenerateRect` — so without this, a
    /// mis-edit routing it through `fail` turns a blip into a stopped hunt.
    #[tokio::test(start_paused = true)]
    async fn executor_aborts_without_halt_when_acquire_fails_recoverably() {
        let rig = rig();
        let (surface, events) = FakeSurface::new(Err(SurfaceError::Recoverable(
            "game window vanished".to_owned(),
        )));
        let releases = surface.releases.clone();
        rig.job_tx.send(refresh(1)).await.unwrap();
        let ran = rig
            .run_bounded(
                surface,
                live_mode(),
                "a recoverable acquire must end the job, then the loop ends on EOF",
            )
            .await;
        assert!(events.lock().unwrap().is_empty());
        assert!(
            ran.gate.is_enabled(),
            "a transient blip must not halt the watch"
        );
        assert_eq!(*releases.lock().unwrap(), 0, "acquire engaged nothing");
        assert!(ran.journal.to_entries().iter().any(|line| {
            line.text
                .contains("actuator: game window vanished — aborted remaining clicks")
        }));
    }

    /// The preflight's verdict has to *reach the player*: this is the rest of
    /// the trip, through `fail` into the journal the window renders. Built from
    /// the real classifier, not a copy of its text, so a reworded diagnosis
    /// cannot pass here while shipping something else.
    #[cfg(all(windows, feature = "actuator"))]
    #[tokio::test(start_paused = true)]
    async fn a_refused_preflight_reaches_the_journal_naming_the_integrity_level() {
        use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;

        let refusal = win::preflight_refusal(&std::io::Error::from_raw_os_error(
            ERROR_ACCESS_DENIED as i32,
        ));
        let rig = rig();
        let (surface, events) = FakeSurface::new(Err(refusal));
        rig.job_tx.send(refresh(1)).await.unwrap();
        let ran = rig
            .run_bounded(
                surface,
                live_mode(),
                "a refused preflight must halt the watch, not wedge the executor",
            )
            .await;

        assert!(events.lock().unwrap().is_empty(), "nothing may be clicked");
        assert!(!ran.gate.is_enabled());
        let line = ran
            .journal
            .to_entries()
            .into_iter()
            .find(|line| line.text.contains("stopping the loop"))
            .expect("the halt must be journaled");
        assert!(
            line.text.contains("higher integrity level"),
            "{}",
            line.text
        );
        // The cause must be the real one (STOVE elevates the game) and the
        // action one the player can perform on their side.
        assert!(line.text.contains("STOVE launcher"), "{}", line.text);
        assert!(
            line.text.contains("restart it as administrator"),
            "{}",
            line.text
        );
    }

    #[tokio::test(start_paused = true)]
    async fn executor_stops_the_loop_when_an_input_fails() {
        let rig = rig();
        let (mut surface, events) = FakeSurface::new(design_rect());
        surface.deny_after = Some((
            0,
            SurfaceError::Fatal("could not raise the input shield".to_owned()),
        ));
        let releases = surface.releases.clone();
        rig.job_tx.send(refresh(1)).await.unwrap();
        let ran = rig
            .run_bounded(
                surface,
                live_mode(),
                "a fatal input must end the job, then the loop ends on EOF",
            )
            .await;
        assert!(events.lock().unwrap().is_empty());
        assert!(!ran.gate.is_enabled());
        assert_eq!(ran.gate.next_halt().await, HaltSource::ActuatorFailed);
        assert_eq!(*releases.lock().unwrap(), 1);
        assert!(ran.journal.to_entries().iter().any(|line| {
            line.text
                .contains("could not raise the input shield — stopping the loop")
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn executor_keeps_landed_inputs_and_stops_once_on_a_mid_job_failure() {
        // The second submit is the point: the executor lives long enough to pick
        // that job up, and must drop it on the downed gate rather than act on
        // it.
        let rig = rig();
        let (mut surface, events) = FakeSurface::new(design_rect());
        surface.deny_after = Some((
            3,
            SurfaceError::Fatal("could not raise the input shield".to_owned()),
        ));
        let releases = surface.releases.clone();
        rig.job_tx
            .send(buy(&[
                plan::Row::new(0).unwrap(),
                plan::Row::new(4).unwrap(),
            ]))
            .await
            .unwrap();
        rig.job_tx.send(refresh(1)).await.unwrap();
        let ran = rig
            .run_bounded(
                surface,
                live_mode(),
                "a fatal mid-job input must drop the queued work, then end on EOF",
            )
            .await;
        assert_eq!(events.lock().unwrap().len(), 3);
        assert!(!ran.gate.is_enabled());
        assert_eq!(ran.gate.next_halt().await, HaltSource::ActuatorFailed);
        // Only the first job ever acquired: the queued one was refused before
        // the surface was touched.
        assert_eq!(*releases.lock().unwrap(), 1);
        let lines = ran.journal.to_entries();
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
        // [`run_executor`]'s task-survives-a-fatal invariant, end to end.
        let rig = rig();
        let (mut surface, events) = FakeSurface::new(design_rect());
        // One-shot: it fails the first input and never again, standing in for
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
            live_mode(),
            no_reload,
        ));

        job_tx.send(refresh(1)).await.unwrap();
        assert_eq!(gate.next_halt().await, HaltSource::ActuatorFailed);
        assert!(!gate.is_enabled());
        assert!(
            events.lock().unwrap().is_empty(),
            "the fatal must deliver nothing"
        );

        // What the session does once the player presses Start again.
        gate.acknowledge_halt(HaltSource::ActuatorFailed);
        gate.set(true);
        assert!(
            gate.is_enabled(),
            "the acknowledged cause must let it re-arm"
        );

        job_tx.send(refresh(2)).await.unwrap();
        drop(job_tx);
        tokio::time::timeout(Duration::from_secs(10), executor)
            .await
            .expect("the executor must still be running to serve the second job")
            .expect("the executor task must not panic");

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
        let rig = rig();
        let (mut surface, events) = FakeSurface::new(design_rect());
        surface.deny_after = Some((
            3,
            SurfaceError::Recoverable("the game window moved or resized mid-job".to_owned()),
        ));
        let releases = surface.releases.clone();
        rig.job_tx
            .send(buy(&[
                plan::Row::new(0).unwrap(),
                plan::Row::new(4).unwrap(),
            ]))
            .await
            .unwrap();
        let ran = rig.run(surface, live_mode()).await;
        assert_eq!(events.lock().unwrap().len(), 3);
        assert!(ran.gate.is_enabled(), "no halt for a recoverable abort");
        assert_eq!(*releases.lock().unwrap(), 1);
        assert!(ran.journal.to_entries().iter().any(|line| {
            line.text
                .contains("the game window moved or resized mid-job — aborted remaining clicks")
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn executor_serves_the_next_job_after_a_recoverable_abort() {
        let rig = rig();
        let (mut surface, events) = FakeSurface::new(design_rect());
        surface.deny_after = Some((
            1,
            SurfaceError::Recoverable("the game window moved or resized mid-job".to_owned()),
        ));
        let releases = surface.releases.clone();
        rig.job_tx.send(refresh(1)).await.unwrap();
        rig.job_tx.send(refresh(2)).await.unwrap();
        let ran = rig.run(surface, live_mode()).await;
        // First job: one landed click, then the abort. Second job: both.
        assert_eq!(events.lock().unwrap().len(), 3);
        assert!(ran.gate.is_enabled());
        assert_eq!(*releases.lock().unwrap(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn executor_aborts_without_halt_on_a_minimized_acquire() {
        // Classified by `ScreenError::DegenerateRect`, not by a duplicate
        // `rect.width <= 0` test before the step loop: with a `String` error,
        // deleting that duplicate turned a transient minimize into a hard halt
        // with no compiler complaint.
        let rig = rig();
        let (surface, events) = FakeSurface::new(Ok(ClientRect {
            left: 0,
            top: 0,
            width: 0,
            height: 0,
        }));
        let releases = surface.releases.clone();
        rig.job_tx.send(refresh(1)).await.unwrap();
        let ran = rig.run(surface, live_mode()).await;
        assert!(events.lock().unwrap().is_empty());
        assert!(ran.gate.is_enabled());
        assert_eq!(*releases.lock().unwrap(), 1);
        assert!(ran.journal.to_entries().iter().any(|line| {
            line.text
                .contains("degenerate client area 0×0 — aborted remaining clicks")
        }));
    }

    /// What the two verdict tests around this one do not pin: *when* the refusal
    /// lands. Both `ScreenError`s are properties of the rect `acquire` measured,
    /// so the answer is knowable before the job's first delay — yet the
    /// conversion used to run inside the step loop, one `sleep(step.wait_ms)`
    /// in. Paused, the runtime auto-advances only over an awaited `sleep`, so a
    /// zero elapsed proves no step delay was waited out at all.
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
                .send(buy(&[plan::Row::new(0).expect("row 0 is one of the six")]))
                .await
                .unwrap();
            let started = tokio::time::Instant::now();
            rig.run(surface, live_mode()).await;
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
        // The second job is dropped on the downed gate, so it never engages
        // anything and never releases.
        let rig = rig();
        let (mut surface, _events) = FakeSurface::new(design_rect());
        let releases = surface.releases.clone();
        let gate = rig.gate.clone();
        surface.on_input = Box::new(move || gate.set(false));
        rig.job_tx.send(refresh(1)).await.unwrap();
        rig.job_tx.send(refresh(2)).await.unwrap();
        rig.run(surface, live_mode()).await;
        assert_eq!(*releases.lock().unwrap(), 1);
    }

    /// The other half of the classification: a window narrower than 16:9 is not
    /// something the loop can heal, so it halts the watch. The two arms are told
    /// apart by `ScreenError`'s variant, not by which check ran first.
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
        rig.job_tx.send(refresh(1)).await.unwrap();
        let ran = rig
            .run_bounded(
                surface,
                live_mode(),
                "a fatal coordinate conversion must end the job, then the loop ends on EOF",
            )
            .await;
        assert!(events.lock().unwrap().is_empty());
        assert!(!ran.gate.is_enabled());
        assert_eq!(ran.gate.next_halt().await, HaltSource::ActuatorFailed);
        assert_eq!(*releases.lock().unwrap(), 1);
        assert!(
            ran.journal
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
        rig.job_tx.send(refresh(1)).await.unwrap();
        let ran = rig.run(surface, rehearsal_mode()).await;
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(*releases.lock().unwrap(), 1);
        let lines = ran.journal.to_entries();
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.text.contains("dry-run: click"))
                .count(),
            2
        );
    }

    /// The half of "dry run must not touch the game" the tests around this one
    /// cannot see: they prove no *input* is sent, and `acquire` is not an input
    /// — on the `input` backend it is a real `SetForegroundWindow`. The
    /// `dry_run == false` pass is what proves the engaging door still exists.
    #[tokio::test(start_paused = true)]
    async fn a_dry_run_takes_the_door_that_does_not_engage_the_window() {
        for dry_run in [true, false] {
            let rig = rig();
            let (surface, _events) = FakeSurface::new(design_rect());
            let engaged = surface.engaged.clone();
            rig.job_tx.send(refresh(1)).await.unwrap();
            rig.run(
                surface,
                mode_cell(ClickMode {
                    dry_run,
                    ..ClickMode::default()
                }),
            )
            .await;
            assert_eq!(
                *engaged.lock().unwrap(),
                !dry_run,
                "dry_run={dry_run}: only a job that will really click may engage the window"
            );
        }
    }

    /// The test above submits a `refresh_job`, which is two clicks and no
    /// scroll, so the `Input::Scroll` arm of the dry-run branch was proved by
    /// nothing. A bottom-group row is what plans scrolls.
    #[tokio::test(start_paused = true)]
    async fn executor_dry_run_journals_a_scroll_without_touching_the_surface() {
        let rig = rig();
        let (surface, events) = FakeSurface::new(design_rect());
        let releases = surface.releases.clone();
        rig.job_tx
            // First row past `LAST_TOP_ROW`, which is `plan`-private here.
            .send(buy(&[plan::Row::new(4).expect("row 4 is one of the six")]))
            .await
            .unwrap();
        let ran = rig.run(surface, rehearsal_mode()).await;
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(*releases.lock().unwrap(), 1);
        let lines = ran.journal.to_entries();
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
        let job = buy(&[plan::Row::new(1).unwrap()]);
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
        rig.run(surface, live_mode()).await;
        assert_eq!(*events.lock().unwrap(), expected);
        assert_eq!(*releases.lock().unwrap(), 1);
    }
}
