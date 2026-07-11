//! Unified error type for the relay.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid configuration: {0}")]
    Config(String),

    #[error("configuration parse: {0}")]
    ConfigParse(#[from] toml::de::Error),

    #[error("network capture: {0}")]
    Capture(String),

    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}
