//! Unified error type for the relay.
//!
//! **The convention in this file: `Display` carries everything.** Every message
//! is self-contained — the variants that also expose a cause through
//! `#[source]`/`#[from]` interpolate it into their own text as well, so a bare
//! `{err}` at a report site never loses the reason. That is deliberate and it is
//! what the two report sites rely on (`fatal` in `src/main.rs`, `app::supervise`),
//! neither of which walks `source()`. A new variant that carries a `#[source]`
//! must therefore interpolate it too, or its cause disappears everywhere at once.
//! `#[source]` stays on top of that for programmatic inspection (`matches!` on the
//! chain, and a future `{err:#}` reporter).
//!
//! The two TOML payloads are boxed: `toml::de::Error` and `toml_edit::TomlError`
//! are 88 bytes each and can only ever be built once, at startup, while `Error`
//! is the `E` of every `Result` in the crate — including
//! `PacketSource::next_segment` on the capture loop. Boxing them takes `Error`
//! from 96 bytes to 48 (measured on Windows; see the `const` assertion at the
//! bottom, which keeps it there). Neither clippy threshold catches this
//! (`result_large_err` fires at 128, `large_enum_variant` at a 200-byte spread),
//! so the check has to be explicit.

use std::path::PathBuf;

use thiserror::Error;

/// The crate-wide result: every fallible function returns this `E`.
pub type Result<T> = std::result::Result<T, Error>;

/// Every way the relay can fail, from the config loader to the capture backend.
///
/// See the module header for the `Display`-carries-everything convention.
#[derive(Debug, Error)]
pub enum Error {
    /// A value parsed fine but breaks an invariant `Config::validate` enforces.
    #[error("invalid configuration: {0}")]
    Config(String),

    /// `config.toml` is not valid TOML, or does not match `Config`'s shape
    /// (unknown key, wrong type, out-of-range integer). Boxed: see the module
    /// header.
    #[error("configuration parse: {0}")]
    ConfigParse(#[source] Box<toml::de::Error>),

    /// The config file exists but could not be read (locked, permission
    /// denied, a directory). A missing file is not an error — it yields the
    /// defaults — so this never covers `NotFound`. The path is carried because
    /// the file lives out of the way in `%APPDATA%`: a bare
    /// "Access is denied. (os error 5)" would leave the player nothing to fix.
    #[error("could not read {}: {source}", path.display())]
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
    #[error("could not write {}: {source}", path.display())]
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
    #[error("config re-parse: {0}")]
    ConfigReparse(#[source] Box<toml_edit::TomlError>),

    /// A managed section could not be serialized back to TOML. Distinct from
    /// [`Error::ConfigReparse`] on purpose — flattened to one string the two
    /// were indistinguishable in the banner, though only one of them is the
    /// player's fault.
    #[error("config serialize: {0}")]
    ConfigSerialize(#[from] toml_edit::ser::Error),

    /// The capture backend refused or failed: names the Win32 call or the
    /// missing prerequisite, plus the install hint where there is one.
    #[error("network capture: {0}")]
    Capture(String),

    /// A supervised task died unexpectedly (panic or abnormal exit); the string
    /// already names which one, so it renders as-is.
    #[error("{0}")]
    Fatal(String),

    /// A filesystem or OS call failed somewhere that carries no path of its own.
    /// Prefer [`Error::ConfigRead`]/[`Error::ConfigWrite`] whenever a path is
    /// known — this variant is the context-free fallback.
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
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

// The boxing above is the only thing keeping this type small, and nothing else
// would notice if a future variant inlined another 88-byte payload: both clippy
// size lints sit far above this range (see the module header).
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
