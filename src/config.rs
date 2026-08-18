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

/// Smallest effective delay between server connection attempts.
pub(crate) const RECONNECT_FLOOR: Duration = Duration::from_millis(100);

/// Ceiling on a single `[actuator.timings]` extra wait, in milliseconds.
///
/// One minute. The click baselines this adds onto are calibrated to the game's
/// blocking animations and span 100 ms (`scroll_settle`) to 1180 ms
/// (`shop_opened`), and the Setup tab's own meter tops out at 2500 ms total —
/// so 60 000 ms is roughly fifty times the slowest baseline and twenty-four
/// times anything the GUI can produce. Every legitimate "pause like a slow,
/// distracted human" setting stays reachable, plus a wide margin for
/// experimenting past what the UI offers.
///
/// What it makes unreachable is the two ways an unbounded value hurt:
/// a `max_ms` in the tens of minutes silently freezes the refresh loop between
/// two clicks with nothing to distinguish it from a hang, and a value near
/// `u64::MAX` overflows the plain `baseline + extra` sums the timing editor
/// does while painting a range (panic in debug, silent wrap in release). Every
/// other knob in this file is validated aggressively; this one was not
/// validated at all.
const MAX_TIMING_MS: u64 = 60_000;

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
    /// Receive buffer size for one WinDivert packet (bytes). Raised to the
    /// driver's own maximum when set lower.
    pub buffer_size: usize,
    /// Explicit WinDivert filter; otherwise derived from `game_port` +
    /// `forward`.
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
/// the machine. Accepts `host` or `host:port`, IPv6 in brackets. The scheme is
/// matched case-insensitively, and any `user:pass@` userinfo is dropped: the
/// real host is what follows the last `@` (what `http::Uri`, and thus the
/// WebSocket client, actually connects to), so `ws://127.0.0.1@evil.com` is
/// correctly seen as `evil.com` and rejected rather than passing as loopback.
fn is_loopback_ws_host(url: &str) -> bool {
    let after = match url.get(..5) {
        Some(prefix) if prefix.eq_ignore_ascii_case("ws://") => &url[5..],
        _ => return false,
    };
    // Authority ends at the first path/query separator.
    let authority = after.split(['/', '?']).next().unwrap_or("");
    // Drop any userinfo: the host is what follows the last '@'. Honoring this is
    // what stops a userinfo-embedded loopback from leaking traffic in cleartext
    // to a remote host the WebSocket client would actually dial.
    let authority = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
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
    ///
    /// # Errors
    ///
    /// - [`Error::ConfigRead`] — the file exists but could not be read (locked
    ///   by an antivirus, permission denied, a directory in its place). A
    ///   *missing* file is deliberately not an error: it yields
    ///   `Config::default()`, which is how a fresh machine starts.
    /// - [`Error::ConfigParse`] — not valid TOML, or it does not match this
    ///   struct: unknown or misspelled key (every section is
    ///   `deny_unknown_fields`, so a typo is refused rather than silently
    ///   ignored), wrong type, or an integer out of range.
    /// - [`Error::Config`] — it parsed but breaks an invariant:
    ///   - `game_port = 0`;
    ///   - `[forward]` with both directions off — nothing would be captured at
    ///     all;
    ///   - an empty `server_url`, a scheme other than `ws://` / `wss://`, or a
    ///     `ws://` URL pointing anywhere but loopback (it would forward the
    ///     captured game stream, session tokens included, in cleartext);
    ///   - an unrecognized value in `[filter] kinds`, which the wire-tolerant
    ///     `ItemKind` would otherwise fold into `Unknown` and match nothing;
    ///   - a `capture.filter` that never mentions `game_port`: the direction of
    ///     a segment is inferred from that port, so such a filter would deliver
    ///     packets nothing can classify — zero segments and no error;
    ///   - an `[actuator.timings]` range that is reversed (`min_ms > max_ms`)
    ///     or whose `max_ms` exceeds the 60 000 ms ceiling.
    ///
    /// [`Error::Config`]: crate::Error::Config
    /// [`Error::ConfigParse`]: crate::Error::ConfigParse
    /// [`Error::ConfigRead`]: crate::Error::ConfigRead
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let config: Config = toml::from_str(&text)?;
                config.validate()?;
                Ok(config)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            // Name the file: it lives out of the way under %APPDATA%, so a bare
            // "Access is denied. (os error 5)" leaves nothing to act on.
            Err(source) => Err(crate::Error::ConfigRead {
                path: path.to_path_buf(),
                source,
            }),
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
        // URL schemes are case-insensitive, so match `WSS://` too.
        let lower = url.to_ascii_lowercase();
        if lower.starts_with("wss://") {
            // TLS: fine.
        } else if lower.starts_with("ws://") {
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
        // `[actuator.timings]` reaches both the refresh loop and the Setup
        // meter unchecked otherwise. Two shapes have to be refused here, at the
        // root, rather than absorbed downstream:
        //
        // - a reversed range. With this TOML's inline form
        //   (`{ min_ms = 800, max_ms = 200 }`) swapping the two is an ordinary
        //   typo, and `DelayRange::draw` reads it as a fixed 800 ms delay — the
        //   player configures variability and silently gets none, while the
        //   Setup tab shows "Custom" with no clue why.
        // - an unbounded `max_ms`. It is what freezes the loop for ten minutes
        //   between two clicks, and what overflows the editor's `baseline + max`
        //   sums near `u64::MAX`.
        for (name, range) in self.actuator.timings.named_ranges() {
            if range.min_ms > range.max_ms {
                return Err(crate::Error::Config(format!(
                    "actuator.timings.{name} is reversed: min_ms = {} is above max_ms = {} — swap them (it would be read as a fixed {} ms delay, not a range)",
                    range.min_ms, range.max_ms, range.min_ms
                )));
            }
            if range.max_ms > MAX_TIMING_MS {
                return Err(crate::Error::Config(format!(
                    "actuator.timings.{name} max_ms = {} exceeds the {MAX_TIMING_MS} ms ceiling — that would stall the refresh loop between two clicks",
                    range.max_ms
                )));
            }
        }
        Ok(())
    }

    /// Effective WinDivert filter: only the directions to forward.
    ///
    /// The shop response travels server -> client (`tcp.SrcPort == game_port`).
    /// A `capture.filter` in the config replaces the derived expression
    /// wholesale — `validate` has already checked it names `game_port`, without
    /// which nothing downstream could classify a segment's direction.
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
        Duration::from_millis(self.reconnect.initial_ms).max(RECONNECT_FLOOR)
    }

    pub fn reconnect_max(&self) -> Duration {
        Duration::from_millis(self.reconnect.max_ms).max(self.reconnect_initial())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_and_validate(text: &str) -> Result<Config> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn capture_buffer_zero_is_accepted() {
        let config = parse_and_validate("[capture]\nbuffer_size = 0").expect("zero is compatible");
        assert_eq!(config.capture.buffer_size, 0);
    }

    #[test]
    fn capture_buffer_legacy_lower_value_is_accepted() {
        let config =
            parse_and_validate("[capture]\nbuffer_size = 65535").expect("legacy value is valid");
        assert_eq!(config.capture.buffer_size, 65_535);
    }

    #[test]
    fn capture_buffer_larger_than_the_driver_maximum_is_accepted() {
        let config =
            parse_and_validate("[capture]\nbuffer_size = 999999").expect("an oversized buffer");
        assert_eq!(config.capture.buffer_size, 999_999);
    }

    /// A custom filter replaces the derived one wholesale, so it is accepted —
    /// but only if it still names `game_port`, the port every direction check
    /// downstream is made against.
    #[test]
    fn custom_capture_filter_is_accepted_and_must_name_the_game_port() {
        let config = parse_and_validate("[capture]\nfilter = \"tcp and tcp.SrcPort == 3333\"")
            .expect("a filter naming game_port");
        assert_eq!(config.capture_filter(), "tcp and tcp.SrcPort == 3333");

        let error = parse_and_validate("[capture]\nfilter = \"tcp and tcp.SrcPort == 4444\"")
            .expect_err("a filter on another port classifies nothing");
        assert!(error.to_string().contains("capture.filter"));
    }

    #[test]
    fn derived_capture_filter_follows_the_forwarded_directions() {
        let mut config = Config::default();
        assert_eq!(config.capture_filter(), "tcp and (tcp.SrcPort == 3333)");

        config.forward.client_to_server = true;
        assert_eq!(
            config.capture_filter(),
            "tcp and (tcp.SrcPort == 3333 or tcp.DstPort == 3333)"
        );

        config.forward.server_to_client = false;
        config.game_port = 4444;
        assert_eq!(config.capture_filter(), "tcp and (tcp.DstPort == 4444)");
    }

    #[test]
    fn both_forward_directions_off_is_rejected() {
        let error =
            parse_and_validate("[forward]\nserver_to_client = false\nclient_to_server = false")
                .expect_err("nothing would be captured");
        assert!(error.to_string().contains("forward"));
    }

    /// The WinDivert filter expresses either direction, so asking for the
    /// client -> server context stream is a legitimate configuration.
    #[test]
    fn client_to_server_direction_is_accepted() {
        let config =
            parse_and_validate("[forward]\nserver_to_client = true\nclient_to_server = true")
                .expect("both directions are expressible");
        assert!(config.forward.client_to_server);
    }

    #[test]
    fn capture_buffer_overflow_is_rejected_during_deserialization() {
        let error = parse_and_validate("[capture]\nbuffer_size = 18446744073709551616")
            .expect_err("integer overflow must fail deserialization");
        assert!(matches!(error, crate::Error::ConfigParse(_)));
    }

    #[test]
    fn capture_buffer_default_is_the_documented_value() {
        assert_eq!(Config::default().capture.buffer_size, 65_535);
    }

    #[test]
    fn reconnect_durations_enforce_floor_and_order() {
        let mut config = Config::default();
        assert_eq!(config.reconnect_initial(), Duration::from_millis(1_000));
        assert_eq!(config.reconnect_max(), Duration::from_millis(30_000));

        config.reconnect.initial_ms = 1;
        config.reconnect.max_ms = 10;
        assert_eq!(config.reconnect_initial(), RECONNECT_FLOOR);
        assert_eq!(config.reconnect_max(), RECONNECT_FLOOR);

        config.reconnect.initial_ms = 2_000;
        config.reconnect.max_ms = 1_000;
        assert_eq!(config.reconnect_initial(), Duration::from_millis(2_000));
        assert_eq!(config.reconnect_max(), Duration::from_millis(2_000));
    }

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
    fn reversed_timing_range_is_rejected_and_names_both_values() {
        // `{ min_ms = 800, max_ms = 200 }` is a plausible typo in this inline
        // form. Accepted, it is silently reread as a fixed 800 ms delay: the
        // player gets none of the variability they configured.
        let error =
            parse_and_validate("[actuator.timings]\nrefreshed = { min_ms = 800, max_ms = 200 }")
                .expect_err("a reversed range must not be silently reinterpreted");
        let message = error.to_string();
        assert!(matches!(error, crate::Error::Config(_)));
        assert!(message.contains("actuator.timings.refreshed"), "{message}");
        assert!(
            message.contains("800") && message.contains("200"),
            "{message}"
        );
    }

    #[test]
    fn an_oversized_timing_range_is_rejected() {
        // Ten minutes between two clicks is indistinguishable from a hang.
        let error =
            parse_and_validate("[actuator.timings]\nrefreshed = { min_ms = 0, max_ms = 600000 }")
                .expect_err("a ten-minute extra wait must be refused");
        assert!(matches!(error, crate::Error::Config(_)));
        assert!(error.to_string().contains("actuator.timings.refreshed"));

        // The overflow case: four bare additions in the timing editor sum this
        // with a baseline. Rejecting it here is the guard at the root.
        let error = parse_and_validate(
            "[actuator.timings]\nshop_opened = { min_ms = 0, max_ms = 18446744073709551615 }",
        )
        .expect_err("a u64::MAX extra wait must be refused");
        assert!(matches!(error, crate::Error::Config(_)));
        assert!(error.to_string().contains("actuator.timings.shop_opened"));
    }

    #[test]
    fn every_timing_range_is_checked_not_just_the_first() {
        // A per-field guard that only walked one range would leave the other
        // seven exactly as unvalidated as before.
        for name in Timings::default().named_ranges().map(|(name, _)| name) {
            let text = format!("[actuator.timings]\n{name} = {{ min_ms = 9, max_ms = 1 }}");
            match parse_and_validate(&text) {
                Ok(_) => panic!("actuator.timings.{name} is not validated"),
                Err(error) => assert!(error.to_string().contains(name), "{name}: {error}"),
            }
        }
    }

    #[test]
    fn a_timing_range_at_the_ceiling_is_accepted() {
        // The bound is inclusive, and a wide-but-sane range must stay usable:
        // the ceiling exists to stop a frozen loop, not to narrow the knob.
        let config = parse_and_validate(&format!(
            "[actuator.timings]\nrefreshed = {{ min_ms = 0, max_ms = {MAX_TIMING_MS} }}"
        ))
        .expect("the ceiling itself is a legal setting");
        assert_eq!(config.actuator.timings.refreshed.max_ms, MAX_TIMING_MS);
        // A point range (min == max) is a fixed extra, not a reversed range.
        assert!(
            parse_and_validate("[actuator.timings]\nbuy_modal = { min_ms = 500, max_ms = 500 }")
                .is_ok()
        );
        // And the all-zero default still validates.
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn every_timing_preset_survives_validation() {
        // The Setup tab writes these verbatim through persist::save; a preset
        // the loader then refuses would lock the player out on next launch.
        for preset in crate::actuator::plan::TimingPreset::ALL {
            let config = Config {
                actuator: ActuatorConfig {
                    timings: preset.timings(),
                    ..ActuatorConfig::default()
                },
                ..Config::default()
            };
            assert!(
                config.validate().is_ok(),
                "preset {} must round-trip through the config",
                preset.label()
            );
        }
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
        assert!(
            config_with_url("wss://ingest.arkyve.dev/refresh-shop")
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn ws_loopback_ipv4_accepted() {
        assert!(
            config_with_url("ws://127.0.0.1:3001/refresh-shop")
                .validate()
                .is_ok()
        );
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
        let err = config_with_url("ws://example.com/x")
            .validate()
            .unwrap_err();
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn non_ws_scheme_rejected() {
        let err = config_with_url("http://example.com")
            .validate()
            .unwrap_err();
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn empty_still_rejected() {
        let err = config_with_url("").validate().unwrap_err();
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn ws_userinfo_loopback_is_rejected() {
        // The loopback text sits in the userinfo; the real host (evil.com) is
        // remote, so this must be refused — not accepted as loopback.
        let err = config_with_url("ws://127.0.0.1:3001@evil.com/refresh-shop")
            .validate()
            .unwrap_err();
        assert!(matches!(err, crate::Error::Config(_)));
        // Even a bare userinfo form must be caught.
        let err = config_with_url("ws://localhost@evil.com/x")
            .validate()
            .unwrap_err();
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn bundled_example_config_parses_validates_and_is_restrictive() {
        // `main::seed_config_if_missing` writes this exact text to
        // %APPDATA% on every player's first launch. Nothing else ever
        // deserializes it: as a bare `&'static str` it can rot (a renamed Rust
        // field, a key retired with a backend swap, a typo in a line someone
        // uncomments) while CI stays green — and then the shipped exe hands
        // 100% of new players an "Invalid configuration" window before they see
        // the app. Every `deny_unknown_fields` in this file is a way for that
        // to happen.
        let text = include_str!("../config.example.toml");
        let config: Config = toml::from_str(text).expect("the bundled example must parse");
        config
            .validate()
            .expect("the bundled example must validate");
        // The relay refuses to arm on an unrestricted filter (`app::run`), so a
        // criterion-less example would seed a file that cannot start a hunt.
        assert!(
            !config.filter.is_unrestricted(),
            "the example must carry a hunt criterion"
        );
    }

    /// Scratch directory removed on drop — *including* when an assertion
    /// panics, unlike the hand-rolled after-the-fact cleanup in `crash.rs`,
    /// which leaks files on every failure. The name mixes the pid with a
    /// process-local counter so neither two test binaries running at once nor
    /// two parallel tests in one binary can collide on it.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        /// Note: the directory is deliberately **not** created. The save test
        /// needs it absent to prove `persist::save` builds it (the first-Apply
        /// case on a machine whose %APPDATA% subdir does not exist yet).
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "arkyve-refresh-shop-test-{tag}-{}-{unique}",
                std::process::id()
            )))
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }

        fn join(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn save_then_load_round_trips_the_edited_sections_through_disk() {
        use crate::actuator::plan::DelayRange;
        use crate::config::persist::{self, Section};

        let dir = TempDir::new("save-load");
        assert!(!dir.path().exists(), "the fixture must start absent");
        let path = dir.join("config.toml");

        let filter = Filter {
            names: vec!["ticketrare_name".to_owned()],
            ..Filter::default()
        };
        let limits = Limits {
            max_refreshes: Some(10),
            max_spend: Some(30),
            ..Limits::default()
        };
        let timings = Timings {
            refreshed: DelayRange {
                min_ms: 200,
                max_ms: 800,
            },
            ..Timings::default()
        };

        persist::save(
            &path,
            &[
                Section::Filter(filter.clone()),
                Section::Limits(limits.clone()),
                Section::Timings(timings),
            ],
        )
        .expect("save must create the missing directory and the file");

        assert!(path.exists(), "create_dir_all covered the missing parent");
        assert!(
            !path.with_extension("toml.tmp").exists(),
            "the atomic-write temp must not survive a successful save"
        );

        let config = Config::load(&path).expect("the file we just wrote must load");
        config.validate().expect("and must validate");
        assert_eq!(config.filter, filter);
        assert_eq!(config.limits, limits);
        assert_eq!(config.actuator.timings, timings);
    }

    #[test]
    fn load_on_a_missing_file_yields_the_defaults() {
        // The `NotFound` branch is what every machine without a config.toml
        // takes at startup; turning it into an error would be invisible in CI.
        let dir = TempDir::new("missing");
        let path = dir.join("config.toml");
        assert!(!path.exists());

        let config = Config::load(&path).expect("a missing file is not an error");
        assert_eq!(config.game_port, DEFAULT_GAME_PORT);
        assert_eq!(config.server_url, Config::default().server_url);
        assert!(config.forward.server_to_client);
        assert!(!config.forward.client_to_server);
        assert!(config.filter.is_unrestricted(), "defaults set no criterion");
    }

    #[test]
    fn a_failed_save_leaves_the_original_config_intact() {
        // The atomicity guarantee, pinned: `save` writes a sibling temp and
        // renames. Squatting the temp path with a directory makes that write
        // fail — and a "simplification" to a direct `fs::write(path, ..)` would
        // instead succeed here, having already truncated the player's file.
        use crate::config::persist::{self, Section};

        let dir = TempDir::new("failed-save");
        std::fs::create_dir_all(dir.path()).expect("fixture dir");
        let path = dir.join("config.toml");
        let original = "# hand-written\ngame_port = 3333\n";
        std::fs::write(&path, original).expect("seed the original");
        std::fs::create_dir(path.with_extension("toml.tmp")).expect("squat the temp path");

        let error = persist::save(
            &path,
            &[Section::Limits(Limits {
                max_refreshes: Some(7),
                ..Limits::default()
            })],
        )
        .expect_err("the temp write cannot succeed onto a directory");

        assert!(
            matches!(error, crate::Error::ConfigWrite { .. }),
            "expected a path-carrying write error, got {error:?}"
        );
        assert!(
            error.to_string().contains("config.toml"),
            "the message must name the file: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("original still readable"),
            original,
            "a failed save must not touch the player's file"
        );
    }

    #[test]
    fn an_unreadable_config_reports_the_path_not_a_bare_os_error() {
        // A directory where the file should be: not `NotFound`, so it takes the
        // read-error branch. Before this carried a path, the player saw
        // "i/o: Access is denied. (os error 5)" and nothing else — while the
        // file sits somewhere under %APPDATA% they never navigate to.
        let dir = TempDir::new("unreadable");
        std::fs::create_dir_all(dir.join("config.toml")).expect("fixture dir");

        let error = Config::load(dir.join("config.toml")).expect_err("a directory cannot be read");
        assert!(
            matches!(error, crate::Error::ConfigRead { .. }),
            "expected a path-carrying read error, got {error:?}"
        );
        assert!(
            error.to_string().contains("config.toml"),
            "the message must name the file: {error}"
        );
    }

    #[test]
    fn scheme_match_is_case_insensitive() {
        // URL schemes are case-insensitive; an uppercase scheme must not be
        // rejected when the WebSocket client would accept it.
        assert!(
            config_with_url("WSS://ingest.arkyve.dev/refresh-shop")
                .validate()
                .is_ok()
        );
        assert!(config_with_url("WS://127.0.0.1:3001/x").validate().is_ok());
        // A userinfo bypass must still be caught regardless of scheme case.
        assert!(
            config_with_url("WS://127.0.0.1@evil.com/x")
                .validate()
                .is_err()
        );
    }
}
