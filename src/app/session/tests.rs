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

#[test]
fn actuator_failure_command_halts_with_the_clicker_label() {
    let gate = WatchGate::new(false);
    let controller = Mutex::new(Controller::new(
        Filter::matching_default_items(),
        Limits::default(),
    ));
    handle_command(&controller, &gate, &off(), Command::Start, 0);
    assert!(gate.is_enabled());
    let lines = handle_command(&controller, &gate, &off(), Command::ActuatorFailed, 1);
    assert!(lines.iter().any(|line| line.contains("clicker failed")));
    assert!(!lines.iter().any(|line| line.contains("player stopped")));
    assert_eq!(
        controller.lock().unwrap().status(),
        Status::Stopped(StopReason::ActuatorFailed)
    );
    assert!(!gate.is_enabled());
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
fn match_hint_warns_when_some_matches_untracked() {
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
