//! Unified error type for the relay.
//!
//! **`Display` is this error's own layer; the cause is reached through
//! [`std::error::Error::source`].** No message interpolates its own
//! `#[source]`, so nothing double-prints, and [`Error::report`] — the
//! spelling every report site uses — walks the chain and joins it.
//!
//! Earlier, each message inlined its cause so a bare `{err}` never lost it.
//! That relied on every author remembering, and made `#[source]` decorative:
//! a variant added with `#[source]` and no interpolation would lose its
//! cause everywhere at once, silently. With the reporter doing the walking,
//! a new `#[source]` variant is correct by default.
//!
//! Every player-facing report site says `err.report()`, not `{err}`:
//! `main`'s two `fatal`/`eprintln!` arms, `app::supervise`, `ui::App`'s
//! "config.toml not saved" journal line, and `RetiredKeys::NotRewritten`. A
//! `tracing` field spelled `error = ?err` is unaffected — `Debug` on a
//! `thiserror` enum prints the nested source structurally.
//!
//! The two TOML payloads are boxed: `toml::de::Error` and
//! `toml_edit::TomlError` are 88 bytes each, built once at startup, while
//! `Error` is the `E` of every `Result` in the crate — including
//! `PacketSource::next_segment` on the capture loop. Boxing them takes
//! `Error` from 96 bytes to 48 (measured on Windows; see the `const`
//! assertion at the bottom). Neither clippy threshold catches this
//! (`result_large_err` fires at 128, `large_enum_variant` at a 200-byte
//! spread), so the check is explicit.

use std::path::PathBuf;

use thiserror::Error;

/// The crate-wide result: every fallible function returns this `E`.
pub type Result<T> = std::result::Result<T, Error>;

/// Every way the relay can fail, from the config loader to the capture backend.
///
/// See the module header: `Display` is one layer, [`Error::report`] is the chain.
#[derive(Debug, Error)]
pub enum Error {
    /// A value parsed fine but breaks an invariant `Config::validate` enforces.
    #[error("invalid configuration: {0}")]
    Config(String),

    /// `config.toml` is not valid TOML, or does not match `Config`'s shape
    /// (unknown key, wrong type, out-of-range integer). Boxed: see the module
    /// header.
    #[error("config.toml is not valid")]
    ConfigParse(#[source] Box<toml::de::Error>),

    /// The config file exists but could not be read (locked, permission
    /// denied, a directory). A missing file is not an error — it yields the
    /// defaults — so this never covers `NotFound`. The path is carried
    /// because the file lives out of the way in `%APPDATA%`: a bare "Access
    /// is denied. (os error 5)" would leave the player nothing to fix.
    #[error("could not read {}", path.display())]
    ConfigRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Persisting the GUI-editable sections failed on the filesystem side
    /// (parent directory, temp file, or the rename). Carries the path for the
    /// same reason as [`Error::ConfigRead`]: a read-only or antivirus-locked
    /// `config.toml` silently discards every Setup change, and the banner has
    /// to be able to name the file.
    #[error("could not write {}", path.display())]
    ConfigWrite {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The on-disk `config.toml` could not be re-parsed by the
    /// format-preserving editor before splicing the managed sections in. Kept
    /// as the source type rather than a `String`: `toml_edit::TomlError`
    /// carries the offending span, which is the whole point of reporting it.
    /// Boxed: see the module header.
    #[error("config.toml could not be re-parsed to be edited")]
    ConfigReparse(#[source] Box<toml_edit::TomlError>),

    /// A managed section could not be serialized back to TOML. Distinct from
    /// [`Error::ConfigReparse`] on purpose — flattened to one string the two
    /// were indistinguishable in the banner, though only one of them is the
    /// player's fault.
    #[error("a config section could not be serialized")]
    ConfigSerialize(#[from] toml_edit::ser::Error),

    /// The capture backend refused or failed: names the Win32 call or the
    /// missing prerequisite, plus the install hint where there is one.
    #[error("network capture: {0}")]
    Capture(String),

    /// A supervised task died unexpectedly (panic or abnormal exit); the string
    /// already names which one, so it renders as-is.
    #[error("{0}")]
    Fatal(String),
}

// There is deliberately no context-free `Io(#[from] std::io::Error)` variant
// above. It had exactly one reachable producer — a blanket `?` on
// `std::thread::Builder::spawn` in `app::workers::spawn_capture_with_budget`
// — and that site now names what it was doing (`Error::Capture("starting
// the capture thread: …")`), because "i/o: The system cannot find the file
// specified." is a message no player can act on. A `#[from] std::io::Error`
// would make silent conversion the default for every future `?`, at any
// depth, losing the path or call that failed. `ConfigRead`/`ConfigWrite`
// carry a path; anything else should name its operation the way `Capture`
// does. Re-adding this variant reopens that door.

impl Error {
    /// This error and every cause behind it, joined with `": "` — the spelling
    /// every player-facing report site uses instead of a bare `{err}`.
    ///
    /// Walks [`std::error::Error::source`], so it picks up the layer a variant's
    /// own `Display` deliberately leaves out: `could not write
    /// C:\Users\…\config.toml: Access is denied. (os error 5)` is the path
    /// followed by the reason. A variant with no `#[source]` reports exactly
    /// its own message.
    #[must_use]
    pub fn report(&self) -> String {
        let mut out = self.to_string();
        let mut cause: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(self);
        while let Some(err) = cause {
            // Boxed payloads are already `Deref`ed by `source()`, so this reads
            // the real `toml` error, not a `Box`'s (identical) rendering.
            out.push_str(": ");
            out.push_str(&err.to_string());
            cause = err.source();
        }
        out
    }
}

// Hand-written rather than `#[from]`: `#[from] Box<T>` would generate
// `From<Box<toml::de::Error>>`, and the `?` on `toml::from_str` (`config.rs:330`,
// `:440`) needs `From<toml::de::Error>`. `Display` and `#[source]` behaviour are
// unchanged — `Box<E>` derefs to `E`.
impl From<toml::de::Error> for Error {
    fn from(source: toml::de::Error) -> Self {
        Self::ConfigParse(Box::new(source))
    }
}

impl From<toml_edit::TomlError> for Error {
    fn from(source: toml_edit::TomlError) -> Self {
        Self::ConfigReparse(Box::new(source))
    }
}

// The boxing above is the only thing keeping this type small: no clippy lint
// would catch a future 88-byte payload added inline (see the module header).
//
// 48 is the measured size on Windows, where the largest remaining variant is
// `ConfigRead`/`ConfigWrite` at 40 bytes: `PathBuf` is 32 there rather than 24,
// because `OsString` wraps a `Wtf8Buf` that carries an `is_known_utf8` flag
// beside its `Vec<u8>`. On a unix dev machine the same enum is 40. `<=` rather
// than `==` so the assertion does not fire on the platform it was not measured
// on, and so a future 8-byte field is not a build break — 88 inline bytes is.
const _: () = assert!(
    size_of::<Error>() <= 48,
    "Error is the E of every Result in the crate, including the capture loop's — box a large variant instead of growing it"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_appends_the_cause_that_display_no_longer_inlines() {
        let err = Error::ConfigWrite {
            path: PathBuf::from("C:/Users/x/config.toml"),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        // One layer each, neither repeated — the whole point of dropping
        // `{source}` from the message.
        let displayed = err.to_string();
        assert_eq!(displayed, "could not write C:/Users/x/config.toml");
        let reported = err.report();
        assert!(
            reported.starts_with(&format!("{displayed}: ")),
            "{reported}"
        );
        assert!(
            reported
                .contains(&std::io::Error::from(std::io::ErrorKind::PermissionDenied).to_string()),
            "{reported}"
        );
    }

    #[test]
    fn report_of_a_sourceless_variant_is_just_its_message() {
        let err = Error::Capture("no adapter".to_owned());
        assert_eq!(err.report(), err.to_string());
        assert_eq!(err.report(), "network capture: no adapter");
    }

    /// The trap the convention closes: a `#[source]` whose message does *not*
    /// interpolate it is now the correct shape, not a silent loss.
    #[test]
    fn no_variants_display_repeats_its_own_source() {
        let source = std::io::Error::from(std::io::ErrorKind::NotFound);
        let text = source.to_string();
        for err in [
            Error::ConfigRead {
                path: PathBuf::from("p"),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            },
            Error::ConfigWrite {
                path: PathBuf::from("p"),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            },
        ] {
            assert!(!err.to_string().contains(&text), "{err}");
            assert_eq!(err.report().matches(text.as_str()).count(), 1, "{err:?}");
        }
    }
}
