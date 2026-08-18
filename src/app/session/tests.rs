//! Tests for the session loop and its handlers, in the order the module
//! declares them.
//!
//! 1. **The loop itself** — how each `break` path leaves the controller and the
//!    gate, the safety-halt latch against a saturated command queue, the
//!    post-loop grace drain that keeps a worker panic from reading as a clean
//!    end, and the cooperative shutdown signal.
//! 2. **Commands** (`handle_command`) — every `Command` variant, including the
//!    `Toggle` resolution and the retune confirmations.
//! 3. **Server messages** (`on_message`, `handle_purchase`) — shop snapshots,
//!    purchase echoes and the lines each renders.
//! 4. **`apply`** — the gate transitions, the advice wording, and the job
//!    submissions (refresh, buy, confirm re-click) per actuator mode.
//! 5. **Watchdog recovery** — each `Recovery` rung and what it queues.
//!
//! The shared fixtures (`never_shutdown`, `timings`, `off`, `recording`,
//! `dud_shop`) open the file; the narrower ones (`one_item_shop`,
//! `controller_with_named_item`, `armed`, `armed_recovering`) sit immediately
//! above the group that uses them.

use std::sync::Arc;

use super::*;
use crate::actuator::SnapshotEpoch;
use crate::domain::control::{Limits, StopReason};
use crate::domain::filter::Filter;
use crate::domain::shop::{ItemKind, PurchaseLimit, ShopItem, ShopSnapshot};

/// A shutdown signal nobody ever raises: the loop must exit through its own
/// paths. The sender is dropped straight away, which the loop reads as "no
/// stop can ever arrive" (the branch disables itself), never as a stop.
fn never_shutdown() -> watch::Receiver<bool> {
    watch::channel(false).1
}

/// A fresh shared-timings cell at the calibrated baselines (all extras 0).
fn timings() -> Arc<Mutex<plan::Timings>> {
    Arc::new(Mutex::new(plan::Timings::default()))
}

/// Off-mode actuator: decisions keep the advice wording, nothing is
/// ever submitted.
fn off() -> ActuatorHandle {
    ActuatorHandle::new(
        Mode::Off,
        SnapshotEpoch::default(),
        mpsc::channel(8).0,
        timings(),
    )
}

/// An actuator whose submitted jobs the test inspects.
fn recording(mode: Mode) -> (ActuatorHandle, mpsc::Receiver<plan::Job>) {
    let (jobs, rx) = mpsc::channel(8);
    (
        ActuatorHandle::new(mode, SnapshotEpoch::default(), jobs, timings()),
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
        never_shutdown(),
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

#[tokio::test(start_paused = true)]
async fn a_worker_panic_is_reported_when_the_uplink_channel_closes() {
    let gate = WatchGate::new(false);
    let journal = EventLog::default();
    let controller = Mutex::new(Controller::new(
        Filter::matching_default_items(),
        Limits::default(),
    ));

    let (_command_tx, command_rx) = mpsc::channel::<Command>(1);
    let (message_tx, message_rx) = mpsc::channel::<UplinkEvent>(1);
    let (error_tx, error_rx) = mpsc::channel::<String>(1);
    // The uplink task's message sender drops, so the loop takes the
    // uplink-closed break. Its supervisor delivers the panic report a beat
    // LATER — during the grace window, not before — so only the post-loop
    // grace-drain can surface it. Without that drain the loop returns None.
    drop(message_tx);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let _ = error_tx.send("uplink task panicked".to_owned()).await;
    });

    let failure = session_loop(
        &controller,
        &gate,
        &journal,
        &off(),
        command_rx,
        message_rx,
        error_rx,
        never_shutdown(),
    )
    .await;

    assert_eq!(failure, Some("uplink task panicked".to_owned()));
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
async fn actuator_failure_latch_halts_with_the_clicker_label() {
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
    gate.request_halt(crate::watch::HaltSource::ActuatorFailed);
    drop(message_tx);
    session_loop(
        &controller,
        &gate,
        &journal,
        &off(),
        command_rx,
        message_rx,
        error_rx,
        never_shutdown(),
    )
    .await;

    let lines = journal.entries();
    assert!(
        lines
            .iter()
            .any(|line| line.text.contains("clicker failed"))
    );
    assert!(
        !lines
            .iter()
            .any(|line| line.text.contains("player stopped"))
    );
    assert_eq!(
        controller.lock().unwrap().status(),
        Status::Stopped(StopReason::ActuatorFailed)
    );
    assert!(!gate.is_enabled());
}

#[tokio::test]
async fn saturated_command_queue_cannot_drop_an_actuator_halt() {
    let gate = WatchGate::new(false);
    let journal = EventLog::default();
    let controller = Mutex::new(Controller::new(
        Filter::matching_default_items(),
        Limits::default(),
    ));
    on_command(&controller, &gate, &journal, &off(), Command::Start, 0);
    assert!(gate.is_enabled());

    let (command_tx, command_rx) = mpsc::channel::<Command>(1);
    command_tx
        .try_send(Command::SetTimings(plan::Timings::default()))
        .expect("fill the bounded command queue");
    let (message_tx, message_rx) = mpsc::channel::<UplinkEvent>(1);
    let (_error_tx, error_rx) = mpsc::channel::<String>(1);
    gate.request_halt(crate::watch::HaltSource::ActuatorFailed);
    assert!(!gate.is_enabled(), "the safety cutoff is synchronous");
    drop(message_tx);

    session_loop(
        &controller,
        &gate,
        &journal,
        &off(),
        command_rx,
        message_rx,
        error_rx,
        never_shutdown(),
    )
    .await;

    assert_eq!(
        controller.lock().unwrap().status(),
        Status::Stopped(StopReason::ActuatorFailed)
    );
    assert!(!gate.is_enabled());
    assert!(
        journal
            .entries()
            .iter()
            .any(|line| { line.text.contains("clicker failed") })
    );
    drop(command_tx);
}

#[test]
fn queued_start_cannot_rearm_before_pending_halt_is_applied() {
    let gate = WatchGate::new(false);
    let controller = Mutex::new(Controller::new(
        Filter::matching_default_items(),
        Limits::default(),
    ));
    let (command_tx, mut command_rx) = mpsc::channel::<Command>(1);
    command_tx.try_send(Command::Start).unwrap();
    gate.request_halt(crate::watch::HaltSource::ActuatorFailed);

    let queued = command_rx.try_recv().expect("queued Start");
    handle_command(&controller, &gate, &off(), queued, 0);

    assert_eq!(controller.lock().unwrap().status(), Status::Watching);
    assert!(
        !gate.is_enabled(),
        "set(true) must not bypass a pending safety halt"
    );
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
        never_shutdown(),
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
        never_shutdown(),
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
        never_shutdown(),
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

/// Closing the window must unwind the pipeline: without this branch the loop
/// keeps running on a detached task and the process dies with a live capture
/// session still open in the driver.
#[tokio::test(start_paused = true)]
async fn shutdown_signal_ends_the_loop_and_stops_the_watch() {
    let gate = WatchGate::new(false);
    let journal = EventLog::default();
    let controller = Mutex::new(Controller::new(
        Filter::matching_default_items(),
        Limits::default(),
    ));
    on_command(&controller, &gate, &journal, &off(), Command::Start, 0);
    assert!(gate.is_enabled());

    // Every other source stays open: only the shutdown signal can end this.
    let (_command_tx, command_rx) = mpsc::channel::<Command>(1);
    let (_message_tx, message_rx) = mpsc::channel::<UplinkEvent>(1);
    let (_error_tx, error_rx) = mpsc::channel::<String>(1);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let signal = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        shutdown_tx.send_replace(true);
        shutdown_tx
    });

    let failure = session_loop(
        &controller,
        &gate,
        &journal,
        &off(),
        command_rx,
        message_rx,
        error_rx,
        shutdown_rx,
    )
    .await;

    let _sender = signal.await.unwrap();
    // A requested shutdown is a clean end, not a failure — and the player's
    // own stop, so the controller must not report "session ended".
    assert_eq!(failure, None);
    assert!(!gate.is_enabled());
    assert_eq!(
        controller.lock().unwrap().status(),
        Status::Stopped(StopReason::PlayerStopped)
    );
}

/// A signal already raised before the loop starts must still be honoured:
/// `changed()` alone would never fire for it.
#[tokio::test]
async fn a_shutdown_requested_before_the_loop_starts_is_honoured() {
    let gate = WatchGate::new(false);
    let journal = EventLog::default();
    let controller = Mutex::new(Controller::new(
        Filter::matching_default_items(),
        Limits::default(),
    ));

    let (_command_tx, command_rx) = mpsc::channel::<Command>(1);
    let (_message_tx, message_rx) = mpsc::channel::<UplinkEvent>(1);
    let (_error_tx, error_rx) = mpsc::channel::<String>(1);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    shutdown_tx.send_replace(true);

    let failure = session_loop(
        &controller,
        &gate,
        &journal,
        &off(),
        command_rx,
        message_rx,
        error_rx,
        shutdown_rx,
    )
    .await;

    assert_eq!(failure, None);
    assert!(!gate.is_enabled());
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
fn set_timings_swaps_the_actuator_waits() {
    let gate = WatchGate::new(false);
    let controller = Mutex::new(Controller::new(Filter::default(), Limits::default()));
    let actuator = off();
    let timings = plan::Timings {
        refreshed: plan::DelayRange {
            min_ms: 200,
            max_ms: 800,
        },
        ..plan::Timings::default()
    };
    let lines = handle_command(
        &controller,
        &gate,
        &actuator,
        Command::SetTimings(timings),
        0,
    );
    assert!(lines.iter().any(|line| line.contains("timings updated")));
    assert_eq!(actuator.timings(), timings);
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

    let _ = ctrl.handle(Event::Start { now_ms: 0 });
    apply(&[], &ctrl, &gate, &off(), None, 0);
    assert!(gate.is_enabled()); // Watching

    // Default filter matches the default item: Buy -> Paused, gate stays on.
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
    let _ = controller.handle(Event::Snapshot {
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
fn match_hint_warns_when_some_matches_untracked() {
    let gate = WatchGate::new(false);
    let mut ctrl = Controller::new(Filter::matching_default_items(), Limits::default());
    let _ = ctrl.handle(Event::Start { now_ms: 0 });
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
    let _ = controller
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
    assert_eq!(first.epoch, plan::Epoch(1));

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
    assert_eq!(second.epoch, plan::Epoch(2));
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
    assert_eq!(resume.epoch, plan::Epoch(1));
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
    let _ = controller
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
        .try_send(plan::refresh_job(
            Trigger::Refreshed,
            plan::Timings::default(),
            plan::Epoch(0),
            0,
        ))
        .expect("fills the queue");
    let actuator = ActuatorHandle::new(Mode::Live, SnapshotEpoch::default(), job_tx, timings());
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

/// A closed channel is not a full one, and must not be reported as one.
///
/// The two are one `false` apart if the submit result is collapsed to a bool,
/// and the wrong label sends the player hunting a slow actuator while the real
/// problem is that nobody is at the other end — a state no retry and no re-arm
/// can fix.
#[test]
fn a_gone_executor_is_journaled_as_gone_not_as_a_full_queue() {
    let gate = WatchGate::new(true);
    let journal = EventLog::default();
    let (job_tx, job_rx) = mpsc::channel(8);
    drop(job_rx); // the executor task is over: the queue is empty, not full
    let actuator = ActuatorHandle::new(Mode::Live, SnapshotEpoch::default(), job_tx, timings());
    let controller = armed();
    on_message(
        &controller,
        &gate,
        &journal,
        &actuator,
        ServerMessage::Shop(dud_shop(10)),
        1,
    );
    let lines = journal.entries();
    assert!(
        lines
            .iter()
            .any(|line| line.text.contains("the actuator is gone, restart the app")),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|line| line.text.contains("queue full")),
        "{lines:?}"
    );
}

/// An armed controller with the recovery watchdog on, matching
/// `ShopItem::default()`.
fn armed_recovering() -> Mutex<Controller> {
    let mut ctrl = Controller::new(Filter::matching_default_items(), Limits::default());
    ctrl.enable_recovery();
    let _ = ctrl.handle(Event::Start { now_ms: 0 });
    Mutex::new(ctrl)
}

#[test]
fn watchdog_confirm_retry_submits_one_click_at_current_epoch() {
    let gate = WatchGate::new(true);
    let journal = EventLog::default();
    let (actuator, mut jobs) = recording(Mode::Live);
    let controller = armed_recovering();
    on_message(
        &controller,
        &gate,
        &journal,
        &actuator,
        ServerMessage::Shop(dud_shop(10)),
        1,
    );
    jobs.try_recv().expect("refresh job");
    // The shop message bumped the epoch: the retry must carry the bumped
    // value or the executor would drop it as stale.
    dispatch(
        &controller,
        &gate,
        &journal,
        &actuator,
        Event::Tick { now_ms: 10_001 },
        10_001,
    );
    let retry = jobs.try_recv().expect("confirm retry job");
    assert_eq!(retry.steps.len(), 1);
    assert_eq!(retry.epoch, plan::Epoch(1));
    assert!(journal.entries().iter().any(|line| {
        line.text
            .contains("no shop after refresh — re-clicking confirm")
    }));
}

#[test]
fn watchdog_refresh_reissue_uses_recovery_pre_wait() {
    let gate = WatchGate::new(true);
    let journal = EventLog::default();
    let (actuator, mut jobs) = recording(Mode::Live);
    let controller = armed_recovering();
    on_message(
        &controller,
        &gate,
        &journal,
        &actuator,
        ServerMessage::Shop(dud_shop(10)),
        1,
    );
    jobs.try_recv().expect("refresh job");
    dispatch(
        &controller,
        &gate,
        &journal,
        &actuator,
        Event::Tick { now_ms: 10_001 },
        10_001,
    );
    jobs.try_recv().expect("confirm retry job");
    dispatch(
        &controller,
        &gate,
        &journal,
        &actuator,
        Event::Tick { now_ms: 20_001 },
        20_001,
    );
    let reissue = jobs.try_recv().expect("re-issued refresh job");
    // Full refresh sequence, but into an idle game: dispatch margin only.
    assert_eq!(reissue.steps.len(), 2);
    assert_eq!(reissue.steps[0].wait_ms, 400);
    assert_eq!(reissue.epoch, plan::Epoch(1));
    assert!(
        journal
            .entries()
            .iter()
            .any(|line| line.text.contains("re-issuing the refresh"))
    );
}

#[test]
fn watchdog_buy_reissue_clicks_only_outstanding_rows() {
    let gate = WatchGate::new(true);
    let journal = EventLog::default();
    let (actuator, mut jobs) = recording(Mode::Live);
    let controller = armed_recovering();
    // Two trackable matches: Paused with checklist [42, 43].
    let snapshot = ShopSnapshot {
        merchant: None,
        slots: vec![
            ShopItem {
                id: 42,
                ..ShopItem::default()
            },
            ShopItem {
                id: 43,
                ..ShopItem::default()
            },
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
    jobs.try_recv().expect("initial buy job");
    // One echo lands: only id 43 (slot 2) stays outstanding.
    on_message(
        &controller,
        &gate,
        &journal,
        &actuator,
        ServerMessage::Purchase(PurchaseNotice {
            item: 42,
            gold: None,
        }),
        2,
    );
    dispatch(
        &controller,
        &gate,
        &journal,
        &actuator,
        Event::Tick { now_ms: 12_001 },
        12_001,
    );
    jobs.try_recv().expect("confirm retry job");
    dispatch(
        &controller,
        &gate,
        &journal,
        &actuator,
        Event::Tick { now_ms: 22_001 },
        22_001,
    );
    let reissue = jobs.try_recv().expect("re-issued buy job");
    // Scroll-to-top + one buy/confirm pair — the bought row is not re-clicked.
    assert_eq!(reissue.steps.len(), 3);
    // Only the lines after the re-issue marker: the initial buy job
    // legitimately clicked both slots.
    let entries = journal.entries();
    let reissued_at = entries
        .iter()
        .position(|line| line.text.contains("re-issuing buys"))
        .expect("re-issue line journaled");
    let after = &entries[reissued_at..];
    assert!(after.iter().any(|line| line.text.contains("buying slot 2")));
    assert!(!after.iter().any(|line| line.text.contains("buying slot 1")));
}

#[test]
fn watchdog_buy_reissue_without_clickable_rows_journals_the_gap() {
    let gate = WatchGate::new(true);
    let journal = EventLog::default();
    let (actuator, mut jobs) = recording(Mode::Live);
    let controller = armed_recovering();
    // Seven slots, only the last one matches: trackable (id 42) so the
    // purchase ladder arms, but position 7 sits beyond the six clickable
    // rows — no buy job can target it.
    let mut slots: Vec<ShopItem> = (0..6)
        .map(|_| ShopItem {
            kind: ItemKind::Equipment,
            ..ShopItem::default()
        })
        .collect();
    slots.push(ShopItem {
        id: 42,
        ..ShopItem::default()
    });
    let snapshot = ShopSnapshot {
        merchant: None,
        slots,
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
    assert_eq!(controller.lock().unwrap().status(), Status::Paused);
    dispatch(
        &controller,
        &gate,
        &journal,
        &actuator,
        Event::Tick { now_ms: 10_001 },
        10_001,
    );
    jobs.try_recv().expect("confirm retry job");
    // The re-issue rung finds nothing clickable: no job, but the journal
    // must say so instead of ending on the announcement.
    dispatch(
        &controller,
        &gate,
        &journal,
        &actuator,
        Event::Tick { now_ms: 20_001 },
        20_001,
    );
    assert!(jobs.try_recv().is_err(), "no job for row-less targets");
    let entries = journal.entries();
    assert!(
        entries
            .iter()
            .any(|line| line.text.contains("re-issuing buys"))
    );
    assert!(entries.iter().any(|line| {
        line.text
            .contains("no clickable slot for the outstanding buys")
    }));
}

#[test]
fn unresponsive_halt_reaches_the_journal() {
    let gate = WatchGate::new(true);
    let journal = EventLog::default();
    let (actuator, mut jobs) = recording(Mode::Live);
    let controller = armed_recovering();
    on_message(
        &controller,
        &gate,
        &journal,
        &actuator,
        ServerMessage::Shop(dud_shop(10)),
        1,
    );
    for now in [10_001, 20_001, 30_001] {
        dispatch(
            &controller,
            &gate,
            &journal,
            &actuator,
            Event::Tick { now_ms: now },
            now,
        );
    }
    assert_eq!(
        controller.lock().unwrap().status(),
        Status::Stopped(StopReason::Unresponsive)
    );
    assert!(!gate.is_enabled());
    assert!(
        journal
            .entries()
            .iter()
            .any(|line| line.text.contains("no response from the game"))
    );
    // The whole ladder went out: refresh, confirm retry, re-issue.
    assert!(jobs.try_recv().is_ok());
    assert!(jobs.try_recv().is_ok());
    assert!(jobs.try_recv().is_ok());
    assert!(jobs.try_recv().is_err());
}
