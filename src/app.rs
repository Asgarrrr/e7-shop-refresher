//! Orchestration: capture -> reassembly -> gate -> uplink -> display.

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::capture::{Direction, PacketSource, Segment};
use crate::config::ForwardConfig;
use crate::stream::Reassembler;
use crate::uplink::protocol::{Alert, ItemKind, ServerMessage, ShopItem, ShopSnapshot};
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

/// Runs the relay and blocks until shutdown (Ctrl+C or end of stream).
pub async fn run(config: Config) -> Result<()> {
    let gate = WatchGate::new(true);

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

    // Interactive control of the Shop Watch switch.
    tokio::spawn(control_loop(gate.clone()));

    info!(server = %config.server_url, "relay started — Shop Watch active");
    print_controls();

    display_loop(message_rx).await;
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

/// Reads keyboard commands to drive the Shop Watch switch.
async fn control_loop(gate: WatchGate) {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match line.trim().to_ascii_lowercase().as_str() {
            "" | "t" | "toggle" => {
                let on = gate.toggle();
                println!(">> Shop Watch {}", if on { "ON" } else { "OFF" });
            }
            "on" | "start" => {
                gate.set(true);
                println!(">> Shop Watch ON");
            }
            "off" | "stop" => {
                gate.set(false);
                println!(">> Shop Watch OFF");
            }
            other => println!(">> unknown command: {other:?} (enter = toggle, on, off)"),
        }
    }
}

/// Displays server messages until Ctrl+C or the link closes.
async fn display_loop(mut messages: mpsc::Receiver<ServerMessage>) {
    tokio::select! {
        _ = async {
            while let Some(message) = messages.recv().await {
                render(&message);
            }
        } => {}
        _ = tokio::signal::ctrl_c() => println!("\n>> Ctrl+C, stopping"),
    }
}

fn render(message: &ServerMessage) {
    match message {
        ServerMessage::Ack | ServerMessage::Unknown => {}
        ServerMessage::Shop(snapshot) => render_shop(snapshot),
        ServerMessage::Alert(alert) => render_alert(alert),
    }
}

fn render_shop(snapshot: &ShopSnapshot) {
    let merchant = snapshot.merchant.as_deref().unwrap_or("Secret Shop");
    println!("\n[{merchant}]");
    for item in &snapshot.slots {
        println!("  {}", format_item(item));
    }
}

fn render_alert(alert: &Alert) {
    println!("\n[ALERT] {}", alert.message);
    for item in &alert.items {
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
    if item.interesting {
        line.push_str(" (interesting)");
    }
    line
}

fn print_controls() {
    println!("Commands: [Enter] toggle Shop Watch, on, off, Ctrl+C to quit");
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
