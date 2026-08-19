//! The one *per-process* precondition both backends establish before they touch
//! the game window: that this process is per-monitor DPI aware.
//!
//! Its own file because it is the only thing under `win` that is about the
//! process rather than about the window, and because the verdict is a pure
//! function of an awareness value — which is what lets the wording and the
//! classification be tested with no Win32 anywhere.
//! [`ensure_dpi_awareness`] is what both `acquire`s call first.

use std::sync::OnceLock;

use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, DPI_AWARENESS_INVALID,
    DPI_AWARENESS_PER_MONITOR_AWARE, DPI_AWARENESS_SYSTEM_AWARE, DPI_AWARENESS_UNAWARE,
    GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
    SetProcessDpiAwarenessContext,
};

use crate::actuator::SurfaceError;

/// Names an awareness value for the log line and the refusal.
fn awareness_name(awareness: DPI_AWARENESS) -> &'static str {
    match awareness {
        DPI_AWARENESS_UNAWARE => "unaware",
        DPI_AWARENESS_SYSTEM_AWARE => "system-aware",
        DPI_AWARENESS_PER_MONITOR_AWARE => "per-monitor-aware",
        DPI_AWARENESS_INVALID => "invalid",
        _ => "unrecognized",
    }
}

/// The verdict on an awareness value: `Ok(())` only for per-monitor awareness.
///
/// Pure, so the wording and the classification can be tested without any Win32
/// — the same split `preflight_refusal` uses.
fn awareness_verdict(awareness: DPI_AWARENESS) -> Result<(), SurfaceError> {
    if awareness == DPI_AWARENESS_PER_MONITOR_AWARE {
        return Ok(());
    }
    Err(SurfaceError::Fatal(format!(
        "this process is DPI {} rather than per-monitor-aware, so Windows reports the game \
         window's size in virtualized pixels and every click would be planned at the wrong \
         place — check for a \"Override high DPI scaling behavior\" compatibility setting on \
         this app's exe, or a __COMPAT_LAYER environment variable, and remove it",
        awareness_name(awareness)
    )))
}

/// Establishes — once per process — that this process is *per-monitor* DPI aware,
/// which is what makes every client rect below come back in physical pixels.
///
/// # Why this is checked rather than assumed
///
/// The whole coordinate chain is physical-pixel arithmetic: `client_rect` reads
/// `GetClientRect` + `ClientToScreen`, `plan::to_screen` scales design-space
/// points against that rect, and `move_cursor` normalizes the result against
/// `SM_CXVIRTUALSCREEN`, which is always physical. A DPI-unaware or system-aware
/// process gets *virtualized* rects on a scaled display, so every planned point
/// is off by the scale factor and the clicks land on the wrong buttons — and
/// nothing reports it, because `SendInput` is documented not to signal that kind
/// of failure at all (see [`sendinput_result`]) and `PostMessageW` cheerfully
/// posts a well-formed message to a wrong coordinate.
///
/// This code used to call `SetProcessDpiAwarenessContext` and drop the return
/// with a bare semicolon, on the argument that "a failed call means it was
/// already set, which is what we want". That conflated *already set* with *set
/// to what we want*: the call answers `FALSE` for **any** already-set awareness,
/// `UNAWARE` and `SYSTEM_AWARE` included. And in the shipped GUI build winit sets
/// the awareness before the actuator's first `acquire()` ever runs, so the call
/// *always* failed — meaning the entire click chain rested on winit's
/// undocumented choice, verified nowhere. A compatibility shim, a
/// `__COMPAT_LAYER` variable or a future winit is enough to change that choice.
///
/// The winit half of that is now settled somewhere this function cannot reach:
/// `build.rs` declares `dpiAwareness = permonitorv2, permonitor` in the embedded
/// application manifest, which the loader applies *before any code runs*, so the
/// value is the product's rather than a dependency's. Measured on the built exe:
/// awareness is already per-monitor at process entry, and every later setter —
/// ours, winit's v2 attempt and its v1 fallback — fails with
/// `ERROR_ACCESS_DENIED` without changing it. The setter below therefore still
/// always fails in the GUI build, but now because *we* got there first.
///
/// What the manifest cannot outrank, and why this check stays: a `__COMPAT_LAYER`
/// shim is applied over it. `__COMPAT_LAYER=DPIUNAWARE` on a manifested build was
/// measured landing at `unaware` — the exact case the refusal below names.
///
/// So the answer comes from reading the context back, not from the setter: on the
/// success path because a set value can still be re-read, and on the failure path
/// because that is the only way to learn *whose* value won. A mis-aimed click is
/// worse than no click, so anything but per-monitor awareness refuses the acquire
/// with a `Fatal` the player can read, in the same voice as the UIPI preflight.
///
/// # Errors
///
/// [`SurfaceError::Fatal`] when the effective awareness is anything other than
/// per-monitor.
///
/// [`sendinput_result`]: super::send_input::sendinput_result
pub(super) fn ensure_dpi_awareness() -> Result<(), SurfaceError> {
    static DPI: OnceLock<Result<(), SurfaceError>> = OnceLock::new();
    DPI.get_or_init(|| {
        // SAFETY: the argument is a well-known Win32 constant and the call only
        // flips process-global DPI state — it borrows nothing and hands back
        // nothing to keep alive. A zero answer means *some* awareness was
        // already set, which is why the value is read back below rather than
        // inferred from this return.
        let set =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        // Read before the two getters below, per the rule this module states at
        // three other Win32 call sites (`find_game_window` and
        // `probe_window_reachable` in the root, `post` in `post_message`):
        // `GetLastError` is per-thread and *any* later call may overwrite it.
        // Neither getter is documented to set it,
        // which is exactly why reading it after them was not safe to rely on —
        // "not documented to write the slot" is not "documented not to". The
        // embedded manifest has already set the awareness on *every* build of
        // this exe, so this branch fires on every launch and this value is the
        // difference between a reproducible bug report and "the clicks miss
        // sometimes".
        let set_error = (set == 0).then(std::io::Error::last_os_error);
        // One Win32 call per block, so each `// SAFETY:` answers for exactly the
        // call above it.
        //
        // SAFETY: takes no argument, returns an opaque process-global token, and
        // borrows nothing.
        let context = unsafe { GetThreadDpiAwarenessContext() };
        // SAFETY: `context` is the token the previous call just produced, and
        // this is its only documented consumer; it takes no pointer and no
        // handle, and answers `DPI_AWARENESS_INVALID` for anything it does not
        // recognize.
        let awareness = unsafe { GetAwarenessFromDpiAwarenessContext(context) };
        let verdict = awareness_verdict(awareness);
        // `Some` exactly when the setter refused, so this is the same branch the
        // bare `set == 0` used to spell — with the error it names captured back
        // when it still belonged to that call.
        if let Some(error) = set_error {
            // Since the manifest declares the awareness, this branch is the
            // *expected* path, not a surprise: the loader set the value before
            // any code ran, so our setter was always going to be refused
            // (`ERROR_ACCESS_DENIED`, measured). The message says so, because a
            // line that reads like an anomaly on every single launch is a line
            // people learn to skip — and the one launch where `accepted` is
            // `false` is the one that matters. That case means something
            // outranked the manifest: a `__COMPAT_LAYER` shim, or the
            // "Override high DPI scaling behavior" checkbox.
            tracing::info!(
                awareness = awareness_name(awareness),
                accepted = verdict.is_ok(),
                error = %error,
                "the DPI awareness was already established before the actuator asked — \
                 expected, the manifest sets it; `accepted=false` means something outranked it"
            );
        }
        verdict
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The finding this replaces: the old code dropped
    /// `SetProcessDpiAwarenessContext`'s return on the argument that a failure
    /// means "already set, which is what we want" — but the call answers FALSE
    /// for *any* already-set awareness. Only per-monitor gives physical-pixel
    /// client rects, which is what every coordinate below assumes.
    #[test]
    fn only_per_monitor_awareness_is_accepted_for_the_coordinate_maths() {
        assert_eq!(awareness_verdict(DPI_AWARENESS_PER_MONITOR_AWARE), Ok(()));
        for (awareness, name) in [
            (DPI_AWARENESS_UNAWARE, "unaware"),
            (DPI_AWARENESS_SYSTEM_AWARE, "system-aware"),
            (DPI_AWARENESS_INVALID, "invalid"),
            // A future Windows value must refuse too, not fall through.
            (99, "unrecognized"),
        ] {
            let Err(SurfaceError::Fatal(reason)) = awareness_verdict(awareness) else {
                panic!("{name} awareness must refuse: a mis-aimed click is worse than none");
            };
            assert!(reason.contains(name), "{reason}");
            // The player is pointed at something they can actually change.
            assert!(reason.contains("compatibility setting"), "{reason}");
            assert!(reason.contains("__COMPAT_LAYER"), "{reason}");
        }
    }

    #[test]
    fn a_refused_awareness_is_named_rather_than_reported_as_a_raw_number() {
        assert_eq!(awareness_name(DPI_AWARENESS_UNAWARE), "unaware");
        assert_eq!(awareness_name(DPI_AWARENESS_SYSTEM_AWARE), "system-aware");
        assert_eq!(
            awareness_name(DPI_AWARENESS_PER_MONITOR_AWARE),
            "per-monitor-aware"
        );
        assert_eq!(awareness_name(DPI_AWARENESS_INVALID), "invalid");
        assert_eq!(awareness_name(7), "unrecognized");
    }
}
