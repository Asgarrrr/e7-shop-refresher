//! The shared cell holding the filter vocabulary the server pushed.
//!
//! Shell state, deliberately not domain state: the vocabulary decides nothing.
//! [`crate::domain::filter::Filter`] matches on the raw ids a shop payload
//! carries, so this only ever changes what the editor can OFFER. Putting it on
//! `Controller` would put a list of words inside a refresh-loop state machine.
//!
//! One writer (the session loop, on a `catalog` message) and one reader (the
//! egui thread, once per frame), so a `Mutex` behind an `Arc` is enough — the
//! same short-lock discipline `SessionHandles::controller` already follows.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::uplink::protocol::FilterVocabulary;

/// A clonable handle on the vocabulary. Empty until a server sends one, and
/// empty forever against a server that has no Catalog to read.
#[derive(Clone, Default)]
pub struct VocabularyCell(Arc<Shared>);

#[derive(Default)]
struct Shared {
    vocabulary: Mutex<FilterVocabulary>,
    /// Bumped on every write, so a view can tell "unchanged" from "changed
    /// back" without comparing the lists. The window redraws at 60fps and the
    /// vocabulary arrives once per session, so a per-frame `get()` would clone
    /// forty-odd strings sixty times a second to learn nothing.
    generation: AtomicU64,
}

impl VocabularyCell {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the vocabulary wholesale. A later message wins outright rather
    /// than merging: a partial merge would leave a set the game removed sitting
    /// in the picker with nothing behind it.
    pub fn set(&self, vocabulary: FilterVocabulary) {
        *self.lock() = vocabulary;
        // Released after the write, so a reader that sees this generation sees
        // the vocabulary that produced it.
        self.0.generation.fetch_add(1, Ordering::Release);
    }

    /// How many times the vocabulary has been replaced. `0` until the first
    /// message, which is also a value no write can produce.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.0.generation.load(Ordering::Acquire)
    }

    /// A snapshot for the frame about to be drawn. Clones under the lock so a
    /// view never holds it across a redraw.
    #[must_use]
    pub fn get(&self) -> FilterVocabulary {
        self.lock().clone()
    }

    /// A poisoned lock is recovered from rather than propagated: the guarded
    /// value is a plain list of words with no invariant to violate, and the one
    /// writer replaces it whole. Panicking here would take down a window over
    /// a picker's contents.
    fn lock(&self) -> std::sync::MutexGuard<'_, FilterVocabulary> {
        self.0
            .vocabulary
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl std::fmt::Debug for VocabularyCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let vocabulary = self.lock();
        f.debug_struct("VocabularyCell")
            .field("sets", &vocabulary.sets.len())
            .field("substats", &vocabulary.substats.len())
            .field("slots", &vocabulary.slots.len())
            .field("tokens", &vocabulary.tokens.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uplink::protocol::VocabularyEntry;

    fn entry(id: &str) -> VocabularyEntry {
        VocabularyEntry {
            id: id.to_owned(),
            label: id.to_owned(),
            percent: false,
        }
    }

    /// Empty until a server sends one, and the editor's fallback to free-text
    /// entry hangs off exactly this: it tests the LIST it is about to draw
    /// (`offered_list`, `token_cards`), never the cell as a whole, because a
    /// catalog can name the sets and no tokens.
    #[test]
    fn starts_empty() {
        assert_eq!(VocabularyCell::new().get(), FilterVocabulary::default());
    }

    #[test]
    fn a_received_vocabulary_is_readable() {
        let cell = VocabularyCell::new();
        cell.set(FilterVocabulary {
            sets: vec![entry("set_speed")],
            ..FilterVocabulary::default()
        });
        assert_eq!(cell.get().sets, vec![entry("set_speed")]);
    }

    #[test]
    fn a_later_message_replaces_rather_than_merges() {
        let cell = VocabularyCell::new();
        cell.set(FilterVocabulary {
            sets: vec![entry("set_speed"), entry("set_retired")],
            ..FilterVocabulary::default()
        });
        cell.set(FilterVocabulary {
            sets: vec![entry("set_speed")],
            ..FilterVocabulary::default()
        });
        assert_eq!(cell.get().sets, vec![entry("set_speed")]);
    }

    #[test]
    fn the_generation_starts_at_zero_and_moves_only_on_a_write() {
        let cell = VocabularyCell::new();
        assert_eq!(cell.generation(), 0);
        let _ = cell.get();
        assert_eq!(cell.generation(), 0, "reading is not a change");
        cell.set(FilterVocabulary::default());
        assert_eq!(cell.generation(), 1);
    }

    #[test]
    fn an_identical_vocabulary_still_bumps_the_generation() {
        // The counter answers "was it written", not "did it differ" — a view
        // re-reads and finds the same lists, which costs one clone and no
        // correctness. Comparing instead would mean holding a second copy to
        // compare against, which is the allocation this exists to avoid.
        let cell = VocabularyCell::new();
        cell.set(FilterVocabulary::default());
        cell.set(FilterVocabulary::default());
        assert_eq!(cell.generation(), 2);
    }

    #[test]
    fn clones_share_one_cell() {
        let cell = VocabularyCell::new();
        let clone = cell.clone();
        cell.set(FilterVocabulary {
            slots: vec![entry("helm")],
            ..FilterVocabulary::default()
        });
        assert_eq!(clone.get().slots, vec![entry("helm")]);
    }
}
