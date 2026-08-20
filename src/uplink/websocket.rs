//! WebSocket client: streams the raw bytes, receives server messages.
//!
//! The connection re-establishes automatically (capped exponential backoff). A
//! closed outbound channel signals a clean shutdown: reconnection then stops.

use std::time::Duration;

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tracing::{Instrument, debug, info, warn};

use crate::config::{RECONNECT_FLOOR, ServerUrl};
use crate::stream::BudgetedChunk;

use super::UplinkEvent;
use super::protocol::ServerMessage;

/// A send that cannot finish within this window means a stalled peer (TCP
/// zero-window): the socket stays "connected" but nothing moves, backpressure
/// parks the capture thread, and the kernel starts dropping packet copies.
/// Dropping the connection turns the stall into a normal reconnect cycle.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// The same class of stall on the way in: a handshake that opens but never
/// completes (captive portal, a middlebox that accepts the SYN and never speaks
/// TLS) has no upper bound of its own, and nothing downstream recovers it —
/// without a `LinkDown` the watchdog never arms, so the relay looks connected
/// and forwards nothing, forever.
///
/// 15 s sits above a TLS handshake on a bad connection and below Windows' own
/// ~21 s SYN-retry give-up, so a black-holed address is reported sooner than the
/// OS would.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the link must stay up before it counts as a *link* rather than a
/// completed handshake — accumulated across one outage's reconnects, not
/// demanded of a single one of them.
///
/// A peer can accept the upgrade and close in the same millisecond (auth
/// rejected on the wire, a rate limiter, a draining load balancer), which at the
/// instant of the upgrade is indistinguishable from the real thing. So the
/// handshake may not reset the backoff or end an outage; only measured uptime
/// may. 10 s because that is the watchdog's own
/// `EXPECT_SNAPSHOT_MS`/`EXPECT_PURCHASE_MS` window (`crate::domain::control`):
/// a connection that cannot hold it cannot carry a refresh and the snapshot
/// answering it.
///
/// Accumulating rather than demanding one unbroken stretch is what stops a link
/// that keeps dying at six seconds from reporting one `LinkDown` and nothing
/// ever again — only `LinkUp` sets `Controller::link_up`, so that would suspend
/// the watchdog for the rest of the session. Anti-flap survives it because
/// uptime cannot be spent faster than it is earned: accept-then-close peers
/// contribute a round-trip apiece and never reach the window, and a healthy link
/// reports at most one recovery per `LINK_SETTLED` of *uptime*.
///
/// A link too poor to accumulate 10 s across a whole outage reports no recovery
/// at all, deliberately: it cannot carry a purchase confirmation either, so the
/// standing `LinkDown` is the honest report. Halting the hunt with
/// `Unresponsive` would blame the game for the network.
const LINK_SETTLED: Duration = Duration::from_secs(10);

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

enum Outcome {
    /// Do not reconnect.
    Shutdown,
    /// The link dropped: reconnection expected. Carries the reason, so a
    /// rejected TLS record, a protocol violation and a peer that closed the
    /// socket do not all read as one fixed string in the log and the journal.
    Disconnected(String),
}

/// Connection loop, to be spawned in its own task.
///
/// - `url`: a [`ServerUrl`] rather than a `String` so that the credential-bearing
///   dial form is reachable only through [`ServerUrl::as_str`], used once below.
///   Every `%url` here resolves through `Display` — the redacted authority — so
///   no log line in this module can leak one.
/// - `outbound`: closing it stops the loop.
/// - `shutdown`: raced against every window this loop can park in, so teardown
///   does not have to reach the task by `abort`.
pub async fn run(
    url: ServerUrl,
    outbound: mpsc::Receiver<BudgetedChunk>,
    inbound: mpsc::Sender<UplinkEvent>,
    initial_backoff: Duration,
    max_backoff: Duration,
    shutdown: watch::Receiver<bool>,
) {
    run_with_connector(
        url,
        outbound,
        inbound,
        initial_backoff,
        max_backoff,
        shutdown,
        |url| async move { connect_async(url).await.map(|(stream, _response)| stream) },
    )
    .await;
}

async fn run_with_connector<C, S>(
    url: ServerUrl,
    mut outbound: mpsc::Receiver<BudgetedChunk>,
    inbound: mpsc::Sender<UplinkEvent>,
    initial_backoff: Duration,
    max_backoff: Duration,
    mut shutdown: watch::Receiver<bool>,
    mut connect: C,
) where
    C: AsyncFnMut(String) -> Result<S, WsError>,
    S: Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
{
    let mut backoff = Backoff::new(initial_backoff, max_backoff);
    // The player only hears transitions: the first failure reports the outage,
    // each retry stays a tracing detail, recovery reports once.
    let mut outage_reported = false;
    // Connected time this outage has accumulated towards `LINK_SETTLED`, across
    // however many reconnects it took.
    let mut connected_for = Duration::ZERO;
    // Logged throughout: what makes the 1st reconnect legible from the 40th.
    let mut attempt: u64 = 0;

    loop {
        // The signal may already be set when the task first runs (a window
        // closed during startup), in which case `changed()` never fires.
        if is_stopping(&shutdown) {
            return;
        }
        attempt += 1;
        let connecting = tokio::time::timeout(CONNECT_TIMEOUT, connect(url.as_str().to_owned()));
        let connected = tokio::select! {
            biased;
            // Without this arm a stop waits out CONNECT_TIMEOUT — 15 s of the
            // player staring at a closed window. Dropping `connecting` cancels
            // the handshake, which owns nothing but the socket it is opening.
            () = wait_for_shutdown(&mut shutdown) => return,
            result = connecting => result,
        };
        match connected {
            Ok(Ok(stream)) => {
                // "established" claims only that the upgrade completed. Whether
                // it is a *link* is decided `LINK_SETTLED` later, below.
                info!(server = %url, attempt, "server link established");
                // The span carries `attempt` down into `pump` and `forward`, so a
                // read error in a long session is attributable to *which*
                // connection. `.instrument()`, not an `.entered()` guard: `pump`
                // awaits, and a guard held across an await leaks the span onto
                // whatever task the executor polls next.
                let session = pump(stream, &mut outbound, &inbound, &mut shutdown)
                    .instrument(tracing::info_span!("link", server = %url, attempt));
                tokio::pin!(session);
                let connected_at = tokio::time::Instant::now();
                // What this connection still owes, not the whole window: earlier
                // reconnects in this same outage count (see `LINK_SETTLED`).
                let owed = LINK_SETTLED.saturating_sub(connected_for);
                // Two phases rather than a `select!` loop: the deadline matters
                // exactly once per connection, and past it there is nothing left
                // to race. `biased` puts the session first, so a tie reads as
                // "did not hold" — the conservative half, costing one extra
                // backoff step.
                let ended_before_settling = tokio::select! {
                    biased;
                    outcome = &mut session => Some(outcome),
                    () = tokio::time::sleep(owed) => None,
                };
                let outcome = match ended_before_settling {
                    Some(outcome) => {
                        // Short of the window, but not nothing: what this
                        // connection did serve is what the next one builds on.
                        connected_for = connected_for.saturating_add(connected_at.elapsed());
                        outcome
                    }
                    None => {
                        // Deliberately inseparable: the connection that earns
                        // the delay back is the same one allowed to end the
                        // outage. Splitting them is how this went wrong before.
                        debug!(server = %url, attempt, "server link held — retry delay reset");
                        backoff.reset();
                        // Spent, not banked: the next outage starts from zero,
                        // however long this link goes on to live.
                        connected_for = Duration::ZERO;
                        if std::mem::take(&mut outage_reported) {
                            let _ = inbound.send(UplinkEvent::LinkUp).await;
                        }
                        session.await
                    }
                };
                match outcome {
                    Outcome::Shutdown => return,
                    Outcome::Disconnected(reason) => {
                        warn!(server = %url, attempt, reason = %reason, "server link interrupted");
                        // Latched like the two failure arms below: a peer that
                        // accepts and hangs up is one outage that keeps failing,
                        // not an outage per second. The `warn!` above still fires
                        // every cycle — the log carries the churn, the journal
                        // the transition.
                        if !outage_reported {
                            outage_reported = true;
                            let _ = inbound.send(UplinkEvent::LinkDown(reason)).await;
                        }
                    }
                }
            }
            Ok(Err(err)) => {
                warn!(server = %url, attempt, error = ?err, "server connection failed");
                if !outage_reported {
                    outage_reported = true;
                    // Safe to mirror into the journal: no `WsError` variant
                    // reachable from `connect_async` embeds the URL in its
                    // `Display` (checked against tungstenite 0.29).
                    let _ = inbound.send(UplinkEvent::LinkDown(err.to_string())).await;
                }
            }
            Err(_elapsed) => {
                warn!(server = %url, attempt, "server handshake stalled — retrying");
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
        // channel fills, reassembly blocks, and the stall reaches the capture
        // thread, where the kernel drops packets and the gaps are unfillable.
        // Better to drop bytes while the server is unreachable — it resyncs on
        // reconnect.
        if drain_until(&mut outbound, backoff.current(), &mut shutdown).await {
            return; // outbound closed or shutdown requested.
        }
        backoff.advance();
    }
}

/// A sync `fn` rather than an inline `*shutdown.borrow()`: `watch::Ref` is not
/// `Send`, so a borrow guard whose temporary outlives an `await` in the same
/// statement would make the whole future non-`Send`, which
/// `SessionWorkers::spawn` requires.
fn is_stopping(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow()
}

/// Parks until the session-wide stop signal is set, or until the last sender is
/// gone. `watch::Receiver::changed` only reports a *change*, so the current
/// value is checked first; a dropped sender resolves this too, since it can only
/// mean the session is already gone.
async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if is_stopping(shutdown) {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

/// Absorbs and discards outbound batches for `wait`, without stalling upstream.
/// Returns `true` if the loop should stop — the outbound channel closed, or the
/// session asked to shut down.
///
/// `wait` is `backoff.current()`, which climbs to `reconnect.max_ms`, so without
/// the shutdown arm a stop requested one tick into a backed-off retry waits the
/// whole delay out.
async fn drain_until(
    outbound: &mut mpsc::Receiver<BudgetedChunk>,
    wait: Duration,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    let deadline = tokio::time::sleep(wait);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;
            () = wait_for_shutdown(shutdown) => return true,
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
/// two arms of one `select!` body: awaiting the send *inside* an arm stops
/// polling `read` for up to `SEND_TIMEOUT`, which is exactly the watchdog's
/// expectation window. A write stall would then hold back an already-arrived
/// `Shop` or `Purchase` proof past its own deadline and let the `Tick` escalate
/// to a refresh re-issue, which spends crystals and re-rolls the shop out from
/// under a purchase in flight.
///
/// Whichever half finishes first ends the connection; a send still in flight is
/// dropped with its budget lease, the same tolerance `drain_until` documents.
///
/// The third arm exists because a connected link parks indefinitely on purpose —
/// the reader waits for a server that may have nothing to say for minutes — so
/// without it teardown reached this task only through `SessionWorkers`' `abort`.
async fn pump<S>(
    stream: S,
    outbound: &mut mpsc::Receiver<BudgetedChunk>,
    inbound: &mpsc::Sender<UplinkEvent>,
    shutdown: &mut watch::Receiver<bool>,
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

    // Latched per connection — see `forward`.
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
                // Handled by the library. Named rather than `_` because
                // `tungstenite::Message` is not `#[non_exhaustive]`, so a
                // variant added by a major bump must stop compiling here instead
                // of being silently dropped.
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                Some(Err(err)) => {
                    warn!(error = ?err, "WebSocket read error");
                    return Outcome::Disconnected(format!("read error: {err}"));
                }
            }
        }
    };

    // A cooperative stop, reported as the same `Shutdown` a closed outbound
    // channel produces: both mean "do not reconnect".
    let stopping = async {
        wait_for_shutdown(shutdown).await;
        Outcome::Shutdown
    };

    tokio::pin!(writer, reader, stopping);
    // `biased`, so a requested stop wins over a writer or reader ready in the
    // same poll. Between those two the order only decides a tie, and the
    // writer's `Shutdown` is the conservative reading of one.
    tokio::select! {
        biased;
        outcome = &mut stopping => outcome,
        outcome = &mut writer => outcome,
        outcome = &mut reader => outcome,
    }
}

/// Decodes a server message and pushes it downstream (undecodable ones dropped).
///
/// Both ways the inbound path can fail are recorded, because from the outside
/// they look like a mute server. The second is the easy one to miss: a payload
/// whose `type` tag this build does not know deserializes *successfully* into
/// `ServerMessage::Unknown`, which the session drops as a no-op.
/// `reported_dialect` latches it to one line per connection.
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
    use std::future::{Future, pending, ready};
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use super::*;
    use crate::stream::{BudgetLimits, PipelineBudget};

    /// `wss://`, not `ws://`: `ServerUrl::parse` refuses cleartext to a
    /// non-loopback host.
    fn test_url() -> ServerUrl {
        ServerUrl::parse("wss://test.invalid").expect("a wss:// URL is dialable")
    }

    /// A stop signal nothing ever sets. The sender is handed back so the caller
    /// can keep it alive: dropping the last sender *is* a stop.
    fn no_shutdown() -> (watch::Sender<bool>, watch::Receiver<bool>) {
        watch::channel(false)
    }

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

    /// Yields a scripted sequence of inbound frames, then pends forever like
    /// `StalledLink`. `stalling_sends` additionally freezes the outbound half,
    /// so the two directions can be tested against each other.
    struct ScriptedLink {
        frames: VecDeque<Result<Message, WsError>>,
        send_stalls: bool,
        ends_after_script: bool,
        /// Every frame the writer handed to the sink, shared so a test can read
        /// it while `pump` still owns the link.
        sent: Arc<Mutex<Vec<Message>>>,
    }

    impl ScriptedLink {
        fn new(frames: Vec<Result<Message, WsError>>) -> Self {
            Self {
                frames: frames.into(),
                send_stalls: false,
                ends_after_script: false,
                sent: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn sent(&self) -> Arc<Mutex<Vec<Message>>> {
            Arc::clone(&self.sent)
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
        fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), WsError> {
            self.sent.lock().unwrap().push(item);
            Ok(())
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
            self.sink_state()
        }
        fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
            self.sink_state()
        }
    }

    /// A peer that carries the connection for a while and *then* hangs up. That
    /// middle ground is the whole subject of `LINK_SETTLED`, and nothing else
    /// here can express it: a link either lives forever or dies instantly.
    struct BriefLink {
        alive: Duration,
        closing: Option<Pin<Box<tokio::time::Sleep>>>,
    }

    impl BriefLink {
        fn new(alive: Duration) -> Self {
            Self {
                alive,
                closing: None,
            }
        }
    }

    impl Stream for BriefLink {
        type Item = Result<Message, WsError>;
        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            // The timer starts at the first poll, not at construction: the
            // connector builds the link, and `pump` is what puts it on the wire.
            let this = self.get_mut();
            let alive = this.alive;
            let closing = this
                .closing
                .get_or_insert_with(|| Box::pin(tokio::time::sleep(alive)));
            closing
                .as_mut()
                .poll(cx)
                .map(|()| Some(Ok(Message::Close(None))))
        }
    }

    impl Sink<Message> for BriefLink {
        type Error = WsError;
        // Frozen: these tests hold their outbound channel open and empty, so the
        // writer parks on `recv` and never gets here.
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

    async fn pump_inbound(link: ScriptedLink) -> (Outcome, Vec<UplinkEvent>) {
        let (raw_tx, mut raw_rx) = mpsc::channel::<BudgetedChunk>(1);
        let (event_tx, mut event_rx) = mpsc::channel::<UplinkEvent>(8);

        let (_stop_tx, mut stop_rx) = no_shutdown();
        let outcome = pump(link, &mut raw_rx, &event_tx, &mut stop_rx).await;

        drop(event_tx);
        drop(raw_tx);
        let mut events = Vec::new();
        while let Some(event) = event_rx.recv().await {
            events.push(event);
        }
        (outcome, events)
    }

    /// A test that waits on an event its producer no longer emits would
    /// otherwise park forever under `start_paused`, and a hang proves nothing.
    async fn next_event(events: &mut mpsc::Receiver<UplinkEvent>) -> Option<UplinkEvent> {
        tokio::time::timeout(Duration::from_secs(60), events.recv())
            .await
            .expect("an event was expected and none arrived")
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
        let budget = PipelineBudget::with_test_limits(BudgetLimits {
            global: 64,
            capture: 64,
            reassembly: 64,
            outbound: 64,
        });
        let (raw_tx, mut raw_rx) = mpsc::channel::<BudgetedChunk>(4);
        let (event_tx, mut event_rx) = mpsc::channel::<UplinkEvent>(4);
        raw_tx
            .send(budget.admit_outbound_for_test(vec![1, 2, 3]))
            .await
            .unwrap();

        // A frozen receive window: the read half must not wait out
        // `SEND_TIMEOUT` behind the write half.
        let link = ScriptedLink::new(vec![Ok(Message::text(r#"{"type":"ack"}"#))]).stalling_sends();
        let (_stop_tx, mut stop_rx) = no_shutdown();
        let mut pumping = std::pin::pin!(pump(link, &mut raw_rx, &event_tx, &mut stop_rx));

        tokio::select! {
            _ = &mut pumping => panic!("the stalled send must not have completed yet"),
            event = event_rx.recv() => assert!(matches!(
                event,
                Some(UplinkEvent::Message(ServerMessage::Ack))
            )),
        }

        assert_eq!(disconnect_reason(pumping.await), "send stalled");
        assert_eq!(budget.snapshot().current_total, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn drain_until_discards_batches_until_deadline() {
        let (raw_tx, mut raw_rx) = mpsc::channel::<BudgetedChunk>(1);
        let (_stop_tx, mut stop_rx) = no_shutdown();
        let drain = tokio::spawn(async move {
            drain_until(&mut raw_rx, Duration::from_millis(100), &mut stop_rx).await
        });

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

        let (_stop_tx, mut stop_rx) = no_shutdown();
        assert!(drain_until(&mut raw_rx, Duration::from_secs(1), &mut stop_rx).await);
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_drain_releases_all_outbound_bytes() {
        let budget = PipelineBudget::with_test_limits(BudgetLimits {
            global: 128,
            capture: 128,
            reassembly: 128,
            outbound: 128,
        });
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

        let (_stop_tx, mut stop_rx) = no_shutdown();
        let drain = tokio::spawn(async move {
            drain_until(&mut raw_rx, Duration::from_millis(100), &mut stop_rx).await
        });
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
        let (_stop_tx, stop_rx) = no_shutdown();

        let task = tokio::spawn(run_with_connector(
            test_url(),
            raw_rx,
            event_tx,
            Duration::from_millis(1),
            Duration::from_millis(10),
            stop_rx,
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
        let (_stop_tx, stop_rx) = no_shutdown();

        let task = tokio::spawn(run_with_connector(
            test_url(),
            raw_rx,
            event_tx,
            Duration::from_millis(100),
            Duration::from_secs(1),
            stop_rx,
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

    /// `run_with_connector` is the only producer of `LinkDown`/`LinkUp`. Delete
    /// either send and every controller test still passes, while a session that
    /// survived one outage sits with its watchdog suspended forever.
    ///
    /// The redial is observed twice on purpose: right after it lands, where it
    /// must report nothing, and past `LINK_SETTLED`, where it must report the
    /// recovery.
    #[tokio::test(start_paused = true)]
    async fn a_reconnect_reports_the_outage_and_its_end() {
        // Held on purpose: a closed outbound channel ends the loop before the
        // second dial, which would pass the LinkDown half for the wrong reason.
        let (_raw_tx, raw_rx) = mpsc::channel::<BudgetedChunk>(1);
        let (event_tx, mut event_rx) = mpsc::channel::<UplinkEvent>(4);
        let (stop_tx, stop_rx) = no_shutdown();
        let dials = Arc::new(Mutex::new(0_u32));
        let counted = Arc::clone(&dials);

        let task = tokio::spawn(run_with_connector(
            test_url(),
            raw_rx,
            event_tx,
            Duration::from_millis(100),
            Duration::from_millis(100),
            stop_rx,
            move |_url| {
                let dial = {
                    let mut count = counted.lock().unwrap();
                    *count += 1;
                    *count
                };
                let link = if dial == 1 {
                    ScriptedLink::new(vec![Ok(Message::Close(None))])
                } else {
                    ScriptedLink::new(Vec::new()).stalling_sends()
                };
                ready(Ok::<_, WsError>(link))
            },
        ));

        let outage = next_event(&mut event_rx).await;
        assert!(
            matches!(&outage, Some(UplinkEvent::LinkDown(reason)) if reason.contains("closed")),
            "expected the outage to be reported, got {outage:?}"
        );

        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        assert_eq!(*dials.lock().unwrap(), 2, "the retry redialed");
        // `try_recv`, not `next_event`: under paused time an awaited `recv` lets
        // the clock run to the next timer, and the next timer *is* the settle
        // deadline — the "nothing yet" half has to be asked without waiting.
        assert!(
            matches!(event_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "an accepted handshake is not a recovery: nothing may be reported yet"
        );

        tokio::time::advance(LINK_SETTLED).await;
        let recovery = next_event(&mut event_rx).await;
        assert!(
            matches!(recovery, Some(UplinkEvent::LinkUp)),
            "expected the outage's end to be reported, got {recovery:?}"
        );
        assert_eq!(
            *dials.lock().unwrap(),
            2,
            "the outage ended on the redial that held, not on a later one"
        );

        stop_tx.send(true).expect("the receiver is in the task");
        task.await.unwrap();
    }

    /// A peer that completes the upgrade and hangs up is what turned the
    /// exponential backoff into a fixed one: with `reset()` on the handshake,
    /// each cycle's doubling was thrown away at the start of the next.
    #[tokio::test(start_paused = true)]
    async fn a_peer_that_accepts_and_hangs_up_still_backs_off() {
        // Held on purpose: a closed outbound channel ends the loop after the
        // first cycle, and the first cycle is the one this test must get past.
        let (_raw_tx, raw_rx) = mpsc::channel::<BudgetedChunk>(1);
        let (event_tx, _event_rx) = mpsc::channel::<UplinkEvent>(32);
        let (stop_tx, stop_rx) = no_shutdown();
        let dials = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&dials);
        let started = tokio::time::Instant::now();

        let task = tokio::spawn(run_with_connector(
            test_url(),
            raw_rx,
            event_tx,
            Duration::from_millis(100),
            Duration::from_secs(10),
            stop_rx,
            move |_url| {
                recorded
                    .lock()
                    .unwrap()
                    .push(tokio::time::Instant::now().duration_since(started));
                ready(Ok::<_, WsError>(ScriptedLink::new(vec![Ok(
                    Message::Close(None),
                )])))
            },
        ));

        tokio::task::yield_now().await;
        assert_eq!(*dials.lock().unwrap(), [Duration::ZERO]);

        // Ticked, not jumped one expected delay at a time: `tokio::time::advance`
        // wakes the parked task *once* however far it moved, so a coarse step
        // records its own schedule rather than the backoff's, and passes against
        // the very bug it was written for. Ticks shorter than the shortest delay
        // give every dial its own wake-up.
        for _ in 0..14 {
            tokio::time::advance(Duration::from_millis(50)).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(
            *dials.lock().unwrap(),
            [0, 100, 300, 700].map(Duration::from_millis),
            "an accept-then-close peer must be dialed on a doubling delay"
        );

        stop_tx.send(true).expect("the receiver is in the task");
        task.await.unwrap();
    }

    /// The journal half of the same peer: clearing `outage_reported` on the
    /// handshake published a `LinkDown` *and* a `LinkUp` per retry — a pair per
    /// second, by default, and every `LinkUp` re-grants the watchdog.
    #[tokio::test(start_paused = true)]
    async fn a_peer_that_accepts_and_hangs_up_reports_one_outage_not_one_per_retry() {
        let (_raw_tx, raw_rx) = mpsc::channel::<BudgetedChunk>(1);
        // Roomy on purpose: the point of the test is how many events a broken
        // peer produces, so the channel must not be what stops them.
        let (event_tx, mut event_rx) = mpsc::channel::<UplinkEvent>(32);
        let (stop_tx, stop_rx) = no_shutdown();

        let task = tokio::spawn(run_with_connector(
            test_url(),
            raw_rx,
            event_tx,
            Duration::from_millis(100),
            Duration::from_secs(10),
            stop_rx,
            move |_url| {
                ready(Ok::<_, WsError>(ScriptedLink::new(vec![Ok(
                    Message::Close(None),
                )])))
            },
        ));

        // Four cycles, none lasting anywhere near `LINK_SETTLED`. See the tick
        // note in the backoff test above for why the clock is walked.
        tokio::task::yield_now().await;
        for _ in 0..14 {
            tokio::time::advance(Duration::from_millis(50)).await;
            tokio::task::yield_now().await;
        }

        stop_tx.send(true).expect("the receiver is in the task");
        task.await.unwrap();
        let mut events = Vec::new();
        while let Some(event) = event_rx.recv().await {
            events.push(event);
        }
        let [UplinkEvent::LinkDown(reason)] = &events[..] else {
            panic!("expected exactly one outage report for one outage, got {events:?}");
        };
        assert!(reason.contains("closed"), "reason was {reason:?}");
    }

    /// The state a per-connection settle window had no way out of: a link that
    /// keeps coming back but never holds `LINK_SETTLED` in one stretch reports
    /// one `LinkDown` and then nothing, forever, leaving the watchdog suspended
    /// over a wire that was carrying traffic most of the time.
    #[tokio::test(start_paused = true)]
    async fn a_link_that_only_ever_holds_briefly_still_reports_its_recovery() {
        // Held open: a closed outbound channel would end the loop on the first
        // cycle, and the second cycle is what this test is about.
        let (_raw_tx, raw_rx) = mpsc::channel::<BudgetedChunk>(1);
        let (event_tx, mut event_rx) = mpsc::channel::<UplinkEvent>(32);
        let (stop_tx, stop_rx) = no_shutdown();
        let dials = Arc::new(Mutex::new(0_u32));
        let counted = Arc::clone(&dials);

        let task = tokio::spawn(run_with_connector(
            test_url(),
            raw_rx,
            event_tx,
            Duration::from_millis(100),
            Duration::from_millis(100),
            stop_rx,
            move |_url| {
                *counted.lock().unwrap() += 1;
                // Long enough to carry a refresh and its snapshot, short enough
                // that no single connection ever settles.
                ready(Ok::<_, WsError>(BriefLink::new(Duration::from_secs(6))))
            },
        ));

        let outage = next_event(&mut event_rx).await;
        assert!(
            matches!(&outage, Some(UplinkEvent::LinkDown(reason)) if reason.contains("closed")),
            "expected the first drop to report the outage, got {outage:?}"
        );

        let recovery = next_event(&mut event_rx).await;
        assert!(
            matches!(recovery, Some(UplinkEvent::LinkUp)),
            "a link that keeps coming back must end its outage, got {recovery:?}"
        );
        assert_eq!(
            *dials.lock().unwrap(),
            2,
            "six seconds twice is the ten this link owes: the second dial ends it"
        );

        stop_tx.send(true).expect("the receiver is in the task");
        task.await.unwrap();
    }

    /// The half of the trade that must survive the fix above: what accumulates
    /// is connected *time*, never connect attempts.
    #[tokio::test(start_paused = true)]
    async fn a_peer_that_hangs_up_a_millisecond_in_never_earns_a_recovery() {
        let (_raw_tx, raw_rx) = mpsc::channel::<BudgetedChunk>(1);
        let (event_tx, mut event_rx) = mpsc::channel::<UplinkEvent>(32);
        let (stop_tx, stop_rx) = no_shutdown();

        let task = tokio::spawn(run_with_connector(
            test_url(),
            raw_rx,
            event_tx,
            Duration::from_millis(100),
            // The fastest this loop can legally dial, so it is the most cycles
            // this peer can possibly fit into the window below.
            Duration::from_millis(100),
            stop_rx,
            move |_url| ready(Ok::<_, WsError>(BriefLink::new(Duration::from_millis(1)))),
        ));

        // Three times `LINK_SETTLED` of wall clock, ~290 cycles of it, ~290 ms
        // of actual connectivity. Slept through rather than walked in steps:
        // every timer in flight belongs to the task, so paused time advances to
        // each of them in turn.
        tokio::time::sleep(LINK_SETTLED * 3).await;

        stop_tx.send(true).expect("the receiver is in the task");
        task.await.unwrap();
        let mut events = Vec::new();
        while let Some(event) = event_rx.recv().await {
            events.push(event);
        }
        let [UplinkEvent::LinkDown(_)] = &events[..] else {
            panic!("a peer that never stays cannot recover; got {events:?}");
        };
    }

    /// A refactor that retained the budget lease — batching sends, parking
    /// chunks for a retry — leaks the outbound quota until reassembly stalls,
    /// and the stalled-send test would not notice: there the lease is released
    /// by the error path.
    #[tokio::test(start_paused = true)]
    async fn a_completed_send_returns_its_budget() {
        let budget = PipelineBudget::with_test_limits(BudgetLimits {
            global: 64,
            capture: 64,
            reassembly: 64,
            outbound: 64,
        });
        // Held open: the writer must park on an empty channel after the send, so
        // the budget is read while the connection is still alive.
        let (raw_tx, mut raw_rx) = mpsc::channel::<BudgetedChunk>(4);
        let (event_tx, _event_rx) = mpsc::channel::<UplinkEvent>(4);
        raw_tx
            .send(budget.admit_outbound_for_test(vec![1, 2, 3]))
            .await
            .unwrap();
        assert_eq!(budget.snapshot().current_outbound, 3);

        let link = ScriptedLink::new(Vec::new());
        let sent = link.sent();
        let (_stop_tx, mut stop_rx) = no_shutdown();
        let mut pumping = std::pin::pin!(pump(link, &mut raw_rx, &event_tx, &mut stop_rx));

        tokio::select! {
            _ = &mut pumping => panic!("the connection must still be open"),
            () = tokio::time::sleep(Duration::from_millis(1)) => {}
        }

        let frames = sent.lock().unwrap();
        let [Message::Binary(payload)] = &frames[..] else {
            panic!("expected exactly one binary frame, got {frames:?}");
        };
        assert_eq!(payload.as_ref(), &[1, 2, 3]);
        assert_eq!(budget.snapshot().current_total, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_send_releases_outbound_bytes_after_timeout() {
        let budget = PipelineBudget::with_test_limits(BudgetLimits {
            global: 64,
            capture: 64,
            reassembly: 64,
            outbound: 64,
        });
        let (raw_tx, mut raw_rx) = mpsc::channel::<BudgetedChunk>(4);
        let (event_tx, _event_rx) = mpsc::channel::<UplinkEvent>(4);
        raw_tx
            .send(budget.admit_outbound_for_test(vec![1, 2, 3]))
            .await
            .unwrap();
        assert_eq!(budget.snapshot().current_outbound, 3);

        let (_stop_tx, mut stop_rx) = no_shutdown();
        let outcome = pump(StalledLink, &mut raw_rx, &event_tx, &mut stop_rx).await;
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

        // Without CONNECT_TIMEOUT the task parks here forever, emitting no
        // LinkDown and never retrying.
        let (_stop_tx, stop_rx) = no_shutdown();
        let task = tokio::spawn(run_with_connector(
            test_url(),
            raw_rx,
            event_tx,
            Duration::from_millis(100),
            Duration::from_millis(100),
            stop_rx,
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

    #[tokio::test(start_paused = true)]
    async fn a_stop_ends_a_stalled_handshake_without_waiting_for_the_timeout() {
        // The worst of the three shutdown windows: a handshake that opens and
        // never completes, 15 s of `CONNECT_TIMEOUT` after the window is gone.
        // `_raw_tx` is held on purpose — a closed outbound channel is the *other*
        // way this loop stops, and would pass this for the wrong reason.
        let (_raw_tx, raw_rx) = mpsc::channel::<BudgetedChunk>(1);
        let (event_tx, _event_rx) = mpsc::channel::<UplinkEvent>(4);
        let (stop_tx, stop_rx) = no_shutdown();

        let task = tokio::spawn(run_with_connector(
            test_url(),
            raw_rx,
            event_tx,
            Duration::from_millis(100),
            Duration::from_millis(100),
            stop_rx,
            move |_url| pending::<Result<StalledLink, WsError>>(),
        ));

        tokio::task::yield_now().await;
        assert!(!task.is_finished(), "parked in the handshake, as intended");

        stop_tx.send(true).expect("the receiver is in the task");
        tokio::task::yield_now().await;
        assert!(
            task.is_finished(),
            "a requested stop must not wait out CONNECT_TIMEOUT"
        );
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_ends_the_backoff_wait_instead_of_sitting_it_out() {
        // The longest window of the three: the backoff drain waits
        // `backoff.current()`, which climbs to `reconnect.max_ms`.
        let (_raw_tx, raw_rx) = mpsc::channel::<BudgetedChunk>(1);
        let (event_tx, _event_rx) = mpsc::channel::<UplinkEvent>(4);
        let (stop_tx, stop_rx) = no_shutdown();

        let task = tokio::spawn(run_with_connector(
            test_url(),
            raw_rx,
            event_tx,
            Duration::from_secs(30),
            Duration::from_secs(30),
            stop_rx,
            move |_url| ready(Err::<StalledLink, _>(WsError::ConnectionClosed)),
        ));

        tokio::task::yield_now().await;
        assert!(!task.is_finished(), "parked in the backoff drain");

        stop_tx.send(true).expect("the receiver is in the task");
        tokio::task::yield_now().await;
        assert!(task.is_finished(), "a requested stop ends the backoff");
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_ends_a_connected_link_that_has_nothing_to_say() {
        // The third window: a link that connected and is simply quiet. Both
        // halves of `pump` park indefinitely there, which is the normal state of
        // a healthy idle relay, not an error case.
        let (_raw_tx, raw_rx) = mpsc::channel::<BudgetedChunk>(1);
        let (event_tx, _event_rx) = mpsc::channel::<UplinkEvent>(4);
        let (stop_tx, stop_rx) = no_shutdown();

        let task = tokio::spawn(run_with_connector(
            test_url(),
            raw_rx,
            event_tx,
            Duration::from_millis(100),
            Duration::from_millis(100),
            stop_rx,
            move |_url| ready(Ok::<_, WsError>(StalledLink)),
        ));

        tokio::task::yield_now().await;
        assert!(!task.is_finished(), "connected and idle");

        stop_tx.send(true).expect("the receiver is in the task");
        tokio::task::yield_now().await;
        assert!(task.is_finished(), "a requested stop drops the connection");
        task.await.unwrap();
    }

    #[test]
    fn what_this_module_can_log_is_the_redacted_authority() {
        // Asserted at the module that writes the lines, not only at the type:
        // `run` takes a `ServerUrl`, so every `server = %url` field above
        // resolves through `Display` whatever the author of the next line
        // intends.
        let url = ServerUrl::parse("wss://token:secret@ingest.arkyve.dev:8443/path?key=abc")
            .expect("a wss:// URL with userinfo is dialable, just not loggable");
        assert_eq!(url.to_string(), "wss://ingest.arkyve.dev:8443");
        assert!(!format!("{url:?}").contains("secret"));
        assert!(url.as_str().contains("token:secret"));
    }
}
