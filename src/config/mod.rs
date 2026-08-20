//! Relay configuration, loaded from a TOML file (defaults otherwise).
//!
//! Two seams live beside the schema: [`persist`] rewrites sections through
//! `toml_edit` so the player's comments survive, and [`ServerUrl`] keeps the
//! cleartext rule where its fields are private to one file.

pub mod persist;
mod server_url;

// The only path to the type: `server_url` is not a public module.
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

/// TCP port of the Epic Seven game server (`msg://`). A [`NonZeroU16`] in a
/// `const` context, so the `expect` is a compile error, not a runtime one.
pub const DEFAULT_GAME_PORT: NonZeroU16 =
    NonZeroU16::new(3333).expect("the default game port is not zero");

/// The hosted analysis server, used when `config.toml` sets no `server_url`.
const DEFAULT_SERVER_URL: &str = "wss://ingest.arkyve.dev/refresh-shop";

/// Smallest effective delay between server connection attempts.
pub(crate) const RECONNECT_FLOOR: Duration = Duration::from_millis(100);

/// Everything `config.toml` can say. Every section is `default` +
/// `deny_unknown_fields`: an absent file is valid, a misspelled key is a parse
/// error quoting the offending line.
///
/// **Its invariants live in the field types, not in [`Config::validate`]** —
/// [`ServerUrl`] the cleartext rule, [`NonZeroU16`] `game_port != 0`,
/// [`DelayRange`] `min_ms <= max_ms <= MAX_TIMING_MS`, `filter::hunt_kinds` the
/// catch-all `ItemKind` — because the loader is not the only writer:
/// [`persist::save`] runs with no `Config` in the path, which is how a checkbox
/// once wrote `kinds = ["unknown"]` that the next launch refused fatally.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// TCP port of the game server, remote side.
    ///
    /// [`NonZeroU16`], not `u16`: the BPF filter string and `parse_segment`'s
    /// server-side test both read it with no `Config` in scope, and port 0
    /// there runs, forwards nothing, and looks like a wrong port rather than
    /// failing.
    pub game_port: NonZeroU16,

    /// Analysis server URL (`ws://` or `wss://`).
    ///
    /// [`ServerUrl`], not `String`, so the cleartext rule holds for a `Config`
    /// built any other way — and so this struct can derive `Debug`: a `wss://`
    /// URL may carry a `user:pass@` credential, and `ServerUrl`'s `Debug`
    /// prints only `scheme://host[:port]`, which is README's "the log never
    /// contains the server URL's credentials" promise.
    pub server_url: ServerUrl,

    /// Vestigial; see [`ForwardConfig`].
    pub forward: ForwardConfig,

    pub reconnect: ReconnectConfig,

    /// Vestigial; see [`CaptureConfig`].
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

/// Vestigial: only the server's half of a connection is ever decoded, so both
/// keys are parsed and both are ignored.
///
/// **Do not delete the fields** while a two-release upgrade is still plausible.
/// `config.example.toml` shipped `[forward]` uncommented and
/// `main::seed_config_if_missing` wrote it to every `%APPDATA%`, so under
/// `deny_unknown_fields` deleting them fails `Config::load` on every existing
/// installation; editing `config.example.toml` does nothing for files already
/// written. [`persist::strip_retired_keys`] takes the keys out at the same
/// startup that warns about them, so the warning fires once per install.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ForwardConfig {
    /// Ignored: server -> client is the only half captured, so this cannot be
    /// turned off.
    pub server_to_client: Option<bool>,
    /// Ignored: client -> server is never captured, so this cannot be turned
    /// on.
    pub client_to_server: Option<bool>,
}

/// Exponential-backoff policy for the server link: where it starts and where it
/// stops doubling. Both are floored and ordered by the accessors, not here —
/// see [`Config::reconnect_initial`] and [`Config::reconnect_max`].
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReconnectConfig {
    pub initial_ms: u64,
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

/// Vestigial: the backend builds its own BPF expression from `game_port` and
/// sizes its buffer from its own snaplen, so both keys are parsed and ignored.
///
/// **Do not delete the fields** — `config.example.toml` shipped `buffer_size`
/// uncommented, so this is [`ForwardConfig`]'s case exactly; see there. The
/// warning here comes from [`CaptureConfig::retired_keys`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    /// Ignored: sized by the backend from the snaplen it asks libpcap for.
    pub buffer_size: Option<usize>,
    /// Ignored: built by the backend from `game_port`, never from this file.
    pub filter: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            game_port: DEFAULT_GAME_PORT,
            // Through the one gate: a literal that stopped satisfying the
            // cleartext rule fails every test that builds a default.
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
    /// The retired keys this file actually sets, or `None` when it sets
    /// neither. `main` turns it into one startup warning: a key that silently
    /// does nothing is worse than one that is refused.
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
    /// Same contract, and same reason, as [`CaptureConfig::retired_keys`].
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

// Do not reintroduce a `validate_timings` loop here: `plan::DelayRange`'s
// private fields, `try_new` and `#[serde(try_from)]` already close both
// boundaries, the loader's and the Setup tab's.

impl Config {
    /// Loads the configuration from `path`. A missing file yields the defaults.
    ///
    /// # Errors
    ///
    /// - [`Error::ConfigRead`] — the file exists but could not be read. A
    ///   *missing* file is deliberately not an error: it yields
    ///   `Config::default()`, which is how a fresh machine starts.
    /// - [`Error::ConfigParse`] — not valid TOML, or it does not match this
    ///   struct: misspelled key, wrong type, integer out of range. Three rules
    ///   land here rather than in [`Error::Config`] because a type enforces
    ///   them: the cleartext rule ([`ServerUrl`]), `game_port = 0`
    ///   ([`NonZeroU16`]), an unknown `[filter] kinds` (`filter::hunt_kinds`).
    /// - [`Error::Config`] — a non-finite `[[filter.required_substats]] min`
    ///   (`nan`/`inf` are legal TOML floats), which nothing can satisfy.
    ///
    /// A reversed or over-ceiling `[actuator.timings]` range is not an error
    /// here at all — see [`drop_unreadable_timing_ranges`].
    ///
    /// [`Error::Config`]: crate::Error::Config
    /// [`Error::ConfigParse`]: crate::Error::ConfigParse
    /// [`Error::ConfigRead`]: crate::Error::ConfigRead
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Self::load_reporting(path).map(|(config, _)| config)
    }

    /// [`load`](Config::load), plus the timing ranges it had to drop.
    ///
    /// The salvage is already in the log; it comes back as a *value* for the
    /// caller that cannot reach its audience yet, the journal panel being the
    /// windowed build's only surface and not existing until `app::setup` has
    /// run. Same deferral as [`crate::LogSetup`] and
    /// [`crate::migrate::Leftovers`].
    ///
    /// # Errors
    ///
    /// Exactly [`load`](Config::load)'s: the report rides along, it does not
    /// change what is refused.
    pub fn load_reporting(path: impl AsRef<Path>) -> Result<(Self, DroppedRanges)> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let (config, dropped) = parse(&text)?;
                config.validate()?;
                Ok((config, dropped))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Ok((Self::default(), DroppedRanges::default()))
            }
            // Name the file: it lives out of the way under %APPDATA%, so a bare
            // "Access is denied. (os error 5)" leaves nothing to act on.
            Err(source) => Err(crate::Error::ConfigRead {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    fn validate(&self) -> Result<()> {
        // No clause for `game_port`, `server_url`, `[filter] kinds` or
        // `[actuator.timings]`: their types already hold the proof.
        //
        // TOML 1.0 lets `min = nan` parse, and nothing satisfies `value >=
        // min`, so a non-finite min matches nothing while `is_unrestricted`
        // reports it restricted. `NaN` adds a second symptom: `Filter`'s
        // derived `PartialEq` is the Setup tab's dirty check, so `NaN != NaN`
        // leaves Apply lit forever, rewriting `config.toml` on every click.
        for req in &self.filter.required_substats {
            if req.min.is_some_and(|min| !min.is_finite()) {
                return Err(crate::Error::Config(format!(
                    "filter.required_substats \"{}\" has a non-finite min — no substat value can \
                     ever satisfy it, so the filter would match nothing",
                    req.name
                )));
            }
        }
        // Do NOT reintroduce the old `capture.filter` / `[forward]` clauses:
        // they would refuse a stale value for an inert setting, locking that
        // player out of the app on upgrade.
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
/// The strict parse runs first, so a well-formed file pays no `toml_edit`
/// re-parse. The salvage pass *removes candidates* and never rules on them —
/// only a second strict parse decides what is loadable, so `serde` stays the
/// crate's one definition of a valid config file. When that second parse also
/// fails, its error is the one returned: the first named a key since dropped.
fn parse(text: &str) -> Result<(Config, DroppedRanges)> {
    let refused = match toml::from_str::<Config>(text) {
        Ok(config) => return Ok((config, DroppedRanges::default())),
        Err(refused) => refused,
    };
    let Some((salvaged, dropped)) = drop_unreadable_timing_ranges(text) else {
        return Err(refused.into());
    };
    let config: Config = toml::from_str(&salvaged)?;
    let mut names = Vec::with_capacity(dropped.len());
    for (key, reason) in dropped {
        let key = format!("actuator.timings.{key}");
        // Not fatal and not silent: the range is *not in force*, and nothing
        // rewrites the file, so this repeats until the line is fixed.
        tracing::warn!(
            key = %key,
            reason = %reason,
            "this timing range could not be read, so it is not in force: the action keeps its tuned baseline with no extra wait. Fix the line in config.toml, or set the range from the Setup tab's Click timing section, which writes a valid one"
        );
        names.push(key);
    }
    Ok((config, DroppedRanges(names)))
}

/// Which `[actuator.timings]` ranges [`parse`] dropped, so a caller can say so
/// again somewhere the log is not. Key names only: the `warn!` in `parse`
/// already holds the diagnosis, and this feeds the journal panel.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DroppedRanges(Vec<String>);

impl DroppedRanges {
    /// The journal lines, empty when nothing was dropped. One line for all of
    /// them: the advice does not vary by key, and the journal is a bounded ring.
    #[must_use]
    pub fn journal_lines(&self) -> Vec<String> {
        if self.0.is_empty() {
            return Vec::new();
        }
        vec![format!(
            ">> config: {} could not be read, so {} not in force — the action keeps its tuned baseline. Fix the line in config.toml, or set the range in Setup ▸ Click timing, which writes a valid one",
            self.0.join(", "),
            if self.0.len() == 1 {
                "it is"
            } else {
                "they are"
            }
        )]
    }
}

/// `text` with every unreadable `[actuator.timings]` range removed, and the
/// key and reason for each, or `None` when there is nothing of the sort to
/// remove.
///
/// **The value is refused; the file is not.** `{ min_ms = 800, max_ms = 200 }`
/// was an accepted config before `DelayRange` started refusing it, and a
/// `toml::from_str` failure in `Config::load` is fatal in `main` — an error
/// window instead of the app, curable only by hand-editing a file the app
/// otherwise owns, with nothing in the upgrade path reaching it first.
///
/// So the range is dropped to its default (`0..=0`, also the absent-key
/// behaviour) and named in a startup warning, *not* repaired into
/// `{ min_ms = 200, max_ms = 800 }`, which would write a guess into their file.
///
/// The pass walks only the eight keys `Timings::named_ranges` knows and asks
/// `DelayRange::try_new` itself, so it can neither re-derive the rule nor reach
/// a key it does not own. Anything else — a misspelled key, `min_ms = "800"`, a
/// negative — is left for the second strict parse to refuse.
fn drop_unreadable_timing_ranges(text: &str) -> Option<(String, Vec<(&'static str, String)>)> {
    let mut doc: DocumentMut = text.parse().ok()?;
    // `as_table_like_mut`, not `as_table_mut`, at both hops: the header, inline
    // and dotted spellings are one document to `toml`, and a pass that
    // understood only headers would leave the other two bricking the app.
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
            // `DelayRangeError`'s own sentence, not a reworded one: it names
            // both of the player's numbers and how the pair would have read.
            dropped.push((key, reason.to_string()));
        }
    }
    (!dropped.is_empty()).then(|| (doc.to_string(), dropped))
}

/// The `(min_ms, max_ms)` pair `RawDelayRange`'s own `TryFrom` (in
/// `actuator::plan::timings`) would build from this table, so the salvage
/// pass judges an authored range exactly as `serde` does. An absent `min_ms`
/// reads `0`; an absent `max_ms` reads the *resolved* `min_ms`, not `0` —
/// `refreshed = { min_ms = 200 }` is the fixed delay `200..=200`, and judging
/// it as `(200, 0)` is the defect this function exists to not repeat.
/// `RawDelayRange` is the specification; keep this in agreement with it.
///
/// `None` for anything that is not two non-negative integers written: a
/// string, float or negative is a different complaint, reported better by the
/// strict parse. An absent key is not that case — it resolves per the rule
/// above, exactly as `#[serde(default)]` would.
fn written_range(item: &Item) -> Option<(u64, u64)> {
    let table = item.as_table_like()?;
    // `Ok(None)` for a key that is simply absent, `Ok(Some(ms))` for one that
    // reads as a non-negative integer, `Err(())` for anything else — kept
    // distinct from "absent" so an unreadable key still fails the read below
    // and defers to the strict parse, rather than being treated as unwritten.
    let field = |name: &str| -> std::result::Result<Option<u64>, ()> {
        match table.get(name) {
            None => Ok(None),
            Some(written) => written
                .as_integer()
                .and_then(|ms| u64::try_from(ms).ok())
                .map(Some)
                .ok_or(()),
        }
    };
    let min_ms = field("min_ms").ok()?.unwrap_or(0);
    let max_ms = field("max_ms").ok()?.unwrap_or(min_ms);
    Some((min_ms, max_ms))
}
