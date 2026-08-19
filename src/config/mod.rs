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
use toml_edit::{DocumentMut, Item};

use crate::actuator::plan::{DelayRange, Timings};
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

/// Everything `config.toml` can say, in one value: a section per subsystem,
/// each `default` + `deny_unknown_fields`, so an absent file is valid and a
/// misspelled key is a parse error quoting the offending line.
///
/// Produced in exactly two places outside the test modules: [`Config::load`],
/// which returns [`Config::default`] verbatim when the file is missing, and
/// [`persist`], which rewrites named sections through `toml_edit` so the
/// player's own comments and key order survive a Setup-tab save.
///
/// **Most of its invariants are no longer enforced here, on purpose.**
/// [`Config::validate`] used to be the single gate; today it checks only that
/// a `required_substats[].min` is finite (TOML 1.0 spells `nan`, and
/// `Option<f64>` is the one field type left that can hold a value no
/// comparison satisfies). Every other rule moved *into* a type, holding for a
/// struct literal or a mutated [`Config::default`] just as much as for the
/// load path: [`ServerUrl`] carries the cleartext rule, [`NonZeroU16`]
/// carries `game_port != 0`, [`DelayRange`]
/// carries `min_ms <= max_ms <= MAX_TIMING_MS`, and `filter::hunt_kinds` (the
/// `deserialize_with` behind [`Filter`]'s `kinds`) refuses the wire's
/// catch-all `ItemKind`.
///
/// The move matters because the loader is not the only writer: the Setup tab
/// reaches the file through [`persist::save`] with no `Config` in the path,
/// which is how a checkbox once wrote `kinds = ["unknown"]` that the next
/// launch refused fatally. `validate`'s body is now a record of which rule
/// left and where — kept so none of them comes back as a second
/// implementation of a proof the type already holds.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// TCP port of the game server, remote side.
    ///
    /// A [`NonZeroU16`], not a `u16`: `game_port = 0` used to be refused by a
    /// clause in [`Config::validate`], which made the loader the only
    /// producer able to enforce it. The value reaches two places with no
    /// `Config` in scope — the BPF filter string (`tcp and src port
    /// {game_port}`) and `parse_segment`'s server-side test — where port 0
    /// would run, forward nothing, and look exactly like a wrong port rather
    /// than fail. `serde` now refuses the zero at parse time, line quoted.
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

// `Config`'s `Debug` is a plain derive, safely: `server_url` is a
// `ServerUrl`, whose own `Debug` prints `scheme://host[:port]` only. A
// `wss://` URL may carry a `user:pass@` credential, and `Config` is exactly
// the kind of value that ends up in a startup line or `#[instrument]`
// argument list, so README's "the log never contains the server URL's
// credentials" promise is kept by the type. (Hand-written for one release,
// while the field was still a `String`.)

/// Vestigial. Both keys are parsed and both are ignored.
///
/// Only the server's half of a connection was ever decoded — the analysis
/// server reads shop responses, and nothing has ever read the client's
/// requests — so `server_to_client` was a knob whose only useful position
/// was `true`, and `client_to_server` a knob for a feature that does not
/// exist. `parse_segment` now refuses anything the game server did not send,
/// leaving the two keys describing a distinction the code cannot express.
///
/// **Kept accepted-and-ignored on purpose — do not delete the fields.**
/// `config.example.toml` shipped the whole `[forward]` block uncommented,
/// and `main::seed_config_if_missing` writes that file to `%APPDATA%` on
/// every first run. With `deny_unknown_fields` on this struct and on
/// [`Config`], deleting the fields turns the next launch of every existing
/// installation into `Config::load` failing and an app that no longer
/// starts. Editing `config.example.toml` does nothing for files already
/// written.
///
/// Plan: keep them accepted-and-ignored for this release, with the startup
/// warning `main` emits from [`ForwardConfig::retired_keys`], then delete
/// both fields (and this struct) once a player upgrading across two
/// releases is no longer plausible. [`persist::strip_retired_keys`] already
/// deletes the keys from `config.toml` at the same startup that warns about
/// them, so the warning only ever fires once per installation.
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

/// Vestigial. Both keys are parsed and both are ignored — same shape as
/// [`ForwardConfig`], and for the same reason: `config.example.toml` shipped
/// `buffer_size` uncommented, so the key is on disk for every installation,
/// and `deny_unknown_fields` makes deleting the field a startup failure for
/// all of them. See [`ForwardConfig`] for the full removal plan; the warning
/// here comes from [`CaptureConfig::retired_keys`].
///
/// They stopped meaning anything once the capture filter stopped being
/// something this file could supply: the backend builds its own BPF
/// expression from the validated `game_port` (`tcp and src port
/// {game_port}`) and sizes its buffer from its own snaplen. The filter *had*
/// to stop being configurable while capture still ran a kernel driver, since
/// this file lives in per-user roaming app-data that any medium-integrity
/// process can rewrite, and its contents were handed to that driver's filter
/// compiler inside an administrator process. The driver is gone; nothing
/// needs the key any more.
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

// `validate_timings` is gone on purpose. `[actuator.timings]` used to be
// bounded by a loop here, called from `Config::validate` and separately from
// `persist::write_sections` (the Setup tab hands a `Timings` to
// `config::persist` with no `Config` in the path) — enforced at the loader
// alone, the GUI was one missing clamp away from writing a file the next
// launch refused, the same shape as the `kinds = ["unknown"]` incident above.
//
// Both boundaries are now closed by the type: `plan::DelayRange` has private
// fields, a `try_new` carrying `min_ms <= max_ms <= plan::MAX_TIMING_MS`, and
// a `#[serde(try_from)]` hook, so an invalid range cannot be built at all.
// See `plan::MAX_TIMING_MS` for why the ceiling is one minute, and
// `plan::DelayRangeError` for its two messages.
//
// Refusing the *value* is not refusing the *file*, and closing the second
// boundary made that distinction load-bearing: a `config.toml` written while
// the pair was still representable now meets a deserializer that rejects it,
// and a rejected deserialize is a fatal startup. `drop_unreadable_timing_ranges`
// below is the seam that keeps the two apart.

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
    /// Three rules are absent from that list because they are enforced where
    /// the value is *built*, so they surface as [`Error::ConfigParse`] (line
    /// quoted) rather than [`Error::Config`]: `server_url`'s cleartext rule
    /// ([`ServerUrl`]), `game_port = 0` ([`NonZeroU16`]), and an unrecognized
    /// `[filter] kinds` entry (`filter::hunt_kinds`).
    ///
    /// A reversed or over-ceiling `[actuator.timings]` range is the fourth
    /// such rule and the one exception to that shape: it is not an error here
    /// at all. See [`drop_unreadable_timing_ranges`] for why a value the type
    /// refuses must not be a value the loader dies on.
    ///
    /// [`Error::Config`]: crate::Error::Config
    /// [`Error::ConfigParse`]: crate::Error::ConfigParse
    /// [`Error::ConfigRead`]: crate::Error::ConfigRead
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let config = parse(&text)?;
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
        // `game_port`, `server_url` and `[actuator.timings]` need no clause
        // here: each is a type that cannot hold an invalid value on any path,
        // load or struct literal (see their field docs above and
        // `plan::DelayRange`), so a check here would be a second
        // implementation of a proof already held.
        //
        // No `[filter] kinds` clause either. `ItemKind` is wire-tolerant
        // (`serde(other)` -> `Unknown`), which in a config file would let a
        // typo silently match nothing — so `filter::hunt_kinds`, the field's
        // `deserialize_with`, refuses the catch-all itself and quotes the
        // offending entry (`kinds = ["equipement"]` names what the player
        // typed and the three legal values). A narrowed hunt-only enum was
        // tried and reverted: `ItemKind::Unknown` is meaningful *in the
        // domain* too (see `hunt_kinds`). This also covers the Setup tab's
        // write path, which has no `Config` in scope — the boundary that once
        // let a checkbox write `kinds = ["unknown"]` and get refused fatally
        // on the next launch.
        //
        // TOML 1.0 lets `min = nan` parse, and nothing satisfies `value >=
        // min`, so a non-finite min matches nothing while `is_unrestricted`
        // reports it restricted — and `Filter`'s derived `PartialEq` (the
        // Setup tab's dirty check) recurses into this `Option<f64>`, so `NaN
        // != NaN` leaves Apply lit forever, rewriting `config.toml` on every
        // click. `+inf` gives the first half without the second.
        for req in &self.filter.required_substats {
            if req.min.is_some_and(|min| !min.is_finite()) {
                return Err(crate::Error::Config(format!(
                    "filter.required_substats \"{}\" has a non-finite min — no substat value can \
                     ever satisfy it, so the filter would match nothing",
                    req.name
                )));
            }
        }
        // `capture.filter` (naming `game_port`) and `[forward]` (both
        // directions off) used to be refused here; both checks are gone with
        // the things they guarded, since the backend now builds its own
        // filter and there is only one capture direction. Do NOT reintroduce
        // either: a config written before the change may carry a stale value
        // for a setting that no longer has any effect, and refusing it would
        // lock that player out of the app on upgrade.
        Ok(())
    }

    pub fn reconnect_initial(&self) -> Duration {
        Duration::from_millis(self.reconnect.initial_ms).max(RECONNECT_FLOOR)
    }

    pub fn reconnect_max(&self) -> Duration {
        Duration::from_millis(self.reconnect.max_ms).max(self.reconnect_initial())
    }
}

/// Deserialize `text`, dropping any `[actuator.timings]` range this build
/// cannot read rather than refusing the whole file over it.
///
/// The strict parse is tried first and is the normal path — no `toml_edit`
/// re-parse, no allocation, nothing for a well-formed file to pay. Only when
/// it fails does [`drop_unreadable_timing_ranges`] get a look, and only a
/// second strict parse decides whether the result is loadable: the salvage
/// pass *removes candidates*, it never rules on them, so there is exactly one
/// definition of a valid config file in the crate and it is `serde`'s.
///
/// When the salvage finds nothing, the original refusal is returned unchanged.
/// When it finds something and the file still will not parse, the *second*
/// error is returned: the first named a key this pass has since dropped, so
/// repeating it would send the player to a line that is no longer the problem.
fn parse(text: &str) -> Result<Config> {
    let refused = match toml::from_str::<Config>(text) {
        Ok(config) => return Ok(config),
        Err(refused) => refused,
    };
    let Some((salvaged, dropped)) = drop_unreadable_timing_ranges(text) else {
        return Err(refused.into());
    };
    let config: Config = toml::from_str(&salvaged)?;
    for (key, reason) in dropped {
        // Not a `fatal` and not silence: the range is *not in force* (the
        // action runs at its calibrated baseline, the setting's own default),
        // and this is the only place that says so. It repeats every launch,
        // because nothing has rewritten the file — which is the pressure on
        // whoever typed it to fix it, and the reason this is a warning rather
        // than a shrug.
        tracing::warn!(
            key = %format_args!("actuator.timings.{key}"),
            reason = %reason,
            "this timing range could not be read, so it is not in force: the action keeps its tuned baseline with no extra wait. Fix the line in config.toml, or set the range from the Setup tab's Click timing section, which writes a valid one"
        );
    }
    Ok(config)
}

/// `text` with every unreadable `[actuator.timings]` range removed, and the
/// key and reason for each, or `None` when there is nothing of the sort to
/// remove.
///
/// **Why the loader forgives what the type refuses.**
/// [`DelayRange`] is right to refuse
/// `{ min_ms = 800, max_ms = 200 }`: read leniently it silently becomes a
/// fixed 800 ms delay, which is the opposite of the variability the player
/// asked for. But a *file already on disk* carrying that pair predates the
/// refusal — `DelayRange::draw` used to read it exactly that leniently, so it
/// was an accepted config for the whole life of the setting — and a
/// `toml::from_str` failure in `Config::load` is fatal in `main`: no main
/// window, only an error window, and the only cure is hand-editing a file
/// under `%APPDATA%` that the app otherwise owns and rewrites for them.
/// Nothing in the upgrade path reaches it first: `strip_retired_keys` runs
/// *after* the load that failed, `persist::save` only ever rewrites an
/// already-valid `Timings`, and `migrate` deals in ACLs.
///
/// That is the `kinds = ["unknown"]` incident again, exactly — and `type-010`
/// in `docs/tech-debt/10-type.md`, the entry that asked for these private
/// fields in the first place, is the one that describes it: "the *next launch*
/// refused, giving a fatal error window and no way out but hand-editing the
/// file the app owns". Making the range unrepresentable was the right half of
/// that fix; it is only half, because a file predating the type is still on
/// disk and nobody warned it. `validate`'s closing comment says the same thing
/// about the retired `[capture]` / `[forward]` keys in as many words: do not
/// lock a player out over a setting they cannot reach.
///
/// So the value is refused and the file is not. The refused range is dropped
/// to its default (`0..=0`, the calibrated baseline and the setting's own
/// absent-key behaviour) and named in a startup warning — deliberately *not*
/// repaired into `{ min_ms = 200, max_ms = 800 }`, which would be a guess at
/// which of the two numbers the player meant, written into their file, in a
/// module whose neighbouring pass promises "no setting of yours is changed".
/// Someone hand-writing this key fresh still learns their pair is wrong, at
/// the same launch, by name and with the reason — they simply also get an app.
///
/// The pass walks only the eight keys `Timings::named_ranges` knows and asks
/// `DelayRange::try_new` itself, so it can neither re-derive the rule nor
/// reach a key it does not own. Anything else about the table — a misspelled
/// key under `deny_unknown_fields`, `min_ms = "800"`, a negative integer — is
/// left exactly where it is for the second strict parse to refuse.
fn drop_unreadable_timing_ranges(text: &str) -> Option<(String, Vec<(&'static str, String)>)> {
    let mut doc: DocumentMut = text.parse().ok()?;
    // `as_table_like_mut` rather than `as_table_mut`, at both hops: a player
    // may have written `[actuator.timings]`, `actuator = { timings = { .. } }`
    // or `actuator.timings.refreshed = { .. }`, and all three are the same
    // document to `toml` — so a pass that only understood the header form
    // would leave the other two bricking the app it was written to unbrick.
    let timings = doc
        .as_table_mut()
        .get_mut("actuator")?
        .as_table_like_mut()?
        .get_mut("timings")?
        .as_table_like_mut()?;
    let mut dropped = Vec::new();
    for (key, _) in Timings::default().named_ranges() {
        let Some(written) = timings.get(key).and_then(written_range) else {
            continue;
        };
        if let Err(reason) = DelayRange::try_new(written.0, written.1) {
            timings.remove(key);
            // `DelayRangeError`'s own sentence, resolved here rather than
            // reworded: it is what the player would have read in the error
            // window this pass exists to prevent, and it names both of their
            // numbers and what the pair would have been read as.
            dropped.push((key, reason.to_string()));
        }
    }
    (!dropped.is_empty()).then(|| (doc.to_string(), dropped))
}

/// The `(min_ms, max_ms)` pair an authored range spells out, with an omitted
/// key reading 0 exactly as `RawDelayRange`'s `serde(default)` does — so the
/// `{ max_ms = 120000 }` shorthand is judged on the same numbers `serde` would
/// have judged it on.
///
/// `None` for anything that is not two non-negative integers, which is the
/// pass declining to have an opinion: a string, a float or a negative is a
/// different complaint, and the strict parse makes it far better than this
/// could.
fn written_range(item: &Item) -> Option<(u64, u64)> {
    let table = item.as_table_like()?;
    let field = |name: &str| match table.get(name) {
        None => Some(0),
        Some(written) => written.as_integer().and_then(|ms| u64::try_from(ms).ok()),
    };
    Some((field("min_ms")?, field("max_ms")?))
}
