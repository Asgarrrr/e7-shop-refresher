//! Bounded session journal: the same lines the console prints, kept for a
//! view. The session loop writes, readers copy entries out.
//!
//! The journal also owns the session clock (`now_ms`): domain events and
//! journal stamps read the same timeline by construction.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// One journal entry: a console line stamped with the session clock.
#[derive(Debug, Clone)]
pub struct LogLine {
    pub at_ms: u64,
    pub text: String,
}

/// Oldest entries drop out first: a session left running for hours must not
/// grow the journal without bound.
const JOURNAL_CAP: usize = 500;

#[derive(Clone)]
pub struct EventLog {
    epoch: Instant,
    /// Bumped on every push so readers can cache [`EventLog::entries`] and
    /// re-clone only when something actually changed.
    generation: Arc<AtomicU64>,
    entries: Arc<Mutex<VecDeque<LogLine>>>,
}

impl Default for EventLog {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
            generation: Arc::default(),
            entries: Arc::default(),
        }
    }
}

impl EventLog {
    /// Milliseconds since the journal was created — the session clock.
    pub fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Single sink for player-facing lines: the journal and the console stay
    /// in step by construction — never print session lines around it.
    pub fn emit(&self, lines: &[String]) {
        self.push(lines);
        for line in lines {
            println!("{line}");
        }
    }

    pub fn push(&self, lines: &[String]) {
        if lines.is_empty() {
            return;
        }
        let at_ms = self.now_ms();
        let mut entries = self.entries.lock().expect("journal mutex poisoned");
        for text in lines {
            entries.push_back(LogLine {
                at_ms,
                text: text.clone(),
            });
        }
        while entries.len() > JOURNAL_CAP {
            entries.pop_front();
        }
        drop(entries);
        self.generation.fetch_add(1, Ordering::Release);
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Poison-tolerant: the GUI reads this after a session panic and must
    /// still show the history that led there.
    pub fn entries(&self) -> Vec<LogLine> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_caps_entries() {
        let journal = EventLog::default();
        for i in 0..(JOURNAL_CAP + 100) {
            journal.push(&[format!("line {i}")]);
        }
        let entries = journal.entries();
        assert_eq!(entries.len(), JOURNAL_CAP);
        assert_eq!(entries.first().unwrap().text, "line 100");
        assert_eq!(
            entries.last().unwrap().text,
            format!("line {}", JOURNAL_CAP + 99)
        );
    }

    #[test]
    fn empty_push_is_ignored() {
        let journal = EventLog::default();
        journal.push(&[]);
        assert!(journal.entries().is_empty());
        assert_eq!(journal.generation(), 0);
    }

    #[test]
    fn timestamps_track_elapsed_time() {
        let journal = EventLog::default();
        journal.push(&["first".to_owned()]);
        std::thread::sleep(std::time::Duration::from_millis(30));
        journal.push(&["second".to_owned()]);
        let entries = journal.entries();
        assert!(entries[0].at_ms < 5_000, "first stamp sits near the epoch");
        assert!(
            entries[1].at_ms > entries[0].at_ms,
            "a later push carries a later stamp"
        );
    }

    #[test]
    fn generation_changes_on_push() {
        let journal = EventLog::default();
        let before = journal.generation();
        journal.push(&["line".to_owned()]);
        assert_ne!(journal.generation(), before);
    }
}
