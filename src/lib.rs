//! Secret Shop relay.
//!
//! A fully passive pipeline: it *observes* a copy of the game's traffic,
//! reassembles it into an ordered stream, and forwards the raw bytes to the
//! analysis server, which interprets them. The relay never alters
//! the game's communications and never sends anything back to the game.
//!
//! ```text
//! WinDivert SNIFF ─▶ parse IP/TCP ─▶ TCP reassembly ─▶ gate ─▶ WebSocket ─▶ server
//!    (blocking)                       (ordered/dedup)                  ▲         │
//!                                                                  alerts ◀──────┘
//! ```

pub mod app;
pub mod capture;
pub mod config;
pub mod domain;
pub mod error;
pub mod stream;
pub mod uplink;
pub mod watch;

pub use config::Config;
pub use error::{Error, Result};
