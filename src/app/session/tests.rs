//! Tests for the session loop and its handlers, in the order the module
//! declares them: the loop's exit paths, commands, server messages, `apply`,
//! then watchdog recovery.
//!
//! The shared fixtures open the file; the narrower ones sit immediately above
//! the group that uses them.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::actuator::SnapshotEpoch;
use crate::domain::control::{Limits, StopReason, past_rung};
use crate::domain::filter::Filter;
use crate::domain::shop::{CatalogId, Gold, ItemKind, PurchaseLimit, ShopItem, ShopSnapshot};

/// The sender drops immediately, which the loop reads as "no stop can ever
/// arrive" and disables the branch — not as a stop.
fn never_shutdown() -> watch::Receiver<bool> {
    watch::channel(false).1
}

/// A fresh shared-timings cell at the calibrated baselines (all extras 0).
fn timings() -> Arc<Mutex<plan::Timings>> {
    Arc::new(Mutex::new(plan::Timings::default()))
}

/// Off-mode actuator: decisions keep the advice wording, nothing is submitted.
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

/// Panics on `0`: [`CatalogId`] treats the wire's "no id" as `None`, not a
/// magic number a test could pass by accident.
fn cid(id: u32) -> CatalogId {
    CatalogId::new(id).expect("a fixture catalog id is never zero")
}

/// No match for `Filter::matching_default_items()` (kind `Unknown`):
/// the controller advises a refresh.
fn dud_shop(id: u32) -> ShopSnapshot {
    ShopSnapshot {
        merchant: None,
        slots: vec![ShopItem {
            id: Some(cid(id)),
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
    // The panic report lands a beat LATER than the uplink-closed break, so
    // only the post-loop grace-drain can surface it.
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

/// `messages` is the one branch a remote party can keep permanently ready, and
/// while `fatal_errors` sat below it "break immediately" meant "after the flood
/// drains". *When* is the assertion, not the exit — the old order reached
/// `Exit::Fatal` too, just last — so the test reads the journal: one shop
/// handled before the abort line is one too many.
#[tokio::test]
async fn a_flood_of_messages_cannot_delay_a_fatal() {
    const QUEUED: usize = 8;

    let gate = WatchGate::new(false);
    let journal = EventLog::default();
    let controller = Mutex::new(Controller::new(
        Filter::matching_default_items(),
        Limits::default(),
    ));

    // Armed, so the flood is not silent: an idle controller ignores shop
    // messages, and a silent flood cannot tell the two orders apart.
    on_command(&controller, &gate, &journal, &off(), Command::Start, 0);
    let armed_lines = journal.to_entries().len();

    let (_command_tx, command_rx) = mpsc::channel::<Command>(1);
    // Kept alive: a closed channel would end the loop through `UplinkClosed`,
    // which is the branch this test is not about.
    let (message_tx, message_rx) = mpsc::channel::<UplinkEvent>(QUEUED);
    for id in 0..QUEUED {
        message_tx
            .try_send(UplinkEvent::Message(ServerMessage::Shop(dud_shop(
                u32::try_from(id).expect("a fixture index fits") + 1,
            ))))
            .expect("fill the bounded uplink queue");
    }
    assert_eq!(message_tx.capacity(), 0, "the flood is queued and ready");

    let (error_tx, error_rx) = mpsc::channel::<String>(1);
    error_tx
        .try_send("network capture: the adapter died".to_owned())
        .expect("one fatal, waiting behind the flood");

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

    assert_eq!(
        failure,
        Some("network capture: the adapter died".to_owned())
    );
    // The journal, not `Sender::capacity`: dropping the receiver discards the
    // buffered messages too, so only the journal shows what was *processed*.
    let said: Vec<Arc<str>> = journal
        .to_entries()
        .into_iter()
        .skip(armed_lines)
        .map(|line| line.text)
        .collect();
    assert!(
        said.first()
            .is_some_and(|text| text.contains("session aborted")),
        "the fatal must be the first thing the loop says, not the {QUEUED}th: {said:?}"
    );
    drop(message_tx);
}

/// The other half of the same hazard: a tick that never fires is a `Limits`
/// check and a watchdog rung that never fire, with Buy and Refresh still going
/// out. The assertion is behavioural — under a sustained flood the session must
/// still stop itself on its own time limit. `ServerMessage::Unknown` is the
/// cheapest message there is, so the flood does nothing but hold the branch
/// ready: the worst case, not a contrived one.
#[tokio::test]
async fn a_flood_of_messages_cannot_starve_the_tick() {
    /// The uplink's own depth, so the flood is the shipped shape.
    const UPLINK_SLOTS: usize = 256;
    /// One sender's cooperative budget matches the loop's drain rate exactly;
    /// several pin the queue at capacity instead of racing it.
    const FLOODERS: usize = 4;
    /// Long enough that the loop's *immediate* first tick does not already trip
    /// it, short enough to be crossed by the next tick one `TICK_PERIOD` later.
    const TIME_LIMIT_MS: u64 = 500;
    /// The flood's own dead man's switch, reached only when the tick is starved
    /// — that is, when this test is failing, and a failure must not be a hang.
    const GIVE_UP: Duration = Duration::from_secs(5);

    let gate = WatchGate::new(false);
    let journal = EventLog::default();
    let controller = Mutex::new(Controller::new(
        Filter::matching_default_items(),
        Limits {
            max_duration_ms: Some(TIME_LIMIT_MS),
            ..Limits::default()
        },
    ));
    // Armed at 0 on the session clock the loop itself reads, so the elapsed
    // time the limit is checked against is the loop's own running time.
    on_command(&controller, &gate, &journal, &off(), Command::Start, 0);
    assert!(gate.is_enabled());

    let (_command_tx, command_rx) = mpsc::channel::<Command>(1);
    let (message_tx, message_rx) = mpsc::channel::<UplinkEvent>(UPLINK_SLOTS);
    let (_error_tx, error_rx) = mpsc::channel::<String>(1);
    // Full before the loop's first poll: the senders below only have to keep
    // it that way, never to win a race to fill it.
    for _ in 0..UPLINK_SLOTS {
        message_tx
            .try_send(UplinkEvent::Message(ServerMessage::Unknown))
            .expect("fill the bounded uplink queue");
    }
    assert_eq!(message_tx.capacity(), 0, "the flood is queued and ready");

    // The gate is the flood's stop signal, so nothing here waits on wall-clock
    // guesswork: the halt closes it, the senders drop, the loop ends.
    let sent = Arc::new(AtomicU64::new(0));
    let floods: Vec<_> = (0..FLOODERS)
        .map(|_| {
            let gate = gate.clone();
            let sent = Arc::clone(&sent);
            let messages = message_tx.clone();
            tokio::spawn(async move {
                let until = std::time::Instant::now() + GIVE_UP;
                while gate.is_enabled() && std::time::Instant::now() < until {
                    if messages
                        .send(UplinkEvent::Message(ServerMessage::Unknown))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    sent.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    drop(message_tx); // only the senders above may keep the uplink open.

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
    for flood in floods {
        flood.await.expect("a flood task never panics");
    }

    assert_eq!(failure, None);
    // The stop reason, not merely "stopped": teardown stops the controller too,
    // so a halt alone would pass on a loop that ticked exactly never.
    assert_eq!(
        controller.lock().unwrap().status(),
        Status::Stopped(StopReason::Timeout),
        "the time limit must fire while the uplink queue is saturated"
    );
    assert!(
        journal
            .to_entries()
            .iter()
            .any(|line| line.text.contains("session time limit reached"))
    );
    // A flood nobody measured may not have happened; refilling the queue many
    // times over is what makes the other branches unreachable.
    assert!(
        sent.load(Ordering::Relaxed) >= (UPLINK_SLOTS * 8) as u64,
        "the flood must have been sustained, not a single fill: {} messages",
        sent.load(Ordering::Relaxed)
    );
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
    gate.request_halt(HaltSource::ActuatorFailed);
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

    let lines = journal.to_entries();
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
    gate.request_halt(HaltSource::ActuatorFailed);
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
            .to_entries()
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
    gate.request_halt(HaltSource::ActuatorFailed);

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

    let entries = journal.to_entries();
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
            .to_entries()
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
            .to_entries()
            .iter()
            .all(|line| !line.text.contains("stopped"))
    );
}

/// Without this branch the loop keeps running on a detached task and the
/// process dies with a live capture session still open in the driver.
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
    // A clean end, and the player's own: not "session ended".
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
    let lines = handle_command(&controller, &gate, &off(), Command::SetLimits(limits), 0);
    assert!(lines.iter().any(|line| line.contains("limits updated")));
    assert_eq!(controller.lock().unwrap().limits(), &limits);
}

#[test]
fn set_timings_swaps_the_actuator_waits() {
    let gate = WatchGate::new(false);
    let controller = Mutex::new(Controller::new(Filter::default(), Limits::default()));
    let actuator = off();
    let timings = plan::Timings {
        refreshed: plan::DelayRange::try_new(200, 800).expect("a valid fixture range"),
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
    let entries = journal.to_entries();
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
    snapshot.slots[0].id = Some(cid(42));
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
        item: Some(cid(42)),
        gold: Some(Gold::new(100)),
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

/// Stores a one-item shop whose slot carries id 42 and a name.
fn controller_with_named_item() -> Controller {
    let mut controller = Controller::new(Filter::default(), Limits::default());
    let mut snapshot = one_item_shop();
    snapshot.slots[0].id = Some(cid(42));
    snapshot.slots[0].name = Some("Reforged Sword".to_owned());
    let _ = controller.handle(Event::Snapshot {
        snapshot,
        now_ms: 0,
    });
    controller
}

/// The balance is thousands-grouped by `impl Display for Gold`, so the journal
/// cannot opt out by forgetting to call a formatter — this line used to print
/// `250000` while the slot table two panes over showed `250,000`.
#[test]
fn purchase_line_names_item_from_snapshot_and_groups_the_balance() {
    let notice = PurchaseNotice {
        item: Some(cid(42)),
        gold: Some(Gold::new(250_000)),
    };
    assert_eq!(
        purchase_line(&controller_with_named_item(), &notice),
        ">> bought: Reforged Sword — 250,000 gold left"
    );
}

#[test]
fn purchase_line_falls_back_to_id_when_name_unknown() {
    let controller = Controller::new(Filter::default(), Limits::default());
    let notice = PurchaseNotice {
        item: Some(cid(7)),
        gold: Some(Gold::new(100)),
    };
    assert_eq!(
        purchase_line(&controller, &notice),
        ">> bought: item 7 — 100 gold left"
    );
}

#[test]
fn purchase_line_omits_missing_gold() {
    let notice = PurchaseNotice {
        item: Some(cid(42)),
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
    // Two matches, only one trackable: the id-0 slot would be refreshed over.
    let snapshot = ShopSnapshot {
        merchant: None,
        slots: vec![
            ShopItem::default(),
            ShopItem {
                id: Some(cid(42)),
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
            .to_entries()
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
    snapshot.slots[0].id = Some(cid(42));
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
        item: Some(cid(42)),
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
                id: Some(cid(42)),
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
    let entries = journal.to_entries();
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
fn buy_job_names_the_slot_it_cannot_click_and_still_clicks_the_rest() {
    let gate = WatchGate::new(true);
    let journal = EventLog::default();
    let (actuator, mut jobs) = recording(Mode::Live);
    let controller = armed();
    // Two trackable matches, but slot 7 sits past the six clickable rows: one
    // row to click, one refusal to report.
    let mut slots = vec![ShopItem {
        id: Some(cid(42)),
        ..ShopItem::default()
    }];
    slots.extend((0..5).map(|_| ShopItem {
        kind: ItemKind::Equipment,
        ..ShopItem::default()
    }));
    slots.push(ShopItem {
        id: Some(cid(43)),
        ..ShopItem::default()
    });
    on_message(
        &controller,
        &gate,
        &journal,
        &actuator,
        ServerMessage::Shop(ShopSnapshot {
            merchant: None,
            slots,
            refresh: None,
        }),
        1,
    );
    let job = jobs.try_recv().expect("buy job for the clickable slot");
    // Scroll-to-top + one buy/confirm pair: only slot 1 is reachable.
    assert_eq!(job.steps.len(), 3);
    let entries = journal.to_entries();
    assert!(
        entries
            .iter()
            .any(|line| line.text.contains("buying slot 1"))
    );
    assert!(
        entries.iter().any(|line| {
            line.text
                .contains("slot 7 is outside the six clickable rows")
        }),
        "the refused slot must be named, not dropped: {:?}",
        entries.iter().map(|line| &line.text).collect::<Vec<_>>()
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
            .to_entries()
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
    hit.slots[0].id = Some(cid(42));
    on_message(
        &controller,
        &gate,
        &journal,
        &actuator,
        ServerMessage::Shop(hit),
        2,
    );
    let entries = journal.to_entries();
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
            id: Some(cid(42)),
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
    let entries = journal.to_entries();
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
            .to_entries()
            .iter()
            .any(|line| line.text.contains("queue full"))
    );
}

/// Collapsing the submit result to a bool would merge these one `false` apart,
/// sending the player hunting a slow actuator when nobody is at the other end.
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
    let lines = journal.to_entries();
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
        Event::Tick {
            now_ms: past_rung(1),
        },
        past_rung(1),
    );
    let retry = jobs.try_recv().expect("confirm retry job");
    assert_eq!(retry.steps.len(), 1);
    assert_eq!(retry.epoch, plan::Epoch(1));
    assert!(journal.to_entries().iter().any(|line| {
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
        Event::Tick {
            now_ms: past_rung(1),
        },
        past_rung(1),
    );
    jobs.try_recv().expect("confirm retry job");
    dispatch(
        &controller,
        &gate,
        &journal,
        &actuator,
        Event::Tick {
            now_ms: past_rung(2),
        },
        past_rung(2),
    );
    let reissue = jobs.try_recv().expect("re-issued refresh job");
    // Full refresh sequence, but into an idle game: dispatch margin only.
    assert_eq!(reissue.steps.len(), 2);
    assert_eq!(reissue.steps[0].wait_ms, 400);
    assert_eq!(reissue.epoch, plan::Epoch(1));
    assert!(
        journal
            .to_entries()
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
                id: Some(cid(42)),
                ..ShopItem::default()
            },
            ShopItem {
                id: Some(cid(43)),
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
            item: Some(cid(42)),
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
    let entries = journal.to_entries();
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
    // The only match is trackable, so the purchase ladder arms, but position 7
    // is beyond the six clickable rows: no buy job can target it.
    let mut slots: Vec<ShopItem> = (0..6)
        .map(|_| ShopItem {
            kind: ItemKind::Equipment,
            ..ShopItem::default()
        })
        .collect();
    slots.push(ShopItem {
        id: Some(cid(42)),
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
        Event::Tick {
            now_ms: past_rung(1),
        },
        past_rung(1),
    );
    jobs.try_recv().expect("confirm retry job");
    // Nothing clickable: no job, but the journal must say so rather than end
    // on the announcement.
    dispatch(
        &controller,
        &gate,
        &journal,
        &actuator,
        Event::Tick {
            now_ms: past_rung(2),
        },
        past_rung(2),
    );
    assert!(jobs.try_recv().is_err(), "no job for row-less targets");
    let entries = journal.to_entries();
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
    for now in [past_rung(1), past_rung(2), past_rung(3)] {
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
            .to_entries()
            .iter()
            .any(|line| line.text.contains("no response from the game"))
    );
    // The whole ladder went out: refresh, confirm retry, re-issue.
    assert!(jobs.try_recv().is_ok());
    assert!(jobs.try_recv().is_ok());
    assert!(jobs.try_recv().is_ok());
    assert!(jobs.try_recv().is_err());
}
