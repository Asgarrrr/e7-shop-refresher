//! Unified error type for the relay.
//!
//! **`Display` is this error's own layer; the cause is reached through
//! [`std::error::Error::source`].** No message interpolates its own
//! `#[source]`, so nothing double-prints, and [`Error::report`] — the
//! spelling every report site uses — walks the chain and joins it.
//!
//! The alternative — every message interpolating its own cause — makes
//! `#[source]` decorative: a variant added with `#[source]` and no interpolation
//! loses its cause everywhere at once, silently. A `tracing` field spelled
//! `error = ?err` is unaffected.
//!
//! The two TOML payloads are boxed: 88 bytes each, built once at startup, while
//! `Error` is the `E` of every `Result` in the crate including the capture
//! loop's — boxing takes it from 96 bytes to 48. No clippy threshold catches
//! this (`result_large_err` fires at 128, `large_enum_variant` at a 200-byte
//! spread), so the `const` assertion at the bottom is the check.

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

    /// `config.toml` is not valid TOML, or does not match `Config`'s shape.
    /// Boxed: see the module header.
    #[error("config.toml is not valid")]
    ConfigParse(#[source] Box<toml::de::Error>),

    /// The config file exists but could not be read. Never `NotFound`: a missing
    /// file yields the defaults. Carries the path because the file lives out of
    /// the way in `%APPDATA%`, and a bare "Access is denied. (os error 5)" leaves
    /// the player nothing to fix.
    #[error("could not read {}", path.display())]
    ConfigRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Persisting the GUI-editable sections failed on the filesystem side
    /// (parent directory, temp file, or the rename). Carries the path: a
    /// read-only or antivirus-locked `config.toml` silently discards every Setup
    /// change, and the banner has to be able to name the file.
    #[error("could not write {}", path.display())]
    ConfigWrite {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The on-disk `config.toml` could not be re-parsed by the format-preserving
    /// editor before splicing the managed sections in. Kept as the source type
    /// rather than a `String` because `toml_edit::TomlError` carries the
    /// offending span. Boxed: see the module header.
    #[error("config.toml could not be re-parsed to be edited")]
    ConfigReparse(#[source] Box<toml_edit::TomlError>),

    /// A managed section could not be serialized back to TOML. Distinct from
    /// [`Error::ConfigReparse`] on purpose: only one of the two is the player's
    /// fault, and flattened to one string the banner cannot tell them apart.
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

// No context-free `Io(#[from] std::io::Error)` variant, deliberately: it makes
// silent conversion the default for every future `?` at any depth, losing the
// path or call that failed, and "i/o: The system cannot find the file
// specified." is a message no player can act on. Name the operation the way
// `Capture` does instead. Re-adding the variant reopens that door.

impl Error {
    /// This error and every cause behind it, joined with `": "` — what every
    /// player-facing report site uses instead of a bare `{err}`. Walks
    /// [`std::error::Error::source`], picking up the layer a variant's `Display`
    /// deliberately leaves out: `could not write C:\Users\…\config.toml: Access
    /// is denied. (os error 5)`.
    #[must_use]
    pub fn report(&self) -> String {
        let mut out = self.to_string();
        let mut cause: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(self);
        while let Some(err) = cause {
            out.push_str(": ");
            out.push_str(&err.to_string());
            cause = err.source();
        }
        out
    }
}

// Hand-written rather than `#[from]`: `#[from] Box<T>` would generate
// `From<Box<toml::de::Error>>`, and the `?` on `toml::from_str` in `config`
// needs `From<toml::de::Error>`.
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

// 48 is the measured size on Windows, where `PathBuf` is 32 bytes rather than
// 24 (`OsString` wraps a `Wtf8Buf` carrying an `is_known_utf8` flag); the same
// enum is 40 on a unix dev machine. Hence `<=` rather than `==`: the assertion
// must not fire on the platform it was not measured on, and a future 8-byte
// field is not a build break — 88 inline bytes is.
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
        // One layer each, neither repeated.
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
