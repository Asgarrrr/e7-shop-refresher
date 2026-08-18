//! WebSocket client: streams the raw bytes, receives server messages.
//!
//! The connection re-establishes automatically (capped exponential backoff). A
//! closed outbound channel signals a clean shutdown: reconnection then stops.

use std::future::Future;
use std::time::Duration;

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tracing::{debug, info, warn};

use crate::config::RECONNECT_FLOOR;
use crate::stream::BudgetedChunk;

use super::UplinkEvent;
use super::protocol::ServerMessage;

/// A send that cannot finish within this window means a stalled peer (TCP
/// zero-window): the socket stays "connected" but nothing moves, backpressure
/// parks the capture thread, and the kernel starts dropping packet copies.
/// Dropping the connection turns the stall into a normal reconnect cycle.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Normalized, bounded exponential reconnect delay.
struct Backoff {
    initial: Duration,
    current: Duration,
    max: Duration,
}

impl Backoff {
    fn new(initial: Duration, max: Duration) -> Self {
        let initial = initial.max(RECONNECT_FLOOR);
        let max = max.max(initial);
        Self {
            initial,
            current: initial,
            max,
        }
    }

    fn current(&self) -> Duration {
        self.current
    }

    fn advance(&mut self) {
        self.current = self
            .current
            .checked_mul(2)
            .unwrap_or(self.max)
            .min(self.max);
    }

    fn reset(&mut self) {
        self.current = self.initial;
    }
}

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
    outbound: mpsc::Receiver<BudgetedChunk>,
    inbound: mpsc::Sender<UplinkEvent>,
    initial_backoff: Duration,
    max_backoff: Duration,
) {
    run_with_connector(
        url,
        outbound,
        inbound,
        initial_backoff,
        max_backoff,
        |url| async move { connect_async(url).await.map(|(stream, _response)| stream) },
    )
    .await;
}

async fn run_with_connector<C, F, S>(
    url: String,
    mut outbound: mpsc::Receiver<BudgetedChunk>,
    inbound: mpsc::Sender<UplinkEvent>,
    initial_backoff: Duration,
    max_backoff: Duration,
    mut connect: C,
) where
    C: FnMut(String) -> F,
    F: Future<Output = Result<S, WsError>>,
    S: Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
{
    let mut backoff = Backoff::new(initial_backoff, max_backoff);
    // The player only hears transitions: the first failure reports the outage,
    // each retry stays a tracing detail, recovery reports once.
    let mut outage_reported = false;

    loop {
        match connect(url.clone()).await {
            Ok(stream) => {
                info!(url = %url, "server link established");
                if std::mem::take(&mut outage_reported) {
                    let _ = inbound.send(UplinkEvent::LinkUp).await;
                }
                backoff.reset();
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
        if drain_until(&mut outbound, backoff.current()).await {
            return; // outbound closed: shutdown requested.
        }
        backoff.advance();
    }
}

/// Absorbs and discards outbound batches for `wait`, without stalling upstream.
/// Returns `true` if the outbound channel closed (shutdown), `false` if the
/// delay simply elapsed.
async fn drain_until(outbound: &mut mpsc::Receiver<BudgetedChunk>, wait: Duration) -> bool {
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
    outbound: &mut mpsc::Receiver<BudgetedChunk>,
    inbound: &mpsc::Sender<UplinkEvent>,
) -> Outcome
where
    S: Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
{
    let (mut write, mut read) = stream.split();

    loop {
        tokio::select! {
            outgoing = outbound.recv() => match outgoing {
                Some(chunk) => {
                    let (bytes, lease) = chunk.into_parts();
                    let send = write.send(Message::Binary(bytes.into()));
                    match tokio::time::timeout(SEND_TIMEOUT, send).await {
                        Ok(Ok(())) => {}
                        Ok(Err(_)) => return Outcome::Disconnected,
                        Err(_elapsed) => {
                            warn!("server send stalled — dropping the connection");
                            return Outcome::Disconnected;
                        }
                    }
                    drop(lease);
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
    use std::future::ready;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use super::*;
    use crate::stream::PipelineBudget;

    fn chunk(bytes: Vec<u8>) -> BudgetedChunk {
        PipelineBudget::new().admit_outbound_for_test(bytes)
    }

    #[test]
    fn backoff_normalizes_floor_and_cap() {
        let mut backoff = Backoff::new(Duration::from_millis(100), Duration::from_millis(350));
        assert_eq!(backoff.current(), Duration::from_millis(100));
        backoff.advance();
        assert_eq!(backoff.current(), Duration::from_millis(200));
        backoff.advance();
        assert_eq!(backoff.current(), Duration::from_millis(350));
        backoff.advance();
        assert_eq!(backoff.current(), Duration::from_millis(350));
        backoff.reset();
        assert_eq!(backoff.current(), Duration::from_millis(100));

        let mut below_floor = Backoff::new(Duration::from_millis(1), Duration::from_millis(10));
        assert_eq!(below_floor.current(), RECONNECT_FLOOR);
        below_floor.advance();
        assert_eq!(below_floor.current(), RECONNECT_FLOOR);
    }

    #[test]
    fn backoff_growth_caps_without_overflow() {
        let near_max = Duration::MAX - Duration::from_nanos(1);
        let mut backoff = Backoff::new(near_max, Duration::MAX);
        assert_eq!(backoff.current(), near_max);
        backoff.advance();
        assert_eq!(backoff.current(), Duration::MAX);
        backoff.advance();
        assert_eq!(backoff.current(), Duration::MAX);
        backoff.reset();
        assert_eq!(backoff.current(), near_max);
    }

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
    async fn drain_until_discards_batches_until_deadline() {
        let (raw_tx, mut raw_rx) = mpsc::channel::<BudgetedChunk>(1);
        let drain =
            tokio::spawn(async move { drain_until(&mut raw_rx, Duration::from_millis(100)).await });

        raw_tx.send(chunk(vec![1])).await.unwrap();
        raw_tx.send(chunk(vec![2])).await.unwrap();
        tokio::time::advance(Duration::from_millis(99)).await;
        assert!(!drain.is_finished());
        raw_tx.send(chunk(vec![3])).await.unwrap();
        raw_tx.send(chunk(vec![4])).await.unwrap();
        assert!(!drain.is_finished());

        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert!(!drain.await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn drain_until_reports_closed_outbound() {
        let (raw_tx, mut raw_rx) = mpsc::channel::<BudgetedChunk>(1);
        drop(raw_tx);

        assert!(drain_until(&mut raw_rx, Duration::from_secs(1)).await);
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_drain_releases_all_outbound_bytes() {
        let budget = PipelineBudget::with_test_limits(128, 128, 128, 128);
        let (raw_tx, mut raw_rx) = mpsc::channel::<BudgetedChunk>(4);
        raw_tx
            .send(budget.admit_outbound_for_test(vec![1, 2, 3]))
            .await
            .unwrap();
        raw_tx
            .send(budget.admit_outbound_for_test(vec![4, 5, 6]))
            .await
            .unwrap();
        assert_eq!(budget.snapshot().current_outbound, 6);

        let drain =
            tokio::spawn(async move { drain_until(&mut raw_rx, Duration::from_millis(100)).await });
        tokio::task::yield_now().await;
        assert_eq!(budget.snapshot().current_total, 0);
        tokio::time::advance(Duration::from_millis(100)).await;
        assert!(!drain.await.unwrap());
        assert!(budget.snapshot().high_water_total <= 128);
    }

    #[tokio::test(start_paused = true)]
    async fn run_retries_on_normalized_schedule_without_network() {
        let (raw_tx, raw_rx) = mpsc::channel::<BudgetedChunk>(1);
        let (event_tx, mut event_rx) = mpsc::channel::<UplinkEvent>(4);
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let recorded_attempts = Arc::clone(&attempts);
        let started = tokio::time::Instant::now();

        let task = tokio::spawn(run_with_connector(
            "ws://test.invalid".to_owned(),
            raw_rx,
            event_tx,
            Duration::from_millis(1),
            Duration::from_millis(10),
            move |_url| {
                recorded_attempts
                    .lock()
                    .unwrap()
                    .push(tokio::time::Instant::now().duration_since(started));
                ready(Err::<StalledLink, _>(WsError::ConnectionClosed))
            },
        ));

        tokio::task::yield_now().await;
        assert_eq!(*attempts.lock().unwrap(), [Duration::ZERO]);
        raw_tx.send(chunk(vec![1])).await.unwrap();
        raw_tx.send(chunk(vec![2])).await.unwrap();

        tokio::time::advance(Duration::from_millis(99)).await;
        tokio::task::yield_now().await;
        assert_eq!(*attempts.lock().unwrap(), [Duration::ZERO]);
        raw_tx.send(chunk(vec![3])).await.unwrap();

        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            *attempts.lock().unwrap(),
            [Duration::ZERO, Duration::from_millis(100)]
        );
        raw_tx.send(chunk(vec![4])).await.unwrap();

        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            *attempts.lock().unwrap(),
            [
                Duration::ZERO,
                Duration::from_millis(100),
                Duration::from_millis(200)
            ]
        );

        assert!(matches!(
            event_rx.recv().await,
            Some(UplinkEvent::LinkDown(_))
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        drop(raw_tx);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn run_stops_cleanly_when_outbound_closes_while_connected() {
        let (raw_tx, raw_rx) = mpsc::channel::<BudgetedChunk>(1);
        let (event_tx, mut event_rx) = mpsc::channel::<UplinkEvent>(4);
        let attempts = Arc::new(Mutex::new(0));
        let recorded_attempts = Arc::clone(&attempts);

        let task = tokio::spawn(run_with_connector(
            "ws://test.invalid".to_owned(),
            raw_rx,
            event_tx,
            Duration::from_millis(100),
            Duration::from_secs(1),
            move |_url| {
                *recorded_attempts.lock().unwrap() += 1;
                ready(Ok::<_, WsError>(StalledLink))
            },
        ));

        tokio::task::yield_now().await;
        assert_eq!(*attempts.lock().unwrap(), 1);
        drop(raw_tx);
        tokio::task::yield_now().await;
        assert!(task.is_finished());
        task.await.unwrap();
        assert_eq!(*attempts.lock().unwrap(), 1);
        assert!(matches!(
            event_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_send_releases_outbound_bytes_after_timeout() {
        let budget = PipelineBudget::with_test_limits(64, 64, 64, 64);
        let (raw_tx, mut raw_rx) = mpsc::channel::<BudgetedChunk>(4);
        let (event_tx, _event_rx) = mpsc::channel::<UplinkEvent>(4);
        raw_tx
            .send(budget.admit_outbound_for_test(vec![1, 2, 3]))
            .await
            .unwrap();
        assert_eq!(budget.snapshot().current_outbound, 3);

        let outcome = pump(StalledLink, &mut raw_rx, &event_tx).await;
        assert!(matches!(outcome, Outcome::Disconnected));
        assert_eq!(budget.snapshot().current_total, 0);
        assert!(budget.snapshot().high_water_total <= 64);
    }
}
