//! The actuator: turns controller decisions into input driven into the game
//! window. [`plan`] is the pure half (zones, transform, timed job builders);
//! the executor below replays a job's steps against a [`Surface`], dropping
//! the job the moment the world changes underneath it.

pub mod plan;
#[cfg(all(windows, feature = "actuator"))]
mod shield;
#[cfg(all(windows, feature = "actuator"))]
pub mod win;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::app::Command;
use crate::journal::EventLog;
use crate::watch::WatchGate;

use plan::{Input, Job};

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

/// The session's grip on the executor: submit jobs, bump the epoch.
#[derive(Clone)]
pub struct ActuatorHandle {
    pub mode: Mode,
    pub epoch: SnapshotEpoch,
    jobs: mpsc::Sender<Job>,
}

impl ActuatorHandle {
    pub fn new(mode: Mode, epoch: SnapshotEpoch, jobs: mpsc::Sender<Job>) -> Self {
        Self { mode, epoch, jobs }
    }

    /// Queues a job for the executor; `false` when the queue is full — the
    /// caller journals the drop, a lost click must not be silent.
    pub fn submit(&self, job: Job) -> bool {
        self.jobs.try_send(job).is_ok()
    }
}

/// The input backend the executor drives: real input on Windows, a recorder
/// in tests.
pub trait Surface {
    /// Locates the game window, returning its client area — whether it is
    /// brought to the foreground is backend-specific. An `Err` means any
    /// click would be blind: the executor stops the loop.
    fn acquire(&mut self) -> Result<plan::ClientRect, String>;
    /// An `Err` means the input could not be delivered safely (e.g. the game
    /// window moved or vanished mid-job): the executor stops the loop.
    fn click(&mut self, at: (i32, i32), press_ms: u64) -> Result<(), String>;
    fn scroll(&mut self, at: (i32, i32), notches: i32) -> Result<(), String>;
    /// The job is over (completed or aborted): drop whatever the inputs set
    /// up (the message backend lowers its input shield). Default: nothing.
    fn release(&mut self) {}
}

/// Replays queued jobs step by step. Before every act it re-checks the gate
/// and the epoch: a stop or a fresh shop mid-job aborts the remaining steps —
/// never click blind. With `dry_run` the resolved screen input is journaled
/// instead of sent.
pub async fn run_executor(
    mut surface: impl Surface,
    mut jobs: mpsc::Receiver<Job>,
    gate: WatchGate,
    epoch: SnapshotEpoch,
    journal: EventLog,
    commands: mpsc::Sender<Command>,
    dry_run: bool,
) {
    while let Some(job) = jobs.recv().await {
        if let Some(reason) = drop_reason(&job, &epoch, &gate) {
            // Dropping is correct; dropping silently is not — the submit
            // side already journaled the promised click.
            journal.emit(&[format!(">> actuator: {reason} — dropped planned clicks")]);
            continue;
        }
        let rect = match surface.acquire() {
            Ok(rect) => rect,
            Err(reason) => {
                fail(&journal, &commands, &reason);
                continue;
            }
        };
        for step in &job.steps {
            tokio::time::sleep(Duration::from_millis(step.wait_ms)).await;
            if let Some(reason) = drop_reason(&job, &epoch, &gate) {
                journal.emit(&[format!(">> actuator: {reason} — aborted remaining clicks")]);
                break;
            }
            let at = match plan::to_screen(rect, step.input.at()) {
                Ok(at) => at,
                Err(reason) => {
                    fail(&journal, &commands, &reason);
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
            if let Err(reason) = delivered {
                fail(&journal, &commands, &reason);
                break;
            }
        }
        // Every exit from the steps loop lands here — completion, abort,
        // failure: the surface must never stay engaged between jobs.
        surface.release();
    }
}

/// Why a job (or its remainder) must not act: a newer shop invalidated the
/// planned coordinates, or the watch is off. `None` means clear to act.
fn drop_reason(job: &Job, epoch: &SnapshotEpoch, gate: &WatchGate) -> Option<&'static str> {
    if job.epoch != epoch.current() {
        Some("the shop changed")
    } else if !gate.is_enabled() {
        Some("the watch is off")
    } else {
        None
    }
}

/// An actuator that cannot act safely stops the whole loop: a hunt that
/// keeps refreshing without its clicker is spend without effect. The halt
/// carries its own label (`StopReason::ActuatorFailed`), never the player's.
fn fail(journal: &EventLog, commands: &mpsc::Sender<Command>, reason: &str) {
    journal.emit(&[format!(">> actuator: {reason} — stopping the loop")]);
    let _ = commands.try_send(Command::ActuatorFailed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use plan::{ClientRect, Trigger};
    use std::sync::Mutex;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Recorded {
        Click((i32, i32), u64),
        Scroll((i32, i32), i32),
    }

    /// Records every input; `on_input` runs after each one (to flip the gate
    /// or bump the epoch mid-job); `deny_after` fails every input once `n`
    /// inputs have landed (`n = 0` = fail the first).
    struct FakeSurface {
        rect: Result<ClientRect, String>,
        events: Arc<Mutex<Vec<Recorded>>>,
        on_input: Box<dyn FnMut() + Send>,
        deny_after: Option<(usize, String)>,
        releases: Arc<Mutex<usize>>,
    }

    impl FakeSurface {
        fn new(rect: Result<ClientRect, String>) -> (Self, Arc<Mutex<Vec<Recorded>>>) {
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

        fn deny(&self) -> Result<(), String> {
            match &self.deny_after {
                Some((n, reason)) if self.events.lock().unwrap().len() >= *n => Err(reason.clone()),
                _ => Ok(()),
            }
        }
    }

    impl Surface for FakeSurface {
        fn acquire(&mut self) -> Result<ClientRect, String> {
            self.rect.clone()
        }

        fn click(&mut self, at: (i32, i32), press_ms: u64) -> Result<(), String> {
            self.deny()?;
            self.events
                .lock()
                .unwrap()
                .push(Recorded::Click(at, press_ms));
            (self.on_input)();
            Ok(())
        }

        fn scroll(&mut self, at: (i32, i32), notches: i32) -> Result<(), String> {
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

    fn design_rect() -> Result<ClientRect, String> {
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
        command_tx: mpsc::Sender<Command>,
        command_rx: mpsc::Receiver<Command>,
    }

    fn rig() -> Rig {
        let (job_tx, job_rx) = mpsc::channel(8);
        let (command_tx, command_rx) = mpsc::channel(4);
        Rig {
            job_tx,
            job_rx,
            gate: WatchGate::new(true),
            epoch: SnapshotEpoch::default(),
            journal: EventLog::default(),
            command_tx,
            command_rx,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn executor_skips_stale_epoch_jobs() {
        let mut rig = rig();
        let (surface, events) = FakeSurface::new(design_rect());
        let job = plan::refresh_job(Trigger::Refreshed, rig.epoch.current(), 1);
        rig.epoch.bump(); // a newer shop arrived before the job started
        rig.job_tx.send(job).await.unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(
            surface,
            rig.job_rx,
            rig.gate,
            rig.epoch,
            rig.journal,
            rig.command_tx,
            false,
        )
        .await;
        assert!(events.lock().unwrap().is_empty());
        assert!(rig.command_rx.try_recv().is_err());
        // Dropped, but never silently: the submit side promised a click.
        assert!(journal.entries().iter().any(|line| {
            line.text
                .contains("the shop changed — dropped planned clicks")
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn executor_skips_jobs_while_gate_off() {
        let mut rig = rig();
        rig.gate.set(false);
        let (surface, events) = FakeSurface::new(design_rect());
        rig.job_tx
            .send(plan::refresh_job(Trigger::Refreshed, 0, 1))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(
            surface,
            rig.job_rx,
            rig.gate,
            rig.epoch,
            rig.journal,
            rig.command_tx,
            false,
        )
        .await;
        assert!(events.lock().unwrap().is_empty());
        assert!(rig.command_rx.try_recv().is_err());
        assert!(journal.entries().iter().any(|line| {
            line.text
                .contains("the watch is off — dropped planned clicks")
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn executor_aborts_mid_job_when_gate_turns_off() {
        let rig = rig();
        let (mut surface, events) = FakeSurface::new(design_rect());
        let gate = rig.gate.clone();
        surface.on_input = Box::new(move || gate.set(false));
        rig.job_tx
            .send(plan::refresh_job(Trigger::Refreshed, 0, 1))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(
            surface,
            rig.job_rx,
            rig.gate,
            rig.epoch,
            rig.journal,
            rig.command_tx,
            false,
        )
        .await;
        // Two steps planned, only the first landed — and the abort is
        // journaled.
        assert_eq!(events.lock().unwrap().len(), 1);
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
        let epoch = rig.epoch.clone();
        surface.on_input = Box::new(move || epoch.bump());
        rig.job_tx
            .send(plan::refresh_job(Trigger::Refreshed, 0, 1))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(
            surface,
            rig.job_rx,
            rig.gate,
            rig.epoch,
            rig.journal,
            rig.command_tx,
            false,
        )
        .await;
        assert_eq!(events.lock().unwrap().len(), 1);
        assert!(journal.entries().iter().any(|line| {
            line.text
                .contains("the shop changed — aborted remaining clicks")
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn executor_stops_the_loop_when_acquire_fails() {
        let mut rig = rig();
        let (surface, events) = FakeSurface::new(Err("game window not found".to_owned()));
        rig.job_tx
            .send(plan::refresh_job(Trigger::Refreshed, 0, 1))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(
            surface,
            rig.job_rx,
            rig.gate,
            rig.epoch,
            rig.journal,
            rig.command_tx,
            false,
        )
        .await;
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(rig.command_rx.try_recv(), Ok(Command::ActuatorFailed));
        assert!(journal.entries().iter().any(|line| {
            line.text
                .contains("actuator: game window not found — stopping the loop")
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn executor_stops_the_loop_when_an_input_fails() {
        // A surface that cannot deliver an input safely (e.g. the game window
        // moved mid-job) must halt the hunt with the actuator's own label,
        // not click blind or skip silently.
        let mut rig = rig();
        let (mut surface, events) = FakeSurface::new(design_rect());
        surface.deny_after = Some((0, "the game window moved".to_owned()));
        rig.job_tx
            .send(plan::refresh_job(Trigger::Refreshed, 0, 1))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(
            surface,
            rig.job_rx,
            rig.gate,
            rig.epoch,
            rig.journal,
            rig.command_tx,
            false,
        )
        .await;
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(rig.command_rx.try_recv(), Ok(Command::ActuatorFailed));
        assert!(journal.entries().iter().any(|line| {
            line.text
                .contains("the game window moved — stopping the loop")
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn executor_keeps_landed_inputs_and_stops_once_on_a_mid_job_failure() {
        // Rows 0 and 4: scroll-top, buy row 0, confirm, then the bottom
        // scroll fails. The three landed inputs stay recorded, exactly one
        // halt command goes out, and the surface is still released.
        let mut rig = rig();
        let (mut surface, events) = FakeSurface::new(design_rect());
        surface.deny_after = Some((3, "the game window moved".to_owned()));
        let releases = surface.releases.clone();
        rig.job_tx
            .send(plan::buy_job(Trigger::ShopOpened, 0, &[0, 4], 42))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(
            surface,
            rig.job_rx,
            rig.gate,
            rig.epoch,
            rig.journal,
            rig.command_tx,
            false,
        )
        .await;
        assert_eq!(events.lock().unwrap().len(), 3);
        assert_eq!(rig.command_rx.try_recv(), Ok(Command::ActuatorFailed));
        assert!(
            rig.command_rx.try_recv().is_err(),
            "one halt, not one per step"
        );
        assert_eq!(*releases.lock().unwrap(), 1);
        assert!(
            journal
                .entries()
                .iter()
                .any(|line| line.text.contains("stopping the loop"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn executor_releases_the_surface_after_every_acquired_job() {
        // Completion and mid-job abort both end with a release; a job dropped
        // before acquire never engaged anything, so nothing to release.
        let rig = rig();
        let (mut surface, _events) = FakeSurface::new(design_rect());
        let releases = surface.releases.clone();
        let gate = rig.gate.clone();
        surface.on_input = Box::new(move || gate.set(false));
        rig.job_tx
            .send(plan::refresh_job(Trigger::Refreshed, 0, 1))
            .await
            .unwrap();
        rig.job_tx
            .send(plan::refresh_job(Trigger::Refreshed, 0, 2))
            .await
            .unwrap();
        drop(rig.job_tx);
        run_executor(
            surface,
            rig.job_rx,
            rig.gate,
            rig.epoch,
            rig.journal,
            rig.command_tx,
            false,
        )
        .await;
        // Job 1 aborts mid-steps (gate off after its first input) and is
        // released; job 2 is dropped by the gate before acquire — no release.
        assert_eq!(*releases.lock().unwrap(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn executor_stops_the_loop_on_a_narrow_window() {
        let mut rig = rig();
        let (surface, events) = FakeSurface::new(Ok(ClientRect {
            left: 0,
            top: 0,
            width: 1280,
            height: 800,
        }));
        rig.job_tx
            .send(plan::refresh_job(Trigger::Refreshed, 0, 1))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(
            surface,
            rig.job_rx,
            rig.gate,
            rig.epoch,
            rig.journal,
            rig.command_tx,
            false,
        )
        .await;
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(rig.command_rx.try_recv(), Ok(Command::ActuatorFailed));
        assert!(
            journal
                .entries()
                .iter()
                .any(|line| line.text.contains("narrower than 16:9"))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn executor_dry_run_journals_without_input() {
        let mut rig = rig();
        let (surface, events) = FakeSurface::new(design_rect());
        rig.job_tx
            .send(plan::refresh_job(Trigger::Refreshed, 0, 1))
            .await
            .unwrap();
        drop(rig.job_tx);
        let journal = rig.journal.clone();
        run_executor(
            surface,
            rig.job_rx,
            rig.gate,
            rig.epoch,
            rig.journal,
            rig.command_tx,
            true,
        )
        .await;
        assert!(events.lock().unwrap().is_empty());
        let lines = journal.entries();
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.text.contains("dry-run: click"))
                .count(),
            2
        );
        assert!(rig.command_rx.try_recv().is_err());
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
        let job = plan::buy_job(Trigger::ShopOpened, 0, &[1], 42);
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
        run_executor(
            surface,
            rig.job_rx,
            rig.gate,
            rig.epoch,
            rig.journal,
            rig.command_tx,
            false,
        )
        .await;
        assert_eq!(*events.lock().unwrap(), expected);
    }
}
