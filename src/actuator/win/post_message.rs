//! The `PostMessageW` backend (the default): synthetic mouse messages posted to
//! the game window, so no focus is stolen and the player keeps the mouse.
//!
//! What it owns beyond [`MessageSurface`] itself is everything that follows from
//! *posting* rather than injecting: the process-global [`shield`] each input
//! re-asserts, the wParam/LPARAM encoders every posted message needs
//! ([`pack_point`], [`wheel_wparam`]) and the verdict on a refused post
//! ([`post_refusal`]). [`Target`]'s inherent methods live here too — the root
//! defines the window both backends carry, but only this one converts it to
//! client coordinates and re-verifies per event.
//!
//! [`post`], [`pack_point`] and [`post_refusal`] are `pub(super)` — visible in
//! the `win` tree and nowhere else — solely because [`Hwnd`]'s doc comment is
//! written *about* them: the whole reason the newtype exists is the transposition
//! `post(lparam, …, target.hwnd)` used to compile. That rationale lives with the
//! type, in the root, and rustdoc will not link into a module-private item.

use std::time::Duration;

use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows_sys::Win32::System::SystemServices::MK_LBUTTON;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    PostMessageW, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
};

use crate::actuator::plan::ClientRect;
use crate::actuator::{Surface, SurfaceError, shield};

use super::dpi::ensure_dpi_awareness;
use super::{
    Hwnd, MOVE_SETTLE_MS, Target, WHEEL_DELTA, client_rect, find_game_window, preflight_refusal,
    probe_window_reachable, rect_change_error, release_twice,
};

/// Posted messages are retrieved before queued hardware input: a freshly
/// placed shield must let the game drain stale real moves before we post.
const SHIELD_DRAIN_MS: u64 = 50;

/// Why a refused `PostMessageW` is fatal or merely recoverable.
///
/// `ERROR_ACCESS_DENIED` here is UIPI. Do not classify it `Recoverable` under
/// "window gone or queue full" — both causes are wrong for it, and the watchdog
/// would re-issue clicks forever against a condition that cannot heal while the
/// game keeps running. It is `Fatal`: acting again would be acting blind,
/// exactly what that variant is for.
///
/// The preflight at acquire catches this first; this branch is the backstop
/// for a window whose integrity level changed *mid-job*, after the probe
/// passed — so it names the cause in one clause and leaves the full
/// explanation and the fix to [`preflight_refusal`], rather than repeating a
/// paragraph on every click.
///
/// Everything else keeps the old verdict, minus the invented certainty: a queue
/// that is genuinely full, or a window that closed between the shield going up
/// and the click going out, both self-heal on the next acquire.
pub(super) fn post_refusal(error: &std::io::Error) -> SurfaceError {
    if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
        SurfaceError::Fatal(format!(
            "Windows refused the click: the game window is at a higher integrity level than this \
             app ({error})"
        ))
    } else {
        SurfaceError::Recoverable(format!(
            "PostMessageW failed — the game window may have closed, or its queue is full ({error})"
        ))
    }
}

/// `PostMessageW` backend (the default): posts synthetic mouse messages to
/// the game window — no focus stolen, the player keeps the mouse. The engine
/// tracks its cursor through move messages, so every input re-asserts the
/// [`shield`] over the game until [`release`](Surface::release).
///
/// Fieldless on purpose: the only thing this backend owns beyond one job is the
/// process-global [`shield`], which its `Drop` lowers. The window travels as a
/// [`Target`] parameter (see [`Surface`]'s "why the window is a parameter"),
/// not a field this type keeps.
///
/// Braced-empty rather than a unit struct so that `MessageSurface::default()` —
/// how `src/app/mod.rs` spawns it, and the shape every other backend in the crate
/// is built with — does not become a `clippy::default_constructed_unit_structs`
/// diagnostic in a file this type does not own.
#[derive(Default)]
pub struct MessageSurface {}

impl Drop for MessageSurface {
    fn drop(&mut self) {
        shield::hide();
    }
}

impl Target {
    /// Screen → client pixels. Pure: dropping the result means the input was
    /// about to be posted at the wrong coordinates.
    #[must_use]
    fn to_client(self, at: (i32, i32)) -> (i32, i32) {
        (at.0 - self.rect.left, at.1 - self.rect.top)
    }

    /// Before every input: the window must be where the job planned it and
    /// the shield seated above it; a (re)placed shield gets the drain beat.
    /// A shield failure is fatal — never click shieldless.
    fn engage(self) -> Result<(), SurfaceError> {
        self.verify()?;
        if shield::raise(self.hwnd, self.rect).map_err(SurfaceError::Fatal)? {
            std::thread::sleep(Duration::from_millis(SHIELD_DRAIN_MS));
        }
        Ok(())
    }

    fn verify(self) -> Result<(), SurfaceError> {
        let rect = client_rect(self.hwnd.raw())?;
        if rect == self.rect {
            return Ok(());
        }
        Err(rect_change_error(rect))
    }
}

impl Surface for MessageSurface {
    type Window = Target;

    fn acquire(&mut self) -> Result<(Target, ClientRect), SurfaceError> {
        ensure_dpi_awareness()?;
        let hwnd = find_game_window()?;
        // One probe per job, at the only moment a clear answer is still useful:
        // the alternative is `shield::raise` failing on the first click of every
        // job with a message that names the wrong cause.
        probe_window_reachable(hwnd).map_err(|error| preflight_refusal(&error))?;
        let rect = client_rect(hwnd)?;
        let target = Target {
            hwnd: Hwnd::new(hwnd),
            rect,
        };
        Ok((target, rect))
    }

    fn click(
        &mut self,
        target: &Target,
        at: (i32, i32),
        press_ms: u64,
    ) -> Result<(), SurfaceError> {
        let target = *target;
        target.engage()?;
        let lparam = pack_point(target.to_client(at))?;
        post(target.hwnd, WM_MOUSEMOVE, 0, lparam)?;
        std::thread::sleep(Duration::from_millis(MOVE_SETTLE_MS));
        post(target.hwnd, WM_LBUTTONDOWN, MK_LBUTTON as usize, lparam)?;
        std::thread::sleep(Duration::from_millis(press_ms));
        // Button is down: always post the release, retrying once on failure so a
        // refused click never leaves the game seeing a held left button. This
        // backend re-verifies per post rather than up front, so there is nothing
        // to revalidate here — the decision itself is `release_twice`'s.
        release_twice(
            || Ok(()),
            || post(target.hwnd, WM_LBUTTONUP, 0, lparam),
            "WM_LBUTTONUP",
        )
    }

    fn scroll(
        &mut self,
        target: &Target,
        at: (i32, i32),
        notches: i32,
    ) -> Result<(), SurfaceError> {
        let target = *target;
        target.engage()?;
        post(
            target.hwnd,
            WM_MOUSEMOVE,
            0,
            pack_point(target.to_client(at))?,
        )?;
        std::thread::sleep(Duration::from_millis(MOVE_SETTLE_MS));
        // `WM_MOUSEWHEEL` takes screen coordinates; the delta rides wParam.
        post(
            target.hwnd,
            WM_MOUSEWHEEL,
            wheel_wparam(notches)?,
            pack_point(at)?,
        )?;
        Ok(())
    }

    /// Lowers the shield the inputs raised. Idempotent — `shield::hide` tolerates
    /// there being nothing up — because it runs both from the executor's guard and
    /// from this type's `Drop`.
    fn release(&mut self, _target: &Target) {
        shield::hide();
    }
}

/// `WM_MOUSEWHEEL`'s wParam: the wheel delta in the high word, *signed 16-bit*.
///
/// Validated exactly like the coordinate sibling below, and for the same reason.
/// The old form was `((delta as u32) << 16)`, which discarded everything above
/// bit 15 with no diagnostic — a shift never reports lost bits, not even in
/// debug — so a large notch count would have scrolled a wrong distance in the
/// *opposite* direction while `PostMessageW` reported success. `Recoverable`
/// because nothing here can heal it but nothing is left half-done either: no
/// message is posted at all.
fn wheel_wparam(notches: i32) -> Result<usize, SurfaceError> {
    let delta = i16::try_from(notches.saturating_mul(WHEEL_DELTA)).map_err(|_| {
        SurfaceError::Recoverable(format!(
            "wheel delta for {notches} notches is out of wParam range"
        ))
    })?;
    Ok(usize::from(delta.cast_unsigned()) << 16)
}

/// `MAKELPARAM`: x in the low word, y in the high word, both signed 16-bit.
///
/// A coordinate that does not fit is refused instead of being masked back
/// inside the window: `& 0xFFFF` would fold it silently onto some other pixel
/// and the click would still be posted, landing somewhere nobody planned.
/// `Recoverable` because the only way to get here is an absurd client rect,
/// and the next `acquire()` reads a fresh one — exactly the self-healing case
/// [`SurfaceError::Recoverable`] describes.
///
/// The word assembly goes through `u32`, not `i32`: shifting a high word past
/// bit 31 in a signed integer sets the sign bit, and the `as isize` widening
/// would then sign-extend into the upper half of a 64-bit LPARAM. Windows
/// only reads the low 32 bits, so the old form works by accident; this one
/// builds the value the doc comment promises.
pub(super) fn pack_point((x, y): (i32, i32)) -> Result<isize, SurfaceError> {
    let x = i16::try_from(x)
        .map_err(|_| SurfaceError::Recoverable(format!("x {x} out of LPARAM range")))?;
    let y = i16::try_from(y)
        .map_err(|_| SurfaceError::Recoverable(format!("y {y} out of LPARAM range")))?;
    let packed = (u32::from(y as u16) << 16) | u32::from(x as u16);
    Ok(packed as i32 as isize)
}

pub(super) fn post(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> Result<(), SurfaceError> {
    // SAFETY: `hwnd` may already be dead — `PostMessageW` reports that with
    // FALSE instead of faulting. Delivery is asynchronous, so nothing may be
    // borrowed by the queue: `wparam`/`lparam` carry packed coordinates and
    // button flags only, never a pointer into this process.
    let ok = unsafe { PostMessageW(hwnd.raw(), msg, wparam, lparam) } != 0;
    if ok {
        return Ok(());
    }
    // Read before any other Win32 call: `GetLastError` is per-thread and the
    // very next call overwrites it. Here it is not colour for a bug report — it
    // is what decides whether the loop stops or the watchdog retries.
    Err(post_refusal(&std::io::Error::last_os_error()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::HWND;

    use crate::actuator::win::tests::{
        GAME_HWND, OTHER_HWND, dead_handle, game_rect, uipi_refusal,
    };

    /// `release` runs from the executor's guard *and* from this type's `Drop`, so
    /// it can be reached two or three times over one job with no shield ever
    /// raised (an `acquire` that succeeded and a first `engage` that did not).
    /// `shield::hide` has to tolerate all of it.
    #[test]
    fn message_surface_cleanup_is_idempotent_without_a_shield() {
        let mut surface = MessageSurface::default();
        let target = Target {
            hwnd: GAME_HWND,
            rect: game_rect(),
        };

        surface.release(&target);
        surface.release(&target);

        drop(surface);
    }

    #[test]
    fn an_access_denied_post_stops_the_loop_instead_of_being_retried_forever() {
        // It was `Recoverable`, so the watchdog re-issued clicks against a
        // condition that cannot heal while both processes keep running.
        let SurfaceError::Fatal(reason) = post_refusal(&uipi_refusal()) else {
            panic!("a UIPI refusal must be fatal");
        };
        assert!(reason.contains("integrity level"), "{reason}");
        // The two causes the old text named are both wrong for this one.
        assert!(!reason.contains("queue is full"), "{reason}");
        assert!(!reason.contains("closed"), "{reason}");
    }

    #[test]
    fn any_other_post_failure_stays_recoverable_and_keeps_its_plain_wording() {
        let SurfaceError::Recoverable(reason) = post_refusal(&dead_handle()) else {
            panic!("only a UIPI refusal is fatal here");
        };
        assert!(reason.contains("PostMessageW failed"), "{reason}");
        assert!(!reason.contains("integrity"), "{reason}");
    }

    // `message_surface_input_without_acquire_is_fatal_not_a_panic` stood here —
    // see the note where its `WinSurface` twin was, in `send_input`'s test
    // module. Same reason: the state it described cannot be constructed now that
    // the window is a parameter.

    /// The low word is x, the high word is y, and the whole thing stays
    /// inside the low 32 bits Win32 reads back.
    #[test]
    fn pack_point_lays_x_low_and_y_high() {
        let packed = pack_point((0x1234, 0x5678)).unwrap();
        assert_eq!(packed as u32 & 0xFFFF, 0x1234);
        assert_eq!((packed as u32) >> 16, 0x5678);
    }

    #[test]
    fn pack_point_keeps_negative_coordinates_two_s_complement() {
        // Above a client area, left of it: Win32's GET_X/Y_LPARAM sign-extend
        // the words back to these values.
        let packed = pack_point((-1, -2)).unwrap();
        assert_eq!(packed as u32, 0xFFFE_FFFF);
    }

    /// What Win32 reads back is the low 32 bits split into two sign-extended
    /// words: every accepted coordinate must survive that round trip, high
    /// words past 0x8000 included.
    #[test]
    fn pack_point_round_trips_through_get_x_y_lparam() {
        for point in [
            (0, 0),
            (1600, 900),
            (-1, -2),
            (10, -20_000),
            (32_767, -32_768),
        ] {
            let packed = pack_point(point).unwrap() as u32;
            let x = i32::from((packed & 0xFFFF) as u16 as i16);
            let y = i32::from((packed >> 16) as u16 as i16);
            assert_eq!((x, y), point, "round trip of {point:?}");
        }
    }

    /// Not reachable from the crate's own `±10` notches — which is the point:
    /// the trait boundary is what a future caller reaches, and (see
    /// [`wheel_wparam`]'s doc) an unchecked shift would truncate silently.
    #[test]
    fn a_wheel_delta_outside_the_signed_word_is_refused_instead_of_truncated() {
        // 300 notches × 120 = 36 000, past `i16::MAX`: the old form posted
        // 36 000 − 65 536 = −29 536, i.e. a scroll the other way.
        assert!(matches!(
            wheel_wparam(300),
            Err(SurfaceError::Recoverable(reason)) if reason.contains("out of wParam range")
        ));
        assert!(matches!(
            wheel_wparam(-300),
            Err(SurfaceError::Recoverable(_))
        ));
    }

    /// The notch counts the planner actually emits, and the sign convention
    /// Win32 reads back out of the high word.
    #[test]
    fn wheel_wparam_carries_the_notches_as_a_signed_high_word() {
        assert_eq!(wheel_wparam(10).unwrap() >> 16, 1_200);
        let down = wheel_wparam(-10).unwrap();
        assert_eq!((down >> 16) as u16 as i16, -1_200);
        assert_eq!(wheel_wparam(0).unwrap(), 0);
        // The low word is wParam's button-state field and must stay clear.
        assert_eq!(wheel_wparam(10).unwrap() & 0xFFFF, 0);
    }

    /// The swap the newtype exists to stop, and the round trip the FFI boundary
    /// depends on. The transposition itself cannot be written here — that is the
    /// point, and a `#[test]` cannot assert a compile error — so what is pinned
    /// is that a handle and a packed point are no longer the same type: the
    /// packed point stays the `isize` LPARAM Win32 reads, while the handle only
    /// becomes an `HWND` through `raw`.
    #[test]
    fn a_handle_survives_the_round_trip_to_the_abi_type_and_back() {
        let packed: isize = pack_point((0x1234, 0x5678)).unwrap();
        let hwnd = Hwnd::new(packed as HWND);
        assert_eq!(hwnd.raw() as isize, packed);
        assert_eq!(Hwnd::new(std::ptr::null_mut()), Hwnd(0));
        assert_ne!(GAME_HWND, OTHER_HWND);
    }

    #[test]
    fn pack_point_refuses_a_coordinate_outside_the_word() {
        assert!(matches!(
            pack_point((40_000, 10)),
            Err(SurfaceError::Recoverable(reason)) if reason.contains("out of LPARAM range")
        ));
        assert!(matches!(
            pack_point((10, -40_000)),
            Err(SurfaceError::Recoverable(reason)) if reason.contains("out of LPARAM range")
        ));
    }
}
