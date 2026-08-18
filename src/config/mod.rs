//! Relay configuration, loaded from a TOML file (defaults otherwise).
//!
//! This module owns the schema (`Config` and its sections, every one of them
//! `default` + `deny_unknown_fields`), the load path, and the validation rules
//! that no field type can carry on its own. Two seams live beside it:
//! [`persist`] writes sections back through `toml_edit` so the player's comments
//! survive, and [`ServerUrl`]'s file keeps the cleartext rule and the credential
//! redaction where its private fields are private to one file.

pub mod persist;
mod server_url;

// Re-exported so `crate::config::ServerUrl` — the path `uplink` and `app` use —
// stays the only path to the type; `server_url` itself is not a public module.
pub use server_url::ServerUrl;

#[cfg(test)]
mod tests;

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::actuator::plan::Timings;
use crate::domain::control::Limits;
use crate::domain::filter::Filter;
use std::num::NonZeroU16;

use crate::error::Result;

/// TCP port of the Epic Seven game server (`msg://`).
///
/// A [`NonZeroU16`] because [`Config::game_port`] is one: the `const` context
/// makes the `expect` a compile error rather than a runtime one, which is the
/// whole reason the constant is spelled this way round.
pub const DEFAULT_GAME_PORT: NonZeroU16 =
    NonZeroU16::new(3333).expect("the default game port is not zero");

/// The hosted analysis server, used when `config.toml` sets no `server_url`.
/// Named because [`Config::default`] has to push it through
/// [`ServerUrl::parse`] and the `expect` there needs something to point at.
const DEFAULT_SERVER_URL: &str = "wss://ingest.arkyve.dev/refresh-shop";

/// Smallest effective delay between server connection attempts.
pub(crate) const RECONNECT_FLOOR: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// TCP port of the game server, remote side.
    ///
    /// A [`NonZeroU16`], not a `u16`, for the same reason [`server_url`] is a
    /// [`ServerUrl`]: `game_port = 0` was refused by a clause in
    /// [`Config::validate`], which made the loader the only producer that could
    /// not express it. It reaches two places that have no `Config` in scope — the
    /// BPF filter string (`tcp and src port {game_port}`) and `parse_segment`'s
    /// server-side test — and port 0 is not a port in either: the filter would
    /// match nothing and the test would classify every packet as client-sent, so
    /// the relay would run, forward nothing, and look exactly like a wrong port.
    /// `serde` refuses the zero at parse time with the offending line quoted.
    ///
    /// [`server_url`]: Config::server_url
    pub game_port: NonZeroU16,

    /// Analysis server URL (`ws://` or `wss://`).
    ///
    /// A [`ServerUrl`], not a `String`, so the cleartext rule is enforced by the
    /// type rather than by `Config::validate` remembering to call it: a `Config`
    /// built any other way — a struct literal, a mutated `Config::default()`, a
    /// future GUI field — cannot carry an unchecked URL to the uplink. It is
    /// also why this struct can derive `Debug` again: `ServerUrl`'s own `Debug`
    /// prints the redacted form.
    pub server_url: ServerUrl,

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

// `Config`'s `Debug` is a plain derive, and safely so: `server_url` is a
// `ServerUrl`, whose own `Debug` prints `scheme://host[:port]` and nothing
// else. A `wss://` URL may carry a `user:pass@` credential, and `Config` is
// exactly the kind of value that ends up in a startup line or an
// `#[instrument]` argument list — so the promise in `README.md` ("the log never
// contains the server URL's credentials") is kept by the type, not by
// remembering to call a helper. This was a hand-written impl for exactly one
// release, while the field was still a `String`.

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
            // Through the one gate, like every other `ServerUrl`: the `expect`
            // is the invariant, and a literal that stopped satisfying the
            // cleartext rule would fail every test that builds a default.
            server_url: ServerUrl::parse(DEFAULT_SERVER_URL)
                .expect("the built-in default server_url must satisfy the cleartext rule"),
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

// `ServerUrl`, its authority/scheme parsing and its cleartext rule moved to
// `config/server_url.rs`, unchanged. The type's promise is that `dial` is
// reachable only through `as_str()`; keeping the fields private to that one file
// rather than to this whole module is what makes the promise checkable.

// There is no `validate_timings` here any more, and the absence is the fix.
//
// `[actuator.timings]` used to be bounded by a loop in this file that walked
// `Timings::named_ranges()` — first from `Config::validate` alone, then from
// `persist::write_sections` too, because there are **two** write boundaries and
// only one of them has a `Config`: the loader, and the Setup tab, which hands a
// `Timings` straight to `config::persist` with no `Config` anywhere in the path.
// Enforced at the loader alone, the GUI was one missing clamp away from writing a
// file the *next* launch refuses — the exact shape of the `kinds = ["unknown"]`
// checkbox that shipped, whose only cure was hand-editing the file the app owns.
//
// Both boundaries are now closed by the type instead of by two callers
// remembering to call one function: `plan::DelayRange` has private fields, a
// `try_new` carrying `min_ms <= max_ms <= plan::MAX_TIMING_MS`, and a
// `#[serde(try_from)]` hook, so an invalid range cannot be deserialized, built,
// or dragged into existence. The ceiling constant moved down to `plan` with the
// check — see `plan::MAX_TIMING_MS` for why the value is one minute, and
// `plan::DelayRangeError` for the two messages, which still say what a bad value
// *would have done* rather than merely that it is invalid.

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
    ///   - an empty `server_url`, a scheme other than `ws://` / `wss://`, or a
    ///     `ws://` URL pointing anywhere but loopback (it would forward the
    ///     captured game stream, session tokens included, in cleartext);
    ///   - a non-finite `[[filter.required_substats]] min` (`nan`/`inf` are
    ///     legal TOML floats), which no substat value can ever satisfy.
    ///
    /// Four rules are absent from that list because they are enforced where the
    /// value is *built*, so they surface as [`Error::ConfigParse`] with the
    /// offending line quoted rather than as [`Error::Config`]: `server_url`'s
    /// cleartext rule ([`ServerUrl`]), `game_port = 0` ([`NonZeroU16`]),
    /// `[actuator.timings]`'s reversed / over-ceiling ranges
    /// ([`plan::DelayRange`](crate::actuator::plan::DelayRange)), and an
    /// unrecognized `[filter] kinds` entry, which the wire-tolerant `ItemKind`
    /// would otherwise fold into `Unknown` and match nothing
    /// (`filter::hunt_kinds`).
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
        // No `game_port` clause here, deliberately, and for the same reason as
        // `server_url` and `[actuator.timings]` below: the field is a
        // `NonZeroU16`, so the zero this used to refuse cannot be built — on the
        // load path *or* in a struct literal — and both consumers (the BPF filter
        // and `parse_segment`) now receive the proof rather than a bare `u16`.
        // No `server_url` clause here, deliberately. It receives the reassembled
        // game stream, which can carry session tokens, so it must be TLS
        // (`wss://`) unless the host is loopback — and that rule is now carried
        // by the field's type: a `ServerUrl` exists only if `ServerUrl::parse`
        // accepted it, on the load path *and* in a struct literal. Re-checking
        // it here would be a second implementation of a proof we already hold.
        // No `[filter] kinds` clause either. `ItemKind` is wire-tolerant
        // (`serde(other)` -> `Unknown`), which in a config file would let a typo
        // silently match nothing — so the field does not hold an `ItemKind` any
        // more. `filter::HuntKind` has the game's three kinds and no catch-all, so
        // `kinds = ["equipement"]` is an `unknown variant` parse error naming the
        // three that are legal, and the typo cannot survive as far as this
        // function. That also closes the boundary this clause never covered: the
        // Setup tab writes `[filter]` through `persist::save` with no `Config` in
        // the path, which is how a checkbox once wrote a `kinds = ["unknown"]`
        // that the next launch refused fatally.
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
        // No `[actuator.timings]` clause either, for the same reason as
        // `server_url`: `plan::DelayRange` cannot hold a reversed or
        // over-ceiling pair, on the load path *and* in a struct literal, so a
        // check here would be a second implementation of a proof we already
        // hold — and it would only cover the loader, not the GUI's write.
        Ok(())
    }

    pub fn reconnect_initial(&self) -> Duration {
        Duration::from_millis(self.reconnect.initial_ms).max(RECONNECT_FLOOR)
    }

    pub fn reconnect_max(&self) -> Duration {
        Duration::from_millis(self.reconnect.max_ms).max(self.reconnect_initial())
    }
}
