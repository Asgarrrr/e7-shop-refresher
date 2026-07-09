//! Orchestration : capture → réassemblage → gate → uplink → affichage.

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::capture::{Direction, PacketSource, Segment};
use crate::config::ForwardConfig;
use crate::stream::Reassembler;
use crate::uplink::protocol::{Alert, ItemKind, ServerMessage, ShopItem, ShopSnapshot};
use crate::uplink::WebSocketUplink;
use crate::watch::WatchGate;
use crate::{Config, Result};

/// Lance le relais et bloque jusqu'à l'arrêt (Ctrl+C ou fin de flux).
pub async fn run(config: Config) -> Result<()> {
    let gate = WatchGate::new(true);

    let (segment_tx, segment_rx) = mpsc::channel::<Segment>(8_192);
    let (raw_tx, raw_rx) = mpsc::channel::<Vec<u8>>(1_024);
    let (message_tx, message_rx) = mpsc::channel::<ServerMessage>(256);

    // Capture bloquante sur un thread dédié (WinDivert::recv est synchrone).
    let source = build_source(&config)?;
    let capture_gate = gate.clone();
    std::thread::Builder::new()
        .name("capture".to_owned())
        .spawn(move || capture_loop(source, segment_tx, capture_gate))?;

    // Liaison serveur avec reconnexion automatique.
    tokio::spawn(WebSocketUplink::run(
        config.server_url.clone(),
        raw_rx,
        message_tx,
        config.reconnect_initial(),
        config.reconnect_max(),
    ));

    // Réassemblage + filtrage des directions à transmettre.
    tokio::spawn(reassemble_loop(segment_rx, raw_tx, config.forward.clone()));

    // Contrôle interactif de l'interrupteur Shop Watch.
    tokio::spawn(control_loop(gate.clone()));

    info!(server = %config.server_url, "relais démarré — Shop Watch actif");
    print_controls();

    display_loop(message_rx).await;
    info!("arrêt du relais");
    Ok(())
}

/// Consomme les segments capturés, les réassemble, transmet le flux ordonné.
async fn reassemble_loop(
    mut segments: mpsc::Receiver<Segment>,
    raw_tx: mpsc::Sender<Vec<u8>>,
    forward: ForwardConfig,
) {
    let mut reassembler = Reassembler::new();
    while let Some(segment) = segments.recv().await {
        let direction = segment.direction;
        let ordered = reassembler.push(&segment);
        if ordered.is_empty() || !should_forward(direction, &forward) {
            continue;
        }
        if raw_tx.send(ordered).await.is_err() {
            break; // uplink arrêté.
        }
    }
}

fn should_forward(direction: Direction, forward: &ForwardConfig) -> bool {
    match direction {
        Direction::ServerToClient => forward.server_to_client,
        Direction::ClientToServer => forward.client_to_server,
    }
}

/// Boucle de capture (contexte synchrone). S'arrête si le pipeline se ferme.
fn capture_loop(mut source: Box<dyn PacketSource>, tx: mpsc::Sender<Segment>, gate: WatchGate) {
    loop {
        match source.next_segment() {
            Ok(segment) => {
                if !gate.is_enabled() {
                    continue; // Shop Watch éteint : on n'émet rien.
                }
                if tx.blocking_send(segment).is_err() {
                    break;
                }
            }
            Err(err) => {
                error!(error = %err, "capture interrompue");
                break;
            }
        }
    }
}

/// Lit les commandes clavier pour piloter l'interrupteur Shop Watch.
async fn control_loop(gate: WatchGate) {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match line.trim().to_ascii_lowercase().as_str() {
            "" | "t" | "toggle" => {
                let on = gate.toggle();
                println!(">> Shop Watch {}", if on { "ACTIVÉ" } else { "éteint" });
            }
            "on" | "start" => {
                gate.set(true);
                println!(">> Shop Watch ACTIVÉ");
            }
            "off" | "stop" => {
                gate.set(false);
                println!(">> Shop Watch éteint");
            }
            other => println!(">> commande inconnue : {other:?} (entrée=toggle, on, off)"),
        }
    }
}

/// Affiche les messages du serveur jusqu'à Ctrl+C ou fermeture de la liaison.
async fn display_loop(mut messages: mpsc::Receiver<ServerMessage>) {
    tokio::select! {
        _ = async {
            while let Some(message) = messages.recv().await {
                render(&message);
            }
        } => {}
        _ = tokio::signal::ctrl_c() => println!("\n>> Ctrl+C — arrêt"),
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
    println!("\n── {merchant} ──");
    for item in &snapshot.slots {
        println!("  {}", format_item(item));
    }
}

fn render_alert(alert: &Alert) {
    println!("\n★★★ ALERTE ★★★  {}", alert.message);
    for item in &alert.items {
        println!("  → {}", format_item(item));
    }
}

fn format_item(item: &ShopItem) -> String {
    let mark = if item.interesting { "★" } else { " " };
    let kind = match item.kind {
        ItemKind::Equipment => "équipement",
        ItemKind::Hero => "héros",
        ItemKind::Token => "jeton",
        ItemKind::Unknown => "?",
    };

    let mut line = format!("[{mark}] slot {} · {kind}", item.slot);
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
        line.push_str(&format!(" · {price} or"));
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
    println!("Commandes : [Entrée] bascule Shop Watch · `on` · `off` · Ctrl+C pour quitter");
}

#[cfg(all(windows, feature = "windivert-backend"))]
fn build_source(config: &Config) -> Result<Box<dyn PacketSource>> {
    use crate::capture::WinDivertSource;
    let filter = config.capture_filter();
    info!(filter = %filter, "ouverture de la capture WinDivert (admin requis)");
    let source = WinDivertSource::open(&filter, config.game_port, config.capture.buffer_size)?;
    Ok(Box::new(source))
}

#[cfg(not(all(windows, feature = "windivert-backend")))]
fn build_source(_config: &Config) -> Result<Box<dyn PacketSource>> {
    Err(crate::Error::Capture(
        "aucun backend de capture compilé — activez la feature `windivert-backend` sur Windows"
            .to_owned(),
    ))
}
