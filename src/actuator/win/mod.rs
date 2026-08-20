//! Input backends driving the Epic Seven window: [`MessageSurface`]
//! (`PostMessageW`, background, shielded — the default) and [`WinSurface`]
//! (`SendInput`, real cursor, foreground — the fallback). No window is ever
//! resized: `to_screen` covers any aspect from 16:9 up, refuses narrower.
//!
//! This root holds what neither backend owns: the window itself, and the two
//! verdicts both must reach identically ([`rect_change_error`],
//! [`release_twice`]).

mod dpi;
mod post_message;
mod send_input;

use std::fmt;

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_NOT_READY, HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetClientRect, IsHungAppWindow, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SetWindowPos,
};

use super::SurfaceError;
use super::plan::ClientRect;

pub use post_message::MessageSurface;
pub use send_input::WinSurface;

const GAME_WINDOW_TITLE: &str = "Epic Seven";
/// One wheel notch, Win32 convention.
const WHEEL_DELTA: i32 = 120;
/// Cursor settle between the absolute move and the button/wheel events.
const MOVE_SETTLE_MS: u64 = 30;

/// A window handle, carried as an `isize`.
///
/// Do not go back to a bare `isize`. An integer it must be — a bare `HWND` is a
/// raw pointer and would make the executor's future `!Send` — but a bare `isize`
/// is interchangeable with [`pack_point`]'s packed coordinates and [`post`]'s
/// LPARAM, so `post(lparam, WM_LBUTTONDOWN, MK_LBUTTON as usize, target.hwnd)`
/// compiled: `PostMessageW` took a coordinate pair as a window handle, and
/// [`post_refusal`] classified the `ERROR_INVALID_WINDOW_HANDLE` `Recoverable`,
/// so the watchdog retried a click that can never land.
///
/// `#[repr(transparent)]` states what the `as HWND` casts already relied on.
///
/// [`post`]: post_message::post
/// [`pack_point`]: post_message::pack_point
/// [`post_refusal`]: post_message::post_refusal
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Hwnd(isize);

impl Hwnd {
    /// A null handle is *not* refused here: `FindWindowW`'s null is checked by
    /// [`find_game_window`], and `GetForegroundWindow`'s null is a legitimate
    /// "nobody has focus" that `ensure_foreground` compares like any other.
    pub(super) fn new(handle: HWND) -> Self {
        Self(handle as isize)
    }

    /// Back to the ABI type, at the Win32 call site and nowhere else.
    pub(super) fn raw(self) -> HWND {
        self.0 as HWND
    }
}

/// [`Hwnd::raw`] hands the inner integer straight to Win32 as a pointer, so the
/// wrapper must stay exactly the ABI type's width.
const _: () = assert!(size_of::<Hwnd>() == size_of::<HWND>());

/// Handles are read in hex everywhere else, and wrapping the integer would
/// otherwise have taken `{:#x}` away.
impl fmt::LowerHex for Hwnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

impl fmt::UpperHex for Hwnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::UpperHex::fmt(&self.0, f)
    }
}

/// Hex too, and this is what a failing assertion prints.
impl fmt::Debug for Hwnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hwnd({self:#x})")
    }
}

/// The verdict when the window is no longer where the job planned it. Both are
/// `Recoverable`: the window is alive, just elsewhere, and the next job's
/// `acquire()` re-reads a fresh rect.
fn rect_change_error(observed: ClientRect) -> SurfaceError {
    SurfaceError::Recoverable(if observed.is_degenerate() {
        "the game window was minimized mid-job".to_owned()
    } else {
        "the game window moved or resized mid-job".to_owned()
    })
}

/// The one implementation of "never leave the left button held".
///
/// The release is *not* guarded by the target check: it cannot initiate a click
/// and is strictly safer than leaving the game seeing a held button. Refusal is
/// retried once; if the second release also fails, that — not the first
/// refusal — is the fault worth telling.
///
/// `revalidate` may only *look*: it runs with the button down, so a check that
/// sleeps or restores something stretches the press into a long-press (the
/// numbers are on `send_input`'s `FOCUS_SETTLE_MS`). Its `Err` is a verdict
/// about the click, reported after the release, never a reason to delay one.
fn release_twice(
    revalidate: impl FnOnce() -> Result<(), SurfaceError>,
    mut release: impl FnMut() -> Result<(), SurfaceError>,
    what: &str,
) -> Result<(), SurfaceError> {
    let original = match revalidate() {
        Ok(()) => match release() {
            Ok(()) => return Ok(()),
            Err(error) => error,
        },
        // The target is unsafe, but the button may be down: release anyway, and
        // report why the target was refused rather than inventing a reason.
        Err(error) => match release() {
            Ok(()) => return Err(error),
            Err(_) => error,
        },
    };

    if release().is_ok() {
        return Err(original);
    }
    Err(SurfaceError::Fatal(format!(
        "left button state could not be proven released after two failed {what} attempts"
    )))
}

/// Null-terminated UTF-16, the shape W-suffixed Win32 calls want.
pub(super) use crate::wide::wide;

/// [`GAME_WINDOW_TITLE`] as the NUL-terminated UTF-16 `FindWindowW` wants,
/// encoded at compile time because `find_game_window` runs before *every*
/// injected event — three times inside one `click`. The `assert!` turns a future
/// non-ASCII character into a build failure rather than a silent truncation.
static GAME_WINDOW_TITLE_W: [u16; GAME_WINDOW_TITLE.len() + 1] = {
    let bytes = GAME_WINDOW_TITLE.as_bytes();
    let mut units = [0u16; GAME_WINDOW_TITLE.len() + 1];
    let mut index = 0;
    while index < bytes.len() {
        assert!(
            bytes[index].is_ascii(),
            "the game window title must be ASCII for this encoder"
        );
        units[index] = bytes[index] as u16;
        index += 1;
    }
    units
};

/// No window at all: nothing to retry against — fatal.
fn find_game_window() -> Result<HWND, SurfaceError> {
    // SAFETY: `GAME_WINDOW_TITLE_W` is a `static` NUL-terminated UTF-16 buffer,
    // so it outlives the call, and the returned handle is borrowed, not owned.
    let hwnd = unsafe { FindWindowW(std::ptr::null(), GAME_WINDOW_TITLE_W.as_ptr()) };
    if hwnd.is_null() {
        // Before any other Win32 call overwrites the thread's last-error slot:
        // without it, "not found" and "denied" read alike.
        let error = std::io::Error::last_os_error();
        return Err(SurfaceError::Fatal(format!(
            "no \"{GAME_WINDOW_TITLE}\" window found ({error})"
        )));
    }
    Ok(hwnd)
}

/// A failing rect API means the window handle itself died (closed mid-job):
/// fatal — unlike a *changed* rect, which is the recoverable case.
fn client_rect(hwnd: HWND) -> Result<ClientRect, SurfaceError> {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: `rect` is an initialized `RECT` this frame owns; the return value
    // is checked before any field is read, so a failed call cannot surface
    // garbage coordinates.
    if unsafe { GetClientRect(hwnd, &mut rect) } == 0 {
        let error = std::io::Error::last_os_error();
        return Err(SurfaceError::Fatal(format!(
            "could not read the game window's client area ({error})"
        )));
    }
    let mut origin = POINT { x: 0, y: 0 };
    // SAFETY: same shape — `origin` is an initialized `POINT` this frame owns,
    // converted in place, and read only after the success check.
    if unsafe { ClientToScreen(hwnd, &mut origin) } == 0 {
        let error = std::io::Error::last_os_error();
        return Err(SurfaceError::Fatal(format!(
            "could not locate the game window on screen ({error})"
        )));
    }
    Ok(ClientRect {
        left: origin.x,
        top: origin.y,
        width: rect.right,
        height: rect.bottom,
    })
}

/// The guard both backends run before every input reaches `post`/`send`: the
/// window this job acquired must still be the one the title names, at the
/// rect the job read. A reviewer should reject any new input path that
/// reaches an actual click without going through this.
///
/// `titled` is gathered by the caller, not read here — the two backends
/// resolve it through different driver seams ([`MessageDriver`](post_message)'s
/// vs. [`InputDriver`](send_input)'s) — but `rect` is taken as a thunk rather
/// than an already-read value: a title mismatch must stop *before* the rect is
/// ever fetched, matching the pre-existing `send_input.rs` behaviour and the
/// `..._before_input` tests that assert the call log ends at the title check.
/// This function exists so neither backend can carry its own copy of the
/// *verdict* — only of the reads.
///
/// A title match with a *different* `HWND` is [`SurfaceError::Fatal`], not
/// `Recoverable`: Windows recycles handle values, so a mismatch here means the
/// window this job acquired is provably gone (or the value now names a
/// stranger), and that does not heal while both processes keep running — a
/// `Recoverable` would make the watchdog re-issue clicks against a target that
/// no longer exists. The residual race is real and not closeable with these
/// APIs: this check runs before each input, not atomically with it, so a swap
/// inside that window is not detected.
fn verify_identity(
    titled: Hwnd,
    target: Target,
    rect: impl FnOnce() -> Result<ClientRect, SurfaceError>,
) -> Result<(), SurfaceError> {
    if titled != target.hwnd {
        return Err(SurfaceError::Fatal(
            "the game window title now identifies a different window".to_owned(),
        ));
    }
    let rect = rect()?;
    if rect.is_degenerate() || rect != target.rect {
        return Err(rect_change_error(rect));
    }
    Ok(())
}

/// Asks Windows, once per acquire, whether this process may drive `hwnd` at
/// all — before any input is planned against it.
///
/// Epic Seven runs at *high* integrity (`STOVE.exe` declares
/// `requireAdministrator`, the game inherits it) and UIPI refuses input from
/// below that level without either backend naming the cause: the `Message`
/// backend retries `shield::raise`'s `ERROR_ACCESS_DENIED` forever, and
/// `SendInput` "will not indicate the failure was caused by UIPI blocking" — it
/// reports one event injected while nothing moves. Only a preflight sees it.
///
/// Not `PostMessageW(WM_NULL)`, which is on UIPI's default allow-list and would
/// pass in exactly the case this exists to catch. Measured on Windows 11 26200,
/// a medium-integrity prober against a high-integrity window: `TRUE` for
/// `WM_NULL`, `FALSE` + `ERROR_ACCESS_DENIED` for every mouse message this
/// backend posts, `WM_USER`, `WM_APP` and every form of `SetWindowPos`. The four
/// `SWP_NO*` flags change nothing, so this is inert where it is allowed, unlike
/// a real mouse message that would nudge the game's own cursor tracking.
///
/// The hang check comes first because `SetWindowPos` on another process's window
/// is synchronous *in that process's message loop* with no timeout: against a
/// frozen Epic Seven it never returns, and under `block_in_place` that parks the
/// actuator task for good, past `JoinHandle::abort`'s reach.
/// [`IsHungAppWindow`] sends nothing, so it cannot itself block.
fn probe_window_reachable(hwnd: HWND) -> std::io::Result<()> {
    // SAFETY: `hwnd` is the handle `find_game_window` just returned; the call
    // takes no pointer and answers FALSE for a window that died in between.
    if unsafe { IsHungAppWindow(hwnd) } != 0 {
        // Not `last_os_error`: nothing failed. `preflight_refusal` classifies on
        // the code, and `ERROR_NOT_READY` is the closest documented truth.
        return Err(std::io::Error::from_raw_os_error(ERROR_NOT_READY as i32));
    }
    // SAFETY: `hwnd` is the handle `find_game_window` just returned, and a
    // window that died in between is reported as FALSE rather than faulting. The
    // null insert-after handle is ignored under `SWP_NOZORDER`, and Win32 keeps
    // no pointer into this frame.
    let answered = unsafe {
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOSIZE | SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
    if answered == 0 {
        // Read before any other Win32 call: `GetLastError` is per-thread and the
        // very next call overwrites it.
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The verdict a refused preflight produces, and the one line the player gets.
///
/// Fatal, not recoverable: an integrity-level mismatch cannot heal while both
/// processes keep running. The refusal itself is the signal and the branch below
/// only picks wording — `ERROR_ACCESS_DENIED` was measured for a UIPI-blocked
/// window (see [`probe_window_reachable`]) but Microsoft does not document it,
/// so another code must still stop the loop and still point at the likely cause.
///
/// Do not tell the player to "relaunch Epic Seven without administrator rights":
/// `STOVE.exe` declares `requireAdministrator`, so there is no unelevated way to
/// start the game. The side that can change is this app.
pub(super) fn preflight_refusal(error: &std::io::Error) -> SurfaceError {
    // The one arm that is not fatal: nothing was refused, and a frozen game
    // heals, so halting would make a two-second stall a stop to undo by hand.
    if error.raw_os_error() == Some(ERROR_NOT_READY as i32) {
        return SurfaceError::Recoverable(
            "the game window has stopped responding, so no click aimed at it would be read — \
             waiting for Epic Seven to answer again"
                .to_owned(),
        );
    }
    let cause = if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
        "the game window runs at a higher integrity level than this app, so Windows refuses every \
         click aimed at it — Epic Seven runs as administrator because the STOVE launcher requires \
         it, so close this app and restart it as administrator, accepting the Windows permission \
         prompt"
    } else {
        "the game window refused a harmless preflight, so no click aimed at it would arrive \
         either — close this app and restart it as administrator, accepting the Windows \
         permission prompt"
    };
    SurfaceError::Fatal(format!("{cause} ({error})"))
}

/// One acquired game window: [`Surface::Window`] for both Windows backends, the
/// value `acquire` hands out and every later call hands back, making "input
/// without an acquire" unrepresentable rather than merely refused.
///
/// `pub` only so the two `impl Surface` blocks do not name a private type in a
/// public associated type (`private_interfaces`); every field and method here is
/// private.
///
/// [`Surface::Window`]: super::Surface::Window
#[derive(Clone, Copy)]
pub struct Target {
    hwnd: Hwnd,
    /// Client area in screen pixels.
    rect: ClientRect,
}

/// `pub(super)` rather than private: both backend test modules share these
/// fixtures, and duplicating one would duplicate the measurement it records.
#[cfg(test)]
mod tests {
    use super::*;

    pub(super) const GAME_HWND: Hwnd = Hwnd(101);
    pub(super) const OTHER_HWND: Hwnd = Hwnd(202);

    pub(super) fn game_rect() -> ClientRect {
        ClientRect {
            left: 10,
            top: 20,
            width: 1600,
            height: 900,
        }
    }

    /// What a UIPI-blocked call answers, measured on Windows 11 26200. Microsoft
    /// documents none of it, hence a classifier that picks wording only.
    pub(super) fn uipi_refusal() -> std::io::Error {
        std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32)
    }

    /// `ERROR_INVALID_WINDOW_HANDLE`: the window really did die.
    pub(super) fn dead_handle() -> std::io::Error {
        std::io::Error::from_raw_os_error(1400)
    }

    #[test]
    fn a_refused_preflight_names_the_integrity_level_and_the_fix_the_player_can_apply() {
        let SurfaceError::Fatal(reason) = preflight_refusal(&uipi_refusal()) else {
            panic!("a window this process cannot drive is not something to retry");
        };
        assert!(reason.contains("higher integrity level"), "{reason}");
        assert!(reason.contains("STOVE launcher"), "{reason}");
        assert!(reason.contains("restart it as administrator"), "{reason}");
        // STOVE is manifested `requireAdministrator`: this advice would be
        // impossible to follow.
        assert!(!reason.contains("without administrator rights"), "{reason}");
    }

    /// The verdict hangs on the call failing, not on the code being 5.
    #[test]
    fn a_preflight_refused_with_any_other_code_is_still_fatal_and_still_points_at_the_cause() {
        let SurfaceError::Fatal(reason) = preflight_refusal(&dead_handle()) else {
            panic!("any refused preflight must stop the loop");
        };
        assert!(reason.contains("restart it as administrator"), "{reason}");
        assert!(!reason.contains("without administrator rights"), "{reason}");
    }

    /// `IsHungAppWindow`'s answer: the one arm of this classifier that is not
    /// fatal, because a frozen game heals.
    #[test]
    fn a_frozen_game_window_is_waited_out_rather_than_halted() {
        let hung = std::io::Error::from_raw_os_error(ERROR_NOT_READY as i32);
        let SurfaceError::Recoverable(reason) = preflight_refusal(&hung) else {
            panic!("a game that stopped answering is the most transient fault here");
        };
        assert!(reason.contains("stopped responding"), "{reason}");
        // The integrity-level advice would be a lie here.
        assert!(!reason.contains("restart it as administrator"), "{reason}");
    }

    /// Wrapping the integer must not cost `{:#x}`.
    #[test]
    fn a_handle_formats_as_hex_in_every_form() {
        let hwnd = Hwnd(0x00AB_CDEF);
        assert_eq!(format!("{hwnd:x}"), "abcdef");
        assert_eq!(format!("{hwnd:#x}"), "0xabcdef");
        assert_eq!(format!("{hwnd:X}"), "ABCDEF");
        assert_eq!(format!("{hwnd:?}"), "Hwnd(0xabcdef)");
    }
}
