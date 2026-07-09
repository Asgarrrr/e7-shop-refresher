//! Client WebSocket : streame les octets bruts, reçoit les messages du serveur.
//!
//! La connexion se rétablit automatiquement (backoff exponentiel plafonné). Le
//! canal sortant fermé signale un arrêt propre : on cesse alors de reconnecter.

use std::time::Duration;

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tracing::{debug, info, warn};

use super::protocol::ServerMessage;

/// Issue d'une session de connexion.
enum Outcome {
    /// Le canal sortant est fermé : arrêt demandé.
    Shutdown,
    /// La liaison est tombée : reconnexion attendue.
    Disconnected,
}

pub struct WebSocketUplink;

impl WebSocketUplink {
    /// Boucle de connexion, à lancer dans une tâche dédiée.
    ///
    /// - `outbound` : lots d'octets bruts à transmettre (fermer ⇒ arrêt).
    /// - `inbound`  : messages décodés reçus du serveur.
    pub async fn run(
        url: String,
        mut outbound: mpsc::Receiver<Vec<u8>>,
        inbound: mpsc::Sender<ServerMessage>,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) {
        let floor = initial_backoff.max(Duration::from_millis(100));
        let mut backoff = floor;

        loop {
            match connect_async(&url).await {
                Ok((stream, _response)) => {
                    info!(url = %url, "liaison serveur établie");
                    backoff = floor;
                    match pump(stream, &mut outbound, &inbound).await {
                        Outcome::Shutdown => return,
                        Outcome::Disconnected => warn!("liaison serveur interrompue"),
                    }
                }
                Err(err) => warn!(url = %url, error = %err, "connexion serveur échouée"),
            }

            if outbound.is_closed() {
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(max_backoff);
        }
    }
}

/// Fait circuler les octets sortants et les messages entrants sur une connexion.
async fn pump<S>(
    stream: S,
    outbound: &mut mpsc::Receiver<Vec<u8>>,
    inbound: &mpsc::Sender<ServerMessage>,
) -> Outcome
where
    S: Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
{
    let (mut write, mut read) = stream.split();

    loop {
        tokio::select! {
            outgoing = outbound.recv() => match outgoing {
                Some(bytes) => {
                    if write.send(Message::Binary(bytes.into())).await.is_err() {
                        return Outcome::Disconnected;
                    }
                }
                None => return Outcome::Shutdown,
            },
            incoming = read.next() => match incoming {
                Some(Ok(Message::Text(text))) => forward(text.as_str().as_bytes(), inbound).await,
                Some(Ok(Message::Binary(bytes))) => forward(&bytes, inbound).await,
                Some(Ok(Message::Close(_))) | None => return Outcome::Disconnected,
                Some(Ok(_)) => {} // ping/pong/frame : géré par la lib.
                Some(Err(err)) => {
                    warn!(error = %err, "erreur de lecture WebSocket");
                    return Outcome::Disconnected;
                }
            },
        }
    }
}

/// Décode un message serveur et le pousse en aval (les indéchiffrables sont ignorés).
async fn forward(payload: &[u8], inbound: &mpsc::Sender<ServerMessage>) {
    match serde_json::from_slice::<ServerMessage>(payload) {
        Ok(message) => {
            let _ = inbound.send(message).await;
        }
        Err(err) => debug!(error = %err, "message serveur non reconnu, ignoré"),
    }
}
