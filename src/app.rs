//! Orchestration: capture -> reassembly -> gate -> uplink -> controller -> display.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::capture::{Direction, PacketSource, Segment};
use crate::config::ForwardConfig;
use crate::domain::control::{Action, Controller, Event, Limits, Status, StopReason};
use crate::domain::filter::Filter;
use crate::domain::shop::{ItemKind, ShopItem, ShopSnapshot};
use crate::stream::Reassembler;
use crate::uplink::protocol::{PurchaseNotice, ServerMessage};
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

/// A player command, decoupled from its source: the stdin task and the GUI
/// push the same values through the same channel (stdin never produces the
/// `Set*` variants).
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Start,
    Stop,
    /// Start or stop, depending on the current status.
    Toggle,
    /// Live filter retune; applies from the next new shop.
    SetFilter(Filter),
    /// Live limits retune; checked before the next refresh.
    SetLimits(Limits),
}

/// One journal entry: a console line with its session-relative timestamp.
#[derive(Debug, Clone)]
pub struct LogLine {
    pub at_ms: u64,
    pub text: String,
}

/// Oldest entries drop out first: a session left running for hours must not
/// grow the journal without bound.
const JOURNAL_CAP: usize = 500;

/// Bounded session journal: the same lines the console prints, kept for a
/// view. The session loop writes, readers copy entries out.
#[derive(Clone, Default)]
pub struct EventLog {
    entries: Arc<Mutex<VecDeque<LogLine>>>,
}

impl EventLog {
    pub fn push(&self, at_ms: u64, lines: &[String]) {
        if lines.is_empty() {
            return;
        }
        let mut entries = self.entries.lock().expect("journal mutex poisoned");
        for text in lines {
            entries.push_back(LogLine {
                at_ms,
                text: text.clone(),
            });
        }
        while entries.len() > JOURNAL_CAP {
            entries.pop_front();
        }
    }

    pub fn entries(&self) -> Vec<LogLine> {
        self.entries
            .lock()
            .expect("journal mutex poisoned")
            .iter()
            .cloned()
            .collect()
    }
}

/// Cheap clones of the shared session state, for a view (the GUI) running
/// beside the session loop: read `status()`/`progress()`/`last_snapshot()`/
/// `checklist()` under short locks, send [`Command`]s, read the journal.
pub struct SessionHandles {
    pub controller: Arc<Mutex<Controller>>,
    pub commands: mpsc::Sender<Command>,
    pub gate: WatchGate,
    pub journal: EventLog,
}

/// The owned half of [`setup`]: everything the relay pipeline consumes.
pub struct Session {
    config: Config,
    controller: Arc<Mutex<Controller>>,
    gate: WatchGate,
    journal: EventLog,
    command_tx: mpsc::Sender<Command>,
    command_rx: mpsc::Receiver<Command>,
}

/// Builds the shared session state and hands out clones before any fallible
/// work runs: a view keeps live handles even when [`Session::run`] fails
/// later (bad filter, no capture backend).
pub fn setup(config: Config) -> (Session, SessionHandles) {
    // Gate off at startup: the session starts Idle and the player arms it
    // with `start`.
    let gate = WatchGate::new(false);
    let journal = EventLog::default();
    let (command_tx, command_rx) = mpsc::channel::<Command>(16);
    let controller = Arc::new(Mutex::new(Controller::new(
        config.filter.clone(),
        config.limits.clone(),
    )));
    let handles = SessionHandles {
        controller: Arc::clone(&controller),
        commands: command_tx.clone(),
        gate: gate.clone(),
        journal: journal.clone(),
    };
    let session = Session {
        config,
        controller,
        gate,
        journal,
        command_tx,
        command_rx,
    };
    (session, handles)
}

/// Console-only entry point: [`setup`] + [`Session::run`], discarding the
/// view handles.
pub async fn run(config: Config) -> Result<()> {
    // The console has no filter editor: an unrestricted filter can only be
    // fixed in config.toml, so fail fast. The GUI path (setup +
    // `Session::run`) boots instead and refuses arming until a filter is set.
    if config.filter.is_unrestricted() {
        return Err(crate::Error::Config(
            "no [filter] criteria in config.toml — define what to hunt (see config.example.toml)"
                .to_owned(),
        ));
    }
    let (session, _handles) = setup(config);
    session.run().await
}

impl Session {
    /// Runs the relay and blocks until shutdown (Ctrl+C or end of stream).
    pub async fn run(self) -> Result<()> {
        let Self {
            config,
            controller,
            gate,
            journal,
            command_tx,
            command_rx,
        } = self;
        let (segment_tx, segment_rx) = mpsc::channel::<CaptureEvent>(8_192);
        let (raw_tx, raw_rx) = mpsc::channel::<Vec<u8>>(1_024);
        let (message_tx, message_rx) = mpsc::channel::<ServerMessage>(256);

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

        info!(server = %config.server_url, "relay started — idle, `start` arms the watch");
        print_controls();

        session_loop(&controller, &gate, &journal, command_rx, message_rx).await;
        info!("relay stopped");
        Ok(())
    }
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
                ">> unknown command: {:?} (start, stop, enter = toggle)",
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
            _ = ticker.tick() => {
                let now_ms = now_ms();
                dispatch(controller, gate, journal, Event::Tick { now_ms }, now_ms);
            }
            _ = &mut ctrl_c => {
                println!("\n>> Ctrl+C, stopping");
                break;
            }
        }
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
    journal.push(now_ms, &lines);
    for line in lines {
        println!("{line}");
    }
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
            let actions = ctrl.handle(Event::FilterChanged(filter));
            let mut lines = apply(&actions, &ctrl, gate);
            lines.push(">> filter updated — applies from the next shop".to_owned());
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
                now_ms,
            );
        }
        ServerMessage::Purchase(notice) => {
            let lines = handle_purchase(controller, gate, &notice, now_ms);
            journal.push(now_ms, &lines);
            for line in lines {
                println!("{line}");
            }
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
fn dispatch(
    controller: &Mutex<Controller>,
    gate: &WatchGate,
    journal: &EventLog,
    event: Event,
    now_ms: u64,
) {
    let mut ctrl = controller.lock().expect("controller mutex poisoned");
    let actions = ctrl.handle(event);
    let lines = apply(&actions, &ctrl, gate);
    drop(ctrl);
    journal.push(now_ms, &lines);
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

pub(crate) fn status_label(controller: &Controller) -> &'static str {
    match controller.status() {
        Status::Idle => "idle (`start` arms the watch)",
        Status::Watching => "watching",
        // An empty checklist never auto-resumes.
        Status::Paused if controller.checklist().is_empty() => "paused (buy, then refresh)",
        Status::Paused => "paused (buy — auto-resumes)",
        Status::Stopped(_) => "stopped (`start` re-arms)",
    }
}

pub(crate) fn describe(reason: StopReason) -> &'static str {
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

/// Player-facing label for an item kind — shared by the console line and the
/// GUI table.
pub(crate) fn kind_label(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Equipment => "equipment",
        ItemKind::Hero => "hero",
        ItemKind::Token => "token",
        ItemKind::Unknown => "?",
    }
}

pub(crate) fn format_item(item: &ShopItem) -> String {
    let kind = kind_label(item.kind);

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
    println!("Commands: start, stop, [Enter] toggle, Ctrl+C to quit");
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

    #[tokio::test]
    async fn run_refuses_unrestricted_filter() {
        let err = run(Config::default()).await.expect_err("must refuse");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn setup_starts_idle_with_gate_off() {
        let (_session, handles) = setup(Config::default());
        assert_eq!(handles.controller.lock().unwrap().status(), Status::Idle);
        assert!(!handles.gate.is_enabled());
        assert!(handles.journal.entries().is_empty());
        // The command channel is wired before the fallible pipeline runs.
        handles
            .commands
            .try_send(Command::Toggle)
            .expect("channel open");
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
    fn kind_label_names_each_kind() {
        assert_eq!(kind_label(ItemKind::Equipment), "equipment");
        assert_eq!(kind_label(ItemKind::Hero), "hero");
        assert_eq!(kind_label(ItemKind::Token), "token");
        assert_eq!(kind_label(ItemKind::Unknown), "?");
    }

    #[test]
    fn journal_receives_command_lines() {
        let gate = WatchGate::new(false);
        let journal = EventLog::default();
        let controller = Mutex::new(Controller::new(match_default_filter(), Limits::default()));
        on_command(&controller, &gate, &journal, Command::Start, 1_000);
        let entries = journal.entries();
        assert!(entries.iter().any(|line| line.text.contains("watching")));
        assert!(entries.iter().all(|line| line.at_ms == 1_000));
    }

    #[test]
    fn journal_caps_entries() {
        let journal = EventLog::default();
        for i in 0..(JOURNAL_CAP as u64 + 100) {
            journal.push(i, &[format!("line {i}")]);
        }
        let entries = journal.entries();
        assert_eq!(entries.len(), JOURNAL_CAP);
        assert_eq!(entries.first().unwrap().text, "line 100");
        assert_eq!(
            entries.last().unwrap().text,
            format!("line {}", JOURNAL_CAP + 99)
        );
    }

    #[test]
    fn parse_command_maps_aliases() {
        assert_eq!(parse_command("start"), Some(Command::Start));
        assert_eq!(parse_command("on"), Some(Command::Start));
        assert_eq!(parse_command("stop"), Some(Command::Stop));
        assert_eq!(parse_command("off"), Some(Command::Stop));
        assert_eq!(parse_command("toggle"), Some(Command::Toggle));
        assert_eq!(parse_command("t"), Some(Command::Toggle));
        assert_eq!(parse_command(""), Some(Command::Toggle));
    }

    #[test]
    fn parse_command_trims_and_ignores_case() {
        assert_eq!(parse_command("  START \t"), Some(Command::Start));
        assert_eq!(parse_command("Stop"), Some(Command::Stop));
    }

    #[test]
    fn parse_command_rejects_unknown() {
        assert_eq!(parse_command("refresh"), None);
        assert_eq!(parse_command("sta rt"), None);
        // The skip command is gone: buying (or a fresh shop) is the only
        // way out of a pause.
        assert_eq!(parse_command("resume"), None);
        assert_eq!(parse_command("r"), None);
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
            3,
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
        let mut ctrl = Controller::new(Filter::default(), Limits::default());
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
            1,
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
            2,
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
            1,
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
