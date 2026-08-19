//! The session loop and its handlers: own the controller, apply its
//! decisions to the capture gate, and echo every outcome to the player.

use std::fmt::Write as _;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use crate::actuator::plan::{self, Trigger};
use crate::actuator::{ActuatorHandle, Mode, SubmitError};
use crate::domain::control::{Action, BuyTarget, Controller, Event, Recovery, Status};
use crate::journal::EventLog;
use crate::render::{format_item, refusal_label, render_shop, status_label, stop_reason_label};
use crate::uplink::UplinkEvent;
use crate::uplink::protocol::{PurchaseNotice, ServerMessage};
use crate::watch::{HaltSource, WatchGate};

use super::Command;

/// The session tick period; the domain's time limits are read against this
/// wall clock. [`HEARTBEAT_EVERY_TICKS`] counts these ticks.
const TICK_PERIOD: Duration = Duration::from_secs(1);

/// One heartbeat every 30 ticks (30 s). A healthy pipeline at rest and a dead
/// one are otherwise indistinguishable in the logs: both are silent.
const HEARTBEAT_EVERY_TICKS: u64 = 30;

/// How long the loop waits, after the uplink channel closed with no fatal
/// captured, for a racing supervisor report. A panicking worker drops its
/// sender one scheduling hop before its supervisor sends the fatal, so
/// without this window a crash reads as a clean "session ended".
const FATAL_REPORT_GRACE: Duration = Duration::from_millis(150);

/// The controller's poison-tolerant lock: a panic elsewhere must not turn
/// every later dispatch into a second panic, matching the journal, view,
/// actuator, and stream budget's policy — and the GUI's, which reads this
/// same mutex through `ui::lock_ignoring_poison`.
///
/// `.expect("controller mutex poisoned")` was rejected: it turned a
/// recoverable frame fault into `supervise` reporting "session crashed" and
/// stopping the relay. `Controller::handle` is pure and saturating, so a
/// guard released by an unwinding thread never leaves state half-written.
fn lock(controller: &Mutex<Controller>) -> MutexGuard<'_, Controller> {
    controller.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Why the loop stopped. Exactly one holds when the loop breaks, and the
/// teardown event and fatal-report grace window are both read off it.
enum Exit {
    /// Ctrl+C or the window closing — the player's own stop.
    PlayerStopped,
    /// The uplink's message channel closed; a racing fatal may still arrive.
    UplinkClosed,
    /// A pipeline death, already journaled.
    Fatal(String),
}

/// Owns the controller for the session: multiplexes player commands, server
/// messages, a 1 s tick (time limits), capture failures, the cooperative
/// shutdown signal, and Ctrl+C.
///
/// The mutex guard is only ever held across synchronous calls, never an
/// `.await`. The wall clock is read here so the domain stays pure. The guard
/// is taken through [`lock`], which stays usable after a panic elsewhere.
///
/// Returns the fatal failure, if one ended the session: the caller turns it
/// into an error outcome (banner + exit code). The message is self-describing
/// (`network capture: …`, `uplink task panicked`).
#[expect(
    clippy::too_many_arguments,
    reason = "four borrowed session handles plus the four channels this loop multiplexes; \
              bundling them would hide which halves the loop owns and which it only borrows"
)]
pub(super) async fn session_loop(
    controller: &Mutex<Controller>,
    gate: &WatchGate,
    journal: &EventLog,
    actuator: &ActuatorHandle,
    mut commands: mpsc::Receiver<Command>,
    mut messages: mpsc::Receiver<UplinkEvent>,
    mut fatal_errors: mpsc::Receiver<String>,
    mut shutdown: watch::Receiver<bool>,
) -> Option<String> {
    let now_ms = || journal.now_ms();
    let mut ticker = tokio::time::interval(TICK_PERIOD);
    // Burst would replay every tick missed during a CPU stall back to back,
    // all carrying near-identical `now_ms`: pointless repeated dispatches
    // (each taking and releasing the controller mutex) right after a hiccup.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    // Stdin EOF and the fatal channel draining are not shutdowns: their
    // branches disable instead of spinning the loop on a drained channel.
    let mut commands_open = true;
    let mut fatal_open = true;
    let mut shutdown_open = true;
    let mut ticks: u64 = 0;
    let mut last_shop_ms: Option<u64> = None;
    // An unknown server tag presents like a mute server; this count lets the
    // heartbeat tell the two apart (see `heartbeat`).
    let mut unknown_messages: u64 = 0;
    let exit = loop {
        // Read here, not just in the branch: a window closed during startup
        // sets the signal before the loop starts, when `changed()` never fires.
        if *shutdown.borrow() {
            journal.emit(&[">> shutting down".to_owned()]);
            // The player's own stop, like Ctrl+C.
            break Exit::PlayerStopped;
        }
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
                // shop, so this is journaled; the controller is told too so
                // the watchdog does not escalate over a dead wire.
                Some(UplinkEvent::LinkDown(reason)) => {
                    // `warn`, not `info`: a log narrowed to info would hide
                    // the reason no shop can arrive.
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
            error = fatal_errors.recv(), if fatal_open => match error {
                Some(error) => {
                    // Break immediately: the channel cascade can take tens
                    // of seconds, during which the window would keep
                    // claiming a healthy watch. `error`, not `info`:
                    // README's troubleshooting tells the player to look for
                    // this line, and a narrowed `RUST_LOG` would delete it.
                    journal.emit_at(
                        tracing::Level::ERROR,
                        &[format!(">> session aborted — {error}")],
                    );
                    break Exit::Fatal(error);
                }
                None => fatal_open = false,
            },
            _ = ticker.tick() => {
                let now = now_ms();
                dispatch(controller, gate, journal, actuator, Event::Tick { now_ms: now }, now);
                ticks = ticks.wrapping_add(1);
                if ticks.is_multiple_of(HEARTBEAT_EVERY_TICKS) {
                    heartbeat(controller, gate, now, last_shop_ms, unknown_messages);
                }
            }
            _ = &mut ctrl_c => {
                journal.emit(&[">> Ctrl+C, stopping".to_owned()]);
                break Exit::PlayerStopped;
            }
        }
    };
    // See [`FATAL_REPORT_GRACE`]: if the loop ended on a closed uplink with
    // no fatal captured yet, wait briefly for a racing supervisor report.
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
    // The window (GUI build) outlives the loop: leave an honest state behind
    // — controller stopped, gate (and thus capture) off, a journal line
    // saying why. The domain ignores both teardown events for a never-armed
    // controller.
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

/// One periodic line saying the loop is alive and what it is waiting on.
///
/// Capture blind, server mute, actuator stuck, and an unparsed server
/// dialect are otherwise indistinguishable in a silent log. `since_last_shop_s`
/// is `None` until the first shop; `unknown_messages` separates "server
/// mute" from "server talking, client not understanding" — both present as
/// no shop arriving. Nothing is awaited here, so the controller guard never
/// crosses an await.
fn heartbeat(
    controller: &Mutex<Controller>,
    gate: &WatchGate,
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

/// Translates a player command into a controller event and echoes an outcome:
/// a command is never silent, even when the controller ignores it.
fn on_command(
    controller: &Mutex<Controller>,
    gate: &WatchGate,
    journal: &EventLog,
    actuator: &ActuatorHandle,
    command: Command,
    now_ms: u64,
) {
    let lines = handle_command(controller, gate, actuator, command, now_ms);
    journal.emit(&lines);
}

/// The command logic behind [`on_command`], returning the lines to print
/// (`Toggle` resolves against the current status).
fn handle_command(
    controller: &Mutex<Controller>,
    gate: &WatchGate,
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
        // Retunes echo their own confirmation: the transition logic below
        // only reads status changes, which these never cause. Acceptance is
        // read from the domain's verdict (absence of `Action::Refused`), not
        // list emptiness, so the confirmation survives a retune that also
        // emits an action.
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
        // Timings are not domain state: swap the actuator's shared waits and
        // acknowledge. The next queued job bakes them in.
        Command::SetTimings(timings) => {
            actuator.set_timings(timings);
            return vec![">> click timings updated — applies to the next queued clicks".to_owned()];
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

/// Renders a shop or a purchase and feeds it to the controller; acks and
/// unknown messages are silent.
fn on_message(
    controller: &Mutex<Controller>,
    gate: &WatchGate,
    journal: &EventLog,
    actuator: &ActuatorHandle,
    message: ServerMessage,
    now_ms: u64,
) {
    match message {
        // Neither is silent overall — see `heartbeat`'s unknown-message
        // handling — just nothing player-facing to print here.
        ServerMessage::Ack | ServerMessage::Unknown => {}
        ServerMessage::Shop(snapshot) => {
            // The full item dump stays console-only: the GUI table shows the
            // same snapshot; the journal only carries the decisions.
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
fn handle_purchase(
    controller: &Mutex<Controller>,
    gate: &WatchGate,
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

/// An omitted-id notice never resolves a name: an echo with no id is `None`,
/// and `None == item.id` would also match an item that has no id — hence the
/// explicit `is_some()` guard.
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

/// Locks, handles, applies; printing happens after the guard is released.
/// No trigger reaches the actuator from here: callers (ticks, teardown,
/// test setup) are never shop or purchase arrivals.
fn dispatch(
    controller: &Mutex<Controller>,
    gate: &WatchGate,
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

/// Applies the controller's decisions: drives the capture gate, translates
/// refresh/buy decisions into click jobs when the actuator is on, and
/// renders everything into lines. Callers print after the guard is dropped
/// — console I/O can block or panic on closed stdout and must not stall or
/// poison the controller the GUI shares.
///
/// `trigger` names the animation the game plays when the actions land.
/// Paths that cannot advise a refresh or buy today (commands, ticks,
/// teardown) pass `None`; if one ever does, the advice line still renders
/// and no job is queued. The gate follows the status: capture flows only
/// while the session is live, and off -> on retriggers the capture
/// thread's existing resync.
fn apply(
    actions: &[Action],
    controller: &Controller,
    gate: &WatchGate,
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
    gate.set(matches!(
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

/// A refresh decision: a click job when the actuator is on (the job's
/// pre-wait covers the animation `trigger` names), the advice line
/// otherwise.
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

/// The refresh-job submission core shared by the normal path and the
/// watchdog re-issue: plan at the current epoch, journal a full queue.
/// Callers narrate the attempt themselves.
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
/// `dropped` names what was lost, e.g. "refresh dropped". A full queue is
/// back-pressure that clears itself; a closed channel means the executor
/// task is gone — no retry, re-arm, or patience brings it back, only
/// relaunching the app — so the two causes get different lines.
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

/// A watchdog retry: journal the rung, then queue its click job. Recovery
/// only ever fires on recovery-enabled (live) sessions, but the mode gate
/// stays — a job must never be queued without an input backend.
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
    // The crossing between the shop's 1-based display slot and the
    // actuator's 0-based click row is enforced by the type system: `Row`
    // exists only via `Slot::row`, so `&rows` cannot be a list of raw slot
    // numbers. That used to compile and clicked the wrong item's Buy button
    // with the player's gold behind it.
    //
    // The loop (not `filter_map`) is what lets an out-of-range slot be
    // refused and named individually — `filter_map` would silently drop it,
    // same as the old `row <= MAX_ROW` filter inside `buy_job`. One good
    // slot beside one out-of-range slot must not lose the latter.
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
        // Normal buys go quiet here (untrackable matches are advice-only),
        // but a watchdog re-issue just announced itself and must not end
        // in silence.
        if trigger == Trigger::Recovery {
            lines.push(">> watchdog: no clickable slot for the outstanding buys".to_owned());
        }
        return;
    }
    for row in &rows {
        // `Row::slot` reverses the conversion above; this is the one place
        // that owns it.
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
    // Written straight into the line: a `Vec<String>` of slot numbers only to
    // `join(", ")` allocated once per target and once more for the join.
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
