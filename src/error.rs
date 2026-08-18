//! Unified error type for the relay.

use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    /// A value parsed fine but breaks an invariant `Config::validate` enforces.
    #[error("invalid configuration: {0}")]
    Config(String),

    /// `config.toml` is not valid TOML, or does not match `Config`'s shape
    /// (unknown key, wrong type, out-of-range integer).
    #[error("configuration parse: {0}")]
    ConfigParse(#[from] toml::de::Error),

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
    #[error("config re-parse: {0}")]
    ConfigReparse(#[from] toml_edit::TomlError),

    /// A managed section could not be serialized back to TOML. Distinct from
    /// [`Error::ConfigReparse`] on purpose — flattened to one string the two
    /// were indistinguishable in the banner, though only one of them is the
    /// player's fault.
    #[error("config serialize: {0}")]
    ConfigSerialize(#[from] toml_edit::ser::Error),

    #[error("network capture: {0}")]
    Capture(String),

    /// A supervised task died unexpectedly (panic or abnormal exit); the string
    /// already names which one, so it renders as-is.
    #[error("{0}")]
    Fatal(String),

    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}
