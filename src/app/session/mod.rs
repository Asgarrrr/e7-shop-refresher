//! The session loop and its handlers: own the controller, apply its
//! decisions to the capture gate, and echo every outcome to the player.

use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::actuator::plan::{self, Trigger};
use crate::actuator::{ActuatorHandle, Mode};
use crate::domain::control::{Action, BuyTarget, Controller, Event, Recovery, Status};
use crate::journal::EventLog;
use crate::render::{describe, format_item, refusal, render_shop, status_label};
use crate::uplink::UplinkEvent;
use crate::uplink::protocol::{PurchaseNotice, ServerMessage};
use crate::watch::WatchGate;

use super::Command;

/// Owns the controller for the session: multiplexes player commands, server
/// messages, a 1 s tick (time limits), capture failures, and Ctrl+C.
///
/// The mutex guard is only ever held across synchronous calls, never an
/// `.await`. The wall clock is read here so the domain stays pure.
///
/// Returns the fatal failure, if one ended the session: the caller turns it
/// into an error outcome (banner + exit code), the loop only reports it. The
/// message is self-describing (`capture: …`, `uplink task panicked`).
pub(super) async fn session_loop(
    controller: &Mutex<Controller>,
    gate: &WatchGate,
    journal: &EventLog,
    actuator: &ActuatorHandle,
    mut commands: mpsc::Receiver<Command>,
    mut messages: mpsc::Receiver<UplinkEvent>,
    mut fatal_errors: mpsc::Receiver<String>,
) -> Option<String> {
    let now_ms = || journal.now_ms();
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    // Stdin closing (EOF) is not a shutdown: the branch is disabled instead of
    // letting the drained channel spin the loop. Same for the fatal channel
    // draining once every supervisor's sender is dropped.
    let mut commands_open = true;
    let mut fatal_open = true;
    let mut fatal_failure = None;
    let mut player_exit = false;
    loop {
        tokio::select! {
            command = commands.recv(), if commands_open => match command {
                Some(command) => on_command(controller, gate, journal, actuator, command, now_ms()),
                None => commands_open = false,
            },
            message = messages.recv() => match message {
                Some(UplinkEvent::Message(message)) => {
                    on_message(controller, gate, journal, actuator, message, now_ms());
                }
                // An armed watch with a dead link looks exactly like a closed
                // shop: without these lines the player cannot tell them apart.
                // The controller is told too: the watchdog must not escalate
                // over a dead wire.
                Some(UplinkEvent::LinkDown(reason)) => {
                    journal.emit(
                        &[format!(">> server link down: {reason} — retrying, no shop can arrive")],
                    );
                    dispatch(controller, gate, journal, actuator, Event::LinkDown, now_ms());
                }
                Some(UplinkEvent::LinkUp) => {
                    journal.emit(&[">> server link restored".to_owned()]);
                    let now = now_ms();
                    dispatch(controller, gate, journal, actuator, Event::LinkUp { now_ms: now }, now);
                }
                None => break, // uplink gone.
            },
            error = fatal_errors.recv(), if fatal_open => match error {
                Some(error) => {
                    // Break immediately: the channel cascade can take tens of
                    // seconds to reach this loop, during which the window
                    // would keep claiming a healthy watch.
                    journal.emit(&[format!(">> session aborted — {error}")]);
                    fatal_failure = Some(error);
                    break;
                }
                None => fatal_open = false,
            },
            _ = ticker.tick() => {
                let now = now_ms();
                dispatch(controller, gate, journal, actuator, Event::Tick { now_ms: now }, now);
            }
            _ = &mut ctrl_c => {
                journal.emit(&[">> Ctrl+C, stopping".to_owned()]);
                player_exit = true;
                break;
            }
        }
    }
    // The window (GUI build) outlives the loop: leave an honest state behind
    // — controller stopped, gate (and thus capture) off, a journal line
    // saying why. Only Ctrl+C is the player's own stop; a pipeline death is
    // a shutdown. The domain ignores both for a never-armed controller.
    let teardown = if player_exit {
        Event::Stop
    } else {
        Event::Shutdown
    };
    dispatch(controller, gate, journal, actuator, teardown, now_ms());
    fatal_failure
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
    let mut ctrl = controller.lock().expect("controller mutex poisoned");
    let event = match command {
        Command::Start => Event::Start { now_ms },
        Command::Stop => Event::Stop,
        Command::ActuatorFailed => Event::ActuatorFailed,
        Command::Toggle => match ctrl.status() {
            Status::Watching | Status::Paused => Event::Stop,
            Status::Idle | Status::Stopped(_) => Event::Start { now_ms },
        },
        // Retunes echo their own confirmation: the transition logic below
        // only reads status changes, which these never cause. Acceptance is
        // read from the domain's explicit verdict — the absence of an
        // `Action::Refused`, not the emptiness of the list — so the
        // confirmation survives an accepted retune that ever emits an action.
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
        ServerMessage::Ack | ServerMessage::Unknown => {}
        ServerMessage::Shop(snapshot) => {
            // The full item dump stays console-only: the GUI table shows the
            // same snapshot; the journal only carries the decisions.
            render_shop(&snapshot);
            // Every shop message bumps, duplicates included: a re-send means
            // the player touched the game, so aborting in-flight clicks is
            // the safe reading.
            actuator.epoch.bump();
            let mut ctrl = controller.lock().expect("controller mutex poisoned");
            // Read before handling: an advised refresh counts itself into
            // `refreshes`, so after `handle` the very first shop would
            // already look refreshed.
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

/// One lock for the whole purchase: the bought line is rendered against the
/// same state the event is applied to, and comes first so the auto-resume
/// refresh advice reads in causal order.
fn handle_purchase(
    controller: &Mutex<Controller>,
    gate: &WatchGate,
    actuator: &ActuatorHandle,
    notice: &PurchaseNotice,
    now_ms: u64,
) -> Vec<String> {
    let mut ctrl = controller.lock().expect("controller mutex poisoned");
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

/// An omitted-id notice never resolves a name: `catalog_id()` is never
/// `Some(0)`.
fn purchase_line(controller: &Controller, notice: &PurchaseNotice) -> String {
    let name = controller.last_snapshot().and_then(|snapshot| {
        snapshot
            .slots
            .iter()
            .find(|item| item.catalog_id() == Some(notice.item))
            .and_then(|item| item.name.as_deref())
    });
    let label = match name {
        Some(name) => name.to_owned(),
        None => format!("item {}", notice.item),
    };
    match notice.gold {
        Some(gold) => format!(">> bought: {label} — {gold} gold left"),
        None => format!(">> bought: {label}"),
    }
}

/// Locks, handles, applies; printing happens after the guard is released.
/// No trigger reaches the actuator from here: the callers (ticks, teardown,
/// test setup) are not shop or purchase arrivals.
fn dispatch(
    controller: &Mutex<Controller>,
    gate: &WatchGate,
    journal: &EventLog,
    actuator: &ActuatorHandle,
    event: Event,
    now_ms: u64,
) {
    let mut ctrl = controller.lock().expect("controller mutex poisoned");
    let actions = ctrl.handle(event);
    let lines = apply(&actions, &ctrl, gate, actuator, None, now_ms);
    drop(ctrl);
    journal.emit(&lines);
}

/// Applies the controller's decisions: drives the capture gate, translates
/// refresh/buy decisions into click jobs when the actuator is on, and
/// renders everything into lines. Callers print them once the guard is
/// dropped — console I/O can block or panic (closed stdout) and must not
/// stall or poison the controller the GUI will share.
///
/// `trigger` names the animation the game plays when the actions land; the
/// paths that cannot advise a refresh or a buy today (commands, ticks,
/// teardown) pass `None` — if one ever does, the advice line still renders
/// and no job is queued. The gate follows the status — capture only flows
/// while the session is live, and the off -> on transition retriggers the
/// capture thread's existing resync.
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
            // Journal-only until the actuation wiring lands: the watchdog
            // narrates its rungs even before it can click.
            Action::Recover(recovery) => lines.push(
                match recovery {
                    Recovery::ConfirmRefresh => {
                        ">> watchdog: no shop after refresh — re-clicking confirm"
                    }
                    Recovery::ConfirmBuy => ">> watchdog: no purchase echo — re-clicking confirm",
                    Recovery::Refresh => ">> watchdog: re-issuing the refresh",
                    Recovery::Buy { .. } => ">> watchdog: re-issuing buys",
                }
                .to_owned(),
            ),
            Action::Halt(reason) => lines.push(format!(">> stopped: {}", describe(*reason))),
            Action::Refused(reason) => lines.push(format!(">> refused: {}", refusal(*reason))),
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
    let submitted = actuator.submit(plan::refresh_job(trigger, actuator.epoch.current(), now_ms));
    lines.push(if actuator.mode == Mode::Live {
        ">> → refresh clicked".to_owned()
    } else {
        ">> → refresh planned (dry-run)".to_owned()
    });
    if !submitted {
        lines.push(">> actuator queue full — refresh dropped".to_owned());
    }
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
    let rows: Vec<u8> = targets
        .iter()
        .filter(|target| target.id.is_some())
        .filter_map(|target| plan::row_for_slot(target.slot))
        .collect();
    if rows.is_empty() {
        return;
    }
    for row in &rows {
        lines.push(if actuator.mode == Mode::Live {
            format!(">> → buying slot {}", row + 1)
        } else {
            format!(">> → buy slot {} planned (dry-run)", row + 1)
        });
    }
    let job = plan::buy_job(trigger, actuator.epoch.current(), &rows, now_ms);
    if !actuator.submit(job) {
        lines.push(">> actuator queue full — buys dropped".to_owned());
    }
}

/// Details of the matched targets, straight from the snapshot that raised
/// them (the controller stored it before emitting).
fn render_match(lines: &mut Vec<String>, targets: &[BuyTarget], controller: &Controller) {
    let list: Vec<String> = targets
        .iter()
        .map(|target| target.slot.to_string())
        .collect();
    let clickable = targets.iter().filter(|target| target.id.is_some()).count();
    // Matched + still Paused means the loop waits on purchases; a Buy with
    // the loop not Paused is dead stock (nothing on it is buyable) and any
    // buy advice would be wrong.
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
    lines.push(format!(">> MATCH — slot(s) {}{hint}", list.join(", ")));
    let Some(snapshot) = controller.last_snapshot() else {
        return;
    };
    // Tracked targets resolve by identity; only the untracked keep the
    // slot-number fallback (non-injective on a degraded shop).
    for (index, item) in snapshot.slots.iter().enumerate() {
        let shown = targets.iter().any(|target| match target.id {
            Some(id) => item.catalog_id() == Some(id),
            None => item.effective_slot(index) == target.slot,
        });
        if shown {
            lines.push(format!("   {}", format_item(item, index)));
        }
    }
}

#[cfg(test)]
mod tests;
