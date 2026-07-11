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

use super::UplinkEvent;
use super::protocol::ServerMessage;

/// A send that cannot finish within this window means a stalled peer (TCP
/// zero-window): the socket stays "connected" but nothing moves, backpressure
/// parks the capture thread, and the kernel starts dropping packet copies.
/// Dropping the connection turns the stall into a normal reconnect cycle.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

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
    inbound: mpsc::Sender<UplinkEvent>,
    initial_backoff: Duration,
    max_backoff: Duration,
) {
    let floor = initial_backoff.max(Duration::from_millis(100));
    let mut backoff = floor;
    // The player only hears transitions: the first failure reports the outage,
    // each retry stays a tracing detail, recovery reports once.
    let mut outage_reported = false;

    loop {
        match connect_async(&url).await {
            Ok((stream, _response)) => {
                info!(url = %url, "server link established");
                if std::mem::take(&mut outage_reported) {
                    let _ = inbound.send(UplinkEvent::LinkUp).await;
                }
                backoff = floor;
                match pump(stream, &mut outbound, &inbound).await {
                    Outcome::Shutdown => return,
                    Outcome::Disconnected => {
                        warn!("server link interrupted");
                        outage_reported = true;
                        let _ = inbound
                            .send(UplinkEvent::LinkDown("connection interrupted".to_owned()))
                            .await;
                    }
                }
            }
            Err(err) => {
                warn!(url = %url, error = %err, "server connection failed");
                if !outage_reported {
                    outage_reported = true;
                    let _ = inbound.send(UplinkEvent::LinkDown(err.to_string())).await;
                }
            }
        }

        if outbound.is_closed() {
            return;
        }
        // Keep draining `outbound` (discarding) during backoff: otherwise the
        // channel fills, reassembly blocks, and the stall propagates back to the
        // capture thread — the kernel then drops packets, creating real gaps that
        // can never be filled. Better to drop bytes while the server is
        // unreachable (it resyncs on reconnect).
        if drain_until(&mut outbound, backoff).await {
            return; // outbound closed: shutdown requested.
        }
        backoff = (backoff * 2).min(max_backoff);
    }
}

/// Absorbs and discards outbound batches for `wait`, without stalling upstream.
/// Returns `true` if the outbound channel closed (shutdown), `false` if the
/// delay simply elapsed.
async fn drain_until(outbound: &mut mpsc::Receiver<Vec<u8>>, wait: Duration) -> bool {
    let deadline = tokio::time::sleep(wait);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return false,
            batch = outbound.recv() => {
                if batch.is_none() {
                    return true;
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
    inbound: &mpsc::Sender<UplinkEvent>,
) -> Outcome
where
    S: Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
{
    let (mut write, mut read) = stream.split();

    loop {
        tokio::select! {
            outgoing = outbound.recv() => match outgoing {
                Some(bytes) => {
                    let send = write.send(Message::Binary(bytes.into()));
                    match tokio::time::timeout(SEND_TIMEOUT, send).await {
                        Ok(Ok(())) => {}
                        Ok(Err(_)) => return Outcome::Disconnected,
                        Err(_elapsed) => {
                            warn!("server send stalled — dropping the connection");
                            return Outcome::Disconnected;
                        }
                    }
                }
                None => return Outcome::Shutdown,
            },
            incoming = read.next() => match incoming {
                Some(Ok(Message::Text(text))) => forward(text.as_bytes(), inbound).await,
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
async fn forward(payload: &[u8], inbound: &mpsc::Sender<UplinkEvent>) {
    match serde_json::from_slice::<ServerMessage>(payload) {
        Ok(message) => {
            let _ = inbound.send(UplinkEvent::Message(message)).await;
        }
        Err(err) => debug!(error = %err, "unrecognized server message, ignored"),
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use super::*;

    /// A connected-but-frozen peer: reads pend forever, sends never become
    /// ready (TCP zero-window).
    struct StalledLink;

    impl Stream for StalledLink {
        type Item = Result<Message, WsError>;
        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }

    impl Sink<Message> for StalledLink {
        type Error = WsError;
        fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
            Poll::Pending
        }
        fn start_send(self: Pin<&mut Self>, _item: Message) -> Result<(), WsError> {
            Ok(())
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
            Poll::Pending
        }
        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
            Poll::Pending
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_send_disconnects_instead_of_hanging() {
        let (raw_tx, mut raw_rx) = mpsc::channel::<Vec<u8>>(4);
        let (event_tx, _event_rx) = mpsc::channel::<UplinkEvent>(4);
        raw_tx.send(vec![1, 2, 3]).await.unwrap();

        let outcome = pump(StalledLink, &mut raw_rx, &event_tx).await;
        assert!(matches!(outcome, Outcome::Disconnected));
    }
}
