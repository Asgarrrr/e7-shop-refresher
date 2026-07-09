//! Type d'erreur unifié du relais.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("configuration invalide : {0}")]
    Config(String),

    #[error("lecture de la configuration : {0}")]
    ConfigParse(#[from] toml::de::Error),

    #[error("capture réseau : {0}")]
    Capture(String),

    #[error("liaison serveur : {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("(dé)sérialisation JSON : {0}")]
    Json(#[from] serde_json::Error),

    #[error("entrée/sortie : {0}")]
    Io(#[from] std::io::Error),
}
