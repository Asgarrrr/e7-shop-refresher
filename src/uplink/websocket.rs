//! WebSocket client: streams the raw bytes, receives server messages.
//!
//! The connection re-establishes automatically (capped exponential backoff). A
//! closed outbound channel signals a clean shutdown: reconnection then stops.

use std::time::Duration;

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tracing::{debug, info, warn};

use super::protocol::ServerMessage;

/// Outcome of one connection session.
enum Outcome {
    /// The outbound channel is closed: shutdown requested.
    Shutdown,
    /// The link dropped: reconnection expected.
    Disconnected,
}

/// Connection loop, to be spawned in its own task.
///
/// - `outbound`: raw byte batches to send (closing it stops the loop).
/// - `inbound`: decoded messages received from the server.
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
                info!(url = %url, "server link established");
                backoff = floor;
                match pump(stream, &mut outbound, &inbound).await {
                    Outcome::Shutdown => return,
                    Outcome::Disconnected => warn!("server link interrupted"),
                }
            }
            Err(err) => warn!(url = %url, error = %err, "server connection failed"),
        }

        if outbound.is_closed() {
            return;
        }
        // Keep draining `outbound` (discarding) during backoff: otherwise the
        // channel fills, reassembly blocks, and the stall propagates back to the
        // capture thread — the kernel then drops packets, creating real gaps that
        // can never be filled. Better to drop bytes while the server is
        // unreachable (it resyncs on reconnect).
        if drain_until(&mut outbound, backoff).await == Drained::Closed {
            return;
        }
        backoff = (backoff * 2).min(max_backoff);
    }
}

#[derive(PartialEq, Eq)]
enum Drained {
    /// The delay elapsed; the channel is still open.
    Elapsed,
    /// The outbound channel closed: shutdown requested.
    Closed,
}

/// Absorbs and discards outbound batches for `wait`, without stalling upstream.
async fn drain_until(outbound: &mut mpsc::Receiver<Vec<u8>>, wait: Duration) -> Drained {
    let deadline = tokio::time::sleep(wait);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return Drained::Elapsed,
            batch = outbound.recv() => {
                if batch.is_none() {
                    return Drained::Closed;
                }
                // batch dropped: server unreachable.
            }
        }
    }
}

/// Pumps outbound bytes and inbound messages over one connection.
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
                Some(Ok(_)) => {} // ping/pong/frame: handled by the library.
                Some(Err(err)) => {
                    warn!(error = %err, "WebSocket read error");
                    return Outcome::Disconnected;
                }
            },
        }
    }
}

/// Decodes a server message and pushes it downstream (undecodable ones dropped).
async fn forward(payload: &[u8], inbound: &mpsc::Sender<ServerMessage>) {
    match serde_json::from_slice::<ServerMessage>(payload) {
        Ok(message) => {
            let _ = inbound.send(message).await;
        }
        Err(err) => debug!(error = %err, "unrecognized server message, ignored"),
    }
}
