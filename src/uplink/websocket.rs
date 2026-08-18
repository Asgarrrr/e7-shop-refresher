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

use crate::config::RECONNECT_FLOOR;
use crate::stream::BudgetedChunk;

use super::UplinkEvent;
use super::protocol::ServerMessage;

/// A send that cannot finish within this window means a stalled peer (TCP
/// zero-window): the socket stays "connected" but nothing moves, backpressure
/// parks the capture thread, and the kernel starts dropping packet copies.
/// Dropping the connection turns the stall into a normal reconnect cycle.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// The same class of stall as `SEND_TIMEOUT`, on the way in: a handshake that
/// opens but never completes (captive portal, a middlebox that accepts the SYN
/// and never speaks TLS, a resolver that never answers) has no upper bound of
/// its own, so the future simply stays pending forever.
///
/// Nothing else recovers that: no `LinkDown` is emitted, so the journal — the
/// only surface a windowed build has — stays empty, and the controller starts
/// `link_up: true` with no refresh issued, so the watchdog never arms an
/// expectation and its recovery ladder never runs. The relay looks armed and
/// forwards nothing, forever. Elapsed is reported and retried like any refused
/// connection.
///
/// 15 s sits above a TLS handshake on a bad connection and below Windows' own
/// ~21 s SYN-retry give-up, so a black-holed address is also reported sooner
/// than the OS would.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

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
    /// The link dropped: reconnection expected. Carries the reason, so a
    /// rejected TLS record, a protocol violation and a peer that closed the
    /// socket do not all read as one fixed string in the log and the journal.
    Disconnected(String),
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

async fn run_with_connector<C, S>(
    url: String,
    mut outbound: mpsc::Receiver<BudgetedChunk>,
    inbound: mpsc::Sender<UplinkEvent>,
    initial_backoff: Duration,
    max_backoff: Duration,
    mut connect: C,
) where
    C: AsyncFnMut(String) -> Result<S, WsError>,
    S: Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
{
    let mut backoff = Backoff::new(initial_backoff, max_backoff);
    // The player only hears transitions: the first failure reports the outage,
    // each retry stays a tracing detail, recovery reports once.
    let mut outage_reported = false;
    // `url` is never a log field. It is `Config::server_url` verbatim, and
    // `Config::validate` accepts any `wss://` URL without inspecting userinfo
    // or query — either can carry a credential, and the log file is what the
    // README asks the player to send us, under an explicit promise that it
    // contains neither. The redacted form is written once at startup by
    // `app::redacted_server_url`; there is exactly one server per process, so
    // these lines only need to say *which attempt*, which is also what makes
    // the 1st reconnect legible from the 40th.
    let mut attempt: u64 = 0;

    loop {
        attempt += 1;
        match tokio::time::timeout(CONNECT_TIMEOUT, connect(url.clone())).await {
            Ok(Ok(stream)) => {
                info!(attempt, "server link established");
                if std::mem::take(&mut outage_reported) {
                    let _ = inbound.send(UplinkEvent::LinkUp).await;
                }
                backoff.reset();
                match pump(stream, &mut outbound, &inbound).await {
                    Outcome::Shutdown => return,
                    Outcome::Disconnected(reason) => {
                        warn!(attempt, reason = %reason, "server link interrupted");
                        outage_reported = true;
                        let _ = inbound.send(UplinkEvent::LinkDown(reason)).await;
                    }
                }
            }
            Ok(Err(err)) => {
                warn!(attempt, error = ?err, "server connection failed");
                if !outage_reported {
                    outage_reported = true;
                    // Safe to mirror into the journal: of the `WsError` variants
                    // reachable from `connect_async`, none embeds the URL in its
                    // `Display` (`UrlError::UnableToConnect`, the only one that
                    // does, is built solely by the blocking `client::connect` —
                    // checked against tungstenite 0.29).
                    let _ = inbound.send(UplinkEvent::LinkDown(err.to_string())).await;
                }
            }
            Err(_elapsed) => {
                warn!(attempt, "server handshake stalled — retrying");
                if !outage_reported {
                    outage_reported = true;
                    let _ = inbound
                        .send(UplinkEvent::LinkDown("handshake stalled".to_owned()))
                        .await;
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
///
/// The two halves are independent futures raced against each other rather than
/// two arms of one `select!` body, because awaiting the send *inside* an arm
/// stops polling `read` for as long as it takes — up to `SEND_TIMEOUT`, 10 s,
/// which is exactly the watchdog's `EXPECT_SNAPSHOT_MS`/`EXPECT_PURCHASE_MS`
/// window. A write stall would then hold back an already-arrived `Shop` or
/// `Purchase` proof past its own deadline and let the `Tick` escalate the
/// recovery ladder to a refresh re-issue, which spends crystals and re-rolls
/// the shop out from under a purchase in flight.
///
/// Whichever half finishes first ends the connection, as before. A send still in
/// flight when the read half ends is dropped along with its budget lease: the
/// same "drop bytes while the link is gone, the server resyncs on reconnect"
/// tolerance `drain_until` documents.
async fn pump<S>(
    stream: S,
    outbound: &mut mpsc::Receiver<BudgetedChunk>,
    inbound: &mpsc::Sender<UplinkEvent>,
) -> Outcome
where
    S: Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
{
    let (mut write, mut read) = stream.split();

    let writer = async {
        while let Some(chunk) = outbound.recv().await {
            let (bytes, lease) = chunk.into_parts();
            let send = write.send(Message::Binary(bytes.into()));
            match tokio::time::timeout(SEND_TIMEOUT, send).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    warn!(error = ?err, "server send failed");
                    return Outcome::Disconnected(format!("send failed: {err}"));
                }
                Err(_elapsed) => {
                    warn!("server send stalled — dropping the connection");
                    return Outcome::Disconnected("send stalled".to_owned());
                }
            }
            drop(lease);
        }
        Outcome::Shutdown
    };

    // Latched per connection: an unknown `type` tag means the server speaks a
    // dialect this build does not, which otherwise reads exactly like a mute
    // server. One line per connection is enough to tell those two apart.
    let mut dialect_reported = false;
    let reader = async {
        loop {
            match read.next().await {
                Some(Ok(Message::Text(text))) => {
                    forward(text.as_bytes(), inbound, &mut dialect_reported).await;
                }
                Some(Ok(Message::Binary(bytes))) => {
                    forward(&bytes, inbound, &mut dialect_reported).await;
                }
                Some(Ok(Message::Close(_))) => {
                    return Outcome::Disconnected("peer closed the connection".to_owned());
                }
                None => return Outcome::Disconnected("server stream ended".to_owned()),
                // ping/pong/frame: handled by the library. Named rather than
                // `_` because `tungstenite::Message` is not `#[non_exhaustive]`,
                // so a variant added by a major bump must stop compiling here
                // instead of being silently dropped.
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                Some(Err(err)) => {
                    warn!(error = ?err, "WebSocket read error");
                    return Outcome::Disconnected(format!("read error: {err}"));
                }
            }
        }
    };

    tokio::pin!(writer, reader);
    tokio::select! {
        outcome = &mut writer => outcome,
        outcome = &mut reader => outcome,
    }
}

/// Decodes a server message and pushes it downstream (undecodable ones dropped).
///
/// Both ways the inbound path can fail are recorded, because they look the same
/// from the outside — `since_last_shop_s` climbing forever. A payload that does
/// not deserialize is `Err` here; a payload whose `type` tag this build does not
/// know deserializes *successfully* into `ServerMessage::Unknown`, which the
/// session drops as a no-op. `reported_dialect` latches the second case so it
/// costs one line per connection rather than one per message.
async fn forward(payload: &[u8], inbound: &mpsc::Sender<UplinkEvent>, reported_dialect: &mut bool) {
    match serde_json::from_slice::<ServerMessage>(payload) {
        Ok(message) => {
            if matches!(message, ServerMessage::Unknown)
                && !std::mem::replace(reported_dialect, true)
            {
                warn!("server sent a message type this build does not understand");
            }
            let _ = inbound.send(UplinkEvent::Message(message)).await;
        }
        Err(err) => debug!(error = ?err, "unrecognized server message, ignored"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future::{pending, ready};
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use super::*;
    use crate::stream::PipelineBudget;

    fn chunk(bytes: Vec<u8>) -> BudgetedChunk {
        PipelineBudget::new().admit_outbound_for_test(bytes)
    }

    #[test]
    fn backoff_doubles_up_to_the_cap_and_resets_to_the_initial() {
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
    }

    #[test]
    fn backoff_below_the_floor_is_raised_to_it_and_cannot_grow_past_the_cap() {
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

    /// A peer that speaks: yields a scripted sequence of inbound frames, then
    /// pends forever like `StalledLink`. `stalling_sends` additionally freezes
    /// the outbound half, so the two directions can be tested against each
    /// other rather than only one at a time.
    struct ScriptedLink {
        frames: VecDeque<Result<Message, WsError>>,
        send_stalls: bool,
        ends_after_script: bool,
    }

    impl ScriptedLink {
        fn new(frames: Vec<Result<Message, WsError>>) -> Self {
            Self {
                frames: frames.into(),
                send_stalls: false,
                ends_after_script: false,
            }
        }

        fn stalling_sends(mut self) -> Self {
            self.send_stalls = true;
            self
        }

        /// The socket goes away rather than pending: the stream ends with `None`
        /// once the script is exhausted, no close frame.
        fn ending(mut self) -> Self {
            self.ends_after_script = true;
            self
        }

        fn sink_state(&self) -> Poll<Result<(), WsError>> {
            if self.send_stalls {
                Poll::Pending
            } else {
                Poll::Ready(Ok(()))
            }
        }
    }

    impl Stream for ScriptedLink {
        type Item = Result<Message, WsError>;
        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            match self.frames.pop_front() {
                Some(frame) => Poll::Ready(Some(frame)),
                None if self.ends_after_script => Poll::Ready(None),
                None => Poll::Pending,
            }
        }
    }

    impl Sink<Message> for ScriptedLink {
        type Error = WsError;
        fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
            self.sink_state()
        }
        fn start_send(self: Pin<&mut Self>, _item: Message) -> Result<(), WsError> {
            Ok(())
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
            self.sink_state()
        }
        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
            self.sink_state()
        }
    }

    /// The inbound half of `pump` with nothing outbound in flight: the returned
    /// outcome plus every event the connection produced.
    async fn pump_inbound(link: ScriptedLink) -> (Outcome, Vec<UplinkEvent>) {
        let (raw_tx, mut raw_rx) = mpsc::channel::<BudgetedChunk>(1);
        let (event_tx, mut event_rx) = mpsc::channel::<UplinkEvent>(8);

        let outcome = pump(link, &mut raw_rx, &event_tx).await;

        drop(event_tx);
        drop(raw_tx);
        let mut events = Vec::new();
        while let Some(event) = event_rx.recv().await {
            events.push(event);
        }
        (outcome, events)
    }

    fn disconnect_reason(outcome: Outcome) -> String {
        match outcome {
            Outcome::Disconnected(reason) => reason,
            Outcome::Shutdown => panic!("expected Disconnected, got Shutdown"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn inbound_text_shop_message_reaches_the_session_decoded() {
        let (outcome, events) = pump_inbound(ScriptedLink::new(vec![
            Ok(Message::text(
                r#"{"type":"shop","merchant":"Secret Shop","slots":[]}"#,
            )),
            Ok(Message::Close(None)),
        ]))
        .await;

        assert!(disconnect_reason(outcome).contains("closed"));
        let [UplinkEvent::Message(ServerMessage::Shop(snapshot))] = &events[..] else {
            panic!("expected exactly one decoded Shop, got {events:?}");
        };
        assert_eq!(snapshot.merchant.as_deref(), Some("Secret Shop"));
    }

    #[tokio::test(start_paused = true)]
    async fn inbound_binary_message_is_decoded_the_same_way_as_text() {
        let (_outcome, events) = pump_inbound(ScriptedLink::new(vec![
            Ok(Message::binary(br#"{"type":"ack"}"#.to_vec())),
            Ok(Message::Close(None)),
        ]))
        .await;

        assert!(matches!(
            &events[..],
            [UplinkEvent::Message(ServerMessage::Ack)]
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn undecodable_payload_and_control_frames_are_dropped_without_ending_the_connection() {
        let (outcome, events) = pump_inbound(ScriptedLink::new(vec![
            Ok(Message::text("not json")),
            Ok(Message::Ping(Vec::new().into())),
            Ok(Message::Pong(Vec::new().into())),
            Ok(Message::Close(None)),
        ]))
        .await;

        // The drop-and-continue policy: nothing forwarded, and the connection
        // survives to the close frame rather than being torn down.
        assert!(events.is_empty(), "unexpected events: {events:?}");
        assert!(disconnect_reason(outcome).contains("closed"));
    }

    #[tokio::test(start_paused = true)]
    async fn an_unknown_message_type_is_reported_and_still_forwarded() {
        let (_outcome, events) = pump_inbound(ScriptedLink::new(vec![
            Ok(Message::text(r#"{"type":"telemetry","whatever":1}"#)),
            Ok(Message::Close(None)),
        ]))
        .await;

        // Forward compatibility is preserved (the message is not an error), but
        // it is no longer silent — see `forward`.
        assert!(matches!(
            &events[..],
            [UplinkEvent::Message(ServerMessage::Unknown)]
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_close_frame_ends_the_connection_for_the_reconnect_cycle() {
        let (outcome, events) =
            pump_inbound(ScriptedLink::new(vec![Ok(Message::Close(None))])).await;
        assert_eq!(disconnect_reason(outcome), "peer closed the connection");
        assert!(events.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_that_ends_without_a_close_frame_is_also_a_disconnect() {
        let (outcome, _events) = pump_inbound(ScriptedLink::new(Vec::new()).ending()).await;
        assert_eq!(disconnect_reason(outcome), "server stream ended");
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_error_names_itself_in_the_disconnect_reason() {
        let (outcome, _events) =
            pump_inbound(ScriptedLink::new(vec![Err(WsError::AttackAttempt)])).await;
        let reason = disconnect_reason(outcome);
        assert!(reason.starts_with("read error"), "reason was {reason:?}");
        assert!(reason.contains("Attack attempt"), "reason was {reason:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn an_inbound_message_lands_while_the_send_half_is_stalled() {
        let budget = PipelineBudget::with_test_limits(64, 64, 64, 64);
        let (raw_tx, mut raw_rx) = mpsc::channel::<BudgetedChunk>(4);
        let (event_tx, mut event_rx) = mpsc::channel::<UplinkEvent>(4);
        raw_tx
            .send(budget.admit_outbound_for_test(vec![1, 2, 3]))
            .await
            .unwrap();

        // The peer has a shop for us and a frozen receive window: the read half
        // must not wait out `SEND_TIMEOUT` behind the write half.
        let link = ScriptedLink::new(vec![Ok(Message::text(r#"{"type":"ack"}"#))]).stalling_sends();
        let mut pumping = std::pin::pin!(pump(link, &mut raw_rx, &event_tx));

        tokio::select! {
            _ = &mut pumping => panic!("the stalled send must not have completed yet"),
            event = event_rx.recv() => assert!(matches!(
                event,
                Some(UplinkEvent::Message(ServerMessage::Ack))
            )),
        }

        // And the write half still gives up on its own schedule.
        assert_eq!(disconnect_reason(pumping.await), "send stalled");
        assert_eq!(budget.snapshot().current_total, 0);
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
        assert_eq!(disconnect_reason(outcome), "send stalled");
        assert_eq!(budget.snapshot().current_total, 0);
        assert!(budget.snapshot().high_water_total <= 64);
    }

    #[tokio::test(start_paused = true)]
    async fn run_retries_after_a_handshake_that_never_completes() {
        let (raw_tx, raw_rx) = mpsc::channel::<BudgetedChunk>(1);
        let (event_tx, mut event_rx) = mpsc::channel::<UplinkEvent>(4);
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let recorded_attempts = Arc::clone(&attempts);
        let started = tokio::time::Instant::now();

        // A connector that opens and never finishes: without CONNECT_TIMEOUT the
        // task parks here forever, emitting no LinkDown and never retrying.
        let task = tokio::spawn(run_with_connector(
            "ws://test.invalid".to_owned(),
            raw_rx,
            event_tx,
            Duration::from_millis(100),
            Duration::from_millis(100),
            move |_url| {
                recorded_attempts
                    .lock()
                    .unwrap()
                    .push(tokio::time::Instant::now().duration_since(started));
                pending::<Result<StalledLink, WsError>>()
            },
        ));

        tokio::task::yield_now().await;
        assert_eq!(*attempts.lock().unwrap(), [Duration::ZERO]);

        tokio::time::advance(CONNECT_TIMEOUT).await;
        tokio::task::yield_now().await;
        assert!(matches!(
            event_rx.recv().await,
            Some(UplinkEvent::LinkDown(reason)) if reason == "handshake stalled"
        ));

        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            *attempts.lock().unwrap(),
            [Duration::ZERO, CONNECT_TIMEOUT + Duration::from_millis(100)]
        );

        drop(raw_tx);
        task.await.unwrap();
    }
}
