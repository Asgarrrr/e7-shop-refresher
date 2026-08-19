//! Input backends driving the Epic Seven window: [`MessageSurface`]
//! (`PostMessageW`, background, shielded — the default) and [`WinSurface`]
//! (`SendInput`, real cursor, foreground — the fallback). No window is ever
//! resized: `to_screen` covers any aspect from 16:9 up, refuses narrower.
//!
//! # Why the seam is here
//!
//! The two backends share no state and are selected between in `src/app/mod.rs`,
//! so each owns a file: [`send_input`] and [`post_message`]. This root holds what
//! both need and neither owns — the *window*: naming it ([`Hwnd`], [`Target`]),
//! finding it ([`find_game_window`]), measuring it ([`client_rect`]), proving
//! this process may drive it at all ([`probe_window_reachable`],
//! [`preflight_refusal`]), and the two verdicts both backends must reach
//! identically ([`rect_change_error`], [`release_twice`]). [`dpi`] is separate
//! because it is a process precondition, not a window one.

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

/// A window handle, carried as the `isize` this module has always carried it as.
///
/// The integer representation is load-bearing (see [`Target`]): `HWND` is a raw
/// pointer, so a bare `HWND` in a struct would make the executor's future
/// `!Send`. Before this type existed the handle was a bare `isize` travelling
/// beside other `isize`s — in [`post`], the LPARAM parameter has the same width,
/// and [`pack_point`]'s result (a packed coordinate pair) returned exactly the
/// handle's type, so `post(lparam, WM_LBUTTONDOWN, MK_LBUTTON as usize,
/// target.hwnd)` compiled and would have handed a coordinate pair to
/// `PostMessageW` as a window handle: `FALSE` + `ERROR_INVALID_WINDOW_HANDLE`,
/// classified `Recoverable` by [`post_refusal`], so the watchdog would retry a
/// click that can never land. Do not go back to a bare `isize` here.
///
/// `#[repr(transparent)]` states the layout the `as HWND` casts throughout this
/// module already relied on implicitly. Every Win32 call still receives the
/// exact ABI type, obtained through [`Hwnd::raw`] at the call itself.
///
/// `Send` needs no `unsafe impl`: the inner value is an `isize`.
///
/// [`post`]: post_message::post
/// [`pack_point`]: post_message::pack_point
/// [`post_refusal`]: post_message::post_refusal
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Hwnd(isize);

impl Hwnd {
    /// Wraps what Win32 just handed back. A null handle is *not* refused here —
    /// `FindWindowW`'s null is checked by [`find_game_window`], and
    /// `GetForegroundWindow`'s null is a legitimate "nobody has focus" that
    /// `ensure_foreground` compares like any other handle.
    pub(super) fn new(handle: HWND) -> Self {
        Self(handle as isize)
    }

    /// Back to the ABI type, at the Win32 call site and nowhere else.
    pub(super) fn raw(self) -> HWND {
        self.0 as HWND
    }
}

/// [`Hwnd::raw`] hands the inner integer straight to Win32 as a pointer, so the
/// wrapper must stay exactly the ABI type's width; this guards that. Adding a
/// second field is then a deliberate decision, not a silent break.
const _: () = assert!(size_of::<Hwnd>() == size_of::<HWND>());

/// Handles are conventionally read in hex, and wrapping the integer would
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

/// Hex, for the same reason: a decimal handle is unrecognizable next to the
/// value any Win32 tool would show for it.
impl fmt::Debug for Hwnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hwnd({self:#x})")
    }
}

/// The verdict when the window is no longer where the job planned it.
///
/// One definition for both backends: a window with no client area is minimized,
/// anything else has moved or been resized. Both are `Recoverable` — the window
/// is alive, just elsewhere, and the next job's `acquire()` re-reads a fresh
/// rect, so the watchdog's retry self-heals it.
fn rect_change_error(observed: ClientRect) -> SurfaceError {
    SurfaceError::Recoverable(if observed.is_degenerate() {
        "the game window was minimized mid-job".to_owned()
    } else {
        "the game window moved or resized mid-job".to_owned()
    })
}

/// The one implementation of "never leave the left button held".
///
/// A successful button-down must always be paired with a release attempt, so the
/// release is *not* guarded by the target check: it cannot initiate a click and
/// is strictly safer than leaving the game seeing a held button. Refusal is
/// retried exactly once, and the retry's own verdict decides which fault is
/// reported — if the second release also fails the game is left holding the
/// button, and that, not the first refusal, is the fault worth telling.
///
/// Both backends route through here rather than each implementing the same
/// three-state decision independently.
///
/// `revalidate` re-establishes the target (a no-op for a backend that has no
/// per-event validation), `release` posts or injects the button-up, and `what`
/// names the release in the fatal message.
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
        // report the reason the target was refused rather than inventing one.
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
///
/// `capture::pcap` had a byte-identical copy of this, down to the sizing
/// argument; both now read [`crate::wide`], which carries that argument once.
pub(super) use crate::wide::wide;

/// [`GAME_WINDOW_TITLE`] as the NUL-terminated UTF-16 `FindWindowW` wants,
/// encoded once at compile time.
///
/// `find_game_window` runs before *every* injected event — `validation_calls()`
/// pins it at three times inside a single `click` — so encoding at run time
/// would repeat the same allocation dozens of times per job. This has nothing
/// left to encode or allocate at run time.
///
/// The title is ASCII, hence one UTF-16 unit per byte; the `assert!` keeps that
/// true and turns a future non-ASCII character into a build failure instead of a
/// silent truncation. The last unit stays the `0` the array is initialized
/// with — the terminator the W-suffixed call reads.
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
    // so it outlives the call outright, and a null class filter means "any
    // class". The returned handle is borrowed, not owned: nothing to free, and
    // NULL is the documented not-found answer.
    let hwnd = unsafe { FindWindowW(std::ptr::null(), GAME_WINDOW_TITLE_W.as_ptr()) };
    if hwnd.is_null() {
        // Captured before any other Win32 call, which would overwrite the
        // thread's last-error slot: "not found" and "denied" look identical
        // in a bug report without it.
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
    // SAFETY: `rect` is an already-initialized `RECT` owned by this frame and
    // written in place; the return value is checked before any field is read,
    // so a failed call can never surface garbage coordinates.
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

/// Asks Windows, once per acquire, whether this process may drive `hwnd` at
/// all — before any input is planned against it.
///
/// # Why this exists
///
/// Epic Seven always runs at *high* integrity: players launch it through
/// `STOVE.exe`, whose manifest declares `requireAdministrator`, and the game
/// inherits the level from its launcher (measured on a real install:
/// `EpicSeven.exe` high). UIPI refuses input from any process below that level,
/// so an unelevated instance of this app cannot reach the window, and neither
/// backend fails in a way that names the cause:
///
/// - The default `Message` backend gets `ERROR_ACCESS_DENIED` from the shield's
///   `SetWindowPos`, previously reported as "the window is gone or its queue is
///   full" and retried forever.
/// - The `Input` backend fails **silently**. `SendInput` is documented as
///   "neither `GetLastError` nor the return value will indicate the failure was
///   caused by UIPI blocking": it reports one event injected, the executor
///   reports success, and nothing moves in the game. No per-call error
///   classification can see that, so the diagnosis has to be a preflight.
///
/// The exe is manifested `requireAdministrator` (see `build.rs`) so the normal
/// run is not the failing one; this probe is a safety net for the cases that
/// still slip through — a declined elevation on a build without the forced
/// manifest, a `-gnu` or hand-linked binary carrying none, a debugger launching
/// this process at medium integrity, or STOVE changing what it does. In each,
/// the alternative is not an error but a silence: clicks reported as delivered,
/// nothing moving on screen.
///
/// # Why a no-op `SetWindowPos`, not `PostMessageW(WM_NULL)`
///
/// `WM_NULL` does not work: it is on UIPI's default allow-list. Measured on
/// Windows 11 26200 — a medium-integrity prober against a high-integrity window
/// answered `TRUE` for `WM_NULL` and `FALSE` + `ERROR_ACCESS_DENIED` for
/// `WM_MOUSEMOVE`, `WM_LBUTTONDOWN`, `WM_MOUSEWHEEL` (the three this backend
/// actually posts), `WM_USER`, `WM_APP` and every form of `SetWindowPos`. A
/// `WM_NULL` probe would have passed in exactly the situation it was written to
/// catch.
///
/// `SWP_NOSIZE | SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE` asks Win32 to
/// change nothing: no geometry, no Z-order, no focus — inert on a window we can
/// reach, unlike a synthetic mouse message which would nudge the game's own
/// cursor tracking, and refused on one we cannot through the same gate
/// `shield::raise` hits a moment later.
///
/// # Why the hang check comes first
///
/// `SetWindowPos` on another process's window is synchronous *in the target's
/// message loop*: Win32 delivers `WM_WINDOWPOSCHANGING` and waits for that
/// thread to answer, and there is no timeout parameter — `SendMessageTimeout`
/// has one, `SetWindowPos` does not. So against a frozen Epic Seven this call
/// does not fail, it never returns, and it is reached through
/// [`blocking`](crate::actuator::blocking) → `block_in_place`: the actuator task
/// parks on a runtime worker and stays parked. Every later job is queued behind
/// a call that will not come back, nothing is journaled, and the wedge outlives
/// window close — `JoinHandle::abort` cannot interrupt a thread inside a Win32
/// call any more than one inside `thread::sleep`.
///
/// [`IsHungAppWindow`] is the OS's own answer to that question and sends
/// nothing, so it cannot itself block. It narrows rather than closes the hole: a
/// game that freezes between this check and the call below still parks the task.
/// What it removes is the case that actually happens — a game already frozen
/// when the job starts — turning a permanent silent wedge into a `Recoverable`
/// with a sentence the player can act on, which the watchdog then retries.
fn probe_window_reachable(hwnd: HWND) -> std::io::Result<()> {
    // SAFETY: `hwnd` is the handle `find_game_window` just returned; the call
    // reads window-manager state, takes no pointer, borrows nothing past the
    // call, and answers FALSE for a window that died in between.
    if unsafe { IsHungAppWindow(hwnd) } != 0 {
        // Not `last_os_error`: nothing failed. The code is what `preflight_refusal`
        // classifies on, and `ERROR_NOT_READY` is the closest documented truth —
        // the window is there and is not answering.
        return Err(std::io::Error::from_raw_os_error(ERROR_NOT_READY as i32));
    }
    // SAFETY: `hwnd` is the handle `find_game_window` just returned; a window
    // that died in between is reported as FALSE rather than faulting. The null
    // insert-after handle is ignored under `SWP_NOZORDER`, the geometry is inert
    // under `SWP_NOMOVE | SWP_NOSIZE`, and nothing is borrowed past the call —
    // Win32 keeps no pointer into this frame.
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
/// processes keep running, so retrying loops forever without explaining itself.
/// The executor's `fail` puts this text in the journal at acquire time, before
/// the first click, instead of after the executor has already given up.
///
/// The refusal itself is the signal: the branch below only picks wording. The
/// code was measured to be `ERROR_ACCESS_DENIED` for a UIPI-blocked window (see
/// [`probe_window_reachable`]) but Microsoft does not document it, so a future
/// Windows answering something else must still stop the loop and still point at
/// the likely cause.
///
/// Do not tell the player to "relaunch Epic Seven without administrator
/// rights" — that is impossible. Epic Seven is started by `STOVE.exe`, which
/// declares `requireAdministrator` in its own manifest, so a player using the
/// normal launcher has no unelevated way to run the game. The side that can
/// change is this app, which is why the advice is to restart *it* as
/// administrator.
pub(super) fn preflight_refusal(error: &std::io::Error) -> SurfaceError {
    // The one arm that is not fatal, and the only one where nothing was refused:
    // a frozen game is the most transient fault this preflight can meet — it
    // heals when the game answers again, or when the player kills it — so it
    // drops the job and leaves the watch armed for the watchdog's retry.
    // Halting here would make a two-second stall a stop the player has to undo
    // by hand. See [`probe_window_reachable`]'s "why the hang check comes
    // first" for why this is detected at all rather than waited out.
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

/// One acquired game window: the handle, and the client area that was measured on
/// it. This is [`Surface::Window`] for both Windows backends — the value
/// `acquire` hands out and every later call hands back, making "input without
/// an acquire" unrepresentable rather than merely refused.
///
/// `pub` only so the two `impl Surface` blocks — one per backend module — do not
/// name a private type in a public associated type (`private_interfaces`).
/// Opaque either way: every field and every method is private to this module
/// tree.
///
/// [`Surface::Window`]: super::Surface::Window
#[derive(Clone, Copy)]
pub struct Target {
    hwnd: Hwnd,
    /// Client area in screen pixels.
    rect: ClientRect,
}

/// `pub(super)` rather than private: both backend test modules use these shared
/// fixtures — the two handles, the client rect they were measured on, and the
/// `GetLastError` values the UIPI story rests on — and duplicating a fixture
/// would duplicate the measurement it records.
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

    /// What a UIPI-blocked call actually answers, measured on Windows 11 26200
    /// with a medium-integrity prober against a high-integrity window:
    /// `PostMessageW` of every mouse message and `SetWindowPos` in every form
    /// return FALSE with this code. Microsoft documents none of that, which is
    /// why the code chooses wording only and never the verdict.
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
        // STOVE is manifested `requireAdministrator`, so no player can start
        // Epic Seven unelevated through it — this advice must never appear.
        assert!(!reason.contains("without administrator rights"), "{reason}");
    }

    /// The verdict hangs on the call failing, not on the code being 5 — the
    /// code is undocumented, and a Windows that answered something else must
    /// still stop the loop rather than click into the void.
    #[test]
    fn a_preflight_refused_with_any_other_code_is_still_fatal_and_still_points_at_the_cause() {
        let SurfaceError::Fatal(reason) = preflight_refusal(&dead_handle()) else {
            panic!("any refused preflight must stop the loop");
        };
        assert!(reason.contains("restart it as administrator"), "{reason}");
        assert!(!reason.contains("without administrator rights"), "{reason}");
    }

    /// `IsHungAppWindow`'s answer, and the one arm of this classifier that is not
    /// fatal.
    ///
    /// Both halves matter. `Recoverable`, because a frozen game heals — on its
    /// own or when the player kills it — and halting the watch would make a
    /// two-second stall a stop they have to undo by hand. And detected at all,
    /// because the alternative is not an error: `SetWindowPos` on another
    /// process's window waits in *that* process's message loop with no timeout,
    /// so the preflight this replaces never returned, and it runs under
    /// `block_in_place` — the actuator task parks on a runtime worker, every
    /// later job queues behind it, and `abort` cannot reach a thread inside a
    /// Win32 call.
    #[test]
    fn a_frozen_game_window_is_waited_out_rather_than_halted() {
        let hung = std::io::Error::from_raw_os_error(ERROR_NOT_READY as i32);
        let SurfaceError::Recoverable(reason) = preflight_refusal(&hung) else {
            panic!("a game that stopped answering is the most transient fault here");
        };
        assert!(reason.contains("stopped responding"), "{reason}");
        // The integrity-level advice would be a lie here, and it is the one
        // sentence in this file that asks the player to restart the app.
        assert!(!reason.contains("restart it as administrator"), "{reason}");
    }

    /// Handles are read in hex everywhere else (Spy++, `WinDbg`, a bug report), so
    /// wrapping the integer must not cost `{:#x}` — and `Debug`, which is what a
    /// failing assertion prints, uses it.
    #[test]
    fn a_handle_formats_as_hex_in_every_form() {
        let hwnd = Hwnd(0x00AB_CDEF);
        assert_eq!(format!("{hwnd:x}"), "abcdef");
        assert_eq!(format!("{hwnd:#x}"), "0xabcdef");
        assert_eq!(format!("{hwnd:X}"), "ABCDEF");
        assert_eq!(format!("{hwnd:?}"), "Hwnd(0xabcdef)");
    }
}
