use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("game window not found (expected title containing {0:?})")]
    WindowNotFound(String),

    #[error(
        "multiple windows matched the filter — narrow `title_contains` or set `process_name`:\n  - {0}",
        candidates.join("\n  - ")
    )]
    MultipleWindowsMatched { candidates: Vec<String> },

    #[error(
        "game window resized mid-run ({initial_w}x{initial_h} → {current_w}x{current_h}) — templates would mis-scale"
    )]
    WindowResized {
        initial_w: u32,
        initial_h: u32,
        current_w: u32,
        current_h: u32,
    },

    #[error("game window appears closed or minimized")]
    WindowGone,

    #[error(
        "stored game window handle is no longer valid — the game likely crashed \
         or was closed and reopened. Restart the bot to re-attach."
    )]
    WindowHandleInvalid,

    #[error("game window could not be brought to the foreground — another app is blocking focus")]
    WindowNotForeground,

    #[error("config file not found at {0}")]
    ConfigNotFound(PathBuf),

    #[error("config invalid: {0}")]
    ConfigInvalid(String),

    #[error("{0} consecutive rounds failed to refresh — bailing")]
    TooManyFailures(u32),

    #[error("template `{0}` not registered")]
    UnknownTemplate(String),

    #[error("xcap: {0}")]
    Xcap(#[from] xcap::XCapError),

    #[error("win32: {0}")]
    Win32(#[from] windows::core::Error),

    #[error("enigo input: {0}")]
    EnigoInput(#[from] enigo::InputError),

    #[error("enigo init: {0}")]
    EnigoNew(#[from] enigo::NewConError),

    #[error(transparent)]
    Image(#[from] image::ImageError),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("toml: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("ctrlc: {0}")]
    CtrlC(#[from] ctrlc::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
