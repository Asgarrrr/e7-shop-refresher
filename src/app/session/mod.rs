//! The session loop and its handlers: own the controller, apply its
//! decisions to the capture gate, and echo every outcome to the player.

use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use crate::actuator::plan::{self, Trigger};
use crate::actuator::{ActuatorBackend, ActuatorHandle, Mode, SubmitError};
use crate::domain::control::{Action, BuyTarget, Controller, Event, Recovery, Status};
use crate::journal::EventLog;
use crate::render::{format_item, refusal_label, render_shop, status_label, stop_reason_label};
use crate::stream::RunBaselineCell;
use crate::uplink::UplinkEvent;
use crate::uplink::protocol::{PurchaseNotice, ServerMessage};
use crate::watch::{HaltSource, WatchGate};

use super::Command;

/// The session tick period; the domain's time limits are read against this
/// wall clock.
const TICK_PERIOD: Duration = Duration::from_secs(1);

/// One heartbeat every 30 ticks (30 s). A healthy pipeline at rest and a dead
/// one are otherwise indistinguishable in the logs: both are silent.
const HEARTBEAT_EVERY_TICKS: u64 = 30;

/// A panicking worker drops its sender one scheduling hop before its supervisor
/// sends the fatal, so without this window a crash reads as a clean
/// "session ended".
const FATAL_REPORT_GRACE: Duration = Duration::from_millis(150);

/// Poison-tolerant: `.expect("controller mutex poisoned")` turned a recoverable
/// frame fault into `supervise` reporting "session crashed" and stopping the
/// relay. [`crate::sync`]'s obligation is discharged by `Controller::handle`
/// being pure and saturating, so an unwinding thread leaves no half-written
/// state behind its guard.
use crate::sync::lock_ignoring_poison as lock;

/// The capture gate this loop projects the controller's status into, and the
/// run baseline that has to move on the same edge.
///
/// One handle rather than two parameters. The baseline is published exactly when
/// [`WatchGate::set`] reports that it opened the gate, and the defect it exists
/// for is a baseline taken at any *other* moment — so "arm the gate and leave the
/// baseline where it was" is not left spellable at the next call site added here.
#[derive(Clone)]
pub(super) struct SessionGate {
    gate: WatchGate,
    run: RunBaselineCell,
}

impl SessionGate {
    pub(super) fn new(gate: WatchGate, run: RunBaselineCell) -> Self {
        Self { gate, run }
    }

    /// Projects `armed` into the gate and, when that is what opens it, publishes
    /// the zero this run's capture verdict counts from.
    ///
    /// The baseline is read *before* the store that opens the gate, and kept only
    /// if that store was the arming edge. No packet crosses a shut gate, so
    /// nothing this run does can land in between; taken after the gate opened,
    /// the read could already include the re-anchor the capture thread records on
    /// the first packet past it — the same fault as the 250 ms the window used to
    /// take, only shorter. See `stream::RunBaselineCell::counters_now` for what
    /// *can* land there, and why charging it here is the safe direction.
    ///
    /// Unconditional, and so is the read in front of it: which projection is the
    /// arming edge is not knowable before [`WatchGate::set`] resolves it, and a
    /// pre-check of [`WatchGate::is_enabled`] to skip the read would be a second,
    /// racy answer to the question that store already answers exactly.
    fn arm(&self, armed: bool) {
        let baseline = self.run.counters_now();
        if self.gate.set(armed) {
            self.run.publish(baseline);
        }
    }

    fn is_enabled(&self) -> bool {
        self.gate.is_enabled()
    }

    async fn next_halt(&self) -> HaltSource {
        self.gate.next_halt().await
    }

    fn acknowledge_halt(&self, dispatched: HaltSource) {
        self.gate.acknowledge_halt(dispatched);
    }

    /// Test-only, and only these two: the shipped safety producers (the GUI, the
    /// actuator, `app::workers`) hold the [`WatchGate`] itself, because a halt
    /// ends a run rather than starting one and owes no baseline.
    #[cfg(test)]
    pub(super) fn request_halt(&self, source: HaltSource) {
        self.gate.request_halt(source);
    }

    /// A gate over a budget nothing else holds, for the suites whose subject is
    /// arming rather than what a run counted.
    #[cfg(test)]
    pub(super) fn for_test(enabled: bool) -> Self {
        Self::new(
            WatchGate::new(enabled),
            RunBaselineCell::new(crate::stream::PipelineBudget::new()),
        )
    }
}

/// Why the loop stopped. The teardown event and the fatal-report grace window
/// are both read off it.
enum Exit {
    /// Ctrl+C or the window closing — the player's own stop.
    PlayerStopped,
    /// The uplink's message channel closed; a racing fatal may still arrive.
    UplinkClosed,
    /// A pipeline death, already journaled.
    Fatal(String),
}

/// Owns the controller for the session. The mutex guard is only ever held
/// across synchronous calls, never an `.await`; the wall clock is read here so
/// the domain stays pure. Returns the fatal failure, if one ended the session,
/// for the caller to turn into a banner and an exit code.
#[expect(
    clippy::too_many_arguments,
    reason = "four borrowed session handles plus the four channels this loop multiplexes; \
              bundling them would hide which halves the loop owns and which it only borrows"
)]
pub(super) async fn session_loop(
    controller: &Mutex<Controller>,
    gate: &SessionGate,
    journal: &EventLog,
    actuator: &ActuatorHandle,
    mut commands: mpsc::Receiver<Command>,
    mut messages: mpsc::Receiver<UplinkEvent>,
    mut fatal_errors: mpsc::Receiver<String>,
    mut shutdown: watch::Receiver<bool>,
) -> Option<String> {
    let now_ms = || journal.now_ms();
    // Not `tokio::time::interval`, because the tick is serviced outside the
    // `select!` — see the note above it. Re-arming from *now* is
    // `MissedTickBehavior::Delay`; Burst would replay a CPU stall's worth of
    // ticks back to back, all carrying near-identical `now_ms`.
    let mut next_tick = tokio::time::Instant::now();
    // Only *wakes* the loop; whether a tick is due is decided by `next_tick`.
    let tick_wakeup = tokio::time::sleep_until(next_tick);
    tokio::pin!(tick_wakeup);
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    // Stdin EOF and the fatal channel draining are not shutdowns: their
    // branches disable instead of spinning the loop on a drained channel.
    let mut commands_open = true;
    let mut fatal_open = true;
    let mut shutdown_open = true;
    let mut ticks: u64 = 0;
    let mut last_shop_ms: Option<u64> = None;
    // An unknown server tag presents like a mute server; `heartbeat` tells the
    // two apart by this count.
    let mut unknown_messages: u64 = 0;
    let exit = loop {
        // Read here, not just in the branch: a window closed during startup
        // sets the signal before the loop starts, when `changed()` never fires.
        if *shutdown.borrow() {
            journal.emit(&[">> shutting down".to_owned()]);
            break Exit::PlayerStopped;
        }
        // The tick is a deadline this loop owns, not a `select!` branch: as a
        // branch under `messages` it was never serviced at all under a flood
        // (measured: five seconds past a 500 ms `max_duration_ms`, no tick),
        // and `Event::Tick` is the only thing that enforces `Limits` and steps
        // the watchdog's rungs. Tokio's budget does not break the tie — it
        // returns *every* branch pending at once and re-polls from the top.
        // Above the exits, but bounded: one dispatch per `TICK_PERIOD`.
        let due = tokio::time::Instant::now();
        if due >= next_tick {
            next_tick = due + TICK_PERIOD;
            tick_wakeup.as_mut().reset(next_tick);
            let now = now_ms();
            dispatch(
                controller,
                gate,
                journal,
                actuator,
                Event::Tick { now_ms: now },
                now,
            );
            ticks = ticks.wrapping_add(1);
            if ticks.is_multiple_of(HEARTBEAT_EVERY_TICKS) {
                heartbeat(controller, gate, now, last_shop_ms, unknown_messages);
            }
        }
        // `biased` makes this a priority list, and an always-ready branch
        // starves everything under it. `messages` is the one a remote party can
        // hold that way, so the four session-ending branches sit above it
        // (`08d62ba`) — safety halt first. Nothing below them can starve:
        // `commands` is human-paced, and `tick_wakeup`'s deadline is absolute,
        // so a wakeup that never comes loses no tick.
        tokio::select! {
            biased;
            source = gate.next_halt() => {
                let event = match source {
                    HaltSource::PlayerStopped => Event::Stop,
                    HaltSource::ActuatorFailed => Event::ActuatorFailed,
                };
                dispatch(controller, gate, journal, actuator, event, now_ms());
                // The durable cause stays pending through dispatch, so a
                // cancelled select branch or racing Start cannot re-arm the
                // safety gate before the controller records the stop.
                gate.acknowledge_halt(source);
            },
            // Only wakes the loop; the value is re-read at the top. An `Err`
            // means the signal's owner is gone and no stop can ever arrive,
            // so the branch disables instead of spinning.
            changed = shutdown.changed(), if shutdown_open => {
                shutdown_open = changed.is_ok();
            },
            _ = &mut ctrl_c => {
                journal.emit(&[">> Ctrl+C, stopping".to_owned()]);
                break Exit::PlayerStopped;
            }
            error = fatal_errors.recv(), if fatal_open => match error {
                Some(error) => {
                    // Break immediately: the channel cascade can take tens of
                    // seconds, during which the window would keep claiming a
                    // healthy watch. `error`, not `info`, because README's
                    // troubleshooting sends the player looking for this line.
                    journal.emit_at(
                        tracing::Level::ERROR,
                        &[format!(">> session aborted — {error}")],
                    );
                    break Exit::Fatal(error);
                }
                None => fatal_open = false,
            },
            command = commands.recv(), if commands_open => match command {
                Some(command) => on_command(controller, gate, journal, actuator, command, now_ms()),
                None => commands_open = false,
            },
            message = messages.recv() => match message {
                Some(UplinkEvent::Message(message)) => {
                    let now = now_ms();
                    // Stamped before the message is consumed: the heartbeat
                    // needs this to tell a blind capture from a mute server.
                    if matches!(message, ServerMessage::Shop(_)) {
                        last_shop_ms = Some(now);
                    }
                    if matches!(message, ServerMessage::Unknown) {
                        unknown_messages = unknown_messages.wrapping_add(1);
                    }
                    on_message(controller, gate, journal, actuator, message, now);
                }
                // An armed watch with a dead link looks exactly like a closed
                // shop: journaled at `warn` so the reason survives a narrowed
                // log, and told to the controller so the watchdog does not
                // escalate over a dead wire.
                Some(UplinkEvent::LinkDown(reason)) => {
                    journal.emit_at(
                        tracing::Level::WARN,
                        &[format!(">> server link down: {reason} — retrying, no shop can arrive")],
                    );
                    dispatch(controller, gate, journal, actuator, Event::LinkDown, now_ms());
                }
                Some(UplinkEvent::LinkUp) => {
                    journal.emit(&[">> server link restored".to_owned()]);
                    let now = now_ms();
                    dispatch(controller, gate, journal, actuator, Event::LinkUp { now_ms: now }, now);
                }
                None => break Exit::UplinkClosed,
            },
            // Parks until the next tick is due; the tick itself runs at the top.
            _ = &mut tick_wakeup => {}
        }
    };
    // See [`FATAL_REPORT_GRACE`].
    let mut raced_failure = None;
    if matches!(exit, Exit::UplinkClosed)
        && fatal_open
        && let Ok(Some(error)) = tokio::time::timeout(FATAL_REPORT_GRACE, fatal_errors.recv()).await
    {
        journal.emit_at(
            tracing::Level::ERROR,
            &[format!(">> session aborted — {error}")],
        );
        raced_failure = Some(error);
    }
    // The window (GUI build) outlives the loop, so leave honest state behind:
    // controller stopped, gate off, a journal line saying why.
    let teardown = match exit {
        Exit::PlayerStopped => Event::Stop,
        Exit::UplinkClosed | Exit::Fatal(_) => Event::Shutdown,
    };
    dispatch(controller, gate, journal, actuator, teardown, now_ms());
    match exit {
        Exit::Fatal(error) => Some(error),
        Exit::PlayerStopped | Exit::UplinkClosed => raced_failure,
    }
}

/// Capture blind, server mute, actuator stuck and an unparsed server dialect
/// are indistinguishable in a silent log; `unknown_messages` is what separates
/// the last two. Nothing is awaited, so the guard never crosses an await.
fn heartbeat(
    controller: &Mutex<Controller>,
    gate: &SessionGate,
    now_ms: u64,
    last_shop_ms: Option<u64>,
    unknown_messages: u64,
) {
    let ctrl = lock(controller);
    let status = status_label(&ctrl);
    let refreshes = ctrl.progress().refreshes;
    drop(ctrl);
    let since_last_shop_s = last_shop_ms.map(|at| now_ms.saturating_sub(at) / 1000);
    tracing::info!(
        status = %status,
        refreshes,
        gate_armed = gate.is_enabled(),
        since_last_shop_s = ?since_last_shop_s,
        unknown_messages,
        "session heartbeat"
    );
}

/// A command is never silent, even when the controller ignores it.
fn on_command(
    controller: &Mutex<Controller>,
    gate: &SessionGate,
    journal: &EventLog,
    actuator: &ActuatorHandle,
    command: Command,
    now_ms: u64,
) {
    let lines = handle_command(controller, gate, actuator, command, now_ms);
    journal.emit(&lines);
}

/// The command logic behind [`on_command`]; `Toggle` resolves against the
/// current status.
fn handle_command(
    controller: &Mutex<Controller>,
    gate: &SessionGate,
    actuator: &ActuatorHandle,
    command: Command,
    now_ms: u64,
) -> Vec<String> {
    let mut ctrl = lock(controller);
    let event = match command {
        Command::Start => Event::Start { now_ms },
        Command::Stop => Event::Stop,
        Command::Toggle => match ctrl.status() {
            Status::Watching | Status::Paused => Event::Stop,
            Status::Idle | Status::Stopped(_) => Event::Start { now_ms },
        },
        // Retunes echo their own confirmation: the transition logic below only
        // reads status changes, which these never cause. Acceptance comes from
        // the domain's verdict, not list emptiness, so it survives a retune
        // that also emits an action.
        Command::SetFilter(filter) => {
            let paused = ctrl.status() == Status::Paused;
            let actions = ctrl.handle(Event::FilterChanged(filter));
            let accepted = !actions.iter().any(Action::is_refusal);
            let mut lines = apply(&actions, &ctrl, gate, actuator, None, now_ms);
            if accepted {
                lines.push(">> filter updated — applies from the next shop".to_owned());
                if paused {
                    // The pending pause still waits on the *previous* filter's
                    // matches; the new criteria cannot retroactively clear it.
                    lines.push(
                        ">> still paused on earlier matches — buy them or wait for a new shop"
                            .to_owned(),
                    );
                }
            }
            return lines;
        }
        Command::SetLimits(limits) => {
            let actions = ctrl.handle(Event::LimitsChanged(limits));
            let accepted = !actions.iter().any(Action::is_refusal);
            let mut lines = apply(&actions, &ctrl, gate, actuator, None, now_ms);
            if accepted {
                lines.push(">> limits updated — checked before the next refresh".to_owned());
            }
            return lines;
        }
        // Timings are not domain state; the next queued job bakes them in.
        Command::SetTimings(timings) => {
            actuator.set_timings(timings);
            return vec![">> click timings updated — applies to the next queued clicks".to_owned()];
        }
        // Not domain state either, with one exception that is: whether the
        // actuator really clicks decides whether the recovery watchdog may arm.
        // A rehearsal yields no wire feedback, so a deadline would self-halt the
        // session — the rule `app::setup` applies once at wiring time, applied
        // again here because the switch is now live.
        Command::SetClickMode(mode) => {
            actuator.set_click_mode(mode);
            // `Mode::Off` means no backend is compiled in at all; nothing
            // clicks whatever the switch says, so recovery stays dark.
            let clicking = actuator.mode != Mode::Off && !mode.dry_run;
            ctrl.set_recovery(clicking);
            let what = if mode.dry_run {
                "rehearsal on — clicks are planned and journaled, none are sent"
            } else {
                match mode.backend {
                    ActuatorBackend::Message => "clicks are posted to the window",
                    ActuatorBackend::Input => "clicks drive the real cursor",
                }
            };
            return vec![format!(">> {what} — applies to the next queued clicks")];
        }
    };
    let before = ctrl.status();
    let actions = ctrl.handle(event);
    let mut lines = apply(&actions, &ctrl, gate, actuator, None, now_ms);
    let after = ctrl.status();
    let has_stored_shop = ctrl.last_snapshot().is_some();
    let label = status_label(&ctrl);
    drop(ctrl);
    if actions.is_empty() {
        // `Start` yields no action, and ignored commands yield nothing at all:
        // synthesize the feedback from the status transition.
        if before != after && after == Status::Watching {
            lines.push(">> watching — open the shop".to_owned());
            if has_stored_shop {
                lines.push(">> the stored shop is not replayed — re-open it in game".to_owned());
            }
        } else if before == after {
            lines.push(format!(">> no effect — status: {label}"));
        }
    }
    lines
}

/// Acks and unknown messages produce no player-facing line.
fn on_message(
    controller: &Mutex<Controller>,
    gate: &SessionGate,
    journal: &EventLog,
    actuator: &ActuatorHandle,
    message: ServerMessage,
    now_ms: u64,
) {
    match message {
        // Counted by `heartbeat`; just nothing player-facing to print here.
        ServerMessage::Ack | ServerMessage::Unknown => {}
        ServerMessage::Shop(snapshot) => {
            // Console-only: the GUI table shows the same snapshot, and the
            // journal carries only the decisions.
            render_shop(&snapshot);
            // Every shop message bumps, duplicates included: a re-send means
            // the player touched the game, so in-flight clicks are aborted.
            actuator.epoch.bump();
            let mut ctrl = lock(controller);
            // Read before handling: an advised refresh counts itself into
            // `refreshes`, so the very first shop would look refreshed after.
            let trigger = if ctrl.progress().refreshes == 0 {
                Trigger::ShopOpened
            } else {
                Trigger::Refreshed
            };
            let actions = ctrl.handle(Event::Snapshot { snapshot, now_ms });
            let lines = apply(&actions, &ctrl, gate, actuator, Some(trigger), now_ms);
            drop(ctrl);
            journal.emit(&lines);
        }
        ServerMessage::Purchase(notice) => {
            let lines = handle_purchase(controller, gate, actuator, &notice, now_ms);
            journal.emit(&lines);
        }
    }
}

/// One lock for the whole purchase: the bought line renders against the same
/// state the event applies to, and comes first, before any auto-resume advice.
/// The guard needs no explicit release the way `dispatch`'s does — its last
/// read is the last statement, and returning drops it before the caller has
/// journalled a line.
fn handle_purchase(
    controller: &Mutex<Controller>,
    gate: &SessionGate,
    actuator: &ActuatorHandle,
    notice: &PurchaseNotice,
    now_ms: u64,
) -> Vec<String> {
    let mut ctrl = lock(controller);
    let mut lines = vec![purchase_line(&ctrl, notice)];
    let actions = ctrl.handle(Event::Purchase {
        item: notice.item,
        gold: notice.gold,
        now_ms,
    });
    lines.extend(apply(
        &actions,
        &ctrl,
        gate,
        actuator,
        Some(Trigger::PurchaseResumed),
        now_ms,
    ));
    lines
}

/// The `is_some()` guard is load-bearing: `None == item.id` would let an
/// echo with no id resolve to the name of an item that also has none.
fn purchase_line(controller: &Controller, notice: &PurchaseNotice) -> String {
    let name = controller.last_snapshot().and_then(|snapshot| {
        snapshot
            .slots
            .iter()
            .find(|item| item.id.is_some() && item.id == notice.item)
            .and_then(|item| item.name.as_deref())
    });
    let label = match name {
        Some(name) => name.to_owned(),
        None => match notice.item {
            Some(id) => format!("item {id}"),
            None => "an unidentified item".to_owned(),
        },
    };
    match notice.gold {
        Some(gold) => format!(">> bought: {label} — {gold} gold left"),
        None => format!(">> bought: {label}"),
    }
}

/// Printing happens after the guard is released. No trigger reaches the
/// actuator from here: callers (ticks, teardown, test setup) are never shop or
/// purchase arrivals.
fn dispatch(
    controller: &Mutex<Controller>,
    gate: &SessionGate,
    journal: &EventLog,
    actuator: &ActuatorHandle,
    event: Event,
    now_ms: u64,
) {
    let mut ctrl = lock(controller);
    let actions = ctrl.handle(event);
    let lines = apply(&actions, &ctrl, gate, actuator, None, now_ms);
    drop(ctrl);
    journal.emit(&lines);
}

/// Callers print after the guard is dropped: console I/O can block or panic on
/// closed stdout and must not stall or poison the controller the GUI shares.
///
/// `trigger` names the animation the game plays when the actions land; paths
/// that cannot advise a refresh or buy pass `None`. The gate follows the
/// status, and off -> on retriggers the capture thread's existing resync — and,
/// through [`SessionGate::arm`], starts the run the capture readout reports on.
/// This projection is the crate's only arming site, and it is unconditional, so
/// no run can begin anywhere else.
fn apply(
    actions: &[Action],
    controller: &Controller,
    gate: &SessionGate,
    actuator: &ActuatorHandle,
    trigger: Option<Trigger>,
    now_ms: u64,
) -> Vec<String> {
    let mut lines = Vec::new();
    for action in actions {
        match action {
            Action::Refresh => submit_refresh(&mut lines, actuator, trigger, now_ms),
            Action::Buy { targets } => {
                render_match(&mut lines, targets, controller);
                submit_buys(&mut lines, controller, actuator, trigger, targets, now_ms);
            }
            Action::Recover(recovery) => {
                submit_recovery(&mut lines, controller, actuator, recovery, now_ms);
            }
            Action::Halt(reason) => {
                lines.push(format!(">> stopped: {}", stop_reason_label(*reason)));
            }
            Action::Refused(reason) => {
                lines.push(format!(">> refused: {}", refusal_label(*reason)));
            }
        }
    }
    gate.arm(matches!(
        controller.status(),
        Status::Watching | Status::Paused
    ));
    lines
}

/// The trigger to plan a job against; `None` when nothing may be clicked —
/// the actuator is off, or the path cannot name the game's animation.
fn active_trigger(actuator: &ActuatorHandle, trigger: Option<Trigger>) -> Option<Trigger> {
    match actuator.mode {
        Mode::Off => None,
        Mode::DryRun | Mode::Live => trigger,
    }
}

/// A refresh decision: a click job when the actuator is on (the job's pre-wait
/// covers the animation `trigger` names), the advice line otherwise.
fn submit_refresh(
    lines: &mut Vec<String>,
    actuator: &ActuatorHandle,
    trigger: Option<Trigger>,
    now_ms: u64,
) {
    let Some(trigger) = active_trigger(actuator, trigger) else {
        lines.push(">> → refresh the shop now".to_owned());
        return;
    };
    lines.push(if actuator.mode == Mode::Live {
        ">> → refresh clicked".to_owned()
    } else {
        ">> → refresh planned (dry-run)".to_owned()
    });
    queue_refresh(lines, actuator, trigger, now_ms);
}

/// The refresh-job submission core shared by the normal path and the watchdog
/// re-issue. Callers narrate the attempt themselves.
fn queue_refresh(
    lines: &mut Vec<String>,
    actuator: &ActuatorHandle,
    trigger: Trigger,
    now_ms: u64,
) {
    let job = plan::refresh_job(
        trigger,
        actuator.timings(),
        actuator.epoch.current(),
        now_ms,
    );
    submit_or_report(lines, actuator, job, "refresh dropped");
}

/// Submits a job, journaling the drop — a lost click must never be silent.
/// A full queue is back-pressure that clears itself; a closed channel means the
/// executor task is gone and only relaunching the app brings it back, so the
/// two causes get different lines.
fn submit_or_report(
    lines: &mut Vec<String>,
    actuator: &ActuatorHandle,
    job: plan::Job,
    dropped: &str,
) {
    match actuator.submit(job) {
        Ok(()) => {}
        Err(SubmitError::QueueFull) => lines.push(format!(">> actuator queue full — {dropped}")),
        Err(SubmitError::ExecutorGone) => {
            lines.push(format!(
                ">> the actuator is gone, restart the app — {dropped}"
            ));
        }
    }
}

/// Recovery only ever fires on recovery-enabled (live) sessions, but the mode
/// gate stays — a job must never be queued without an input backend.
fn submit_recovery(
    lines: &mut Vec<String>,
    controller: &Controller,
    actuator: &ActuatorHandle,
    recovery: &Recovery,
    now_ms: u64,
) {
    match recovery {
        Recovery::ConfirmRefresh => {
            lines.push(">> watchdog: no shop after refresh — re-clicking confirm".to_owned());
            submit_confirm_retry(lines, actuator, plan::CONFIRM_REFRESH, now_ms);
        }
        Recovery::ConfirmBuy => {
            lines.push(">> watchdog: no purchase echo — re-clicking confirm".to_owned());
            submit_confirm_retry(lines, actuator, plan::CONFIRM_BUY, now_ms);
        }
        Recovery::Refresh => {
            lines.push(">> watchdog: re-issuing the refresh".to_owned());
            if let Some(trigger) = active_trigger(actuator, Some(Trigger::Recovery)) {
                queue_refresh(lines, actuator, trigger, now_ms);
            }
        }
        Recovery::Buy { targets } => {
            lines.push(">> watchdog: re-issuing buys".to_owned());
            submit_buys(
                lines,
                controller,
                actuator,
                Some(Trigger::Recovery),
                targets,
                now_ms,
            );
        }
    }
}

fn submit_confirm_retry(
    lines: &mut Vec<String>,
    actuator: &ActuatorHandle,
    zone: plan::Zone,
    now_ms: u64,
) {
    if active_trigger(actuator, Some(Trigger::Recovery)).is_none() {
        return;
    }
    let job = plan::confirm_retry_job(zone, actuator.timings(), actuator.epoch.current(), now_ms);
    submit_or_report(lines, actuator, job, "confirm re-click dropped");
}

/// A buy decision: one job clicking every trackable target. Only `id: Some`
/// targets are clicked — the purchase echo could never confirm the others,
/// so a click there would spend gold the checklist cannot account for.
fn submit_buys(
    lines: &mut Vec<String>,
    controller: &Controller,
    actuator: &ActuatorHandle,
    trigger: Option<Trigger>,
    targets: &[BuyTarget],
    now_ms: u64,
) {
    // Not Paused means the domain decided nothing here is buyable (dead
    // stock — the batch keeps hunting): nothing may be clicked.
    if controller.status() != Status::Paused {
        return;
    }
    let Some(trigger) = active_trigger(actuator, trigger) else {
        return;
    };
    // 1-based display slot to 0-based click row, enforced by the type system:
    // `Row` exists only via `Slot::row`. The version taking raw slot numbers
    // compiled, and clicked the wrong item's Buy button with the player's gold
    // behind it. A loop, not `filter_map`, so an out-of-range slot is named
    // rather than silently dropped.
    let mut rows: Vec<plan::Row> = Vec::with_capacity(targets.len());
    for target in targets.iter().filter(|target| target.id.is_some()) {
        let slot = target.slot;
        match plan::Slot::new(slot).row() {
            Some(row) => rows.push(row),
            None => lines.push(format!(
                ">> actuator: slot {slot} is outside the six clickable rows — not clicked"
            )),
        }
    }
    if rows.is_empty() {
        // Normal buys go quiet here, but a watchdog re-issue just announced
        // itself and must not end in silence.
        if trigger == Trigger::Recovery {
            lines.push(">> watchdog: no clickable slot for the outstanding buys".to_owned());
        }
        return;
    }
    for row in &rows {
        let slot = row.slot().get();
        lines.push(if actuator.mode == Mode::Live {
            format!(">> → buying slot {slot}")
        } else {
            format!(">> → buy slot {slot} planned (dry-run)")
        });
    }
    let job = plan::buy_job(
        trigger,
        actuator.timings(),
        actuator.epoch.current(),
        &rows,
        now_ms,
    );
    submit_or_report(lines, actuator, job, "buys dropped");
}

/// Details of the matched targets, straight from the snapshot that raised
/// them (the controller stored it before emitting).
fn render_match(lines: &mut Vec<String>, targets: &[BuyTarget], controller: &Controller) {
    let clickable = targets.iter().filter(|target| target.id.is_some()).count();
    // Matched + still Paused means the loop waits on purchases; Buy with the
    // loop not Paused is dead stock (nothing on it is buyable).
    let hint = if controller.status() != Status::Paused {
        ""
    } else if clickable == 0 {
        // Buyable in game but untrackable (id omitted): no purchase echo
        // can clear them, only a fresh shop unpauses.
        ": buy in game, then refresh"
    } else if clickable < targets.len() {
        // Some matches won't be clicked (untrackable, dead stock):
        // auto-resume only waits on the tracked ones.
        ": buy in game — some items aren't tracked, refresh when done"
    } else {
        ": buy in game — resumes automatically"
    };
    // Written straight into the line; a `Vec<String>` to `join(", ")` allocated
    // once per target and once more for the join.
    let mut line = String::from(">> MATCH — slot(s) ");
    for (position, target) in targets.iter().enumerate() {
        if position > 0 {
            line.push_str(", ");
        }
        let _ = write!(line, "{}", target.slot);
    }
    line.push_str(hint);
    lines.push(line);
    let Some(snapshot) = controller.last_snapshot() else {
        return;
    };
    // Tracked targets resolve by identity; only the untracked keep the
    // slot-number fallback (non-injective on a degraded shop).
    for (index, item) in snapshot.slots.iter().enumerate() {
        let shown = targets.iter().any(|target| match target.id {
            Some(id) => item.id == Some(id),
            None => item.effective_slot(index) == target.slot,
        });
        if shown {
            lines.push(format!("   {}", format_item(item, index)));
        }
    }
}

#[cfg(test)]
mod tests;
