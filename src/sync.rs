//! The crate's one poison-tolerant lock.
//!
//! # Why every `Mutex` in this crate is read this way
//!
//! A `Mutex` is poisoned when a thread panics holding it, and the default
//! response — `unwrap`, or `expect("… poisoned")` — turns one thread's fault
//! into every later caller's fault. This app cannot afford that. Its threads are
//! a session loop, an actuator executor, a watchdog, a capture funnel and an
//! egui window at 4 Hz, and they share five mutexes between them. A panic on one
//! used to take the *window* down with it, which is the one thread that could
//! have told the player what happened: `supervise` reported "session crashed",
//! the relay stopped, and the banner that names the crash was never drawn
//! because drawing it needed the poisoned lock.
//!
//! So the policy is: keep the data, keep rendering, and let the banner and the
//! journal report the panic. That is only sound because of a property each
//! caller has to check for itself — that its guarded state cannot be left
//! half-written by an unwinding thread — and **that argument belongs at the call
//! site, not here**. `Controller::handle` is pure and saturating; `Timings` is
//! `Copy` and copied straight out; the journal's deque is only ever pushed to.
//! A future caller whose state has an invariant spanning two writes does not get
//! to use this function without saying why it is safe.
//!
//! This lived as three near-identical private helpers plus five inline
//! `unwrap_or_else` calls, and each of the helpers' comments recited the policy
//! and listed the other sites — so adding a sixth meant editing five comments,
//! and they had already drifted out of agreement about which sites existed.

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
        // The whole point, asserted rather than described: after a thread
        // panics holding the guard, the next reader gets the value it wrote,
        // not a second panic.
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
