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

/// The crate's single sink for player-facing lines: several writers (the
/// session loop, the actuator executor, the watchdog) append, one reader (the
/// window) copies entries out. Cloning it is another handle on the same
/// journal, not another journal.
#[derive(Clone)]
pub struct EventLog {
    epoch: Instant,
    /// Bumped on every push so readers can cache [`EventLog::entries`] and
    /// re-clone only when something actually changed.
    ///
    /// A change *hint*, not a publication, so both sides are `Relaxed`:
    /// [`EventLog::entries`] takes the same `Mutex` as `push`, and that
    /// unlock/lock pair is already the happens-before edge — a stronger one
    /// than an atomic gives. A `Relaxed` load that returns a stale value costs
    /// one frame of a 4 Hz repaint, which is exactly what `Acquire` would cost
    /// too: an `Acquire` load carries no freshness guarantee either.
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

    /// Single sink for player-facing lines: the journal, the console and the
    /// log file stay in step by construction — never print session lines
    /// around it.
    ///
    /// The mirror to `tracing` is what makes these lines survive the process:
    /// the in-memory ring dies with it, and the windowed build has no console
    /// at all. `target: "journal"` keeps the player view isolable
    /// (`RUST_LOG=journal=info`) while a single file interleaves technical and
    /// player events chronologically.
    ///
    /// Records at `INFO`. A line that reports a failure — an actuator halt, an
    /// aborted session, a config write the OS refused — belongs at
    /// [`EventLog::emit_at`] instead, so the file stays triageable by level and
    /// not only by prose.
    pub fn emit(&self, lines: &[String]) {
        self.emit_at(tracing::Level::INFO, lines);
    }

    /// [`EventLog::emit`] with a severity for the log-file half.
    ///
    /// The player reads the same text either way; the level is for whoever
    /// reads the file afterwards. Without it every line lands at `INFO`, so
    /// narrowing `RUST_LOG` to `warn` deletes precisely the lines that say what
    /// went wrong.
    pub fn emit_at(&self, level: tracing::Level, lines: &[String]) {
        self.push(lines);
        for line in lines {
            // A callsite's level is part of `tracing`'s static metadata, so a
            // runtime level cannot be handed to one macro — the three the
            // journal has a use for are spelled out.
            if level == tracing::Level::ERROR {
                tracing::error!(target: "journal", line, "journal");
            } else if level == tracing::Level::WARN {
                tracing::warn!(target: "journal", line, "journal");
            } else {
                tracing::info!(target: "journal", line, "journal");
            }
            // The windowed build's stdout is an inert sink; only the console
            // build has a reader.
            #[cfg(not(feature = "gui"))]
            println!("{line}");
        }
    }

    /// Ring only: the line reaches the window and **nothing else** — no
    /// `tracing` event, so no log file, so no record at all once the process is
    /// gone.
    ///
    /// This is not the cheap `emit`, it is the forgetful one. Reserve it for
    /// lines whose entire audience is the player looking at the window right
    /// now; anything a support engineer would later go looking for must go
    /// through [`EventLog::emit`] or [`EventLog::emit_at`].
    pub fn push(&self, lines: &[String]) {
        if lines.is_empty() {
            return;
        }
        let at_ms = self.now_ms();
        // Poison-tolerant like `entries`: `emit` is called from the session
        // loop, the actuator executor and the watchdog, so panicking here
        // after one poisoning would cascade across tasks and freeze the very
        // history the GUI is meant to still show.
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for text in lines {
            entries.push_back(LogLine {
                at_ms,
                text: text.clone(),
            });
        }
        while entries.len() > JOURNAL_CAP {
            entries.pop_front();
        }
        // Bumped outside the lock: right for its own reason (never hold the
        // lock longer than needed), not because the ordering needs it — see the
        // `generation` field.
        drop(entries);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
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
    fn emit_at_records_every_level_in_the_ring() {
        // The level only steers the `tracing` half; the player-facing ring must
        // hold the line whatever the severity, or a failure would be visible in
        // the log file and invisible in the window.
        let journal = EventLog::default();
        for level in [
            tracing::Level::ERROR,
            tracing::Level::WARN,
            tracing::Level::INFO,
        ] {
            journal.emit_at(level, &[format!("{level}")]);
        }
        let texts: Vec<String> = journal
            .entries()
            .into_iter()
            .map(|line| line.text)
            .collect();
        assert_eq!(texts, ["ERROR", "WARN", "INFO"]);
    }

    #[test]
    fn generation_changes_on_push() {
        let journal = EventLog::default();
        let before = journal.generation();
        journal.push(&["line".to_owned()]);
        assert_ne!(journal.generation(), before);
    }
}
