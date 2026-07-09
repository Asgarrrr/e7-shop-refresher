//! Relay configuration, loaded from a TOML file (defaults otherwise).

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::error::Result;

/// TCP port of the Epic Seven game server (`msg://`).
pub const DEFAULT_GAME_PORT: u16 = 3333;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// TCP port of the game server, remote side.
    pub game_port: u16,

    /// Analysis server URL (`ws://` or `wss://`).
    pub server_url: String,

    /// Stream directions to forward to the server.
    pub forward: ForwardConfig,

    /// Reconnection policy for the server link.
    pub reconnect: ReconnectConfig,

    /// Low-level capture settings.
    pub capture: CaptureConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ForwardConfig {
    /// Server -> client responses: carry the shop contents.
    pub server_to_client: bool,
    /// Client -> server requests: context (issued command), optional.
    pub client_to_server: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReconnectConfig {
    /// Initial delay before retrying (milliseconds).
    pub initial_ms: u64,
    /// Cap on the exponential backoff (milliseconds).
    pub max_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    /// Receive buffer size for one WinDivert packet (bytes).
    pub buffer_size: usize,
    /// Explicit WinDivert filter; otherwise derived from `game_port` + `forward`.
    pub filter: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            game_port: DEFAULT_GAME_PORT,
            server_url: "ws://127.0.0.1:3001/refresh-shop".to_owned(),
            forward: ForwardConfig::default(),
            reconnect: ReconnectConfig::default(),
            capture: CaptureConfig::default(),
        }
    }
}

impl Default for ForwardConfig {
    fn default() -> Self {
        // Shop contents live in the server -> client responses.
        Self {
            server_to_client: true,
            client_to_server: false,
        }
    }
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            initial_ms: 1_000,
            max_ms: 30_000,
        }
    }
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            buffer_size: 65_535,
            filter: None,
        }
    }
}

impl Config {
    /// Loads the configuration from `path`. A missing file yields the defaults.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let config: Config = toml::from_str(&text)?;
                config.validate()?;
                Ok(config)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.into()),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.game_port == 0 {
            return Err(crate::Error::Config("game_port cannot be 0".into()));
        }
        if !self.forward.server_to_client && !self.forward.client_to_server {
            return Err(crate::Error::Config(
                "at least one direction must be forwarded (forward)".into(),
            ));
        }
        if self.server_url.trim().is_empty() {
            return Err(crate::Error::Config("server_url is empty".into()));
        }
        // A segment's direction is inferred by comparing its ports to
        // `game_port`: a custom filter capturing a different port delivers
        // traffic nothing can classify — zero segments, no error.
        if let Some(filter) = &self.capture.filter
            && !filter.contains(&self.game_port.to_string())
        {
            return Err(crate::Error::Config(format!(
                "capture.filter does not reference game_port ({}): no packet would be classified",
                self.game_port
            )));
        }
        Ok(())
    }

    /// Effective WinDivert filter: only the directions to forward.
    ///
    /// The shop response travels server -> client (`tcp.SrcPort == game_port`).
    pub fn capture_filter(&self) -> String {
        if let Some(filter) = &self.capture.filter {
            return filter.clone();
        }
        let mut clauses = Vec::new();
        if self.forward.server_to_client {
            clauses.push(format!("tcp.SrcPort == {}", self.game_port));
        }
        if self.forward.client_to_server {
            clauses.push(format!("tcp.DstPort == {}", self.game_port));
        }
        format!("tcp and ({})", clauses.join(" or "))
    }

    pub fn reconnect_initial(&self) -> Duration {
        Duration::from_millis(self.reconnect.initial_ms)
    }

    pub fn reconnect_max(&self) -> Duration {
        Duration::from_millis(self.reconnect.max_ms.max(self.reconnect.initial_ms))
    }
}
