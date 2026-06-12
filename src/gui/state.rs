use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::shop::ProgressSink;

#[derive(Debug, Clone, Default)]
pub struct BotStats {
    pub status: BotStatus,
    pub round: u32,
    pub total_rounds: u32,
    pub items_bought: u32,
    pub mystic_bought: u32,
    pub covenant_bought: u32,
    pub last_error: Option<String>,
    /// Set by the worker after `suspend_to_sleep()` returns; consumed
    /// by the GUI to give `sleep_when_done` one-shot semantics.
    pub sleep_consumed: bool,
    pub sub_status: Option<String>,
    /// `Some("stop_when_…")` when a configured goal fired; consumed by
    /// the GUI to dispatch the Discord webhook exactly once.
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BotStatus {
    #[default]
    Idle,
    Running,
    Stopping,
    Finished,
    Failed,
}

impl BotStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Running => "Running",
            Self::Stopping => "Stopping…",
            Self::Finished => "Finished",
            Self::Failed => "Failed",
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Running | Self::Stopping)
    }
}

#[derive(Debug, Clone, Default)]
pub struct SharedStats {
    inner: Arc<Mutex<BotStats>>,
    poison_logged: Arc<AtomicBool>,
}

impl SharedStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> BotStats {
        match self.inner.lock() {
            Ok(g) => g.clone(),
            Err(e) => {
                self.report_poison();
                let mut s = e.into_inner().clone();
                s.status = BotStatus::Failed;
                s.last_error
                    .get_or_insert_with(|| "worker panicked mid-update".to_string());
                s
            }
        }
    }

    pub fn update<F: FnOnce(&mut BotStats)>(&self, f: F) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => {
                self.report_poison();
                let mut g = e.into_inner();
                g.status = BotStatus::Failed;
                g.last_error
                    .get_or_insert_with(|| "worker panicked mid-update".to_string());
                g
            }
        };
        f(&mut g);
    }

    fn report_poison(&self) {
        if !self.poison_logged.swap(true, Ordering::Relaxed) {
            tracing::error!("BotStats mutex was poisoned — a worker thread panicked");
        }
    }
}

impl ProgressSink for SharedStats {
    fn round_started(&self, _round: u32, total: u32) {
        self.update(|s| {
            s.total_rounds = total;
            // Don't override Stopping — would flip the UI back to Running
            // for the round-in-flight after the user clicked Stop.
            if s.status != BotStatus::Stopping {
                s.status = BotStatus::Running;
            }
        });
    }

    fn round_finished(&self, round: u32, _bought: u32) {
        // `round` is `refreshes_done` AFTER the successful refresh —
        // the count the UI should display. Updating here (not in
        // `round_started`) keeps the displayed count == completed
        // refreshes rather than "in-flight round - 1".
        self.update(|s| s.round = round);
    }

    fn item_bought(&self, alias: &str) {
        self.update(|s| {
            s.items_bought += 1;
            match alias {
                crate::detector::alias::MYSTIC_MEDAL => s.mystic_bought += 1,
                crate::detector::alias::COVENANT => s.covenant_bought += 1,
                _ => {}
            }
        });
    }

    fn bought_count(&self, alias: &str) -> u32 {
        let s = self.snapshot();
        match alias {
            crate::detector::alias::MYSTIC_MEDAL => s.mystic_bought,
            crate::detector::alias::COVENANT => s.covenant_bought,
            _ => 0,
        }
    }

    fn finished(&self, reason: Option<&str>) {
        let reason = reason.map(str::to_string);
        self.update(|s| {
            s.status = BotStatus::Finished;
            s.last_error = None;
            s.sub_status = None;
            s.finish_reason = reason;
        });
    }

    fn failed(&self, err: &str) {
        self.update(|s| {
            s.status = BotStatus::Failed;
            s.last_error = Some(err.to_string());
            s.sub_status = None;
            // A previous goal-driven finish may have left a reason here. Clear it
            // so consume_finish_reason doesn't fire the OLD run's webhook now
            // that the new run actually failed.
            s.finish_reason = None;
        });
    }

    fn sleep_consumed(&self) {
        self.update(|s| s.sleep_consumed = true);
    }

    fn sub_status(&self, text: Option<&str>) {
        let text = text.map(str::to_string);
        self.update(|s| s.sub_status = text);
    }
}
