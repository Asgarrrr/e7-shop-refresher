//! The "Shop Watch" switch.
//!
//! While off, the captured stream is not forwarded: the player turns it on when
//! opening the shop and off when done. Shared lock-free between the capture
//! thread and the interactive control.

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

    /// Flips the state and returns the new value.
    pub fn toggle(&self) -> bool {
        !self.enabled.fetch_xor(true, Ordering::Relaxed)
    }
}

impl Default for WatchGate {
    fn default() -> Self {
        Self::new(true)
    }
}
