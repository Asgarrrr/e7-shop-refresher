//! Configuration du relais, chargée depuis un fichier TOML (défauts sinon).

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::error::Result;

/// Port TCP du serveur de jeu Epic Seven (`msg://`).
pub const DEFAULT_GAME_PORT: u16 = 3333;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Port TCP du serveur de jeu, côté distant.
    pub game_port: u16,

    /// URL du serveur d'analyse (`ws://` ou `wss://`).
    pub server_url: String,

    /// Directions du flux à transmettre au serveur.
    pub forward: ForwardConfig,

    /// Politique de reconnexion à la liaison serveur.
    pub reconnect: ReconnectConfig,

    /// Réglages bas niveau de la capture.
    pub capture: CaptureConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ForwardConfig {
    /// Réponses serveur → client : contiennent le contenu du shop.
    pub server_to_client: bool,
    /// Requêtes client → serveur : contexte (commande émise), optionnel.
    pub client_to_server: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReconnectConfig {
    /// Délai initial avant nouvelle tentative (millisecondes).
    pub initial_ms: u64,
    /// Plafond du backoff exponentiel (millisecondes).
    pub max_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    /// Taille du tampon de réception d'un paquet WinDivert (octets).
    pub buffer_size: usize,
    /// Filtre WinDivert explicite ; sinon dérivé de `game_port` + `forward`.
    pub filter: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            game_port: DEFAULT_GAME_PORT,
            server_url: "ws://127.0.0.1:3001/refresh-shop".to_owned(),
            forward: ForwardConfig::default(),
            reconnect: ReconnectConfig::default(),
            capture: CaptureConfig::default(),
        }
    }
}

impl Default for ForwardConfig {
    fn default() -> Self {
        // Le contenu du shop vit dans les réponses serveur → client.
        Self {
            server_to_client: true,
            client_to_server: false,
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

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            buffer_size: 65_535,
            filter: None,
        }
    }
}

impl Config {
    /// Charge la configuration depuis `path`. Un fichier absent donne les défauts.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let config: Config = toml::from_str(&text)?;
                config.validate()?;
                Ok(config)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.into()),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.game_port == 0 {
            return Err(crate::Error::Config("game_port ne peut pas être 0".into()));
        }
        if !self.forward.server_to_client && !self.forward.client_to_server {
            return Err(crate::Error::Config(
                "au moins une direction doit être transmise (forward)".into(),
            ));
        }
        if self.server_url.trim().is_empty() {
            return Err(crate::Error::Config("server_url est vide".into()));
        }
        // La direction d'un segment est déduite en comparant ses ports à
        // `game_port` : un filtre personnalisé qui capture un autre port livre
        // du trafic que rien ne saura classer — zéro segment, sans erreur.
        if let Some(filter) = &self.capture.filter {
            if !filter.contains(&self.game_port.to_string()) {
                return Err(crate::Error::Config(format!(
                    "capture.filter ne référence pas game_port ({}) : aucun paquet ne serait classé",
                    self.game_port
                )));
            }
        }
        Ok(())
    }

    /// Filtre WinDivert effectif : uniquement les directions à transmettre.
    ///
    /// La réponse du shop transite serveur → client (`tcp.SrcPort == game_port`).
    pub fn capture_filter(&self) -> String {
        if let Some(filter) = &self.capture.filter {
            return filter.clone();
        }
        let mut clauses = Vec::new();
        if self.forward.server_to_client {
            clauses.push(format!("tcp.SrcPort == {}", self.game_port));
        }
        if self.forward.client_to_server {
            clauses.push(format!("tcp.DstPort == {}", self.game_port));
        }
        format!("tcp and ({})", clauses.join(" or "))
    }

    pub fn reconnect_initial(&self) -> Duration {
        Duration::from_millis(self.reconnect.initial_ms)
    }

    pub fn reconnect_max(&self) -> Duration {
        Duration::from_millis(self.reconnect.max_ms.max(self.reconnect.initial_ms))
    }
}
