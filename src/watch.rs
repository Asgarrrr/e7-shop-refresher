//! Interrupteur « Shop Watch ».
//!
//! Quand il est éteint, le flux capturé n'est pas transmis au serveur : le
//! joueur l'active en ouvrant le shop, l'éteint quand il a terminé. Partagé
//! sans verrou entre le thread de capture et le contrôle interactif.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone)]
pub struct WatchGate {
    enabled: Arc<AtomicBool>,
}

impl WatchGate {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(enabled)),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// Bascule l'état et renvoie la nouvelle valeur.
    pub fn toggle(&self) -> bool {
        !self.enabled.fetch_xor(true, Ordering::Relaxed)
    }
}

impl Default for WatchGate {
    fn default() -> Self {
        Self::new(true)
    }
}
