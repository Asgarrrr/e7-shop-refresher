//! Input backends driving the Epic Seven window: [`MessageSurface`]
//! (`PostMessageW`, background, shielded — the default) and [`WinSurface`]
//! (`SendInput`, real cursor, foreground — the fallback). No window is ever
//! resized: `to_screen` covers any aspect from 16:9 up, refuses narrower.
//!
//! # Why the seam is here
//!
//! The two backends share no state and are selected between in `src/app/mod.rs`,
//! so each owns a file: [`send_input`] and [`post_message`]. What stays in this
//! root is what both of them need and neither owns — the *window*, from the four
//! angles a backend only ever consumes: naming it ([`Hwnd`], [`Target`]),
//! finding it ([`find_game_window`]), measuring it ([`client_rect`]), and proving
//! this process may drive it at all ([`probe_window_reachable`],
//! [`preflight_refusal`]) — plus the two verdicts both backends have to reach
//! *identically* ([`rect_change_error`], [`release_twice`]). [`dpi`] holds the
//! remaining precondition, on its own because it is the one thing here that is
//! about the process rather than about the window.

mod dpi;
mod post_message;
mod send_input;

use std::fmt;

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetClientRect, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
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
/// The integer representation is load-bearing and documented on [`Target`]:
/// `HWND` is a raw pointer, so a bare `HWND` in a
/// struct would make the executor's future `!Send`. What was missing was the
/// *type*. The handle used to be a bare `isize` travelling beside other `isize`s
/// — most sharply in [`post`], where the LPARAM parameter has the same width and
/// [`pack_point`], whose result feeds it, returned exactly the handle's type, so
/// `post(lparam, WM_LBUTTONDOWN, MK_LBUTTON as usize, target.hwnd)` compiled. It
/// would have handed a coordinate pair to `PostMessageW` as a window handle:
/// `FALSE` + `ERROR_INVALID_WINDOW_HANDLE`, classified `Recoverable` by
/// [`post_refusal`], after which the watchdog retries a click that can never
/// land.
///
/// `#[repr(transparent)]` states the layout the `as HWND` casts throughout this
/// module already relied on implicitly; it adds no assumption, it writes the
/// existing one down. Every Win32 call still receives the exact ABI type,
/// obtained through [`Hwnd::raw`] at the call itself — the FFI boundary is
/// unchanged.
///
/// `Send` needs no `unsafe impl`: the inner value is an `isize`, which is what
/// keeping it an integer was for.
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

/// What `repr(transparent)` promises, stated where a change to the wrapper would
/// break it: [`Hwnd::raw`] hands the inner integer straight to Win32 as a
/// pointer, so the wrapper must be exactly the ABI type's width. In the style of
/// `Timings`' and `capture`'s canaries — adding a second field is then a decision
/// rather than a surprise.
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
/// button, and *that*, not the first refusal, is the fault worth telling.
///
/// Both backends route through here. It used to be two independent
/// implementations of the same three-state decision — 20 lines in
/// `WinSurface::release_after_down`, 13 inline in `MessageSurface::click` — for
/// the single most safety-critical invariant this module has.
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
/// The buffer *is* the value: dropping it leaves the caller passing a
/// dangling `as_ptr()` to Win32.
///
/// Sized up front rather than `collect()`ed: `EncodeUtf16::size_hint` reports a
/// *lower* bound of `ceil(len / 3)`, which is what `Vec::from_iter` reserves, so
/// the ASCII titles this is called with allocated small and then grew.
/// `text.len() + 1` is exact for ASCII and never short: one UTF-16 unit per byte
/// at most, plus the terminator.
///
/// The one *hot* caller is gone rather than made cheaper: `find_game_window` ran
/// before every injected event and now reads the compile-time
/// `GAME_WINDOW_TITLE_W` instead. Everything still calling this does so once per
/// process (the shield's class name), where one exact allocation is the right
/// shape and a `static` would only add a second spelling of the same encoding.
#[must_use]
pub(super) fn wide(text: &str) -> Vec<u16> {
    let mut buffer = Vec::with_capacity(text.len() + 1);
    buffer.extend(text.encode_utf16());
    buffer.push(0);
    buffer
}

/// [`GAME_WINDOW_TITLE`] as the NUL-terminated UTF-16 `FindWindowW` wants,
/// encoded once at compile time.
///
/// `find_game_window` runs before *every* injected event —
/// `WinSurface::validate_target` calls it, which this module's own
/// `validation_calls()` pins at three times inside a single `click` — so a
/// two-slot buy job used to re-encode these same ten bytes ~60 times, each time
/// through a `Vec` that allocated short and then grew. There is nothing left to
/// encode at run time and nothing left to allocate.
///
/// The title is ASCII, hence one UTF-16 unit per byte; the `assert!` is what
/// keeps that true. A non-ASCII character added here needs a real encoder, and
/// this stops the build instead of silently truncating one. The last unit stays
/// the `0` the array is initialized with — the terminator the W-suffixed call
/// reads.
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
/// `EpicSeven.exe` high). UIPI then refuses input from any process below that
/// level, so if this app is not itself elevated its window is out of reach and
/// every backend fails — but neither fails in a way that names the cause:
///
/// - The default `Message` backend gets `ERROR_ACCESS_DENIED` from the shield's
///   `SetWindowPos`, which used to be reported as "the window is gone or its
///   queue is full" and retried forever.
/// - The `Input` backend fails **silently**. `SendInput` is documented as
///   "neither `GetLastError` nor the return value will indicate the failure was
///   caused by UIPI blocking": it reports one event injected, the executor
///   reports success, and nothing whatsoever moves in the game. No per-call
///   error classification anywhere can see that, which is why the diagnosis has
///   to be a preflight rather than a better error message.
///
/// The exe is manifested `requireAdministrator` (see `build.rs`) precisely so
/// that the normal run is *not* the failing one, which makes this probe a safety
/// net rather than the everyday path. It stays because the net still catches
/// real cases — an elevation the player declined on a build whose manifest did
/// not force it, a `-gnu` or hand-linked binary carrying no manifest, a
/// debugger launching this process at medium integrity, or STOVE changing what
/// it does — and because in every one of them the alternative is not an error
/// but a silence: clicks reported as delivered, nothing moving on screen.
///
/// # Why a no-op `SetWindowPos`, and emphatically *not* `PostMessageW(WM_NULL)`
///
/// `WM_NULL` is the obvious probe and it does not work: it is on UIPI's default
/// allow-list, so a medium-integrity process posting it to a high-integrity
/// window gets `TRUE` back. Measured on Windows 11 26200 — a medium-integrity
/// prober against a high-integrity window answered `TRUE` for `WM_NULL` and
/// `FALSE` + `ERROR_ACCESS_DENIED` for `WM_MOUSEMOVE`, `WM_LBUTTONDOWN`,
/// `WM_MOUSEWHEEL` (the three this backend actually posts), `WM_USER`, `WM_APP`
/// and every form of `SetWindowPos`. A `WM_NULL` probe would therefore have
/// passed in exactly the situation it was written to catch.
///
/// `SWP_NOSIZE | SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE` asks Win32 to
/// change nothing at all: no geometry, no Z-order, no focus. It is inert on a
/// window we *can* reach — unlike a synthetic mouse message, which would nudge
/// the game's own cursor tracking — and it is refused on one we cannot, through
/// the very same gate `shield::raise` hits a moment later.
fn probe_window_reachable(hwnd: HWND) -> std::io::Result<()> {
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
/// processes keep running, so retrying is a loop that never terminates and never
/// explains itself. The executor's `fail` puts this text in the journal as
/// `>> actuator: <this> — stopping the loop`, at acquire time, *before* the
/// first click — which is the whole difference from the per-click message this
/// replaces, one the player only ever read after the executor had already given
/// up for good.
///
/// The refusal itself is the signal: the branch below only picks wording. The
/// code was measured to be `ERROR_ACCESS_DENIED` for a UIPI-blocked window (see
/// [`probe_window_reachable`]) but Microsoft does not document it, so a future
/// Windows answering something else must still stop the loop and still point at
/// the likely cause rather than fall through to a raw Win32 dump.
///
/// # The advice has to be one the player can actually carry out
///
/// This line used to end in "relaunch Epic Seven without administrator rights",
/// and that is impossible: Epic Seven is started by `STOVE.exe`, which declares
/// `requireAdministrator` in its own manifest, so a player using the normal
/// launcher has no unelevated way to run the game. The side that can change is
/// *this* app — it ships manifested `requireAdministrator` for exactly this
/// reason — so the fix names restarting it as administrator, which is a thing a
/// player can do, and the acceptable UAC prompt they may have dismissed.
pub(super) fn preflight_refusal(error: &std::io::Error) -> SurfaceError {
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
/// `acquire` hands out and every later call hands back, which is what makes
/// "input without an acquire" unrepresentable rather than merely refused.
///
/// The handle rides as an integer inside [`Hwnd`] rather than as a bare `HWND`
/// because `HWND` is a raw pointer, and a raw pointer in here would make the
/// executor's future `!Send` and unspawnable.
///
/// `pub` only so the two `impl Surface` blocks — one per backend module — do not
/// name a private type in a public associated type (`private_interfaces`). Opaque
/// either way: every field and every method is private to this module tree.
///
/// [`Surface::Window`]: super::Surface::Window
#[derive(Clone, Copy)]
pub struct Target {
    hwnd: Hwnd,
    /// Client area in screen pixels.
    rect: ClientRect,
}

/// The fixtures below are `pub(super)` rather than private because they name the
/// *shared* things — the two handles, the client rect they were measured on, and
/// the two `GetLastError` values the whole UIPI story rests on — and both backend
/// test modules present them. Duplicating a fixture would be duplicating the
/// measurement it records.
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
        // The cause is named, and named correctly: the game is elevated because
        // its launcher demands it.
        assert!(reason.contains("STOVE launcher"), "{reason}");
        assert!(reason.contains("restart it as administrator"), "{reason}");
        // The advice the measurement disproved: STOVE is manifested
        // `requireAdministrator`, so no player can start Epic Seven unelevated
        // through it. Telling them to try is worse than saying nothing.
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
