//! Link to the analysis server: forwards the raw stream, receives alerts.

mod websocket;

pub mod protocol;

pub use websocket::run;
