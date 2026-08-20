//! The crate's one poison-tolerant lock.
//!
//! # Why every `Mutex` in this crate is read this way
//!
//! `unwrap` on a poisoned `Mutex` turns one thread's fault into every later
//! caller's. This app cannot afford that: a panic on any of its five shared
//! mutexes took the *window* down with it — the one thread that could have told
//! the player what happened, because drawing the banner needed the poisoned lock.
//!
//! So: keep the data, keep rendering, let the banner and journal report the
//! panic. That is only sound given a property each caller must check for itself
//! — that its guarded state cannot be left half-written by an unwinding thread —
//! and **that argument belongs at the call site, not here**. `Controller::handle`
//! is pure and saturating; `Timings` is `Copy`; the journal's deque is only ever
//! pushed to. A caller whose state has an invariant spanning two writes does not
//! get to use this function without saying why it is safe.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks `mutex`, taking the data back if a panicking thread poisoned it.
///
/// See the module header for when this is the right call and for the obligation
/// it puts on the caller.
pub fn lock_ignoring_poison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_lock_poisoned_by_a_panic_still_yields_its_data() {
        let shared = Arc::new(Mutex::new(vec![1_u32]));
        let poisoner = Arc::clone(&shared);
        let panicked = std::thread::spawn(move || {
            let mut guard = lock_ignoring_poison(&poisoner);
            guard.push(2);
            panic!("this thread dies holding the lock");
        })
        .join();
        assert!(panicked.is_err(), "the fixture must actually panic");
        assert!(
            shared.is_poisoned(),
            "and must actually poison the mutex, or this proves nothing"
        );

        assert_eq!(
            *lock_ignoring_poison(&shared),
            vec![1, 2],
            "the write made before the panic must survive"
        );
    }
}
