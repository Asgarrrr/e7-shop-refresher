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
    /// The link came back after a reported outage.
    LinkUp,
}
