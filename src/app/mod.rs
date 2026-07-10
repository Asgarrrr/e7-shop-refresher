//! Orchestration: capture -> reassembly -> gate -> uplink -> controller -> display.

mod session;

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::capture::{Direction, PacketSource, Segment};
use crate::config::ForwardConfig;
use crate::domain::control::{Controller, Limits};
use crate::domain::filter::Filter;
use crate::journal::EventLog;
use crate::render::print_controls;
use crate::stream::Reassembler;
use crate::uplink::protocol::ServerMessage;
use crate::watch::WatchGate;
use crate::{Config, Result};

use session::session_loop;

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
    use crate::domain::control::Status;

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
}
