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
pub struct SharedStats(Arc<Mutex<BotStats>>);

impl SharedStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> BotStats {
        self.0.lock().expect("stats mutex poisoned").clone()
    }

    pub fn update<F: FnOnce(&mut BotStats)>(&self, f: F) {
        let mut g = self.0.lock().expect("stats mutex poisoned");
        f(&mut g);
    }
}

impl ProgressSink for SharedStats {
    fn round_started(&self, round: u32, total: u32) {
        self.update(|s| {
            s.round = round;
            s.total_rounds = total;
            // Preserve Stopping across rounds — clobbering to Running
            // would flip the UI back to Running for one extra round.
            if s.status != BotStatus::Stopping {
                s.status = BotStatus::Running;
            }
        });
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

    fn finished(&self) {
        self.update(|s| {
            s.status = BotStatus::Finished;
            s.last_error = None;
        });
    }

    fn failed(&self, err: &str) {
        self.update(|s| {
            s.status = BotStatus::Failed;
            s.last_error = Some(err.to_string());
        });
    }
}
