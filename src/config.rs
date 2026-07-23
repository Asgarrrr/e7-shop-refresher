//! Relay configuration, loaded from a TOML file (defaults otherwise).

pub mod persist;

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::actuator::plan::Timings;
use crate::domain::control::Limits;
use crate::domain::filter::Filter;
use crate::domain::shop::ItemKind;
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

    /// Player interest criteria; the default (empty) filter matches every
    /// available item.
    pub filter: Filter,

    /// Refresh-loop stop limits; the default sets none.
    pub limits: Limits,

    /// Click-emulation behavior (acted on by the Windows build; the section
    /// always parses).
    pub actuator: ActuatorConfig,
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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActuatorConfig {
    /// Journal the planned clicks (screen coordinates and waits) without
    /// sending any input to the game.
    pub dry_run: bool,

    /// How clicks reach the game window.
    pub backend: ActuatorBackend,

    /// Per-action random extra-wait ranges, added on top of the tuned click
    /// baselines; the all-`0..=0` default keeps the calibrated timing.
    pub timings: Timings,
}

/// Input backend of the Windows build.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActuatorBackend {
    /// `SendInput`: drives the real cursor and forces the game window to the
    /// foreground. Works whatever the engine reads input from — the fallback
    /// if a game update stops honoring posted messages.
    Input,
    /// `PostMessageW`: posts synthetic mouse messages to the window — no
    /// focus stolen, the player keeps the mouse. Live-validated against the
    /// game (refresh, buys, wheel scroll, unfocused window).
    #[default]
    Message,
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
            server_url: "wss://ingest.arkyve.dev/refresh-shop".to_owned(),
            forward: ForwardConfig::default(),
            reconnect: ReconnectConfig::default(),
            capture: CaptureConfig::default(),
            filter: Filter::default(),
            limits: Limits::default(),
            actuator: ActuatorConfig::default(),
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

/// True when the `ws://` authority is loopback, where cleartext never leaves
/// the machine. Accepts `host` or `host:port`, IPv6 in brackets.
fn is_loopback_ws_host(url: &str) -> bool {
    let after = match url.strip_prefix("ws://") {
        Some(rest) => rest,
        None => return false,
    };
    // Authority ends at the first path/query separator.
    let authority = after.split(['/', '?']).next().unwrap_or("");
    // Strip the port: an IPv6 literal is bracketed, so split off a trailing
    // ":port" only when it is not inside brackets.
    let host = if let Some(closing) = authority.strip_prefix('[') {
        // "[::1]:3001" -> "[::1]"
        closing
            .split_once(']')
            .map(|(h, _)| {
                let mut s = String::from("[");
                s.push_str(h);
                s.push(']');
                s
            })
            .unwrap_or_else(|| authority.to_owned())
    } else {
        authority
            .rsplit_once(':')
            .map(|(h, _)| h.to_owned())
            .unwrap_or_else(|| authority.to_owned())
    };
    matches!(
        host.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "[::1]" | "::1"
    )
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
        // `server_url` receives the reassembled game stream, which can carry
        // session tokens: require TLS (`wss://`) unless the host is loopback,
        // where cleartext (`ws://`) never leaves the machine.
        let url = self.server_url.trim();
        if url.starts_with("wss://") {
            // TLS: fine.
        } else if url.starts_with("ws://") {
            if !is_loopback_ws_host(url) {
                return Err(crate::Error::Config(
                    "server_url uses ws:// to a non-loopback host — captured traffic \
                     would be sent in cleartext; use wss:// (or ws:// only for \
                     127.0.0.1/localhost)"
                        .into(),
                ));
            }
        } else {
            return Err(crate::Error::Config(
                "server_url must be a ws:// or wss:// URL".into(),
            ));
        }
        // `ItemKind` is wire-tolerant (`serde(other)` -> Unknown), which in a
        // config file would let a typo silently match nothing: reject it here.
        if self.filter.kinds.contains(&ItemKind::Unknown) {
            return Err(crate::Error::Config(
                "unrecognized kind in [filter] kinds (expected: equipment, hero, token)".into(),
            ));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn misspelled_kind_value_is_rejected() {
        // serde(other) folds unknown kind strings into Unknown; validate()
        // must catch it or the typo silently matches nothing.
        let config: Config =
            toml::from_str("[filter]\nkinds = [\"equipement\"]").expect("parses tolerant");
        assert_eq!(config.filter.kinds, vec![ItemKind::Unknown]);
        assert!(config.validate().is_err());
    }

    #[test]
    fn full_filter_and_limits_sections_parse() {
        let config: Config = toml::from_str(
            r#"
            [filter]
            kinds = ["equipment", "hero"]
            sets = ["set_speed", "set_counter"]
            min_substats = 3
            max_price = 300000
            include_sold_out = true

            [[filter.required_substats]]
            name = "speed"
            min = 8.0

            [[filter.required_substats]]
            name = "cri"

            [limits]
            max_refreshes = 100
            max_spend = 300
            max_matches = 5
            max_duration_ms = 3600000
            "#,
        )
        .expect("config should parse");

        assert_eq!(
            config.filter.kinds,
            vec![ItemKind::Equipment, ItemKind::Hero]
        );
        assert_eq!(config.filter.sets, vec!["set_speed", "set_counter"]);
        assert_eq!(config.filter.min_substats, Some(3));
        assert_eq!(config.filter.max_price, Some(300_000));
        assert!(config.filter.include_sold_out);
        assert_eq!(config.filter.required_substats.len(), 2);
        assert_eq!(config.filter.required_substats[0].name, "speed");
        assert_eq!(config.filter.required_substats[0].min, Some(8.0));
        assert_eq!(config.filter.required_substats[1].name, "cri");
        assert_eq!(config.filter.required_substats[1].min, None);

        assert_eq!(config.limits.max_refreshes, Some(100));
        assert_eq!(config.limits.max_spend, Some(300));
        assert_eq!(config.limits.max_matches, Some(5));
        assert_eq!(config.limits.max_duration_ms, Some(3_600_000));
    }

    #[test]
    fn missing_filter_and_limits_sections_default() {
        let config: Config = toml::from_str("game_port = 3333").expect("config should parse");
        assert!(config.filter.kinds.is_empty());
        assert!(config.filter.required_substats.is_empty());
        assert_eq!(config.filter.max_price, None);
        assert_eq!(config.limits.max_refreshes, None);
        assert_eq!(config.limits.max_spend, None);
    }

    #[test]
    fn partial_sections_leave_other_fields_default() {
        let config: Config = toml::from_str(
            r#"
            [filter]
            min_substats = 4

            [limits]
            max_spend = 50
            "#,
        )
        .expect("config should parse");
        assert_eq!(config.filter.min_substats, Some(4));
        assert!(config.filter.kinds.is_empty());
        assert_eq!(config.limits.max_spend, Some(50));
        assert_eq!(config.limits.max_refreshes, None);
    }

    #[test]
    fn actuator_section_parses_and_defaults_off() {
        let config: Config =
            toml::from_str("[actuator]\ndry_run = true").expect("config should parse");
        assert!(config.actuator.dry_run);
        // Absent section (and absent key) default to a live actuator.
        let config: Config = toml::from_str("[actuator]").expect("config should parse");
        assert!(!config.actuator.dry_run);
        assert!(!Config::default().actuator.dry_run);
    }

    #[test]
    fn misspelled_actuator_key_is_rejected() {
        // A silently ignored `dry_run` typo would send real clicks.
        assert!(toml::from_str::<Config>("[actuator]\ndryrun = true").is_err());
    }

    #[test]
    fn actuator_backend_parses_and_defaults_to_message() {
        let config: Config =
            toml::from_str("[actuator]\nbackend = \"input\"").expect("config should parse");
        assert_eq!(config.actuator.backend, ActuatorBackend::Input);
        // Absent key: the live-validated message backend — the player keeps
        // the mouse.
        let config: Config = toml::from_str("[actuator]").expect("config should parse");
        assert_eq!(config.actuator.backend, ActuatorBackend::Message);
        assert_eq!(Config::default().actuator.backend, ActuatorBackend::Message);
    }

    #[test]
    fn actuator_timings_parse_and_default_to_zero() {
        let config: Config = toml::from_str(
            r#"
            [actuator.timings]
            refreshed = { min_ms = 200, max_ms = 800 }
            between_buys = { min_ms = 100, max_ms = 500 }
            "#,
        )
        .expect("config should parse");
        assert_eq!(config.actuator.timings.refreshed.min_ms, 200);
        assert_eq!(config.actuator.timings.refreshed.max_ms, 800);
        assert_eq!(config.actuator.timings.between_buys.max_ms, 500);
        // Unset ranges stay at the calibrated baseline (0..=0 extra).
        assert_eq!(config.actuator.timings.shop_opened.max_ms, 0);
        assert_eq!(Config::default().actuator.timings.refreshed.max_ms, 0);
    }

    #[test]
    fn misspelled_timings_key_is_rejected() {
        // A silently ignored typo would leave the loop at the baseline while
        // the player thinks they slowed it down.
        assert!(toml::from_str::<Config>("[actuator.timings]\nrefesh = { min_ms = 500 }").is_err());
        // A typo inside a range is caught too (deny_unknown_fields on the range).
        assert!(toml::from_str::<Config>("[actuator.timings.refreshed]\nminms = 500").is_err());
    }

    #[test]
    fn unknown_actuator_backend_is_rejected() {
        // A silently defaulted typo would steal the mouse the player asked
        // to keep.
        assert!(toml::from_str::<Config>("[actuator]\nbackend = \"postmessage\"").is_err());
    }

    #[test]
    fn misspelled_limit_key_is_rejected() {
        // A silently ignored typo would mean a limit that never triggers.
        assert!(toml::from_str::<Config>("[limits]\nmax_refresh = 10").is_err());
        assert!(toml::from_str::<Config>("[filter]\nmax_prices = 10").is_err());
    }

    #[test]
    fn required_substat_without_name_is_rejected() {
        let result = toml::from_str::<Config>(
            r#"
            [[filter.required_substats]]
            min = 8.0
            "#,
        );
        assert!(result.is_err());
    }

    /// Builds a default `Config` (which forwards `server_to_client`, so
    /// `validate` reaches the scheme check) with `server_url` overwritten.
    fn config_with_url(server_url: &str) -> Config {
        Config {
            server_url: server_url.to_owned(),
            ..Config::default()
        }
    }

    #[test]
    fn wss_is_accepted() {
        assert!(config_with_url("wss://ingest.arkyve.dev/refresh-shop")
            .validate()
            .is_ok());
    }

    #[test]
    fn ws_loopback_ipv4_accepted() {
        assert!(config_with_url("ws://127.0.0.1:3001/refresh-shop")
            .validate()
            .is_ok());
    }

    #[test]
    fn ws_localhost_accepted() {
        assert!(config_with_url("ws://localhost:3001/x").validate().is_ok());
    }

    #[test]
    fn ws_ipv6_loopback_accepted() {
        assert!(config_with_url("ws://[::1]:3001/x").validate().is_ok());
    }

    #[test]
    fn ws_remote_host_rejected() {
        let err = config_with_url("ws://ingest.arkyve.dev/x")
            .validate()
            .unwrap_err();
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn ws_example_com_rejected() {
        // Done-criteria spot check: a non-loopback ws:// host is refused.
        let err = config_with_url("ws://example.com/x").validate().unwrap_err();
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn non_ws_scheme_rejected() {
        let err = config_with_url("http://example.com").validate().unwrap_err();
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn empty_still_rejected() {
        let err = config_with_url("").validate().unwrap_err();
        assert!(matches!(err, crate::Error::Config(_)));
    }
}
