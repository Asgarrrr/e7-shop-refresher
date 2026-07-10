//! Bounded session journal: the same lines the console prints, kept for a
//! view. The session loop writes, readers copy entries out.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// One journal entry: a console line with its session-relative timestamp.
#[derive(Debug, Clone)]
pub struct LogLine {
    pub at_ms: u64,
    pub text: String,
}

/// Oldest entries drop out first: a session left running for hours must not
/// grow the journal without bound.
const JOURNAL_CAP: usize = 500;

#[derive(Clone, Default)]
pub struct EventLog {
    entries: Arc<Mutex<VecDeque<LogLine>>>,
}

impl EventLog {
    pub fn push(&self, at_ms: u64, lines: &[String]) {
        if lines.is_empty() {
            return;
        }
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
        for i in 0..(JOURNAL_CAP as u64 + 100) {
            journal.push(i, &[format!("line {i}")]);
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
        journal.push(0, &[]);
        assert!(journal.entries().is_empty());
    }
}
