//! Bounded session journal: the same lines the console prints, kept for a
//! view; the session loop writes, readers copy entries out. Also owns the
//! session clock (`now_ms`), so domain events and journal stamps share it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// One journal entry: a console line stamped with the session clock.
///
/// `text` is `Arc<str>`: written once by [`EventLog::push`], copied out
/// wholesale up to four times a second by [`EventLog::to_entries`] for the
/// GUI's repaint of text that never mutates. A `String` would cost up to
/// [`JOURNAL_CAP`] allocations and memcpys per repaint; `Arc<str>` costs
/// refcount bumps instead, `push` pays the one real allocation, and it skips
/// `Arc<String>`'s second indirection to the bytes.
#[derive(Debug, Clone)]
pub struct LogLine {
    pub at_ms: u64,
    pub text: Arc<str>,
}

/// Oldest entries drop out first: a session left running for hours must not
/// grow the journal without bound.
const JOURNAL_CAP: usize = 500;

/// Several writers (session loop, actuator executor, watchdog) append; one
/// reader (the window) copies entries out. Clone for another handle on the
/// same journal.
#[derive(Clone)]
pub struct EventLog {
    epoch: Instant,
    /// Bumped on every push so readers can cache [`EventLog::entries`] and
    /// re-clone only when something changed. Both sides use `Relaxed`: the
    /// `Mutex` shared with `push` already gives the happens-before edge. A
    /// stale read costs one frame of a 4 Hz repaint at worst — no worse than
    /// `Acquire`, which gives no freshness guarantee either.
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

    /// Single sink for player-facing lines: never print session lines around
    /// it. Mirrors to `tracing` so lines survive the process (the ring dies
    /// with it; the windowed build has no console) under `target: "journal"`
    /// (`RUST_LOG=journal=info` isolates the player view). Records at `INFO`;
    /// failures (actuator halt, aborted session, a refused config write)
    /// belong at [`EventLog::emit_at`] instead, for level-based triage.
    pub fn emit(&self, lines: &[String]) {
        self.emit_at(tracing::Level::INFO, lines);
    }

    /// [`EventLog::emit`] with a severity for the log-file half. The player
    /// reads the same text either way; the level is only for whoever reads
    /// the file. Without it, narrowing `RUST_LOG` to `warn` would delete the
    /// lines that say what went wrong.
    pub fn emit_at(&self, level: tracing::Level, lines: &[String]) {
        self.push(lines);
        for line in lines {
            // A callsite's level is static in `tracing`'s macros, so a runtime
            // level can't be passed to one macro — hence the three-way match.
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

    /// Ring only — no `tracing` event, no log file, no record once the
    /// process ends. Reserve for lines whose entire audience is the player
    /// watching right now; anything worth later triage needs
    /// [`EventLog::emit`] or [`EventLog::emit_at`] instead.
    pub fn push(&self, lines: &[String]) {
        if lines.is_empty() {
            return;
        }
        let at_ms = self.now_ms();
        // Poison-tolerant like `to_entries`: `emit` runs on the session loop,
        // the actuator executor and the watchdog, so panicking here would
        // cascade and freeze the GUI's history. `crate::sync`'s obligation is
        // discharged by the deque only ever being pushed to and read whole.
        let mut entries = crate::sync::lock_ignoring_poison(&self.entries);
        for text in lines {
            entries.push_back(LogLine {
                at_ms,
                // One allocation here, same as `text.clone()`; buys cheap
                // copies later — see `LogLine`.
                text: Arc::from(text.as_str()),
            });
        }
        while entries.len() > JOURNAL_CAP {
            entries.pop_front();
        }
        // Bumped outside the lock to keep it short — see the `generation` field.
        drop(entries);
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Copies the whole ring out — up to [`JOURNAL_CAP`] `LogLine`s. The `to_`
    /// prefix is the warning: this is not a getter, and calling it per frame
    /// is what made the GUI add [`EventLog::generation`] and a cache in front
    /// of it. No longer a deep copy — [`LogLine::text`] is `Arc<str>`, so each
    /// entry costs a refcount bump, not a fresh `String`; the lock, the `Vec`,
    /// and 500 bumps are still real work 4 times a second, hence the cache.
    /// Poison-tolerant, so the GUI can still show history after a panic.
    pub fn to_entries(&self) -> Vec<LogLine> {
        crate::sync::lock_ignoring_poison(&self.entries)
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
        let entries = journal.to_entries();
        assert_eq!(entries.len(), JOURNAL_CAP);
        assert_eq!(&*entries.first().unwrap().text, "line 100");
        assert_eq!(
            &*entries.last().unwrap().text,
            format!("line {}", JOURNAL_CAP + 99)
        );
    }

    #[test]
    fn empty_push_is_ignored() {
        let journal = EventLog::default();
        journal.push(&[]);
        assert!(journal.to_entries().is_empty());
        assert_eq!(journal.generation(), 0);
    }

    #[test]
    fn timestamps_track_elapsed_time() {
        let journal = EventLog::default();
        journal.push(&["first".to_owned()]);
        std::thread::sleep(std::time::Duration::from_millis(30));
        journal.push(&["second".to_owned()]);
        let entries = journal.to_entries();
        assert!(entries[0].at_ms < 5_000, "first stamp sits near the epoch");
        assert!(
            entries[1].at_ms > entries[0].at_ms,
            "a later push carries a later stamp"
        );
    }

    #[test]
    fn emit_at_records_every_level_in_the_ring() {
        // The level only steers the `tracing` half; the ring must hold the
        // line regardless, or a failure would show in the log but not the window.
        let journal = EventLog::default();
        for level in [
            tracing::Level::ERROR,
            tracing::Level::WARN,
            tracing::Level::INFO,
        ] {
            journal.emit_at(level, &[format!("{level}")]);
        }
        let texts: Vec<String> = journal
            .to_entries()
            .into_iter()
            .map(|line| line.text.to_string())
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
