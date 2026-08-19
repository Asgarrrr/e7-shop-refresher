//! Link to the analysis server: forwards the raw stream, receives the
//! decoded shop snapshots and purchase echoes.

mod websocket;

pub mod protocol;

pub use websocket::run;

/// What the uplink task reports to the session: decoded server messages plus
/// link-state transitions. The journal is the only surface a windowed-build
/// player sees — tracing is inert there, so outages must travel this channel.
#[derive(Debug)]
pub enum UplinkEvent {
    /// A decoded server message.
    Message(protocol::ServerMessage),
    /// The link failed or dropped; reported once per outage, not per retry. The
    /// reason becomes a journal line, which is mirrored into the log file the
    /// player is asked to send us — so it must never carry the server URL, whose
    /// userinfo and query can hold a credential.
    LinkDown(String),
    /// The link came back after a reported outage: a connection that was
    /// accepted *and stayed up* long enough to count as one (`LINK_SETTLED` in
    /// `websocket`). A completed handshake alone is deliberately not this event.
    /// A peer that accepts the upgrade and immediately hangs up would otherwise
    /// report a recovery per retry, which is the mirror image of the `LinkDown`
    /// contract above — and worse than a noisy journal, because the controller
    /// re-grants the watchdog's expectation deadline on every one of these, so a
    /// recovery ladder measured in 10 s windows would never climb.
    LinkUp,
}
