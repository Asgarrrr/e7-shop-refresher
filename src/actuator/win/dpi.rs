//! The one *per-process* precondition both backends establish before they touch
//! the game window: that this process is per-monitor DPI aware. Its own file
//! because it is the only thing under `win` about the process, not the window.

use std::sync::OnceLock;

use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, DPI_AWARENESS_INVALID,
    DPI_AWARENESS_PER_MONITOR_AWARE, DPI_AWARENESS_SYSTEM_AWARE, DPI_AWARENESS_UNAWARE,
    GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
    SetProcessDpiAwarenessContext,
};

use crate::actuator::SurfaceError;

fn awareness_name(awareness: DPI_AWARENESS) -> &'static str {
    match awareness {
        DPI_AWARENESS_UNAWARE => "unaware",
        DPI_AWARENESS_SYSTEM_AWARE => "system-aware",
        DPI_AWARENESS_PER_MONITOR_AWARE => "per-monitor-aware",
        DPI_AWARENESS_INVALID => "invalid",
        _ => "unrecognized",
    }
}

/// `Ok(())` only for per-monitor awareness. Pure, so wording and classification
/// are testable with no Win32 — the same split `preflight_refusal` uses.
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

/// Establishes, once per process, the *per-monitor* DPI awareness that makes
/// every client rect below come back in physical pixels.
///
/// A DPI-unaware or system-aware process gets *virtualized* client rects on a
/// scaled display, so every planned point is off by the scale factor — and
/// nothing reports it: `SendInput` cannot signal that kind of failure at all
/// (see [`sendinput_result`]) and `PostMessageW` posts a well-formed message to
/// the wrong coordinate. A mis-aimed click being worse than none, anything but
/// per-monitor awareness refuses the acquire with a `Fatal`.
///
/// Do not drop `SetProcessDpiAwarenessContext`'s return value on the assumption
/// that a failure means "already set to what we want": it answers `FALSE` for
/// **any** already-set awareness, `UNAWARE` included, so only reading the
/// context back says *whose* value won. `build.rs` puts `dpiAwareness =
/// permonitorv2, permonitor` in the manifest and the loader applies it before
/// any code runs, so the setter always fails (`ERROR_ACCESS_DENIED`, measured) —
/// but `__COMPAT_LAYER=DPIUNAWARE` outranks the manifest, measured landing at
/// `unaware`.
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
        // SAFETY: the argument is a well-known Win32 constant, and the call
        // flips process-global state without borrowing anything.
        let set =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        // Read before the two getters below: `GetLastError` is per-thread, and
        // neither getter is documented to leave it alone.
        let set_error = (set == 0).then(std::io::Error::last_os_error);
        // SAFETY: takes no argument, returns an opaque process-global token, and
        // borrows nothing.
        let context = unsafe { GetThreadDpiAwarenessContext() };
        // SAFETY: `context` is the token the previous call just produced, and
        // this call answers `DPI_AWARENESS_INVALID` for anything it does not
        // recognize rather than faulting.
        let awareness = unsafe { GetAwarenessFromDpiAwarenessContext(context) };
        let verdict = awareness_verdict(awareness);
        if let Some(error) = set_error {
            // `info`, not `warn`: this fires on every launch. `accepted=false`
            // is the one case that matters.
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
            // Pointing at something the player can actually change.
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
