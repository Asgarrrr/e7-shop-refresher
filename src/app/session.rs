//! The session loop and its handlers: own the controller, apply its
//! decisions to the capture gate, and echo every outcome to the player.

use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::actuator::plan::{self, Trigger};
use crate::actuator::{ActuatorHandle, Mode};
use crate::domain::control::{Action, BuyTarget, Controller, Event, Status};
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
                Some(UplinkEvent::LinkDown(reason)) => journal.emit(
                    &[format!(">> server link down: {reason} — retrying, no shop can arrive")],
                ),
                Some(UplinkEvent::LinkUp) => {
                    journal.emit(&[">> server link restored".to_owned()]);
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

/// A refresh decision: a click job when the actuator is on (the job's
/// pre-wait covers the animation `trigger` names), the advice line
/// otherwise.
fn submit_refresh(
    lines: &mut Vec<String>,
    actuator: &ActuatorHandle,
    trigger: Option<Trigger>,
    now_ms: u64,
) {
    let trigger = match (actuator.mode, trigger) {
        (Mode::Off, _) | (_, None) => {
            lines.push(">> → refresh the shop now".to_owned());
            return;
        }
        (_, Some(trigger)) => trigger,
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
    let trigger = match (actuator.mode, trigger) {
        (Mode::Off, _) | (_, None) => return,
        (_, Some(trigger)) => trigger,
    };
    // `slot` is 1-based; anything outside 1..=6 is a degraded shop's
    // fallback number, not a clickable row.
    let rows: Vec<u8> = targets
        .iter()
        .filter(|target| target.id.is_some() && (1..=6).contains(&target.slot))
        .map(|target| target.slot - 1)
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
        // Every match is untrackable (sold out or id omitted): no purchase
        // echo can clear them, only a fresh shop unpauses.
        ": buy in game, then refresh"
    } else if clickable < targets.len() {
        // Some matches are untrackable: auto-resume only waits on the
        // tracked ones and would refresh over the rest.
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
mod tests {
    use super::*;
    use crate::actuator::SnapshotEpoch;
    use crate::domain::control::{Limits, StopReason};
    use crate::domain::filter::Filter;
    use crate::domain::shop::{ItemKind, PurchaseLimit, ShopItem, ShopSnapshot};

    /// Off-mode actuator: decisions keep the advice wording, nothing is
    /// ever submitted.
    fn off() -> ActuatorHandle {
        ActuatorHandle::new(Mode::Off, SnapshotEpoch::default(), mpsc::channel(8).0)
    }

    /// An actuator whose submitted jobs the test inspects.
    fn recording(mode: Mode) -> (ActuatorHandle, mpsc::Receiver<plan::Job>) {
        let (jobs, rx) = mpsc::channel(8);
        (
            ActuatorHandle::new(mode, SnapshotEpoch::default(), jobs),
            rx,
        )
    }

    /// No match for `Filter::matching_default_items()` (kind `Unknown`):
    /// the controller advises a refresh.
    fn dud_shop(id: u32) -> ShopSnapshot {
        ShopSnapshot {
            merchant: None,
            slots: vec![ShopItem {
                id,
                kind: ItemKind::Equipment,
                ..ShopItem::default()
            }],
            refresh: None,
        }
    }

    #[tokio::test]
    async fn session_loop_exit_stops_controller_and_gate() {
        let gate = WatchGate::new(false);
        let journal = EventLog::default();
        let controller = Mutex::new(Controller::new(
            Filter::matching_default_items(),
            Limits::default(),
        ));
        on_command(&controller, &gate, &journal, &off(), Command::Start, 0);
        assert!(gate.is_enabled());

        let (_command_tx, command_rx) = mpsc::channel::<Command>(1);
        let (message_tx, message_rx) = mpsc::channel::<UplinkEvent>(1);
        let (_error_tx, error_rx) = mpsc::channel::<String>(1);
        drop(message_tx); // uplink gone: the loop must exit and tear down.
        let failure = session_loop(
            &controller,
            &gate,
            &journal,
            &off(),
            command_rx,
            message_rx,
            error_rx,
        )
        .await;

        assert_eq!(failure, None);
        // The pipeline died on its own: the player did not stop anything.
        assert_eq!(
            controller.lock().unwrap().status(),
            Status::Stopped(StopReason::SessionEnded)
        );
        assert!(!gate.is_enabled());
    }

    #[test]
    fn stop_while_idle_reports_no_effect() {
        let gate = WatchGate::new(false);
        let controller = Mutex::new(Controller::new(
            Filter::matching_default_items(),
            Limits::default(),
        ));
        let lines = handle_command(&controller, &gate, &off(), Command::Stop, 0);
        assert!(lines.iter().any(|line| line.contains("no effect")));
        assert!(!lines.iter().any(|line| line.contains("player stopped")));
        assert_eq!(controller.lock().unwrap().status(), Status::Idle);
    }

    #[tokio::test]
    async fn uplink_outage_and_recovery_reach_the_journal() {
        let gate = WatchGate::new(false);
        let journal = EventLog::default();
        let controller = Mutex::new(Controller::new(
            Filter::matching_default_items(),
            Limits::default(),
        ));

        let (_command_tx, command_rx) = mpsc::channel::<Command>(1);
        let (message_tx, message_rx) = mpsc::channel::<UplinkEvent>(4);
        let (_error_tx, error_rx) = mpsc::channel::<String>(1);
        message_tx
            .send(UplinkEvent::LinkDown("connection refused".to_owned()))
            .await
            .unwrap();
        message_tx.send(UplinkEvent::LinkUp).await.unwrap();
        drop(message_tx); // then the loop exits.
        session_loop(
            &controller,
            &gate,
            &journal,
            &off(),
            command_rx,
            message_rx,
            error_rx,
        )
        .await;

        let entries = journal.entries();
        assert!(
            entries
                .iter()
                .any(|line| line.text.contains("server link down: connection refused"))
        );
        assert!(
            entries
                .iter()
                .any(|line| line.text.contains("server link restored"))
        );
    }

    #[tokio::test]
    async fn fatal_failure_reaches_journal_gate_and_caller() {
        let gate = WatchGate::new(false);
        let journal = EventLog::default();
        let controller = Mutex::new(Controller::new(
            Filter::matching_default_items(),
            Limits::default(),
        ));
        on_command(&controller, &gate, &journal, &off(), Command::Start, 0);
        assert!(gate.is_enabled());

        let (_command_tx, command_rx) = mpsc::channel::<Command>(1);
        // Kept alive: the loop must exit through the fatal channel, not a
        // channel cascade.
        let (_message_tx, message_rx) = mpsc::channel::<UplinkEvent>(1);
        let (fatal_tx, fatal_rx) = mpsc::channel::<String>(4);
        fatal_tx
            .send("uplink task panicked".to_owned())
            .await
            .unwrap();

        let failure = session_loop(
            &controller,
            &gate,
            &journal,
            &off(),
            command_rx,
            message_rx,
            fatal_rx,
        )
        .await;

        assert_eq!(failure.as_deref(), Some("uplink task panicked"));
        assert!(
            journal
                .entries()
                .iter()
                .any(|line| line.text.contains("session aborted") && line.text.contains("uplink"))
        );
        assert!(!gate.is_enabled());
    }

    #[tokio::test]
    async fn session_loop_exit_leaves_never_armed_controller_idle() {
        let gate = WatchGate::new(false);
        let journal = EventLog::default();
        let controller = Mutex::new(Controller::new(Filter::default(), Limits::default()));

        let (_command_tx, command_rx) = mpsc::channel::<Command>(1);
        let (message_tx, message_rx) = mpsc::channel::<UplinkEvent>(1);
        let (_error_tx, error_rx) = mpsc::channel::<String>(1);
        drop(message_tx);
        session_loop(
            &controller,
            &gate,
            &journal,
            &off(),
            command_rx,
            message_rx,
            error_rx,
        )
        .await;

        // A session that never ran must not report "player stopped".
        assert_eq!(controller.lock().unwrap().status(), Status::Idle);
        assert!(
            journal
                .entries()
                .iter()
                .all(|line| !line.text.contains("stopped"))
        );
    }

    #[test]
    fn set_filter_while_paused_warns_about_stale_matches() {
        let gate = WatchGate::new(false);
        let controller = Mutex::new(Controller::new(
            Filter::matching_default_items(),
            Limits::default(),
        ));
        handle_command(&controller, &gate, &off(), Command::Start, 0);
        dispatch(
            &controller,
            &gate,
            &EventLog::default(),
            &off(),
            Event::Snapshot {
                snapshot: one_item_shop(),
                now_ms: 1,
            },
            1,
        );
        assert_eq!(controller.lock().unwrap().status(), Status::Paused);

        let filter = Filter {
            names: vec!["ticketrare_name".to_owned()],
            ..Filter::default()
        };
        let lines = handle_command(&controller, &gate, &off(), Command::SetFilter(filter), 2);
        assert!(lines.iter().any(|line| line.contains("still paused")));
    }

    #[test]
    fn start_refused_while_filter_unrestricted() {
        let gate = WatchGate::new(false);
        let controller = Mutex::new(Controller::new(Filter::default(), Limits::default()));
        let lines = handle_command(&controller, &gate, &off(), Command::Start, 0);
        assert!(lines.iter().any(|line| line.contains(">> refused:")));
        assert_eq!(controller.lock().unwrap().status(), Status::Idle);
        assert!(!gate.is_enabled());
        // Toggle resolves to Start and the domain refuses it the same way.
        let lines = handle_command(&controller, &gate, &off(), Command::Toggle, 1);
        assert!(lines.iter().any(|line| line.contains(">> refused:")));
        assert_eq!(controller.lock().unwrap().status(), Status::Idle);
    }

    #[test]
    fn set_filter_unblocks_arming() {
        let gate = WatchGate::new(false);
        let controller = Mutex::new(Controller::new(Filter::default(), Limits::default()));
        let filter = Filter {
            names: vec!["ticketrare_name".to_owned()],
            ..Filter::default()
        };
        let lines = handle_command(
            &controller,
            &gate,
            &off(),
            Command::SetFilter(filter.clone()),
            0,
        );
        assert!(lines.iter().any(|line| line.contains("filter updated")));
        assert_eq!(controller.lock().unwrap().filter(), &filter);
        let lines = handle_command(&controller, &gate, &off(), Command::Start, 1);
        assert!(lines.iter().any(|line| line.contains("watching")));
        assert_eq!(controller.lock().unwrap().status(), Status::Watching);
    }

    #[test]
    fn set_limits_updates_controller() {
        let gate = WatchGate::new(false);
        let controller = Mutex::new(Controller::new(Filter::default(), Limits::default()));
        let limits = Limits {
            max_refreshes: Some(5),
            ..Limits::default()
        };
        let lines = handle_command(
            &controller,
            &gate,
            &off(),
            Command::SetLimits(limits.clone()),
            0,
        );
        assert!(lines.iter().any(|line| line.contains("limits updated")));
        assert_eq!(controller.lock().unwrap().limits(), &limits);
    }

    #[test]
    fn journal_receives_command_lines() {
        let gate = WatchGate::new(false);
        let journal = EventLog::default();
        let controller = Mutex::new(Controller::new(
            Filter::matching_default_items(),
            Limits::default(),
        ));
        on_command(&controller, &gate, &journal, &off(), Command::Start, 1_000);
        let entries = journal.entries();
        assert!(entries.iter().any(|line| line.text.contains("watching")));
    }

    #[test]
    fn gate_follows_controller_status() {
        let gate = WatchGate::new(false);
        let mut ctrl = Controller::new(Filter::matching_default_items(), Limits::default());
        apply(&[], &ctrl, &gate, &off(), None, 0);
        assert!(!gate.is_enabled()); // Idle

        ctrl.handle(Event::Start { now_ms: 0 });
        apply(&[], &ctrl, &gate, &off(), None, 0);
        assert!(gate.is_enabled()); // Watching

        // Default filter matches the default item: Alert -> Paused, gate stays on.
        let snapshot = ShopSnapshot {
            merchant: None,
            slots: vec![ShopItem::default()],
            refresh: None,
        };
        let actions = ctrl.handle(Event::Snapshot {
            snapshot,
            now_ms: 1,
        });
        apply(&actions, &ctrl, &gate, &off(), None, 0);
        assert_eq!(ctrl.status(), Status::Paused);
        assert!(gate.is_enabled());

        let actions = ctrl.handle(Event::Stop);
        apply(&actions, &ctrl, &gate, &off(), None, 0);
        assert!(!gate.is_enabled()); // Stopped
    }

    fn one_item_shop() -> ShopSnapshot {
        ShopSnapshot {
            merchant: None,
            slots: vec![ShopItem::default()],
            refresh: None,
        }
    }

    #[test]
    fn toggle_resolves_against_status() {
        let gate = WatchGate::new(false);
        let journal = EventLog::default();
        let controller = Mutex::new(Controller::new(
            Filter::matching_default_items(),
            Limits::default(),
        ));

        on_command(&controller, &gate, &journal, &off(), Command::Toggle, 0); // Idle -> Start
        assert_eq!(controller.lock().unwrap().status(), Status::Watching);
        assert!(gate.is_enabled());

        on_command(&controller, &gate, &journal, &off(), Command::Toggle, 1); // Watching -> Stop
        assert_eq!(
            controller.lock().unwrap().status(),
            Status::Stopped(StopReason::PlayerStopped)
        );
        assert!(!gate.is_enabled());

        on_command(&controller, &gate, &journal, &off(), Command::Toggle, 2); // Stopped -> Start
        // Default filter matches the default item -> Paused.
        dispatch(
            &controller,
            &gate,
            &journal,
            &off(),
            Event::Snapshot {
                snapshot: one_item_shop(),
                now_ms: 3,
            },
            3,
        );
        on_command(&controller, &gate, &journal, &off(), Command::Toggle, 4); // Paused -> Stop
        assert_eq!(
            controller.lock().unwrap().status(),
            Status::Stopped(StopReason::PlayerStopped)
        );
        assert!(!gate.is_enabled());
    }

    #[test]
    fn purchase_message_auto_resumes_controller() {
        let gate = WatchGate::new(false);
        let journal = EventLog::default();
        let controller = Mutex::new(Controller::new(
            Filter::matching_default_items(),
            Limits::default(),
        ));
        on_command(&controller, &gate, &journal, &off(), Command::Start, 0);
        let mut snapshot = one_item_shop();
        snapshot.slots[0].id = 42;
        // Default filter matches the default item -> Paused, checklist [42].
        on_message(
            &controller,
            &gate,
            &journal,
            &off(),
            ServerMessage::Shop(snapshot),
            1,
        );
        assert_eq!(controller.lock().unwrap().status(), Status::Paused);

        let notice = PurchaseNotice {
            item: 42,
            gold: Some(100),
        };
        on_message(
            &controller,
            &gate,
            &journal,
            &off(),
            ServerMessage::Purchase(notice),
            2,
        );
        assert_eq!(controller.lock().unwrap().status(), Status::Watching);
        assert!(gate.is_enabled());
    }

    /// A controller that stored a one-item shop whose slot carries id 42 and
    /// a name.
    fn controller_with_named_item() -> Controller {
        let mut controller = Controller::new(Filter::default(), Limits::default());
        let mut snapshot = one_item_shop();
        snapshot.slots[0].id = 42;
        snapshot.slots[0].name = Some("Reforged Sword".to_owned());
        controller.handle(Event::Snapshot {
            snapshot,
            now_ms: 0,
        });
        controller
    }

    #[test]
    fn purchase_line_names_item_from_snapshot() {
        let notice = PurchaseNotice {
            item: 42,
            gold: Some(250_000),
        };
        assert_eq!(
            purchase_line(&controller_with_named_item(), &notice),
            ">> bought: Reforged Sword — 250000 gold left"
        );
    }

    #[test]
    fn purchase_line_falls_back_to_id_when_name_unknown() {
        let controller = Controller::new(Filter::default(), Limits::default());
        let notice = PurchaseNotice {
            item: 7,
            gold: Some(100),
        };
        assert_eq!(
            purchase_line(&controller, &notice),
            ">> bought: item 7 — 100 gold left"
        );
    }

    #[test]
    fn purchase_line_omits_missing_gold() {
        let notice = PurchaseNotice {
            item: 42,
            gold: None,
        };
        assert_eq!(
            purchase_line(&controller_with_named_item(), &notice),
            ">> bought: Reforged Sword"
        );
    }

    #[test]
    fn alert_hint_warns_when_some_matches_untracked() {
        let gate = WatchGate::new(false);
        let mut ctrl = Controller::new(Filter::matching_default_items(), Limits::default());
        ctrl.handle(Event::Start { now_ms: 0 });
        // Two matches, only one trackable: the first slot keeps the id-0
        // sentinel, so auto-resume would refresh over it.
        let snapshot = ShopSnapshot {
            merchant: None,
            slots: vec![
                ShopItem::default(),
                ShopItem {
                    id: 42,
                    ..ShopItem::default()
                },
            ],
            refresh: None,
        };
        let actions = ctrl.handle(Event::Snapshot {
            snapshot,
            now_ms: 1,
        });
        let lines = apply(&actions, &ctrl, &gate, &off(), None, 0);
        assert!(lines.iter().any(|line| line.contains("aren't tracked")));
    }

    #[test]
    fn paused_label_reflects_manual_flow_when_checklist_empty() {
        let gate = WatchGate::new(false);
        let journal = EventLog::default();
        let controller = Mutex::new(Controller::new(
            Filter::matching_default_items(),
            Limits::default(),
        ));
        on_command(&controller, &gate, &journal, &off(), Command::Start, 0);
        // Paused on an untrackable (id-0) match: a no-effect command's echo
        // must advise manual resume, not a phantom auto-resume.
        dispatch(
            &controller,
            &gate,
            &journal,
            &off(),
            Event::Snapshot {
                snapshot: one_item_shop(),
                now_ms: 1,
            },
            1,
        );
        let lines = handle_command(&controller, &gate, &off(), Command::Start, 2);
        assert!(lines.iter().any(|line| line.contains("buy, then refresh")));
        assert!(!lines.iter().any(|line| line.contains("auto-resumes")));
    }

    #[test]
    fn start_hint_printed_only_when_shop_stored() {
        let gate = WatchGate::new(false);
        let controller = Mutex::new(Controller::new(
            Filter::matching_default_items(),
            Limits::default(),
        ));
        // Nothing stored yet: plain watching line, no hint.
        let lines = handle_command(&controller, &gate, &off(), Command::Start, 0);
        assert!(lines.iter().any(|line| line.contains("watching")));
        assert!(!lines.iter().any(|line| line.contains("not replayed")));

        // Stop, receive a shop (stored, not evaluated), restart: hint appears.
        handle_command(&controller, &gate, &off(), Command::Stop, 1);
        dispatch(
            &controller,
            &gate,
            &EventLog::default(),
            &off(),
            Event::Snapshot {
                snapshot: one_item_shop(),
                now_ms: 2,
            },
            2,
        );
        let lines = handle_command(&controller, &gate, &off(), Command::Start, 3);
        assert!(lines.iter().any(|line| line.contains("not replayed")));
    }

    #[test]
    fn ignored_command_leaves_state_and_gate_unchanged() {
        let gate = WatchGate::new(false);
        let journal = EventLog::default();
        let controller = Mutex::new(Controller::new(
            Filter::matching_default_items(),
            Limits::default(),
        ));
        on_command(&controller, &gate, &journal, &off(), Command::Start, 0);
        dispatch(
            &controller,
            &gate,
            &journal,
            &off(),
            Event::Snapshot {
                snapshot: one_item_shop(),
                now_ms: 1,
            },
            1,
        );
        assert_eq!(controller.lock().unwrap().status(), Status::Paused);

        // `start` mid-session is ignored by the controller: still Paused,
        // gate still on, counters untouched.
        on_command(&controller, &gate, &journal, &off(), Command::Start, 2);
        assert_eq!(controller.lock().unwrap().status(), Status::Paused);
        assert_eq!(controller.lock().unwrap().progress().matches_found, 1);
        assert!(gate.is_enabled());
    }

    /// An armed controller matching `ShopItem::default()`.
    fn armed() -> Mutex<Controller> {
        let controller = Mutex::new(Controller::new(
            Filter::matching_default_items(),
            Limits::default(),
        ));
        controller
            .lock()
            .unwrap()
            .handle(Event::Start { now_ms: 0 });
        controller
    }

    #[test]
    fn shop_jobs_carry_open_then_refresh_pre_waits_and_epochs() {
        let gate = WatchGate::new(true);
        let journal = EventLog::default();
        let (actuator, mut jobs) = recording(Mode::Live);
        let controller = armed();

        on_message(
            &controller,
            &gate,
            &journal,
            &actuator,
            ServerMessage::Shop(dud_shop(10)),
            1,
        );
        let first = jobs.try_recv().expect("first refresh job");
        assert_eq!(first.steps[0].wait_ms, 1_180); // shop-open animation
        assert_eq!(first.epoch, 1);

        on_message(
            &controller,
            &gate,
            &journal,
            &actuator,
            ServerMessage::Shop(dud_shop(20)),
            2,
        );
        let second = jobs.try_recv().expect("second refresh job");
        assert_eq!(second.steps[0].wait_ms, 780); // refresh animation
        assert_eq!(second.epoch, 2);
        assert!(
            journal
                .entries()
                .iter()
                .any(|line| line.text.contains("refresh clicked"))
        );
    }

    #[test]
    fn purchase_resume_job_waits_for_the_post_buy_animation() {
        let gate = WatchGate::new(true);
        let journal = EventLog::default();
        let (actuator, mut jobs) = recording(Mode::Live);
        let controller = armed();
        let mut snapshot = one_item_shop();
        snapshot.slots[0].id = 42;
        on_message(
            &controller,
            &gate,
            &journal,
            &actuator,
            ServerMessage::Shop(snapshot),
            1,
        );
        jobs.try_recv().expect("buy job");

        let notice = PurchaseNotice {
            item: 42,
            gold: None,
        };
        on_message(
            &controller,
            &gate,
            &journal,
            &actuator,
            ServerMessage::Purchase(notice),
            2,
        );
        let resume = jobs.try_recv().expect("auto-resume refresh job");
        assert_eq!(resume.steps[0].wait_ms, 400);
        // A purchase never bumps the epoch: the shop is unchanged and the
        // job must not be treated as stale.
        assert_eq!(resume.epoch, 1);
    }

    #[test]
    fn buy_job_clicks_only_trackable_targets() {
        let gate = WatchGate::new(true);
        let journal = EventLog::default();
        let (actuator, mut jobs) = recording(Mode::Live);
        let controller = armed();
        // Two matches: only the id-carrying one may be clicked.
        let snapshot = ShopSnapshot {
            merchant: None,
            slots: vec![
                ShopItem {
                    id: 42,
                    ..ShopItem::default()
                },
                ShopItem::default(), // id 0: untrackable
            ],
            refresh: None,
        };
        on_message(
            &controller,
            &gate,
            &journal,
            &actuator,
            ServerMessage::Shop(snapshot),
            1,
        );
        let job = jobs.try_recv().expect("buy job");
        // Scroll-to-top + one buy/confirm pair — nothing for the id-0 slot.
        assert_eq!(job.steps.len(), 3);
        let entries = journal.entries();
        assert!(
            entries
                .iter()
                .any(|line| line.text.contains("buying slot 1"))
        );
        assert!(
            !entries
                .iter()
                .any(|line| line.text.contains("buying slot 2"))
        );
    }

    #[test]
    fn off_actuator_keeps_advice_and_submits_nothing() {
        let gate = WatchGate::new(true);
        let journal = EventLog::default();
        let (actuator, mut jobs) = recording(Mode::Off);
        let controller = armed();
        on_message(
            &controller,
            &gate,
            &journal,
            &actuator,
            ServerMessage::Shop(dud_shop(10)),
            1,
        );
        assert!(
            journal
                .entries()
                .iter()
                .any(|line| line.text.contains("refresh the shop now"))
        );
        assert!(jobs.try_recv().is_err());
    }

    #[test]
    fn dry_run_wording_marks_planned_actions() {
        let gate = WatchGate::new(true);
        let journal = EventLog::default();
        let (actuator, mut jobs) = recording(Mode::DryRun);
        let controller = armed();
        on_message(
            &controller,
            &gate,
            &journal,
            &actuator,
            ServerMessage::Shop(dud_shop(10)),
            1,
        );
        let mut hit = one_item_shop();
        hit.slots[0].id = 42;
        on_message(
            &controller,
            &gate,
            &journal,
            &actuator,
            ServerMessage::Shop(hit),
            2,
        );
        let entries = journal.entries();
        assert!(
            entries
                .iter()
                .any(|line| line.text.contains("refresh planned (dry-run)"))
        );
        assert!(
            entries
                .iter()
                .any(|line| line.text.contains("buy slot 1 planned (dry-run)"))
        );
        // Dry-run still submits: the executor journals the screen coords.
        assert!(jobs.try_recv().is_ok());
        assert!(jobs.try_recv().is_ok());
    }

    #[test]
    fn dead_stock_match_keeps_refreshing_without_clicks() {
        let gate = WatchGate::new(true);
        let journal = EventLog::default();
        let (actuator, mut jobs) = recording(Mode::Live);
        let filter = Filter {
            include_sold_out: true,
            ..Filter::matching_default_items()
        };
        let controller = Mutex::new(Controller::new(filter, Limits::default()));
        controller
            .lock()
            .unwrap()
            .handle(Event::Start { now_ms: 0 });
        // The only match is sold out: shown, never clicked, hunted over.
        let snapshot = ShopSnapshot {
            merchant: None,
            slots: vec![ShopItem {
                id: 42,
                limit: Some(PurchaseLimit {
                    remaining: 0,
                    total: 1,
                }),
                ..ShopItem::default()
            }],
            refresh: None,
        };
        on_message(
            &controller,
            &gate,
            &journal,
            &actuator,
            ServerMessage::Shop(snapshot),
            1,
        );
        let job = jobs.try_recv().expect("refresh job");
        assert_eq!(job.steps.len(), 2); // refresh + confirm — no buy clicks
        assert!(jobs.try_recv().is_err());
        let entries = journal.entries();
        assert!(entries.iter().any(|line| line.text.contains("MATCH")));
        assert!(!entries.iter().any(|line| line.text.contains("buying slot")));
    }

    #[test]
    fn full_job_queue_journals_the_drop() {
        let gate = WatchGate::new(true);
        let journal = EventLog::default();
        let (job_tx, _job_rx) = mpsc::channel(1);
        job_tx
            .try_send(plan::refresh_job(Trigger::Refreshed, 0, 0))
            .expect("fills the queue");
        let actuator = ActuatorHandle::new(Mode::Live, SnapshotEpoch::default(), job_tx);
        let controller = armed();
        on_message(
            &controller,
            &gate,
            &journal,
            &actuator,
            ServerMessage::Shop(dud_shop(10)),
            1,
        );
        assert!(
            journal
                .entries()
                .iter()
                .any(|line| line.text.contains("queue full"))
        );
    }
}
