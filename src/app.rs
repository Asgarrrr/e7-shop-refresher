//! Orchestration: capture -> reassembly -> gate -> uplink -> controller -> display.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::capture::{Direction, PacketSource, Segment};
use crate::config::ForwardConfig;
use crate::domain::control::{Action, Controller, Event, Status, StopReason};
use crate::domain::shop::{ItemKind, ShopItem, ShopSnapshot};
use crate::stream::Reassembler;
use crate::uplink::protocol::ServerMessage;
use crate::watch::WatchGate;
use crate::{Config, Result};

/// Event flowing from the capture thread to reassembly.
enum CaptureEvent {
    /// A TCP segment to reassemble.
    Segment(Segment),
    /// Shop Watch was just re-enabled after a pause: the reassembler must
    /// re-anchor a fresh origin (the bytes during the pause are lost).
    Resync,
}

/// A player command, decoupled from its source: today a stdin task, tomorrow
/// the GUI pushing the same values through the same channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Start,
    Stop,
    Resume,
    /// Start or stop, depending on the current status.
    Toggle,
}

/// Runs the relay and blocks until shutdown (Ctrl+C or end of stream).
pub async fn run(config: Config) -> Result<()> {
    // Gate off at startup: the session starts Idle and the player arms it
    // with `start`.
    let gate = WatchGate::new(false);

    let (segment_tx, segment_rx) = mpsc::channel::<CaptureEvent>(8_192);
    let (raw_tx, raw_rx) = mpsc::channel::<Vec<u8>>(1_024);
    let (message_tx, message_rx) = mpsc::channel::<ServerMessage>(256);
    let (command_tx, command_rx) = mpsc::channel::<Command>(16);

    // Blocking capture on a dedicated thread (WinDivert::recv is synchronous).
    let source = build_source(&config)?;
    let capture_gate = gate.clone();
    std::thread::Builder::new()
        .name("capture".to_owned())
        .spawn(move || capture_loop(source, segment_tx, capture_gate))?;

    // Server link with automatic reconnection.
    tokio::spawn(crate::uplink::run(
        config.server_url.clone(),
        raw_rx,
        message_tx,
        config.reconnect_initial(),
        config.reconnect_max(),
    ));

    // Reassembly + filtering of the directions to forward.
    tokio::spawn(reassemble_loop(segment_rx, raw_tx, config.forward.clone()));

    // Keyboard input, decoupled from the session loop through the channel.
    tokio::spawn(stdin_loop(command_tx));

    // Shared so the upcoming GUI can clone the Arc and read
    // `status()`/`progress()`/`last_snapshot()` alongside the session loop.
    let controller = Arc::new(Mutex::new(Controller::new(config.filter, config.limits)));

    info!(server = %config.server_url, "relay started — idle, `start` arms the watch");
    print_controls();

    session_loop(&controller, &gate, command_rx, message_rx).await;
    info!("relay stopped");
    Ok(())
}

/// Consumes capture events, reassembles, forwards the ordered stream.
async fn reassemble_loop(
    mut events: mpsc::Receiver<CaptureEvent>,
    raw_tx: mpsc::Sender<Vec<u8>>,
    forward: ForwardConfig,
) {
    let mut reassembler = Reassembler::new();
    while let Some(event) = events.recv().await {
        let segment = match event {
            CaptureEvent::Resync => {
                reassembler.clear();
                continue;
            }
            CaptureEvent::Segment(segment) => segment,
        };
        if !should_forward(segment.direction, &forward) {
            continue;
        }
        let ordered = reassembler.push(&segment);
        if ordered.is_empty() {
            continue;
        }
        if raw_tx.send(ordered).await.is_err() {
            break; // uplink gone.
        }
    }
}

fn should_forward(direction: Direction, forward: &ForwardConfig) -> bool {
    match direction {
        Direction::ServerToClient => forward.server_to_client,
        Direction::ClientToServer => forward.client_to_server,
    }
}

/// Capture loop (synchronous context). Stops when the pipeline closes.
fn capture_loop(
    mut source: Box<dyn PacketSource>,
    tx: mpsc::Sender<CaptureEvent>,
    gate: WatchGate,
) {
    let mut was_enabled = gate.is_enabled();
    loop {
        let segment = match source.next_segment() {
            Ok(segment) => segment,
            Err(err) => {
                error!(error = %err, "capture interrupted");
                break;
            }
        };

        let enabled = gate.is_enabled();
        // Off -> on transition: request a resync before emitting, otherwise the
        // reassembler treats the sequence jump as an unfillable gap and never
        // delivers anything again.
        if enabled && !was_enabled && tx.blocking_send(CaptureEvent::Resync).is_err() {
            break;
        }
        was_enabled = enabled;

        if !enabled {
            continue; // Shop Watch off: emit nothing.
        }
        if tx.blocking_send(CaptureEvent::Segment(segment)).is_err() {
            break;
        }
    }
}

/// Reads stdin lines and forwards them as [`Command`]s; the session loop never
/// touches stdin.
async fn stdin_loop(commands: mpsc::Sender<Command>) {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match parse_command(&line) {
            Some(command) => {
                if commands.send(command).await.is_err() {
                    break; // session loop gone.
                }
            }
            None => println!(
                ">> unknown command: {:?} (start, stop, resume, enter = toggle)",
                line.trim()
            ),
        }
    }
}

fn parse_command(line: &str) -> Option<Command> {
    match line.trim().to_ascii_lowercase().as_str() {
        "" | "t" | "toggle" => Some(Command::Toggle),
        "on" | "start" => Some(Command::Start),
        "off" | "stop" => Some(Command::Stop),
        "r" | "resume" => Some(Command::Resume),
        _ => None,
    }
}

/// Owns the controller for the session: multiplexes player commands, server
/// messages, a 1 s tick (time limits), and Ctrl+C.
///
/// The mutex guard is only ever held across synchronous calls, never an
/// `.await`. The wall clock is read here so the domain stays pure.
async fn session_loop(
    controller: &Mutex<Controller>,
    gate: &WatchGate,
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
                Some(command) => on_command(controller, gate, command, now_ms()),
                None => commands_open = false,
            },
            message = messages.recv() => match message {
                Some(message) => on_message(controller, gate, message, now_ms()),
                None => break, // uplink gone.
            },
            _ = ticker.tick() => dispatch(controller, gate, Event::Tick { now_ms: now_ms() }),
            _ = &mut ctrl_c => {
                println!("\n>> Ctrl+C, stopping");
                break;
            }
        }
    }
}

/// Translates a player command into a controller event (`Toggle` resolves
/// against the current status) and echoes an outcome: a command is never
/// silent, even when the controller ignores it.
fn on_command(controller: &Mutex<Controller>, gate: &WatchGate, command: Command, now_ms: u64) {
    let mut ctrl = controller.lock().expect("controller mutex poisoned");
    let event = match command {
        Command::Start => Event::Start { now_ms },
        Command::Stop => Event::Stop,
        Command::Resume => Event::Resume { now_ms },
        Command::Toggle => match ctrl.status() {
            Status::Watching | Status::Paused => Event::Stop,
            Status::Idle | Status::Stopped(_) => Event::Start { now_ms },
        },
    };
    let before = ctrl.status();
    let actions = ctrl.handle(event);
    let mut lines = apply(&actions, &ctrl, gate);
    let after = ctrl.status();
    drop(ctrl);
    if actions.is_empty() {
        // `Start` yields no action, and ignored commands yield nothing at all:
        // synthesize the feedback from the status transition.
        if before != after && after == Status::Watching {
            lines.push(">> watching — open the shop".to_owned());
        } else if before == after {
            lines.push(format!(">> no effect — status: {}", status_label(after)));
        }
    }
    for line in &lines {
        println!("{line}");
    }
}

/// Renders a shop and feeds it to the controller; other messages are silent.
fn on_message(
    controller: &Mutex<Controller>,
    gate: &WatchGate,
    message: ServerMessage,
    now_ms: u64,
) {
    match message {
        ServerMessage::Ack | ServerMessage::Unknown => {}
        ServerMessage::Shop(snapshot) => {
            render_shop(&snapshot);
            dispatch(controller, gate, Event::Snapshot { snapshot, now_ms });
        }
    }
}

/// Locks, handles, applies; printing happens after the guard is released.
fn dispatch(controller: &Mutex<Controller>, gate: &WatchGate, event: Event) {
    let mut ctrl = controller.lock().expect("controller mutex poisoned");
    let actions = ctrl.handle(event);
    let lines = apply(&actions, &ctrl, gate);
    drop(ctrl);
    for line in &lines {
        println!("{line}");
    }
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
    // Alerted + still Paused means the loop waits on a purchase; on the
    // max-matches path a Halt follows in the same batch and `resume` would
    // be dead advice.
    let hint = if controller.status() == Status::Paused {
        ": buy in game, then `resume`"
    } else {
        ""
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

fn status_label(status: Status) -> &'static str {
    match status {
        Status::Idle => "idle (`start` arms the watch)",
        Status::Watching => "watching",
        Status::Paused => "paused (buy, then `resume`)",
        Status::Stopped(_) => "stopped (`start` re-arms)",
    }
}

fn describe(reason: StopReason) -> &'static str {
    match reason {
        StopReason::PlayerStopped => "player stopped",
        StopReason::OutOfFunds => "out of crystals",
        StopReason::MaxRefreshes => "refresh limit reached",
        StopReason::MaxSpend => "crystal budget reached",
        StopReason::MaxMatches => "match limit reached",
        StopReason::Timeout => "session time limit reached",
    }
}

fn render_shop(snapshot: &ShopSnapshot) {
    let merchant = snapshot.merchant.as_deref().unwrap_or("Secret Shop");
    println!("\n[{merchant}]");
    for item in &snapshot.slots {
        println!("  {}", format_item(item));
    }
}

fn format_item(item: &ShopItem) -> String {
    let kind = match item.kind {
        ItemKind::Equipment => "equipment",
        ItemKind::Hero => "hero",
        ItemKind::Token => "token",
        ItemKind::Unknown => "?",
    };

    let mut line = format!("slot {} · {kind}", item.slot);
    if let Some(name) = &item.name {
        line.push_str(&format!(" · {name}"));
    }
    if let Some(set) = &item.set {
        line.push_str(&format!(" · set {set}"));
    }
    if let Some(grade) = item.grade {
        line.push_str(&format!(" · grade {grade}"));
    }
    if let Some(price) = item.price {
        line.push_str(&format!(" · {price} gold"));
    }
    if !item.substats.is_empty() {
        let stats: Vec<String> = item
            .substats
            .iter()
            .map(|stat| match stat.value {
                Some(value) => format!("{} {value}", stat.name),
                None => stat.name.clone(),
            })
            .collect();
        line.push_str(&format!(" · [{}]", stats.join(", ")));
    }
    if let Some(limit) = item.limit {
        line.push_str(&format!(" · {}/{}", limit.remaining, limit.total));
    }
    line
}

fn print_controls() {
    println!("Commands: start, stop, resume (r), [Enter] toggle, Ctrl+C to quit");
}

#[cfg(all(windows, feature = "windivert-backend"))]
fn build_source(config: &Config) -> Result<Box<dyn PacketSource>> {
    use crate::capture::WinDivertSource;
    let filter = config.capture_filter();
    info!(filter = %filter, "opening WinDivert capture (admin required)");
    let source = WinDivertSource::open(&filter, config.game_port, config.capture.buffer_size)?;
    Ok(Box::new(source))
}

#[cfg(not(all(windows, feature = "windivert-backend")))]
fn build_source(_config: &Config) -> Result<Box<dyn PacketSource>> {
    Err(crate::Error::Capture(
        "no capture backend compiled — enable the `windivert-backend` feature on Windows"
            .to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::control::Limits;
    use crate::domain::filter::Filter;

    #[test]
    fn parse_command_maps_aliases() {
        assert_eq!(parse_command("start"), Some(Command::Start));
        assert_eq!(parse_command("on"), Some(Command::Start));
        assert_eq!(parse_command("stop"), Some(Command::Stop));
        assert_eq!(parse_command("off"), Some(Command::Stop));
        assert_eq!(parse_command("resume"), Some(Command::Resume));
        assert_eq!(parse_command("r"), Some(Command::Resume));
        assert_eq!(parse_command("toggle"), Some(Command::Toggle));
        assert_eq!(parse_command("t"), Some(Command::Toggle));
        assert_eq!(parse_command(""), Some(Command::Toggle));
    }

    #[test]
    fn parse_command_trims_and_ignores_case() {
        assert_eq!(parse_command("  START \t"), Some(Command::Start));
        assert_eq!(parse_command("Resume"), Some(Command::Resume));
    }

    #[test]
    fn parse_command_rejects_unknown() {
        assert_eq!(parse_command("refresh"), None);
        assert_eq!(parse_command("sta rt"), None);
    }

    #[test]
    fn gate_follows_controller_status() {
        let gate = WatchGate::new(false);
        let mut ctrl = Controller::new(Filter::default(), Limits::default());
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
        let controller = Mutex::new(Controller::new(Filter::default(), Limits::default()));

        on_command(&controller, &gate, Command::Toggle, 0); // Idle -> Start
        assert_eq!(controller.lock().unwrap().status(), Status::Watching);
        assert!(gate.is_enabled());

        on_command(&controller, &gate, Command::Toggle, 1); // Watching -> Stop
        assert_eq!(
            controller.lock().unwrap().status(),
            Status::Stopped(StopReason::PlayerStopped)
        );
        assert!(!gate.is_enabled());

        on_command(&controller, &gate, Command::Toggle, 2); // Stopped -> Start
        // Default filter matches the default item -> Paused.
        dispatch(
            &controller,
            &gate,
            Event::Snapshot {
                snapshot: one_item_shop(),
                now_ms: 3,
            },
        );
        on_command(&controller, &gate, Command::Toggle, 4); // Paused -> Stop
        assert_eq!(
            controller.lock().unwrap().status(),
            Status::Stopped(StopReason::PlayerStopped)
        );
        assert!(!gate.is_enabled());
    }

    #[test]
    fn ignored_command_leaves_state_and_gate_unchanged() {
        let gate = WatchGate::new(false);
        let controller = Mutex::new(Controller::new(Filter::default(), Limits::default()));
        on_command(&controller, &gate, Command::Start, 0);
        dispatch(
            &controller,
            &gate,
            Event::Snapshot {
                snapshot: one_item_shop(),
                now_ms: 1,
            },
        );
        assert_eq!(controller.lock().unwrap().status(), Status::Paused);

        // `start` mid-session is ignored by the controller: still Paused,
        // gate still on, counters untouched.
        on_command(&controller, &gate, Command::Start, 2);
        assert_eq!(controller.lock().unwrap().status(), Status::Paused);
        assert_eq!(controller.lock().unwrap().progress().matches_found, 1);
        assert!(gate.is_enabled());
    }
}
