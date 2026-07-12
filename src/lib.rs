//! Secret Shop relay.
//!
//! The capture side *observes* a copy of the game's traffic, reassembles it
//! into an ordered stream, and forwards the raw bytes to the analysis
//! server, which interprets them; the game's network traffic is never
//! altered. On Windows the tool also drives the Secret Shop itself via
//! click emulation (refresh and buy), steered by the decoded snapshots.
//!
//! ```text
//! WinDivert SNIFF ─▶ parse IP/TCP ─▶ TCP reassembly ─▶ gate ─▶ WebSocket ─▶ server
//!    (blocking)                       (ordered/dedup)                  ▲         │
//!                                                               snapshots ◀──────┘
//! ```

pub mod actuator;
pub mod app;
pub mod capture;
pub mod config;
pub mod crash;
pub mod domain;
pub mod error;
pub mod journal;
mod render;
pub mod stream;
#[cfg(feature = "gui")]
pub mod ui;
pub mod uplink;
pub mod watch;

pub use config::Config;
pub use error::{Error, Result};

/// The one place the product name lives: window titles and the welcome
/// screen must never disagree.
pub const APP_NAME: &str = "Arkyve Refresh Shop";
