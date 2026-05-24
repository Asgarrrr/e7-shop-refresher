use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};

use tracing::info;

use crate::capture::WindowCapture;
use crate::config::{Config, ShopConfig};
use crate::detector::Detector;
use crate::error::Result;
use crate::gui::state::{BotStatus, SharedStats};
use crate::input::Clicker;
use crate::shop::ShopRunner;

pub struct BotHandle {
    join: Option<JoinHandle<Result<()>>>,
    stop: Arc<AtomicBool>,
}

impl BotHandle {
    pub fn spawn(
        config: Config,
        live_shop: Arc<RwLock<ShopConfig>>,
        capture: Arc<WindowCapture>,
        detector: Arc<Detector>,
        stats: SharedStats,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_worker = Arc::clone(&stop);

        let stop_for_clicker = Arc::clone(&stop_for_worker);
        let join = thread::spawn(move || -> Result<()> {
            let clicker = Clicker::new(config.timing.clone(), stop_for_clicker)?;
            let mut runner = ShopRunner::new(
                capture,
                detector,
                Box::new(clicker),
                config,
                live_shop,
                stop_for_worker,
            )
            .with_progress(Arc::new(stats));
            runner.run()
        });

        Ok(Self {
            join: Some(join),
            stop,
        })
    }

    pub fn request_stop(&self) {
        info!("bot stop requested");
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn is_running(&self) -> bool {
        self.join.as_ref().is_some_and(|h| !h.is_finished())
    }

    pub fn poll(&mut self) -> Option<thread::Result<Result<()>>> {
        let handle = self.join.as_ref()?;
        if !handle.is_finished() {
            return None;
        }
        let handle = self.join.take()?;
        Some(handle.join())
    }
}

/// Fuses the live `BotHandle` with the latest `SharedStats` status so
/// the UI can distinguish "worker still alive" from "worker exited and
/// the sink hasn't been updated yet".
pub fn effective_status(handle: Option<&BotHandle>, stats_status: BotStatus) -> BotStatus {
    match handle {
        Some(h) if h.is_running() => {
            if stats_status == BotStatus::Idle {
                BotStatus::Running
            } else {
                stats_status
            }
        }
        _ => stats_status,
    }
}

impl Drop for BotHandle {
    fn drop(&mut self) {
        // Signal stop and block until the worker exits. Without the
        // join, closing the GUI window detached the worker, which kept
        // moving the mouse and clicking in-game — a concrete ban risk.
        // The cooperative cancellation path polls the stop flag every
        // ~60 ms so this resolves within a few hundred ms.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}
