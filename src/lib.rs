//! Secret Shop Watcher — relais local.
//!
//! Pipeline entièrement passif : on **observe** une copie du trafic du jeu, on
//! le réassemble en un flux ordonné, et on transmet les octets bruts au serveur
//! d'analyse qui, lui, déchiffre et interprète. Le relais n'altère jamais les
//! communications du jeu et n'envoie rien vers celui-ci.
//!
//! ```text
//! WinDivert SNIFF ─▶ parse IP/TCP ─▶ réassemblage TCP ─▶ gate ─▶ WebSocket ─▶ serveur
//!    (bloquant)                        (ordonné/dédup)                   ▲          │
//!                                                                  alertes ◀───────┘
//! ```

pub mod app;
pub mod capture;
pub mod config;
pub mod error;
pub mod stream;
pub mod uplink;
pub mod watch;

pub use config::Config;
pub use error::{Error, Result};
