//! The "Shop Watch" capture gate.
//!
//! While off, the captured stream is not forwarded. The gate is a projection
//! of the controller's status with a single writer — `app::apply` turns it on
//! for `Watching | Paused` — and is read lock-free by the capture thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
}
