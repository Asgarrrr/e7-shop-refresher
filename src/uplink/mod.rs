//! Link to the analysis server: forwards the raw stream, receives the
//! decoded shop snapshots and purchase echoes.

mod websocket;

pub mod protocol;
pub mod vocabulary;

pub use vocabulary::VocabularyCell;
pub use websocket::run;

/// What the uplink task reports to the session. The journal is the only surface
/// a windowed-build player sees — tracing is inert there, so outages must travel
/// this channel.
#[derive(Debug)]
pub enum UplinkEvent {
    Message(protocol::ServerMessage),
    /// The link failed or dropped; reported once per outage, not per retry. The
    /// reason becomes a journal line, which is mirrored into the log file the
    /// player is asked to send us — so it must never carry the server URL, whose
    /// userinfo and query can hold a credential.
    LinkDown(String),
    /// The link came back after a reported outage: it was accepted *and stayed
    /// up* long enough to count as one (`LINK_SETTLED` in `websocket`, summed
    /// over the outage's reconnects). A completed handshake is deliberately not
    /// this event — the controller re-grants the watchdog's expectation deadline
    /// on every one of these, so a peer that accepts and hangs up would keep a
    /// 10 s recovery ladder from ever climbing.
    LinkUp,
}
