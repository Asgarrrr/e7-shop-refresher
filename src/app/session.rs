//! The session loop and its handlers: own the controller, apply its
//! decisions to the capture gate, and echo every outcome to the player.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::domain::control::{Action, Controller, Event, Status};
use crate::journal::EventLog;
use crate::render::{describe, format_item, render_shop, status_label};
use crate::uplink::protocol::{PurchaseNotice, ServerMessage};
use crate::watch::WatchGate;

use super::Command;

/// Owns the controller for the session: multiplexes player commands, server
/// messages, a 1 s tick (time limits), and Ctrl+C.
///
/// The mutex guard is only ever held across synchronous calls, never an
/// `.await`. The wall clock is read here so the domain stays pure.
pub(super) async fn session_loop(
    controller: &Mutex<Controller>,
    gate: &WatchGate,
    journal: &EventLog,
    mut commands: mpsc::Receiver<Command>,
    mut messages: mpsc::Receiver<ServerMessage>,
) {
    let base = Instant::now();
    let now_ms = || u64::try_from(base.elapsed().as_millis()).unwrap_or(u64::MAX);
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    // Stdin closing (EOF) is not a shutdown: the branch is disabled instead of
    // letting the drained channel spin the loop.
    let mut commands_open = true;
    loop {
        tokio::select! {
            command = commands.recv(), if commands_open => match command {
                Some(command) => on_command(controller, gate, journal, command, now_ms()),
                None => commands_open = false,
            },
            message = messages.recv() => match message {
                Some(message) => on_message(controller, gate, journal, message, now_ms()),
                None => break, // uplink gone.
            },
            _ = ticker.tick() => dispatch(controller, gate, journal, Event::Tick { now_ms: now_ms() }),
            _ = &mut ctrl_c => {
                emit(journal, &[">> Ctrl+C, stopping".to_owned()]);
                break;
            }
        }
    }
    // The window (GUI build) outlives the loop: leave an honest state behind
    // — controller stopped, gate (and thus capture) off, a journal line
    // saying why. Idempotent when the controller is already stopped.
    dispatch(controller, gate, journal, Event::Stop);
}

/// Single sink for player-facing lines: the journal and the console stay in
/// step by construction — never print session lines around it.
fn emit(journal: &EventLog, lines: &[String]) {
    journal.push(lines);
    for line in lines {
        println!("{line}");
    }
}

/// Translates a player command into a controller event and echoes an outcome:
/// a command is never silent, even when the controller ignores it.
fn on_command(
    controller: &Mutex<Controller>,
    gate: &WatchGate,
    journal: &EventLog,
    command: Command,
    now_ms: u64,
) {
    let lines = handle_command(controller, gate, command, now_ms);
    emit(journal, &lines);
}

/// The command logic behind [`on_command`], returning the lines to print
/// (`Toggle` resolves against the current status).
fn handle_command(
    controller: &Mutex<Controller>,
    gate: &WatchGate,
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
        // only reads status changes, which these never cause.
        Command::SetFilter(filter) => {
            // The domain rejects unrestricted swaps; do not claim "updated".
            if filter.is_unrestricted() {
                return vec![">> filter ignored — at least one criterion required".to_owned()];
            }
            let paused = ctrl.status() == Status::Paused;
            let actions = ctrl.handle(Event::FilterChanged(filter));
            let mut lines = apply(&actions, &ctrl, gate);
            lines.push(">> filter updated — applies from the next shop".to_owned());
            if paused {
                // The pending pause still waits on the *previous* filter's
                // matches; the new criteria cannot retroactively clear it.
                lines.push(
                    ">> still paused on earlier matches — buy them or wait for a new shop"
                        .to_owned(),
                );
            }
            return lines;
        }
        Command::SetLimits(limits) => {
            let actions = ctrl.handle(Event::LimitsChanged(limits));
            let mut lines = apply(&actions, &ctrl, gate);
            lines.push(">> limits updated — checked before the next refresh".to_owned());
            return lines;
        }
    };
    // Arming an unrestricted filter would advise buying everything: refuse
    // with advice instead of dispatching (the console build already fails
    // fast at startup; this guards the GUI build, where the filter is edited
    // live).
    if matches!(event, Event::Start { .. }) && ctrl.filter().is_unrestricted() {
        return vec![">> no filter criteria set — define a filter first".to_owned()];
    }
    let before = ctrl.status();
    let actions = ctrl.handle(event);
    let mut lines = apply(&actions, &ctrl, gate);
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
    message: ServerMessage,
    now_ms: u64,
) {
    match message {
        ServerMessage::Ack | ServerMessage::Unknown => {}
        ServerMessage::Shop(snapshot) => {
            // The full item dump stays console-only: the GUI table shows the
            // same snapshot; the journal only carries the decisions.
            render_shop(&snapshot);
            dispatch(
                controller,
                gate,
                journal,
                Event::Snapshot { snapshot, now_ms },
            );
        }
        ServerMessage::Purchase(notice) => {
            let lines = handle_purchase(controller, gate, &notice, now_ms);
            emit(journal, &lines);
        }
    }
}

/// One lock for the whole purchase: the bought line is rendered against the
/// same state the event is applied to, and comes first so the auto-resume
/// refresh advice reads in causal order.
fn handle_purchase(
    controller: &Mutex<Controller>,
    gate: &WatchGate,
    notice: &PurchaseNotice,
    now_ms: u64,
) -> Vec<String> {
    let mut ctrl = controller.lock().expect("controller mutex poisoned");
    let mut lines = vec![purchase_line(&ctrl, notice)];
    let actions = ctrl.handle(Event::Purchase {
        item: notice.item,
        now_ms,
    });
    lines.extend(apply(&actions, &ctrl, gate));
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
fn dispatch(controller: &Mutex<Controller>, gate: &WatchGate, journal: &EventLog, event: Event) {
    let mut ctrl = controller.lock().expect("controller mutex poisoned");
    let actions = ctrl.handle(event);
    let lines = apply(&actions, &ctrl, gate);
    drop(ctrl);
    emit(journal, &lines);
}

/// Applies the controller's decisions: drives the capture gate and renders
/// the actions into lines. Callers print them once the guard is dropped —
/// console I/O can block or panic (closed stdout) and must not stall or
/// poison the controller the GUI will share.
///
/// No actuator yet (tranche 5): `Refresh` is advice printed to the player,
/// not an action taken. The gate follows the status — capture only flows
/// while the session is live, and the off -> on transition retriggers the
/// capture thread's existing resync.
fn apply(actions: &[Action], controller: &Controller, gate: &WatchGate) -> Vec<String> {
    let mut lines = Vec::new();
    for action in actions {
        match action {
            Action::Refresh => lines.push(">> → refresh the shop now".to_owned()),
            Action::Alert { slots } => render_alert(&mut lines, slots, controller),
            Action::Halt(reason) => lines.push(format!(">> stopped: {}", describe(*reason))),
        }
    }
    gate.set(matches!(
        controller.status(),
        Status::Watching | Status::Paused
    ));
    lines
}

/// Details of the matched slots, straight from the snapshot that raised the
/// alert (the controller stored it before emitting).
fn render_alert(lines: &mut Vec<String>, slots: &[u8], controller: &Controller) {
    let list: Vec<String> = slots.iter().map(u8::to_string).collect();
    // Alerted + still Paused means the loop waits on purchases; on the
    // max-matches path a Halt follows in the same batch and any buy advice
    // would be dead.
    let hint = if controller.status() != Status::Paused {
        ""
    } else if controller.checklist().is_empty() {
        // Every match is untrackable (sold out or id omitted): no purchase
        // echo can clear them, only a fresh shop unpauses.
        ": buy in game, then refresh"
    } else if controller.checklist().len() < slots.len() {
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
    for (index, item) in snapshot.slots.iter().enumerate() {
        if slots.contains(&item.effective_slot(index)) {
            lines.push(format!("   {}", format_item(item)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::control::{Limits, StopReason};
    use crate::domain::filter::Filter;
    use crate::domain::shop::{ItemKind, ShopItem, ShopSnapshot};

    #[tokio::test]
    async fn session_loop_exit_stops_controller_and_gate() {
        let gate = WatchGate::new(false);
        let journal = EventLog::default();
        let controller = Mutex::new(Controller::new(match_default_filter(), Limits::default()));
        on_command(&controller, &gate, &journal, Command::Start, 0);
        assert!(gate.is_enabled());

        let (_command_tx, command_rx) = mpsc::channel::<Command>(1);
        let (message_tx, message_rx) = mpsc::channel::<ServerMessage>(1);
        drop(message_tx); // uplink gone: the loop must exit and tear down.
        session_loop(&controller, &gate, &journal, command_rx, message_rx).await;

        assert_eq!(
            controller.lock().unwrap().status(),
            Status::Stopped(StopReason::PlayerStopped)
        );
        assert!(!gate.is_enabled());
    }

    #[test]
    fn set_filter_while_paused_warns_about_stale_matches() {
        let gate = WatchGate::new(false);
        let controller = Mutex::new(Controller::new(match_default_filter(), Limits::default()));
        handle_command(&controller, &gate, Command::Start, 0);
        dispatch(
            &controller,
            &gate,
            &EventLog::default(),
            Event::Snapshot {
                snapshot: one_item_shop(),
                now_ms: 1,
            },
        );
        assert_eq!(controller.lock().unwrap().status(), Status::Paused);

        let filter = Filter {
            names: vec!["ticketrare_name".to_owned()],
            ..Filter::default()
        };
        let lines = handle_command(&controller, &gate, Command::SetFilter(filter), 2);
        assert!(lines.iter().any(|line| line.contains("still paused")));
    }

    #[test]
    fn start_refused_while_filter_unrestricted() {
        let gate = WatchGate::new(false);
        let controller = Mutex::new(Controller::new(Filter::default(), Limits::default()));
        let lines = handle_command(&controller, &gate, Command::Start, 0);
        assert!(lines.iter().any(|line| line.contains("define a filter")));
        assert_eq!(controller.lock().unwrap().status(), Status::Idle);
        assert!(!gate.is_enabled());
        // Toggle resolves to Start and hits the same guard.
        let lines = handle_command(&controller, &gate, Command::Toggle, 1);
        assert!(lines.iter().any(|line| line.contains("define a filter")));
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
        let lines = handle_command(&controller, &gate, Command::SetFilter(filter.clone()), 0);
        assert!(lines.iter().any(|line| line.contains("filter updated")));
        assert_eq!(controller.lock().unwrap().filter(), &filter);
        let lines = handle_command(&controller, &gate, Command::Start, 1);
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
        let lines = handle_command(&controller, &gate, Command::SetLimits(limits.clone()), 0);
        assert!(lines.iter().any(|line| line.contains("limits updated")));
        assert_eq!(controller.lock().unwrap().limits(), &limits);
    }

    #[test]
    fn journal_receives_command_lines() {
        let gate = WatchGate::new(false);
        let journal = EventLog::default();
        let controller = Mutex::new(Controller::new(match_default_filter(), Limits::default()));
        on_command(&controller, &gate, &journal, Command::Start, 1_000);
        let entries = journal.entries();
        assert!(entries.iter().any(|line| line.text.contains("watching")));
    }

    #[test]
    fn gate_follows_controller_status() {
        let gate = WatchGate::new(false);
        let mut ctrl = Controller::new(match_default_filter(), Limits::default());
        apply(&[], &ctrl, &gate);
        assert!(!gate.is_enabled()); // Idle

        ctrl.handle(Event::Start { now_ms: 0 });
        apply(&[], &ctrl, &gate);
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
        apply(&actions, &ctrl, &gate);
        assert_eq!(ctrl.status(), Status::Paused);
        assert!(gate.is_enabled());

        let actions = ctrl.handle(Event::Stop);
        apply(&actions, &ctrl, &gate);
        assert!(!gate.is_enabled()); // Stopped
    }

    /// Restricted (passes the arming guard) yet still matching
    /// `ShopItem::default()` (kind `Unknown`).
    fn match_default_filter() -> Filter {
        Filter {
            kinds: vec![ItemKind::Unknown],
            ..Filter::default()
        }
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
        let controller = Mutex::new(Controller::new(match_default_filter(), Limits::default()));

        on_command(&controller, &gate, &journal, Command::Toggle, 0); // Idle -> Start
        assert_eq!(controller.lock().unwrap().status(), Status::Watching);
        assert!(gate.is_enabled());

        on_command(&controller, &gate, &journal, Command::Toggle, 1); // Watching -> Stop
        assert_eq!(
            controller.lock().unwrap().status(),
            Status::Stopped(StopReason::PlayerStopped)
        );
        assert!(!gate.is_enabled());

        on_command(&controller, &gate, &journal, Command::Toggle, 2); // Stopped -> Start
        // Default filter matches the default item -> Paused.
        dispatch(
            &controller,
            &gate,
            &journal,
            Event::Snapshot {
                snapshot: one_item_shop(),
                now_ms: 3,
            },
        );
        on_command(&controller, &gate, &journal, Command::Toggle, 4); // Paused -> Stop
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
        let controller = Mutex::new(Controller::new(match_default_filter(), Limits::default()));
        on_command(&controller, &gate, &journal, Command::Start, 0);
        let mut snapshot = one_item_shop();
        snapshot.slots[0].id = 42;
        // Default filter matches the default item -> Paused, checklist [42].
        on_message(
            &controller,
            &gate,
            &journal,
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
        let mut ctrl = Controller::new(match_default_filter(), Limits::default());
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
        let lines = apply(&actions, &ctrl, &gate);
        assert!(lines.iter().any(|line| line.contains("aren't tracked")));
    }

    #[test]
    fn paused_label_reflects_manual_flow_when_checklist_empty() {
        let gate = WatchGate::new(false);
        let journal = EventLog::default();
        let controller = Mutex::new(Controller::new(match_default_filter(), Limits::default()));
        on_command(&controller, &gate, &journal, Command::Start, 0);
        // Paused on an untrackable (id-0) match: a no-effect command's echo
        // must advise manual resume, not a phantom auto-resume.
        dispatch(
            &controller,
            &gate,
            &journal,
            Event::Snapshot {
                snapshot: one_item_shop(),
                now_ms: 1,
            },
        );
        let lines = handle_command(&controller, &gate, Command::Start, 2);
        assert!(lines.iter().any(|line| line.contains("buy, then refresh")));
        assert!(!lines.iter().any(|line| line.contains("auto-resumes")));
    }

    #[test]
    fn start_hint_printed_only_when_shop_stored() {
        let gate = WatchGate::new(false);
        let controller = Mutex::new(Controller::new(match_default_filter(), Limits::default()));
        // Nothing stored yet: plain watching line, no hint.
        let lines = handle_command(&controller, &gate, Command::Start, 0);
        assert!(lines.iter().any(|line| line.contains("watching")));
        assert!(!lines.iter().any(|line| line.contains("not replayed")));

        // Stop, receive a shop (stored, not evaluated), restart: hint appears.
        handle_command(&controller, &gate, Command::Stop, 1);
        dispatch(
            &controller,
            &gate,
            &EventLog::default(),
            Event::Snapshot {
                snapshot: one_item_shop(),
                now_ms: 2,
            },
        );
        let lines = handle_command(&controller, &gate, Command::Start, 3);
        assert!(lines.iter().any(|line| line.contains("not replayed")));
    }

    #[test]
    fn ignored_command_leaves_state_and_gate_unchanged() {
        let gate = WatchGate::new(false);
        let journal = EventLog::default();
        let controller = Mutex::new(Controller::new(match_default_filter(), Limits::default()));
        on_command(&controller, &gate, &journal, Command::Start, 0);
        dispatch(
            &controller,
            &gate,
            &journal,
            Event::Snapshot {
                snapshot: one_item_shop(),
                now_ms: 1,
            },
        );
        assert_eq!(controller.lock().unwrap().status(), Status::Paused);

        // `start` mid-session is ignored by the controller: still Paused,
        // gate still on, counters untouched.
        on_command(&controller, &gate, &journal, Command::Start, 2);
        assert_eq!(controller.lock().unwrap().status(), Status::Paused);
        assert_eq!(controller.lock().unwrap().progress().matches_found, 1);
        assert!(gate.is_enabled());
    }
}
