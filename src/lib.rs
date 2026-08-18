//! Secret Shop relay.
//!
//! The capture side *observes* a copy of the game's traffic, reassembles it
//! into an ordered stream, and forwards the raw bytes to the analysis
//! server, which interprets them; the game's network traffic is never
//! altered. On Windows the tool also drives the Secret Shop itself via
//! click emulation (refresh and buy), steered by the decoded snapshots.
//!
//! ```text
//! WinDivert copy ─▶ parse IP/TCP ─▶ TCP reassembly ─▶ gate ─▶ WebSocket ─▶ server
//!    (blocking)                       (ordered/dedup)                  ▲         │
//!                                                               snapshots ◀──────┘
//! ```

pub mod actuator;
pub mod app;
// The elevated capture broker. Declared unconditionally even though everything
// that touches Win32 inside it is gated: the argv validators are pure Rust and
// are the whole surface the low-privilege side has on the administrator process,
// so their tests belong in every lane, including the two portable ones that
// build without `windivert-backend`.
pub mod broker;
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

/// Filesystem-safe app folder name, shared by the per-user data locations:
/// the config under `%APPDATA%` (roaming) and the crash log under
/// `%LOCALAPPDATA%` (local). One constant so the two never diverge.
pub const APP_DIR: &str = "arkyve-refresh-shop";
