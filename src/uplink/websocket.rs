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

/// How long the link has to stay up before it counts as a *link* rather than a
/// completed handshake — accumulated across one outage's reconnects, not
/// demanded of a single one of them. The last two paragraphs are why the
/// accounting is cumulative; the ones before them are why there is a window at
/// all.
///
/// A peer can accept the WebSocket upgrade and close the socket in the same
/// millisecond: an auth reject that answers on the wire instead of in the status
/// line, a rate limiter, a load balancer draining the instance it just handed
/// us, a server that speaks a dialect this build does not and hangs up on the
/// first frame. At the instant of the upgrade every one of those is
/// indistinguishable from the real thing, so the upgrade cannot be what decides
/// that the link is back.
///
/// Treating it as such is what made the retry delay stand still: reset on
/// connect, disconnect immediately, wait `initial_ms`, double it, throw the
/// doubled value away at the next connect. The delay never grew past the
/// initial one — a dial per second, by default, for as long as the peer keeps
/// behaving that way — and the two things riding on that reset were worse than
/// the dial rate. `outage_reported` cleared on each reconnect, so `LinkDown`
/// landed in the journal once per *retry* against its own contract in
/// `super::UplinkEvent` ("once per outage"); and every `LinkUp` re-grants the
/// watchdog's expectation deadline, so a recovery ladder that needs three
/// consecutive 10 s windows to reach an honest halt could never climb past the
/// first while a handshake was re-granting it every second.
///
/// 10 s is the watchdog's own `EXPECT_SNAPSHOT_MS`/`EXPECT_PURCHASE_MS` window
/// (`crate::domain::control`): a connection that cannot hold that long cannot
/// carry a refresh and the snapshot that answers it, which is the only thing
/// this link exists to do. It is also two orders of magnitude above the
/// accept-then-close cases above, which end within an RTT of the upgrade, so
/// the two are separated by a wide margin rather than by a hair.
///
/// Why the ten seconds accumulate instead of being demanded in one unbroken
/// stretch: read per connection, this window had a terminal state. A link that
/// keeps coming back and keeps dying at, say, six seconds — a proxy with an
/// idle timeout shorter than this one, a load balancer recycling connections, a
/// congested uplink — reports its first `LinkDown` and then nothing, ever,
/// because `LinkUp` is emitted from here and `Controller::link_up` is set by
/// that event and by nothing else. `Controller::watchdog` then returns early on
/// every tick for the rest of the session: no confirm re-click, no re-issue,
/// and no honest `Unresponsive` halt either. A hunt paused on a missed purchase
/// echo stays paused forever, over a wire that was carrying traffic six seconds
/// out of every six-and-a-bit. That is strictly worse than the flapping the
/// window exists to prevent, and it is not what the argument above bought.
///
/// Counting connected time across the outage keeps both halves. The
/// accept-then-close peers contribute one round-trip apiece, so ten seconds of
/// them is thousands of retries at a delay that has long since climbed to
/// `reconnect.max_ms` — days of it — and they still never report a recovery. A
/// link that is genuinely up most of the time reports one every `LINK_SETTLED`
/// of *uptime*, which is the rate the anti-flap argument was really about: what
/// made the old reset intolerable was a handshake re-granting the watchdog's
/// deadline every second, and uptime cannot be spent faster than it is earned.
///
/// What it still costs: a link too poor to accumulate ten seconds across a
/// whole outage is sampled at `reconnect.max_ms` (30 s by default) and reports
/// no recovery at all. That half stays deliberate — such a link cannot carry a
/// purchase confirmation, which is the operation those retries are for, and the
/// standing `LinkDown` is the honest report for it. The watchdog stays
/// suspended there on purpose: no proof can arrive over a wire that is not
/// there, and halting the hunt with `Unresponsive` would blame the game for the
/// network.
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
/// - `url`: the server, as a [`ServerUrl`] rather than a `String`. The dial form
///   comes out through [`ServerUrl::as_str`] at the one place that dials, and
///   every log line in this module gets the redacted form through the type's own
///   `Display`, so no `%url` here can put a credential in the file the README
///   asks the player to send us.
/// - `outbound`: raw byte batches to send (closing it stops the loop).
/// - `inbound`: decoded messages received from the server.
/// - `shutdown`: the session-wide stop signal. Raced against every window this
///   loop can park in, so teardown does not have to reach the task by `abort`.
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
    // each retry stays a tracing detail, recovery reports once — and only once
    // the link has been up for `LINK_SETTLED`, since a peer that accepts the
    // upgrade and hangs up is a retry, not a recovery.
    let mut outage_reported = false;
    // Connected time this outage has accumulated towards `LINK_SETTLED`, across
    // however many reconnects it took. Cleared when the link settles: a settled
    // link's own uptime is spent, and the next outage starts from zero.
    let mut connected_for = Duration::ZERO;
    // `server` is `url`'s **redacted** `scheme://host[:port]` form, and it cannot
    // be anything else: `%url` goes through `ServerUrl`'s `Display`, which prints
    // only that. The dial string — userinfo and query intact, either able to
    // carry a credential — is reachable solely through `as_str()`, used once
    // below to hand to the connector. That is the difference between the promise
    // in `README.md` ("the log never contains the server URL's credentials")
    // being kept by every author of every line here and being kept by the type.
    // `attempt` rides along because it is what makes the 1st reconnect legible
    // from the 40th.
    let mut attempt: u64 = 0;

    loop {
        // Read before the attempt, not only in the races below: the signal may
        // already be set when the task first runs (a window closed during
        // startup), in which case `changed()` would never fire.
        if is_stopping(&shutdown) {
            return;
        }
        attempt += 1;
        let connecting = tokio::time::timeout(CONNECT_TIMEOUT, connect(url.as_str().to_owned()));
        let connected = tokio::select! {
            biased;
            // A stop during the handshake window: up to CONNECT_TIMEOUT, 15 s,
            // which is 15 s of the player staring at a closed window before the
            // process goes. Dropping `connecting` here cancels the handshake,
            // which owns nothing but the socket it is opening.
            () = wait_for_shutdown(&mut shutdown) => return,
            result = connecting => result,
        };
        match connected {
            Ok(Ok(stream)) => {
                // `established` is the honest word for what just happened and
                // nothing more: the upgrade completed. Whether it is a *link* is
                // decided `LINK_SETTLED` later, below.
                info!(server = %url, attempt, "server link established");
                // The connected half is the only part of this module whose events
                // do not already carry `attempt` — `send failed`, `send stalled`,
                // `WebSocket read error` and `forward`'s two decode lines are all
                // emitted inside. A span carries the pair down to them, so a read
                // error in a long session is attributable to *which* connection.
                // `.instrument()`, not an `.entered()` guard: `pump` awaits, and a
                // guard held across an await is the classic way a span leaks onto
                // whatever task the executor polls next.
                let session = pump(stream, &mut outbound, &inbound, &mut shutdown)
                    .instrument(tracing::info_span!("link", server = %url, attempt));
                tokio::pin!(session);
                let connected_at = tokio::time::Instant::now();
                // What this connection still owes, not the whole window: the
                // seconds earlier reconnects in this same outage already served
                // count towards it (see `LINK_SETTLED`).
                let owed = LINK_SETTLED.saturating_sub(connected_for);
                // Two phases rather than a `select!` loop: the deadline matters
                // exactly once per connection, and past it there is nothing left
                // to race, so the second phase is a bare `await`. `biased` puts
                // the session first, which decides the tie — a connection that
                // ends in the same poll as the deadline reads as "did not hold",
                // the conservative half of a distinction that costs one extra
                // backoff step and hands the whole ten seconds to the next
                // connection, which then settles as soon as it opens. That is
                // the one case where an upgrade alone ends an outage, and it is
                // still ten seconds of measured uptime that says so.
                let ended_before_settling = tokio::select! {
                    biased;
                    outcome = &mut session => Some(outcome),
                    () = tokio::time::sleep(owed) => None,
                };
                let outcome = match ended_before_settling {
                    Some(outcome) => {
                        // Short of the window, but not nothing: what this
                        // connection did serve is what the next one builds on.
                        // Saturating because the alternative is a panic in a
                        // debug build over an arithmetic case a `Duration` this
                        // small cannot reach.
                        connected_for = connected_for.saturating_add(connected_at.elapsed());
                        outcome
                    }
                    None => {
                        // One decision, two effects, deliberately inseparable:
                        // the connection that earned the delay back is the same
                        // one allowed to end the outage. Splitting them is how
                        // this went wrong in the first place.
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
                        // Latched like the two failure arms below and for the
                        // same contract: a peer that accepts and hangs up is one
                        // outage that keeps failing, not an outage per second.
                        // The `warn!` above still fires every cycle — the log
                        // carries the retry churn, the journal carries the
                        // transition.
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
                    // Safe to mirror into the journal: of the `WsError` variants
                    // reachable from `connect_async`, none embeds the URL in its
                    // `Display` (`UrlError::UnableToConnect`, the only one that
                    // does, is built solely by the blocking `client::connect` —
                    // checked against tungstenite 0.29).
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
        // channel fills, reassembly blocks, and the stall propagates back to the
        // capture thread — the kernel then drops packets, creating real gaps that
        // can never be filled. Better to drop bytes while the server is
        // unreachable (it resyncs on reconnect).
        if drain_until(&mut outbound, backoff.current(), &mut shutdown).await {
            return; // outbound closed or shutdown requested.
        }
        backoff.advance();
    }
}

/// The signal's current value, read without awaiting.
///
/// A named helper rather than an inline `*shutdown.borrow()` in each `if`:
/// `watch::Ref` is not `Send`, and a borrow guard whose temporary outlives an
/// `await` in the same statement would make the whole future non-`Send`, which
/// `SessionWorkers::spawn` requires. Confining it to a sync `fn` makes that
/// impossible rather than merely avoided.
fn is_stopping(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow()
}

/// Parks until the session-wide stop signal is set, or until the last sender is
/// gone.
///
/// `watch::Receiver::changed` only reports a *change*, so the current value is
/// checked first — the signal may already be set. A dropped sender resolves this
/// too rather than parking forever: it can only mean the session that owns the
/// signal is already gone, which is the same instruction.
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
/// session asked to shut down — and `false` if the delay simply elapsed.
///
/// The shutdown arm matters more here than anywhere else in this module: `wait`
/// is `backoff.current()`, which climbs to `reconnect.max_ms`, so a stop
/// requested one tick into a backed-off retry used to wait the whole delay out.
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
///
/// A third arm races the session's stop signal against both, because a connected
/// link parks indefinitely on purpose — the reader waits for a server that may
/// have nothing to say for minutes — and a `SEND_TIMEOUT` write can hold the
/// writer for 10 s. Without it the only thing that reached this task at teardown
/// was `SessionWorkers`' `abort` after the grace deadline, which `report_join`
/// then deliberately says nothing about: the one worker with no cooperative exit
/// was also the one that could not report that it had been cancelled.
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

    // A cooperative stop, reported as the same `Shutdown` a closed outbound
    // channel produces: both mean "do not reconnect". The in-flight send, if any,
    // is dropped with its budget lease — the tolerance the module header
    // documents for a link that goes away mid-write.
    let stopping = async {
        wait_for_shutdown(shutdown).await;
        Outcome::Shutdown
    };

    tokio::pin!(writer, reader, stopping);
    // `biased`, so a requested stop wins over a writer or reader that happens to
    // be ready in the same poll. It also makes the writer/reader order between
    // themselves deterministic, which is a change with no consequence: both end
    // the connection, and the outer loop decides what happens next from the
    // `Outcome` — of the two, the writer's `Shutdown` is the more conservative
    // reading when both are ready at once.
    tokio::select! {
        biased;
        outcome = &mut stopping => outcome,
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
    use std::future::{Future, pending, ready};
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use super::*;
    use crate::stream::{BudgetLimits, PipelineBudget};

    /// The server every connector test dials. `wss://`, not the `ws://test.invalid`
    /// these tests used before `run` took a `ServerUrl`: `ServerUrl::parse` refuses
    /// cleartext to a non-loopback host, which is the rule the type carries.
    fn test_url() -> ServerUrl {
        ServerUrl::parse("wss://test.invalid").expect("a wss:// URL is dialable")
    }

    /// A stop signal nothing ever sets. The sender is handed back so the caller
    /// can keep it alive: dropping the last sender *is* a stop, since a signal
    /// with no owner can only mean the session is already gone.
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

    /// A peer that speaks: yields a scripted sequence of inbound frames, then
    /// pends forever like `StalledLink`. `stalling_sends` additionally freezes
    /// the outbound half, so the two directions can be tested against each
    /// other rather than only one at a time.
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

    /// A peer that carries the connection for a while and *then* hangs up: the
    /// middle ground between `StalledLink`, which never ends, and a
    /// `ScriptedLink` that closes in the same poll as the upgrade. That middle
    /// ground is the whole subject of `LINK_SETTLED`, and nothing else here
    /// could express it — a link either lived forever or died instantly.
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
        // Frozen, like `StalledLink`'s: these tests hold their outbound channel
        // open and empty, so the writer parks on `recv` and never gets here.
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

    /// The inbound half of `pump` with nothing outbound in flight: the returned
    /// outcome plus every event the connection produced.
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

    /// The next event, or a failure. A test that waits on an event its producer
    /// no longer emits would otherwise park forever under `start_paused`, and a
    /// hang proves nothing.
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

        // The peer has a shop for us and a frozen receive window: the read half
        // must not wait out `SEND_TIMEOUT` behind the write half.
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

        // And the write half still gives up on its own schedule.
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

    /// Joins the two halves of the outage protocol. `run_with_connector` is the
    /// only producer of `LinkDown`/`LinkUp`, and those are what suspend and
    /// re-grant the recovery watchdog. Delete either send and every controller
    /// test still passes, while a session that survived one outage sits with its
    /// watchdog suspended forever: no escalation, no honest halt.
    ///
    /// It also pins *when* the recovery half is allowed to fire: on a connection
    /// that held for `LINK_SETTLED`, never on the handshake that opened it. The
    /// redial below is deliberately observed twice — once right after it lands,
    /// where it must report nothing, and once past the deadline, where it must
    /// report the recovery.
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
                // First dial: a peer that hangs up. Second: one that connects and
                // stays quiet, so the loop settles after reporting recovery.
                let link = if dial == 1 {
                    ScriptedLink::new(vec![Ok(Message::Close(None))])
                } else {
                    ScriptedLink::new(Vec::new()).stalling_sends()
                };
                ready(Ok::<_, WsError>(link))
            },
        ));

        // Bounded, not a bare `recv().await`: a producer that stops emitting has
        // to fail this test, and under paused time a missing event is a hang.
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
    /// exponential backoff into a fixed one. `reset()` fired on the handshake,
    /// so the doubling computed at the end of each cycle was thrown away at the
    /// start of the next: the delays below were 100, 100, 100 forever, at every
    /// layer that watches this link. Here they are 100, 200, 400 — the cycle
    /// pays for itself only if the connection lives.
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
                // Accepted, then gone within the same poll: an auth reject on
                // the wire, a draining load balancer, a dialect mismatch.
                ready(Ok::<_, WsError>(ScriptedLink::new(vec![Ok(
                    Message::Close(None),
                )])))
            },
        ));

        tokio::task::yield_now().await;
        assert_eq!(*dials.lock().unwrap(), [Duration::ZERO]);

        // Ticked, not jumped one expected delay at a time. `tokio::time::advance`
        // moves the clock and wakes the parked task *once* however far it moved,
        // so a coarse step records its own schedule rather than the backoff's —
        // a version of this loop that advanced 100, then 200, then 400 measured
        // nothing at all and passed against the bug it was written for. Ticks
        // shorter than the shortest delay give every dial its own wake-up.
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

    /// The journal half of the same peer. `UplinkEvent::LinkDown` promises one
    /// report per outage; clearing `outage_reported` on the handshake broke that
    /// promise in both directions at once, publishing a `LinkDown` *and* a
    /// `LinkUp` per retry — a pair per second, by default, into the log file the
    /// player is asked to send us, and into the watchdog, whose expectation
    /// every `LinkUp` re-grants.
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

        // Four cycles of it, none of them lasting anywhere near `LINK_SETTLED`
        // — see the tick note in the backoff test above for why the clock is
        // walked rather than jumped.
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

    /// The state the settle window used to have no way out of. A link that
    /// keeps coming back but never holds `LINK_SETTLED` in one stretch reported
    /// one `LinkDown` and then nothing, forever — and `Controller::link_up` is
    /// set by `LinkUp` and by nothing else, so `Controller::watchdog` returned
    /// early on every tick from then on: no confirm re-click, no re-issue, no
    /// honest `Unresponsive` halt. A hunt paused on a missed purchase echo just
    /// stayed paused, over a wire that was carrying traffic six seconds out of
    /// every six-and-a-tenth.
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
                // Six seconds each time: long enough to carry a refresh and its
                // snapshot, short enough that no single connection ever settles.
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
    /// is connected *time*, never connect attempts. A peer that hangs up a
    /// millisecond after the upgrade — an auth reject answered on the wire, a
    /// draining load balancer, a dialect mismatch — earns no recovery however
    /// many times it does it, which is the whole reason the handshake alone was
    /// never allowed to be one.
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
            // Pinned to the floor rather than a realistic 30 s cap: it is the
            // fastest this loop can legally dial, so it is the most cycles this
            // peer can possibly fit into the window below.
            Duration::from_millis(100),
            stop_rx,
            move |_url| ready(Ok::<_, WsError>(BriefLink::new(Duration::from_millis(1)))),
        ));

        // Three times `LINK_SETTLED` of wall clock, ~290 cycles of it, ~290 ms
        // of actual connectivity. The clock is slept through rather than walked
        // in steps: every timer in flight belongs to the task, so paused time
        // advances to each of them in turn.
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

    /// The success path of the writer: the chunk's bytes reach the wire, and the
    /// iteration keeps none of its budget lease. A refactor that retained the
    /// lease — batching sends, parking chunks for a retry — leaks the whole
    /// outbound quota over a session until reassembly stalls, and the stalled-send
    /// test would not notice: there the lease is released by the error path.
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

        // A connector that opens and never finishes: without CONNECT_TIMEOUT the
        // task parks here forever, emitting no LinkDown and never retrying.
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
        // The uplink was the one worker with no cooperative shutdown: teardown
        // reached it only through `SessionWorkers`' `abort` after the grace
        // deadline, and `report_join` suppresses the cancelled-join line, so
        // nothing said so either. The worst window was a handshake that opens and
        // never completes: 15 s of `CONNECT_TIMEOUT` with the window already gone.
        // `_raw_tx` is held on purpose — a closed outbound channel is the *other*
        // way this loop stops, and it would pass this test for the wrong reason.
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
        // The longest window of the three, and the one nothing else covered:
        // the backoff drain waits `backoff.current()`, which climbs to
        // `reconnect.max_ms` (see `drain_until`'s doc).
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
        // The third window: a link that connected and is simply quiet. Both halves
        // of `pump` park indefinitely there — the reader on a server with nothing
        // to send, the writer on an empty outbound channel — which is the normal
        // state of a healthy idle relay, not an error case.
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
            // `StalledLink` never yields a message and never completes a send.
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
        // `obs-001`'s last piece, asserted at the module that writes the lines
        // rather than only at the type. `run` takes a `ServerUrl`, so every
        // `server = %url` field above resolves through `Display` — the redacted
        // authority — whatever the author of the next line intends. The
        // credential-bearing spelling has exactly one exit, `as_str()`, used once,
        // to hand the connector a dial string.
        let url = ServerUrl::parse("wss://token:secret@ingest.arkyve.dev:8443/path?key=abc")
            .expect("a wss:// URL with userinfo is dialable, just not loggable");
        assert_eq!(url.to_string(), "wss://ingest.arkyve.dev:8443");
        assert!(!format!("{url:?}").contains("secret"));
        assert!(url.as_str().contains("token:secret"));
    }
}
