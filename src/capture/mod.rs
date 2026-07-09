//! Capture passive du trafic et abstraction de la source de paquets.

mod ip;

#[cfg(all(windows, feature = "windivert-backend"))]
mod windivert;

use std::net::SocketAddr;

use crate::error::Result;

pub use ip::parse_segment;

#[cfg(all(windows, feature = "windivert-backend"))]
pub use windivert::WinDivertSource;

/// Sens d'un segment relativement au serveur de jeu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Requête du jeu vers le serveur.
    ClientToServer,
    /// Réponse du serveur vers le jeu — contient le contenu du shop.
    ServerToClient,
}

/// Identifie une connexion TCP indépendamment du sens observé.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FlowKey {
    pub client: SocketAddr,
    pub server: SocketAddr,
}

/// Un segment TCP capturé, normalisé et prêt pour le réassemblage.
#[derive(Debug, Clone)]
pub struct Segment {
    pub flow: FlowKey,
    pub direction: Direction,
    /// Numéro de séquence TCP du premier octet de `payload`.
    pub seq: u32,
    pub syn: bool,
    pub fin: bool,
    pub payload: Vec<u8>,
}

/// Source bloquante de segments TCP.
///
/// Chaque implémentation observe le trafic sans jamais le modifier.
pub trait PacketSource: Send {
    /// Bloque jusqu'au prochain segment TCP correspondant au filtre.
    fn next_segment(&mut self) -> Result<Segment>;
}
