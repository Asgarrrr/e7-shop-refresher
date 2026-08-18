//! Relay configuration, loaded from a TOML file (defaults otherwise).

pub mod persist;

use std::fmt;
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

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// TCP port of the game server, remote side.
    pub game_port: u16,

    /// Analysis server URL (`ws://` or `wss://`).
    ///
    /// Checked by [`ServerUrl::parse`] on the load path, which is the only path
    /// that checks it. It is still a `String` because the two consumers outside
    /// this module take one; making the field a [`ServerUrl`] is what would stop
    /// a `Config` built any other way — a struct literal, a mutated
    /// `Config::default()`, a future GUI field — from reaching the uplink
    /// unchecked.
    pub server_url: String,

    /// Vestigial; see [`ForwardConfig`].
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

/// Hand-written so `server_url` cannot reach the log through this type.
///
/// A `wss://` URL may carry a `user:pass@` credential, and `Config` is exactly
/// the kind of value that ends up in a startup line or an `#[instrument]`
/// argument list. Nothing formats a `Config` today; the whole point is that the
/// next thing to do so is safe by default, because the promise in `README.md`
/// ("the log never contains the server URL's credentials") is currently kept by
/// remembering to call a helper.
///
/// Destructured rather than read field-by-field off `self`: adding a field to
/// `Config` is then a compile error here until it is listed, which is the one
/// real hazard of a hand-written `Debug`. Becomes a plain derive again once the
/// field is a [`ServerUrl`], whose own `Debug` redacts.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            game_port,
            server_url,
            forward,
            reconnect,
            capture,
            filter,
            limits,
            actuator,
        } = self;
        f.debug_struct("Config")
            .field("game_port", game_port)
            .field("server_url", &redacted_authority(server_url))
            .field("forward", forward)
            .field("reconnect", reconnect)
            .field("capture", capture)
            .field("filter", filter)
            .field("limits", limits)
            .field("actuator", actuator)
            .finish()
    }
}

/// Vestigial. Both keys are parsed and both are ignored.
///
/// They described a choice the pipeline no longer has. Only the server's half
/// of a connection was ever decoded — the analysis server reads shop responses,
/// and nothing has ever read the client's requests — so `server_to_client` was
/// a knob whose only useful position was `true`, and `client_to_server` a knob
/// for a feature that does not exist. `parse_segment` now refuses anything the
/// game server did not send, which leaves the two keys describing a distinction
/// the code cannot express.
///
/// **They are still parsed on purpose, and removing them is not a cleanup.**
/// The reasoning is the same as for [`CaptureConfig`], and the evidence is
/// stronger: `config.example.toml` shipped the whole `[forward]` block
/// uncommented, and `main::seed_config_if_missing` writes that file to
/// `%APPDATA%` on every first run. With `deny_unknown_fields` on this struct
/// and on [`Config`], deleting the fields turns the next launch of every
/// existing installation into `Config::load` failing, an "Invalid
/// configuration" window, and an app that no longer starts. Editing
/// `config.example.toml` does nothing for files already written.
///
/// Plan: keep them accepted-and-ignored for this release, with the startup
/// warning `main` emits from [`ForwardConfig::retired_keys`], then delete both
/// fields (and this struct with them) in a later one, once a player upgrading
/// across two releases is no longer plausible.
///
/// The warning is one-time in fact and not just in intent:
/// [`persist::strip_retired_keys`] deletes these keys from `config.toml` at the
/// same startup that warns about them, so a file that has been through one
/// launch of this release no longer sets them. That is also what makes the
/// deletion above safe *sooner* than "never touched" files would allow.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ForwardConfig {
    /// Accepted and ignored: the server -> client responses are the only thing
    /// captured at all, so this can no longer be turned off.
    pub server_to_client: Option<bool>,
    /// Accepted and ignored: the client -> server half is never captured, so
    /// this can no longer be turned on.
    pub client_to_server: Option<bool>,
}

/// Exponential-backoff policy for the server link: where it starts and where it
/// stops doubling. Both are floored and ordered by the accessors, not here —
/// see [`Config::reconnect_initial`] and [`Config::reconnect_max`].
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

/// Vestigial. Both keys are parsed and both are ignored.
///
/// They stopped meaning anything when the capture filter stopped being a
/// string this file could supply. The backend builds its own BPF expression
/// from the validated `u16` `game_port` (`tcp and src port {game_port}`), and
/// sizes its buffer from its own snaplen. The filter *had* to stop being
/// configurable while capture still ran a kernel driver: this file lives in
/// per-user roaming app-data, where any medium-integrity process on the machine
/// can rewrite it, and its contents were handed to that driver's filter
/// compiler inside an administrator process. The driver is gone; the reason not
/// to reopen the key is now simply that nothing needs it.
///
/// **They are still parsed on purpose, and removing them is not a cleanup.**
/// This struct and [`Config`] are both `deny_unknown_fields`, and
/// `config.example.toml` shipped `buffer_size` *uncommented* — a file
/// `main::seed_config_if_missing` writes to `%APPDATA%` on every first run. So
/// the key is on disk for every player who has ever launched this app, and
/// deleting the field turns their next launch into `Config::load` failing, an
/// "Invalid configuration" window, and an app that no longer starts. Editing
/// `config.example.toml` does nothing for files already written.
///
/// Plan: keep them accepted-and-ignored for this release, with the startup
/// warning `main` emits from [`CaptureConfig::retired_keys`], then delete both
/// fields (and this struct with them) in a later one, once a player upgrading
/// across two releases is no longer plausible.
///
/// The warning is one-time in fact and not just in intent:
/// [`persist::strip_retired_keys`] deletes these keys from `config.toml` at the
/// same startup that warns about them, so a file that has been through one
/// launch of this release no longer sets them.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    /// Accepted and ignored: the capture buffer is sized by the backend, from
    /// the snaplen it asks libpcap for.
    pub buffer_size: Option<usize>,
    /// Accepted and ignored: the capture filter is built by the backend from
    /// `game_port` and never comes from this file.
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

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            initial_ms: 1_000,
            max_ms: 30_000,
        }
    }
}

impl CaptureConfig {
    /// The retired keys this file actually sets, as a readable list, or `None`
    /// when it sets neither.
    ///
    /// A key that silently does nothing is worse than one that is refused: the
    /// player who set `capture.filter` to widen their capture would otherwise
    /// spend an evening wondering why. `main` turns this into one warning line
    /// at startup, so the log a player sends us names it too.
    #[must_use]
    pub fn retired_keys(&self) -> Option<String> {
        let mut keys = Vec::new();
        if self.buffer_size.is_some() {
            keys.push("capture.buffer_size");
        }
        if self.filter.is_some() {
            keys.push("capture.filter");
        }
        (!keys.is_empty()).then(|| keys.join(", "))
    }
}

impl ForwardConfig {
    /// The retired keys this file actually sets, as a readable list, or `None`
    /// when it sets neither. Same contract, and same reason, as
    /// [`CaptureConfig::retired_keys`]: a player who set `client_to_server`
    /// expecting their requests to be forwarded has to be told they are not.
    #[must_use]
    pub fn retired_keys(&self) -> Option<String> {
        let mut keys = Vec::new();
        if self.server_to_client.is_some() {
            keys.push("forward.server_to_client");
        }
        if self.client_to_server.is_some() {
            keys.push("forward.client_to_server");
        }
        (!keys.is_empty()).then(|| keys.join(", "))
    }
}

/// The authority of `rest` (everything after a `scheme://`), with any
/// `user:pass@` userinfo dropped: `host` or `host:port`, IPv6 in brackets.
///
/// The real host is what follows the *last* `@` — what `http::Uri`, and thus the
/// WebSocket client, actually connects to — so `127.0.0.1@evil.com` is correctly
/// seen as `evil.com`. That one line does double duty, which is why it lives in
/// exactly one place now: it is what stops a userinfo-embedded loopback from
/// leaking traffic in cleartext to a remote host, *and* what keeps a credential
/// out of the redacted form written to the log.
fn authority_of(rest: &str) -> &str {
    // The authority ends at the first path/query/fragment separator.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
}

/// `scheme://host[:port]` — userinfo, path, query and fragment all gone. The
/// only form of a server URL that may be written to a log or a journal line.
///
/// Deliberately lenient rather than fallible: it is also what `Config`'s `Debug`
/// prints for a `server_url` that has *not* been through [`ServerUrl::parse`],
/// so it has to reduce garbage instead of refusing it (`"garbage"` becomes
/// `"://garbage"`, which is unmistakably not a URL and still carries no secret).
fn redacted_authority(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    format!("{scheme}://{}", authority_of(rest))
}

/// Strip a trailing `:port` from an authority. An IPv6 literal is bracketed, so
/// a trailing `:port` is only a port when it sits outside the brackets.
fn host_of(authority: &str) -> &str {
    if authority.starts_with('[') {
        // "[::1]:3001" -> "[::1]"
        authority
            .split_once(']')
            .map_or(authority, |(head, _)| &authority[..head.len() + 1])
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
    }
}

/// True for the hosts where cleartext never leaves the machine.
fn is_loopback_host(host: &str) -> bool {
    ["127.0.0.1", "localhost", "[::1]", "::1"]
        .iter()
        .any(|loopback| host.eq_ignore_ascii_case(loopback))
}

/// A `server_url` that has been proven safe to dial, carrying the proof.
///
/// The rule is a security property, not a spelling convention: `server_url`
/// receives the reassembled game stream, which can carry session tokens, so it
/// must be `wss://` — or `ws://` to a loopback host, where cleartext never
/// leaves the machine. [`ServerUrl::parse`] is the single place that rule and
/// the authority split it needs are written, and a `ServerUrl` that exists is
/// the evidence that they passed.
///
/// It also carries the redacted `scheme://host[:port]` form, because the same
/// split that defeats `ws://127.0.0.1@evil.com` is the one that keeps a
/// `user:pass@` credential out of the log the player is asked to send us. Those
/// two were written twice — here and in `app::redacted_server_url` — and only
/// this copy had the userinfo tests, so the next parsing subtlety had to be
/// found twice.
///
/// `Debug` and `Display` print the redacted form **only**, so no `?url`, `%url`
/// or `#[instrument]` argument list can put a credential in the log. The dial
/// string — what the WebSocket client is actually given — comes out through
/// [`ServerUrl::as_str`] and nowhere else.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerUrl {
    dial: String,
    redacted: String,
}

impl ServerUrl {
    /// Parses `raw`, enforcing the cleartext rule. Surrounding whitespace is
    /// trimmed: a hand-edited `server_url = " wss://… "` dials the same server.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] — `raw` is empty, carries a scheme other than `ws://`
    /// or `wss://`, or is `ws://` to anywhere but loopback (which would forward
    /// the captured game stream, session tokens included, in cleartext).
    ///
    /// [`Error::Config`]: crate::Error::Config
    pub fn parse(raw: &str) -> Result<Self> {
        let dial = raw.trim();
        if dial.is_empty() {
            return Err(crate::Error::Config("server_url is empty".into()));
        }
        // URL schemes are case-insensitive, so match `WSS://` too.
        let (rest, tls) = if let Some(rest) = strip_scheme(dial, "wss://") {
            (rest, true)
        } else if let Some(rest) = strip_scheme(dial, "ws://") {
            (rest, false)
        } else {
            return Err(crate::Error::Config(
                "server_url must be a ws:// or wss:// URL".into(),
            ));
        };
        if !tls && !is_loopback_host(host_of(authority_of(rest))) {
            return Err(crate::Error::Config(
                "server_url uses ws:// to a non-loopback host — captured traffic \
                 would be sent in cleartext; use wss:// (or ws:// only for \
                 127.0.0.1/localhost)"
                    .into(),
            ));
        }
        Ok(Self {
            redacted: redacted_authority(dial),
            dial: dial.to_owned(),
        })
    }

    /// The dial string: what the WebSocket client connects to, userinfo and
    /// query intact. Never log this — see [`ServerUrl::redacted`].
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.dial
    }

    /// `scheme://host[:port]`, the only form safe to write to the log file the
    /// player is asked to send us. Also what `Debug`/`Display` print.
    #[must_use]
    pub fn redacted(&self) -> &str {
        &self.redacted
    }
}

/// Case-insensitive scheme prefix strip. `get` rather than a slice index because
/// `raw` is arbitrary player text and may not have a char boundary there.
fn strip_scheme<'a>(url: &'a str, scheme: &str) -> Option<&'a str> {
    url.get(..scheme.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(scheme))
        .map(|prefix| &url[prefix.len()..])
}

impl fmt::Debug for ServerUrl {
    /// The redacted form, deliberately — see the type's documentation.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ServerUrl").field(&self.redacted).finish()
    }
}

impl fmt::Display for ServerUrl {
    /// The redacted form, deliberately — see the type's documentation.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.redacted)
    }
}

/// The serde hook: `#[serde(try_from = "String")]` on a `ServerUrl` field parses
/// through [`ServerUrl::parse`], so the cleartext rule stops depending on
/// `Config::load` being the only constructor.
impl TryFrom<String> for ServerUrl {
    type Error = crate::Error;

    fn try_from(raw: String) -> Result<Self> {
        Self::parse(&raw)
    }
}

/// Reject an `[actuator.timings]` table the refresh loop cannot honour.
///
/// Separate from [`Config::validate`], and `pub(crate)`, because there are
/// **two** write boundaries and only one of them has a `Config`: the loader, and
/// [`persist::save`], which serializes whatever `Timings` the Setup tab hands it
/// with no `Config` anywhere in the path. Enforced at the loader alone, the GUI
/// was one missing clamp away from writing a file the *next* launch refuses —
/// the exact shape of the `kinds = ["unknown"]` checkbox that shipped, whose
/// only cure was hand-editing the file the app owns.
///
/// Better still would be for `DelayRange` to carry the invariant itself, where no
/// producer at all could bypass it. That is a bigger move than it looks: the type
/// lives in [`crate::actuator::plan`] and [`MAX_TIMING_MS`] lives here, so it
/// inverts the current dependency direction — and the reversed-range message
/// below, which names the key *and* says what the value would be read as, has to
/// survive the move.
///
/// # Errors
///
/// [`Error::Config`] — a range is reversed (`min_ms > max_ms`), or its `max_ms`
/// exceeds the [`MAX_TIMING_MS`] ceiling. The message names the key.
///
/// [`Error::Config`]: crate::Error::Config
pub(crate) fn validate_timings(timings: &Timings) -> Result<()> {
    // `[actuator.timings]` reaches both the refresh loop and the Setup meter
    // unchecked otherwise. Two shapes have to be refused:
    //
    // - a reversed range. With this TOML's inline form
    //   (`{ min_ms = 800, max_ms = 200 }`) swapping the two is an ordinary
    //   typo, and `DelayRange::draw` reads it as a fixed 800 ms delay — the
    //   player configures variability and silently gets none, while the
    //   Setup tab shows "Custom" with no clue why.
    // - an unbounded `max_ms`. It is what freezes the loop for ten minutes
    //   between two clicks, and what overflows the editor's `baseline + max`
    //   sums near `u64::MAX`.
    for (name, range) in timings.named_ranges() {
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
    ///   - an empty `server_url`, a scheme other than `ws://` / `wss://`, or a
    ///     `ws://` URL pointing anywhere but loopback (it would forward the
    ///     captured game stream, session tokens included, in cleartext);
    ///   - an unrecognized value in `[filter] kinds`, which the wire-tolerant
    ///     `ItemKind` would otherwise fold into `Unknown` and match nothing;
    ///   - a non-finite `[[filter.required_substats]] min` (`nan`/`inf` are
    ///     legal TOML floats), which no substat value can ever satisfy;
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
        // `server_url` receives the reassembled game stream, which can carry
        // session tokens: require TLS (`wss://`) unless the host is loopback,
        // where cleartext (`ws://`) never leaves the machine. Parsed rather than
        // spot-checked, so that the rule and the authority split it needs have
        // one implementation — `ServerUrl` — instead of one here and one in the
        // log redactor.
        //
        // The parsed value is dropped because the field is still a `String`;
        // storing it is what would extend the proof to a `Config` that did not
        // come from disk. See the field.
        ServerUrl::parse(&self.server_url)?;
        // `ItemKind` is wire-tolerant (`serde(other)` -> Unknown), which in a
        // config file would let a typo silently match nothing: reject it here.
        if self.filter.kinds.contains(&ItemKind::Unknown) {
            return Err(crate::Error::Config(
                "unrecognized kind in [filter] kinds (expected: equipment, hero, token)".into(),
            ));
        }
        // The same failure mode as `kinds`, by a different route, and TOML 1.0
        // supplies the literal: `min = nan` parses. Nothing can then satisfy
        // `value >= min`, so the filter matches nothing while `is_unrestricted`
        // reports it restricted and the loop arms and burns crystals — and
        // `Filter`'s derived `PartialEq` (the Setup tab's dirty check) recurses
        // into this `Option<f64>`, so `NaN != NaN` leaves Apply lit forever,
        // rewriting `config.toml` on every click. `+inf` gives the first half
        // without the second.
        for req in &self.filter.required_substats {
            if req.min.is_some_and(|min| !min.is_finite()) {
                return Err(crate::Error::Config(format!(
                    "filter.required_substats \"{}\" has a non-finite min — no substat value can \
                     ever satisfy it, so the filter would match nothing",
                    req.name
                )));
            }
        }
        // `capture.filter` used to be checked here for naming `game_port`,
        // because a filter on another port delivered traffic nothing could
        // classify. That check went away with the thing it guarded: the
        // backend builds its own filter from `game_port` itself, so the
        // mismatch it caught can no longer be expressed. Note
        // that this rejection must NOT come back in another form — a config
        // written before the change may well carry a filter naming a different
        // port, and refusing it would lock that player out of the app on
        // upgrade for a setting that no longer has any effect.
        //
        // `[forward]` with both directions off was refused here for the same
        // kind of reason — it asked for a capture that forwarded nothing — and
        // it is gone for the same reason: there is one direction now, the keys
        // are inert, and no combination of them can express a broken relay.
        // Refusing any `[forward]` value would only lock out a player whose
        // file predates the change, which is every player's file.
        //
        // The timing ranges are checked by the free `validate_timings`, which
        // `persist::save` calls too — the disk is written from the GUI without a
        // `Config` in the path, so the loader cannot be the only boundary.
        validate_timings(&self.actuator.timings)
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

    /// **The regression this whole compatibility shim exists to prevent.**
    ///
    /// `config.example.toml` shipped `buffer_size = 65575` uncommented, and
    /// `main::seed_config_if_missing` writes that file to `%APPDATA%` on every
    /// first run — so this exact text is on disk for every player who has ever
    /// launched the app, and some of them uncommented `filter` too. With
    /// `deny_unknown_fields` on both structs, retiring the keys by deleting
    /// them turns the next launch into an "Invalid configuration" window and an
    /// app that will not start. Updating the example does nothing for files
    /// already written; only still parsing the keys does.
    #[test]
    fn a_config_written_before_the_capture_keys_were_retired_still_loads() {
        let config = parse_and_validate(
            "[capture]\nbuffer_size = 65575\nfilter = \"tcp and tcp.SrcPort == 3333\"",
        )
        .expect("an upgrading player's existing config must still load");
        assert_eq!(config.capture.buffer_size, Some(65_575));
        assert_eq!(
            config.capture.filter.as_deref(),
            Some("tcp and tcp.SrcPort == 3333")
        );
    }

    #[test]
    fn a_retired_filter_naming_another_port_is_no_longer_a_startup_failure() {
        // It used to be refused, because a filter on another port delivered
        // traffic nothing could classify. The backend builds its own filter
        // from `game_port` now, so the value is inert — and refusing it would
        // lock an upgrading player out of the app over a setting that has no
        // effect at all.
        let config = parse_and_validate("[capture]\nfilter = \"tcp and tcp.SrcPort == 4444\"")
            .expect("an inert setting must not stop the app from starting");
        assert!(config.capture.filter.is_some());
    }

    #[test]
    fn the_retired_capture_keys_are_named_only_when_they_are_actually_set() {
        // This list is what the startup warning prints; an empty file must not
        // produce a warning about keys the player never wrote.
        assert_eq!(Config::default().capture.retired_keys(), None);
        assert_eq!(
            parse_and_validate("[capture]\nbuffer_size = 65575")
                .expect("still accepted")
                .capture
                .retired_keys()
                .as_deref(),
            Some("capture.buffer_size")
        );
        assert_eq!(
            parse_and_validate("[capture]\nfilter = \"tcp\"")
                .expect("still accepted")
                .capture
                .retired_keys()
                .as_deref(),
            Some("capture.filter")
        );
        assert_eq!(
            parse_and_validate("[capture]\nbuffer_size = 0\nfilter = \"tcp\"")
                .expect("still accepted")
                .capture
                .retired_keys()
                .as_deref(),
            Some("capture.buffer_size, capture.filter")
        );
    }

    /// **The same regression, for the `[forward]` block.**
    ///
    /// `config.example.toml` shipped this exact text uncommented, so it is on
    /// disk for every player who has ever launched the app — the user's live
    /// `%APPDATA%\arkyve-refresh-shop\config.toml` included. With
    /// `deny_unknown_fields` on both structs, retiring the keys by deleting
    /// them turns the next launch into an "Invalid configuration" window and an
    /// app that will not start.
    #[test]
    fn a_config_written_before_the_forward_keys_were_retired_still_loads() {
        let config =
            parse_and_validate("[forward]\nserver_to_client = true\nclient_to_server = false")
                .expect("an upgrading player's existing config must still load");
        assert_eq!(config.forward.server_to_client, Some(true));
        assert_eq!(config.forward.client_to_server, Some(false));
    }

    #[test]
    fn a_retired_forward_combination_is_no_longer_a_startup_failure() {
        // Both directions off used to be refused, because it described a relay
        // that forwarded nothing. There is one direction now and neither key
        // reaches the pipeline, so no combination can describe a broken relay —
        // and refusing one would lock an upgrading player out of the app over a
        // setting that has no effect at all.
        assert!(
            parse_and_validate("[forward]\nserver_to_client = false\nclient_to_server = false")
                .is_ok()
        );
        // Asking for the client -> server stream is inert, not fatal: it is
        // never captured, so it is simply not forwarded.
        let config = parse_and_validate("[forward]\nclient_to_server = true")
            .expect("an inert setting must not stop the app from starting");
        assert_eq!(config.forward.client_to_server, Some(true));
    }

    #[test]
    fn the_retired_forward_keys_are_named_only_when_they_are_actually_set() {
        // This list is what the startup warning prints; an empty file must not
        // produce a warning about keys the player never wrote.
        assert_eq!(Config::default().forward.retired_keys(), None);
        assert_eq!(
            parse_and_validate("[forward]\nserver_to_client = true")
                .expect("still accepted")
                .forward
                .retired_keys()
                .as_deref(),
            Some("forward.server_to_client")
        );
        assert_eq!(
            parse_and_validate("[forward]\nclient_to_server = false")
                .expect("still accepted")
                .forward
                .retired_keys()
                .as_deref(),
            Some("forward.client_to_server")
        );
        assert_eq!(
            parse_and_validate("[forward]\nserver_to_client = true\nclient_to_server = false")
                .expect("still accepted")
                .forward
                .retired_keys()
                .as_deref(),
            Some("forward.server_to_client, forward.client_to_server")
        );
    }

    #[test]
    fn a_misspelled_forward_key_is_still_rejected() {
        // The section is vestigial, not untyped: `deny_unknown_fields` still
        // catches a typo, which is the only way a player learns the key they
        // meant does not exist.
        assert!(toml::from_str::<Config>("[forward]\nserver_to_clients = true").is_err());
    }

    #[test]
    fn capture_buffer_overflow_is_still_rejected_during_deserialization() {
        // The key is ignored, not untyped: a value that cannot be a `usize` is
        // still a malformed file, and reporting it as a parse error is more
        // useful than silently reading it as "unset".
        let error = parse_and_validate("[capture]\nbuffer_size = 18446744073709551616")
            .expect_err("integer overflow must fail deserialization");
        assert!(matches!(error, crate::Error::ConfigParse(_)));
    }

    #[test]
    fn an_absent_capture_or_forward_section_leaves_every_retired_key_unset() {
        assert_eq!(Config::default().capture.buffer_size, None);
        assert_eq!(Config::default().capture.filter, None);
        assert_eq!(Config::default().forward.server_to_client, None);
        assert_eq!(Config::default().forward.client_to_server, None);
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

    /// Builds a default `Config` with `server_url` overwritten, so `validate`
    /// reaches the scheme check with nothing else able to fail first.
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
    }

    #[test]
    fn ws_bare_userinfo_loopback_is_rejected() {
        // The same bypass without a port in the userinfo.
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
        // And it must not re-plant the retired keys it warns about: uncommenting
        // either line here would hand every *new* player a first launch that
        // warns about the example the app just seeded, then rewrites it.
        assert_eq!(config.capture.retired_keys(), None);
        assert_eq!(config.forward.retired_keys(), None);

        // The same thing proved on disk, through the real entry point: seeded
        // and immediately offered to the stripper, the example comes back
        // untouched — no rewrite, no log line, and its `[capture]`/`[forward]`
        // headers (whose comments are where the retired keys are explained)
        // still there. An empty table nobody touched is a commented section,
        // not a leftover.
        let dir = TempDir::new("example-strip");
        std::fs::create_dir_all(dir.path()).expect("fixture dir");
        let path = dir.join("config.toml");
        std::fs::write(&path, text).expect("seed the example");
        assert_eq!(
            crate::config::persist::strip_retired_keys(&path).expect("must not fail"),
            None,
            "a fresh install must have nothing to strip"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("still readable"),
            text,
            "the seeded example must be left byte-identical"
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
    fn stripping_a_players_config_clears_the_retired_warning_for_good() {
        // The whole point, end to end: a file that warns on this launch must
        // not warn on the next one. Load -> the keys are set -> strip -> load
        // again -> nothing set, and everything else the player wrote is intact.
        let dir = TempDir::new("strip-retired");
        std::fs::create_dir_all(dir.path()).expect("fixture dir");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# hand-written\ngame_port = 3333\n\n[forward]\nserver_to_client = true\n\n\
             [capture]\nbuffer_size = 65575\n\n[filter]\nnames = [\"ticketrare_name\"]\n",
        )
        .expect("seed a pre-retirement config");

        let before = Config::load(&path).expect("an upgrading player's config still loads");
        assert!(before.capture.retired_keys().is_some());
        assert!(before.forward.retired_keys().is_some());

        let removed = crate::config::persist::strip_retired_keys(&path)
            .expect("the rewrite must succeed on a writable file")
            .expect("both keys were set, so it must have rewritten");
        assert_eq!(removed, "capture.buffer_size, forward.server_to_client");
        assert!(
            !path.with_extension("toml.tmp").exists(),
            "the atomic-write temp must not survive a successful strip"
        );

        let after = Config::load(&path).expect("the stripped file must still load");
        assert_eq!(after.capture.retired_keys(), None, "no warning next launch");
        assert_eq!(after.forward.retired_keys(), None, "no warning next launch");
        assert_eq!(after.game_port, 3333);
        assert_eq!(after.filter, before.filter, "the hunt is untouched");
        let text = std::fs::read_to_string(&path).expect("readable");
        assert!(text.contains("# hand-written"), "comments survive: {text}");

        // Idempotent: the second launch finds nothing and writes nothing.
        assert_eq!(
            crate::config::persist::strip_retired_keys(&path).expect("must not fail"),
            None
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("readable"),
            text,
            "a second pass must leave the file byte-identical"
        );
    }

    #[test]
    fn a_failed_strip_leaves_the_retired_keys_in_place_to_warn_about() {
        // The best-effort path: the strip is not allowed to be fatal, and when
        // it fails the keys really are still on disk — which is why `main` keeps
        // the present-tense warning for this branch.
        let dir = TempDir::new("failed-strip");
        std::fs::create_dir_all(dir.path()).expect("fixture dir");
        let path = dir.join("config.toml");
        let original = "[capture]\nbuffer_size = 65575\n";
        std::fs::write(&path, original).expect("seed the original");
        std::fs::create_dir(path.with_extension("toml.tmp")).expect("squat the temp path");

        let error = crate::config::persist::strip_retired_keys(&path)
            .expect_err("the temp write cannot succeed onto a directory");
        assert!(
            matches!(error, crate::Error::ConfigWrite { .. }),
            "expected a path-carrying write error, got {error:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("original still readable"),
            original,
            "a failed strip must not touch the player's file"
        );
        assert!(
            Config::load(&path)
                .expect("and the app must still start")
                .capture
                .retired_keys()
                .is_some()
        );
    }

    #[test]
    fn stripping_a_missing_config_is_not_an_error() {
        // `seed_config_if_missing` can fail (unwritable %APPDATA%), leaving
        // `Config::load` on the in-memory defaults. Those set no retired key so
        // `main` never calls this — but a missing file must be "nothing to do",
        // not a startup-time error report.
        let dir = TempDir::new("strip-missing");
        assert_eq!(
            crate::config::persist::strip_retired_keys(dir.join("config.toml"))
                .expect("a missing file is not an error"),
            None
        );
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
    fn an_uppercase_wss_scheme_is_accepted() {
        // URL schemes are case-insensitive; an uppercase scheme must not be
        // rejected when the WebSocket client would accept it.
        assert!(
            config_with_url("WSS://ingest.arkyve.dev/refresh-shop")
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn an_uppercase_ws_scheme_to_loopback_is_accepted() {
        assert!(config_with_url("WS://127.0.0.1:3001/x").validate().is_ok());
    }

    #[test]
    fn an_uppercase_ws_userinfo_bypass_is_still_rejected() {
        // The case-insensitive match must not become a way around the host check.
        assert!(
            config_with_url("WS://127.0.0.1@evil.com/x")
                .validate()
                .is_err()
        );
    }

    #[test]
    fn a_non_finite_substat_threshold_is_rejected() {
        // `nan` and `inf` are TOML 1.0 float literals, so this is a legal parse.
        // Accepted, it is the worst kind of silent failure: `value >= min` is
        // false for every value so the filter matches nothing, while
        // `is_unrestricted()` counts the requirement as a real criterion, so the
        // loop arms and refreshes forever debiting crystals. `nan` adds a second
        // symptom — `Filter`'s derived `PartialEq` recurses into the
        // `Option<f64>`, so the Setup tab's dirty check never clears and every
        // Apply rewrites `config.toml`.
        for literal in ["nan", "inf", "-inf", "-nan"] {
            let text = format!("[[filter.required_substats]]\nname = \"speed\"\nmin = {literal}\n");
            let error = parse_and_validate(&text)
                .expect_err("a threshold no value can satisfy must be refused");
            assert!(matches!(error, crate::Error::Config(_)), "{literal}");
            let message = error.to_string();
            assert!(
                message.contains("speed"),
                "must name the requirement: {message}"
            );
        }
    }

    #[test]
    fn a_finite_substat_threshold_including_zero_and_negative_is_accepted() {
        // Only *non-finite* is refused. A zero or negative floor is meaningful
        // (some substats are stored as deltas) and must stay reachable.
        let config = parse_and_validate(
            "[[filter.required_substats]]\nname = \"speed\"\nmin = 0.0\n\n\
             [[filter.required_substats]]\nname = \"cri\"\nmin = -1.5\n\n\
             [[filter.required_substats]]\nname = \"atk\"\n",
        )
        .expect("finite thresholds are legal");
        assert_eq!(config.filter.required_substats.len(), 3);
        assert_eq!(config.filter.required_substats[2].min, None);
    }

    #[test]
    fn a_parsed_server_url_keeps_the_dial_string_and_redacts_the_credential() {
        // The two halves the crate used to write twice: what gets dialed, and
        // what is safe to log. One parse now produces both.
        let url = ServerUrl::parse("wss://token:secret@ingest.arkyve.dev:8443/path?key=abc")
            .expect("wss is accepted whatever the authority carries");
        assert_eq!(
            url.as_str(),
            "wss://token:secret@ingest.arkyve.dev:8443/path?key=abc"
        );
        assert_eq!(url.redacted(), "wss://ingest.arkyve.dev:8443");
    }

    #[test]
    fn a_server_urls_debug_and_display_cannot_leak_the_credential() {
        // The reason `Debug` is hand-written: the log file is what the player is
        // asked to email us, and `README.md` promises it carries no credential.
        let url = ServerUrl::parse("wss://token:secret@ingest.arkyve.dev/x").expect("accepted");
        for rendered in [format!("{url:?}"), format!("{url}")] {
            assert!(!rendered.contains("secret"), "{rendered}");
            assert!(rendered.contains("ingest.arkyve.dev"), "{rendered}");
        }
    }

    #[test]
    fn a_configs_debug_redacts_the_server_url_it_has_not_parsed() {
        // `Config` is exactly the kind of value that ends up in a startup line.
        // The field is still a bare `String`, so the redaction has to work on
        // whatever is in it — including a value that never went through
        // `validate` at all.
        let rendered = format!(
            "{:?}",
            config_with_url("wss://token:secret@host:8443/p?k=v")
        );
        assert!(!rendered.contains("secret"), "{rendered}");
        assert!(rendered.contains("wss://host:8443"), "{rendered}");
    }

    #[test]
    fn a_query_or_fragment_never_reaches_the_redacted_form() {
        // A fragment used to be authority text to the loopback check and not to
        // the log redactor; one parser now handles both.
        let url = ServerUrl::parse("ws://127.0.0.1:9000/?key=abc#frag").expect("loopback");
        assert_eq!(url.redacted(), "ws://127.0.0.1:9000");
    }

    #[test]
    fn a_surrounding_whitespace_server_url_is_accepted_and_dials_trimmed() {
        // A hand-edited `server_url = " wss://… "` names the same server; the
        // trim has to happen before the scheme match *and* before the dial
        // string is kept, or the client gets a URL with a leading space.
        let url = ServerUrl::parse("  wss://ingest.arkyve.dev/x  ").expect("accepted");
        assert_eq!(url.as_str(), "wss://ingest.arkyve.dev/x");
    }

    #[test]
    fn the_serde_hook_parses_through_the_same_rule() {
        // `#[serde(try_from = "String")]` on a `ServerUrl` field is what makes
        // the cleartext rule stop depending on `Config::load` being the only
        // constructor; the conversion must be the same parse, not a second one.
        assert!(ServerUrl::try_from("wss://ingest.arkyve.dev/x".to_owned()).is_ok());
        let error = ServerUrl::try_from("ws://evil.com/x".to_owned())
            .expect_err("a non-loopback ws:// must not become a ServerUrl");
        assert!(matches!(error, crate::Error::Config(_)));
    }
}
