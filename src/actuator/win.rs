//! Input backends driving the Epic Seven window: [`MessageSurface`]
//! (`PostMessageW`, background, shielded — the default) and [`WinSurface`]
//! (`SendInput`, real cursor, foreground — the fallback). No window is ever
//! resized: `to_screen` covers any aspect from 16:9 up, refuses narrower.

use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
use windows_sys::Win32::System::SystemServices::MK_LBUTTON;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, DPI_AWARENESS_INVALID,
    DPI_AWARENESS_PER_MONITOR_AWARE, DPI_AWARENESS_SYSTEM_AWARE, DPI_AWARENESS_UNAWARE,
    GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
    SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetClientRect, GetForegroundWindow, GetSystemMetrics, PostMessageW,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow, SetWindowPos, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
};

use super::plan::ClientRect;
use super::{Surface, SurfaceError, shield};

const GAME_WINDOW_TITLE: &str = "Epic Seven";
/// One wheel notch, Win32 convention.
const WHEEL_DELTA: i32 = 120;
/// Cursor settle between the absolute move and the button/wheel events.
const MOVE_SETTLE_MS: u64 = 30;
/// The foreground switch is asynchronous: give it a beat before verifying.
const FOCUS_SETTLE_MS: u64 = 100;
/// Posted messages are retrieved before queued hardware input: a freshly
/// placed shield must let the game drain stale real moves before we post.
const SHIELD_DRAIN_MS: u64 = 50;
/// Full range of a `MOUSEEVENTF_ABSOLUTE` coordinate — a Win32 protocol
/// constant, like `WHEEL_DELTA` above it.
const ABSOLUTE_COORD_MAX: i64 = 65_535;

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
fn ensure_dpi_awareness() -> Result<(), SurfaceError> {
    static DPI: OnceLock<Result<(), SurfaceError>> = OnceLock::new();
    DPI.get_or_init(|| {
        // SAFETY: the argument is a well-known Win32 constant and the call only
        // flips process-global DPI state — it borrows nothing and hands back
        // nothing to keep alive. A zero answer means *some* awareness was
        // already set, which is why the value is read back below rather than
        // inferred from this return.
        let set =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        // Read before the two getters below, per the rule this file states at
        // three other Win32 call sites: `GetLastError` is per-thread and *any*
        // later call may overwrite it. Neither getter is documented to set it,
        // which is exactly why reading it after them was not safe to rely on —
        // "not documented to write the slot" is not "documented not to". On the
        // shipped GUI build winit has already set the awareness, so this branch
        // fires on every launch and this value is the difference between a
        // reproducible bug report and "the clicks miss sometimes".
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
            // Whoever set it first won: winit in the GUI build, or a shim.
            // Recorded either way.
            tracing::info!(
                awareness = awareness_name(awareness),
                accepted = verdict.is_ok(),
                error = %error,
                "process DPI awareness was already set before the actuator asked"
            );
        }
        verdict
    })
    .clone()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputEvent {
    Move((i32, i32)),
    LeftDown,
    LeftUp,
    Wheel(i32),
}

/// The process-global input calls used by [`WinSurface`]. Keeping the Win32
/// boundary behind this trait lets the event-order tests prove that validation
/// happens before injection without ever touching the real cursor or focus.
trait InputDriver: Send {
    fn find_game_window(&mut self) -> Result<Hwnd, SurfaceError>;
    /// The preflight probe of [`probe_window_reachable`], raw.
    ///
    /// Hands back the thread's last-error untouched instead of a
    /// [`SurfaceError`] so that the *classification* — which message the player
    /// reads, and whether the loop stops — stays in one pure function
    /// ([`preflight_refusal`]) that the tests can drive with a synthetic
    /// `ERROR_ACCESS_DENIED` and no Win32 anywhere.
    fn probe_reachable(&mut self, hwnd: Hwnd) -> std::io::Result<()>;
    fn foreground_window(&mut self) -> Hwnd;
    fn request_foreground(&mut self, hwnd: Hwnd);
    fn client_rect(&mut self, hwnd: Hwnd) -> Result<ClientRect, SurfaceError>;
    fn send(&mut self, event: InputEvent) -> Result<(), SurfaceError>;
    fn sleep(&mut self, duration: Duration);
}

struct SystemInputDriver;

impl InputDriver for SystemInputDriver {
    fn find_game_window(&mut self) -> Result<Hwnd, SurfaceError> {
        ensure_dpi_awareness()?;
        find_game_window().map(Hwnd::new)
    }

    fn probe_reachable(&mut self, hwnd: Hwnd) -> std::io::Result<()> {
        probe_window_reachable(hwnd.raw())
    }

    fn foreground_window(&mut self) -> Hwnd {
        // SAFETY: no arguments, no ownership — the returned HWND (possibly
        // NULL) is only ever compared here, never dereferenced.
        Hwnd::new(unsafe { GetForegroundWindow() })
    }

    fn request_foreground(&mut self, hwnd: Hwnd) {
        // The refusal is dropped on purpose: `SetForegroundWindow` reports
        // FALSE both when the switch is denied and when it merely has not
        // happened yet, so its verdict is worthless. `ensure_foreground`
        // sleeps and then re-reads the actual foreground window — that read
        // is the authority, and it is the one that produces the error.
        // SAFETY: `hwnd` is the handle `acquire` found; the call validates it
        // itself and answers FALSE for a window that died in between.
        let _ = unsafe { SetForegroundWindow(hwnd.raw()) };
    }

    fn client_rect(&mut self, hwnd: Hwnd) -> Result<ClientRect, SurfaceError> {
        client_rect(hwnd.raw())
    }

    fn send(&mut self, event: InputEvent) -> Result<(), SurfaceError> {
        match event {
            InputEvent::Move(at) => move_cursor(at),
            InputEvent::LeftDown => send_mouse(0, MOUSEEVENTF_LEFTDOWN),
            InputEvent::LeftUp => send_mouse(0, MOUSEEVENTF_LEFTUP),
            InputEvent::Wheel(notches) => {
                send_mouse(notches.saturating_mul(WHEEL_DELTA), MOUSEEVENTF_WHEEL)
            }
        }
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// `SendInput` backend: real cursor, real foreground.
///
/// The driver stays erased behind `Box<dyn InputDriver>` even though production
/// only ever holds the one ZST. `trait-002` proposed
/// `WinSurface<D: InputDriver = SystemInputDriver>` instead, and it does not
/// stand alone: `InputDriver`/`SystemInputDriver` are private to this module, so
/// a type parameter over them makes `WinSurface` leak a private type
/// (`private_bounds`, `private_interfaces` — red under `-D warnings`) and
/// `run_executor(WinSurface::default(), …)` in `src/app/mod.rs` becomes a hard
/// error. Measured, not assumed. The allocation is one per session.
pub struct WinSurface {
    driver: Box<dyn InputDriver>,
}

impl Default for WinSurface {
    fn default() -> Self {
        Self {
            driver: Box::new(SystemInputDriver),
        }
    }
}

impl WinSurface {
    #[cfg(test)]
    fn with_driver(driver: impl InputDriver + 'static) -> Self {
        Self {
            driver: Box::new(driver),
        }
    }

    fn ensure_foreground(&mut self, hwnd: Hwnd) -> Result<(), SurfaceError> {
        if self.driver.foreground_window() == hwnd {
            return Ok(());
        }
        self.driver.request_foreground(hwnd);
        self.driver.sleep(Duration::from_millis(FOCUS_SETTLE_MS));
        if self.driver.foreground_window() != hwnd {
            return Err(SurfaceError::Fatal(
                "could not focus the game window".to_owned(),
            ));
        }
        Ok(())
    }

    /// Re-establishes that the acquired title still names this exact HWND,
    /// that its planned screen rectangle is unchanged and usable, and that it
    /// owns the foreground. The ordering is part of the safety contract.
    ///
    /// `target` arrives from the caller — the executor's guard, holding what
    /// `acquire` produced — rather than out of a field this type kept: everything
    /// checked below is about the *world* having moved, which is the only failure
    /// left once "was there an acquire at all" is carried by the type.
    fn validate_target(&mut self, target: Target) -> Result<(), SurfaceError> {
        let titled = self.driver.find_game_window()?;
        if titled != target.hwnd {
            return Err(SurfaceError::Fatal(
                "the game window title now identifies a different window".to_owned(),
            ));
        }

        let rect = self.driver.client_rect(target.hwnd)?;
        if rect.is_degenerate() || rect != target.rect {
            return Err(rect_change_error(rect));
        }

        self.ensure_foreground(target.hwnd)
    }

    fn send_guarded(&mut self, target: Target, event: InputEvent) -> Result<(), SurfaceError> {
        self.validate_target(target)?;
        self.driver.send(event)
    }

    /// This backend's half of [`release_twice`]: revalidate, then `LEFTUP`.
    ///
    /// The borrow has to be split by hand because both closures want
    /// `&mut self` — hence the raw pointer-free dance of validating first into a
    /// `Result` and handing `release_twice` a closure that only needs the driver.
    fn release_after_down(&mut self, target: Target) -> Result<(), SurfaceError> {
        let validated = self.validate_target(target);
        let driver = &mut self.driver;
        release_twice(|| validated, || driver.send(InputEvent::LeftUp), "LEFTUP")
    }
}

impl Surface for WinSurface {
    type Window = Target;

    fn acquire(&mut self) -> Result<(Target, ClientRect), SurfaceError> {
        let hwnd = self.driver.find_game_window()?;
        // Before the foreground is stolen and before any coordinate is planned:
        // a window this process may not drive is not worth pulling forward, and
        // `SendInput` will not report the refusal later — see
        // [`probe_window_reachable`].
        self.driver
            .probe_reachable(hwnd)
            .map_err(|error| preflight_refusal(&error))?;
        self.ensure_foreground(hwnd)?;
        let rect = self.driver.client_rect(hwnd)?;
        Ok((Target { hwnd, rect }, rect))
    }

    fn click(
        &mut self,
        target: &Target,
        at: (i32, i32),
        press_ms: u64,
    ) -> Result<(), SurfaceError> {
        let target = *target;
        self.send_guarded(target, InputEvent::Move(at))?;
        self.driver.sleep(Duration::from_millis(MOVE_SETTLE_MS));
        self.send_guarded(target, InputEvent::LeftDown)?;
        self.driver.sleep(Duration::from_millis(press_ms));
        self.release_after_down(target)
    }

    fn scroll(
        &mut self,
        target: &Target,
        at: (i32, i32),
        notches: i32,
    ) -> Result<(), SurfaceError> {
        let target = *target;
        self.send_guarded(target, InputEvent::Move(at))?;
        self.driver.sleep(Duration::from_millis(MOVE_SETTLE_MS));
        self.send_guarded(target, InputEvent::Wheel(notches))
    }

    // No `release`: this backend engaged nothing outside the events it already
    // sent, and the `Option<Target>` its `release` used to clear does not exist
    // any more — the window is the executor guard's, and it drops with the job.
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

/// One `SM_*` metric. Win32 has no error channel here: an unknown index (or
/// a metric the session cannot answer) simply reads back 0.
fn system_metric(index: i32) -> i32 {
    // SAFETY: the call reads a process-global integer and takes no pointer or
    // handle; every index reaching it is a documented `SM_*` constant, and any
    // other value would return 0 rather than fault.
    unsafe { GetSystemMetrics(index) }
}

/// Absolute cursor move, normalized to the virtual desktop so multi-monitor
/// setups resolve the same physical pixel.
fn move_cursor(at: (i32, i32)) -> Result<(), SurfaceError> {
    let left = system_metric(SM_XVIRTUALSCREEN);
    let top = system_metric(SM_YVIRTUALSCREEN);
    let width = system_metric(SM_CXVIRTUALSCREEN).max(1);
    let height = system_metric(SM_CYVIRTUALSCREEN).max(1);
    // Widen *before* subtracting: `left`/`top` go negative as soon as a
    // monitor sits left of or above the primary one, and an i32 subtraction
    // done first would be the thing that overflows.
    let dx = (i64::from(at.0) - i64::from(left)) * ABSOLUTE_COORD_MAX / i64::from(width);
    let dy = (i64::from(at.1) - i64::from(top)) * ABSOLUTE_COORD_MAX / i64::from(height);
    send_input(MOUSEINPUT {
        // Clamped, not truncated: a failed `GetSystemMetrics` reads 0, the
        // `.max(1)` above turns that into a width of 1, and the ratio then
        // leaves i32 entirely. Landing on the desktop edge is wrong but
        // bounded; a wrapped `as i32` would aim anywhere.
        dx: dx.clamp(0, ABSOLUTE_COORD_MAX) as i32,
        dy: dy.clamp(0, ABSOLUTE_COORD_MAX) as i32,
        mouseData: 0,
        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        time: 0,
        dwExtraInfo: 0,
    })
}

fn send_mouse(data: i32, flags: u32) -> Result<(), SurfaceError> {
    send_input(MOUSEINPUT {
        dx: 0,
        dy: 0,
        // `mouseData` is a `u32` holding a signed wheel delta in its low word:
        // spelled out rather than left to `as _`, so the reinterpretation is
        // visible instead of inferred from the field's declaration in another
        // crate.
        mouseData: data.cast_unsigned(),
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    })
}

fn send_input(mi: MOUSEINPUT) -> Result<(), SurfaceError> {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi },
    };
    // SAFETY: `input` is one fully initialized `INPUT` living for the whole
    // call. The count is 1, matching the single-element pointer, and the
    // third argument is `size_of::<INPUT>()` — the stride of *one* structure,
    // not the byte length of the array. Passing the total size there is the
    // classic mistake that makes `SendInput` reject everything; this is the
    // correct form.
    let inserted = unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) };
    sendinput_result(inserted)
}

/// `SendInput` returns the number of events inserted; we always send 1, so
/// anything else means the input was blocked (foreground lock, full queue).
/// Recoverable: the watchdog re-issues against a fresh acquire.
///
/// Note what is *not* in that list: UIPI. The documentation is explicit —
/// "neither `GetLastError` nor the return value will indicate the failure was
/// caused by UIPI blocking" — so a window out of this process's reach makes this
/// function answer `Ok(())` while nothing moves in the game. That blind spot is
/// covered at acquire time by [`probe_window_reachable`], and it cannot be
/// covered here.
fn sendinput_result(inserted: u32) -> Result<(), SurfaceError> {
    if inserted == 1 {
        Ok(())
    } else {
        Err(SurfaceError::Recoverable(format!(
            "SendInput injected {inserted}/1 events — input blocked ({})",
            std::io::Error::last_os_error()
        )))
    }
}

/// Why a refused `PostMessageW` is fatal or merely recoverable.
///
/// `ERROR_ACCESS_DENIED` here is UIPI, and it used to be classified
/// `Recoverable` under the text "window gone or queue full" — two causes that
/// are both wrong for it, and a verdict that had the watchdog re-issuing clicks
/// forever against a condition which cannot heal while the game keeps running.
/// It is `Fatal`: acting again would be acting blind, exactly what that variant
/// is for.
///
/// In practice the preflight at acquire catches this first and this branch is
/// the backstop for a window whose integrity level changed *mid-job*, after the
/// probe passed — so it names the cause in one clause and leaves the full
/// explanation and the fix to [`preflight_refusal`], rather than repeating a
/// paragraph on every click.
///
/// Everything else keeps the old verdict, minus the invented certainty: a queue
/// that is genuinely full, or a window that closed between the shield going up
/// and the click going out, both self-heal on the next acquire.
fn post_refusal(error: &std::io::Error) -> SurfaceError {
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
/// Fieldless on purpose: the only thing this backend owns beyond one job is the
/// process-global [`shield`], which its `Drop` lowers. The job-scoped
/// `Option<Target>` it used to carry — and the `target()` guard that read it —
/// are gone: the window is a [`Target`] the executor's guard holds and hands back
/// to every call (`api-004`).
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

/// One acquired game window: the handle, and the client area that was measured on
/// it. This is [`Surface::Window`] for both Windows backends — the value
/// `acquire` hands out and every later call hands back, which is what makes
/// "input without an acquire" unrepresentable rather than merely refused.
///
/// The handle rides as an integer inside [`Hwnd`] rather than as a bare `HWND`
/// because `HWND` is a raw pointer, and a raw pointer in here would make the
/// executor's future `!Send` and unspawnable.
///
/// `pub` only so the two `impl Surface` blocks below do not name a private type
/// in a public associated type (`private_interfaces`). Opaque either way: every
/// field and every method is private to this module.
#[derive(Clone, Copy)]
pub struct Target {
    hwnd: Hwnd,
    /// Client area in screen pixels.
    rect: ClientRect,
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
fn pack_point((x, y): (i32, i32)) -> Result<isize, SurfaceError> {
    let x = i16::try_from(x)
        .map_err(|_| SurfaceError::Recoverable(format!("x {x} out of LPARAM range")))?;
    let y = i16::try_from(y)
        .map_err(|_| SurfaceError::Recoverable(format!("y {y} out of LPARAM range")))?;
    let packed = (u32::from(y as u16) << 16) | u32::from(x as u16);
    Ok(packed as i32 as isize)
}

fn post(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> Result<(), SurfaceError> {
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
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    const GAME_HWND: Hwnd = Hwnd(101);
    const OTHER_HWND: Hwnd = Hwnd(202);

    /// `release` runs from the executor's guard *and* from this type's `Drop`, so
    /// it can be reached two or three times over one job with no shield ever
    /// raised (an `acquire` that succeeded and a first `engage` that did not).
    /// `shield::hide` has to tolerate all of it.
    ///
    /// This used to also assert `surface.target.is_none()` after the calls. That
    /// state is gone — `api-004` moved the window out of the backend — so what is
    /// left to pin is the idempotence, which is the part that runs from a
    /// destructor.
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum DriverCall {
        FindWindow,
        Probe(Hwnd),
        Foreground,
        RequestForeground(Hwnd),
        ClientRect(Hwnd),
        Send(InputEvent),
        Sleep(u64),
    }

    struct FakeState {
        calls: Vec<DriverCall>,
        window: Hwnd,
        foreground: Hwnd,
        rect: ClientRect,
        find_results: VecDeque<Result<Hwnd, SurfaceError>>,
        /// Scripted preflight outcomes, raw: an `Err` here is the thread's
        /// last-error as Win32 would have left it, so the tests exercise the
        /// real classification instead of a pre-classified verdict.
        probe_results: VecDeque<std::io::Result<()>>,
        foreground_results: VecDeque<Hwnd>,
        rect_results: VecDeque<Result<ClientRect, SurfaceError>>,
        send_results: VecDeque<Result<(), SurfaceError>>,
    }

    impl Default for FakeState {
        fn default() -> Self {
            Self {
                calls: Vec::new(),
                window: GAME_HWND,
                foreground: GAME_HWND,
                rect: game_rect(),
                find_results: VecDeque::new(),
                probe_results: VecDeque::new(),
                foreground_results: VecDeque::new(),
                rect_results: VecDeque::new(),
                send_results: VecDeque::new(),
            }
        }
    }

    struct FakeInputDriver {
        state: Arc<Mutex<FakeState>>,
    }

    impl InputDriver for FakeInputDriver {
        fn find_game_window(&mut self) -> Result<Hwnd, SurfaceError> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(DriverCall::FindWindow);
            if let Some(result) = state.find_results.pop_front() {
                result
            } else {
                Ok(state.window)
            }
        }

        fn probe_reachable(&mut self, hwnd: Hwnd) -> std::io::Result<()> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(DriverCall::Probe(hwnd));
            state.probe_results.pop_front().unwrap_or(Ok(()))
        }

        fn foreground_window(&mut self) -> Hwnd {
            let mut state = self.state.lock().unwrap();
            state.calls.push(DriverCall::Foreground);
            if let Some(hwnd) = state.foreground_results.pop_front() {
                hwnd
            } else {
                state.foreground
            }
        }

        fn request_foreground(&mut self, hwnd: Hwnd) {
            self.state
                .lock()
                .unwrap()
                .calls
                .push(DriverCall::RequestForeground(hwnd));
        }

        fn client_rect(&mut self, hwnd: Hwnd) -> Result<ClientRect, SurfaceError> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(DriverCall::ClientRect(hwnd));
            if let Some(result) = state.rect_results.pop_front() {
                result
            } else {
                Ok(state.rect)
            }
        }

        fn send(&mut self, event: InputEvent) -> Result<(), SurfaceError> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(DriverCall::Send(event));
            state.send_results.pop_front().unwrap_or(Ok(()))
        }

        fn sleep(&mut self, duration: Duration) {
            self.state
                .lock()
                .unwrap()
                .calls
                .push(DriverCall::Sleep(duration.as_millis() as u64));
        }
    }

    fn game_rect() -> ClientRect {
        ClientRect {
            left: 10,
            top: 20,
            width: 1600,
            height: 900,
        }
    }

    fn moved_rect() -> ClientRect {
        ClientRect {
            left: 11,
            ..game_rect()
        }
    }

    fn blocked_input() -> SurfaceError {
        SurfaceError::Recoverable("input blocked".to_owned())
    }

    fn dead_window() -> SurfaceError {
        SurfaceError::Fatal("could not read the game window's client area".to_owned())
    }

    fn fake_surface() -> (WinSurface, Arc<Mutex<FakeState>>) {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let surface = WinSurface::with_driver(FakeInputDriver {
            state: state.clone(),
        });
        (surface, state)
    }

    /// Acquires, checks the rect, and hands back the window every later call in
    /// the test has to present — which is the whole shape of `api-004`: the test
    /// cannot reach an input without holding the proof of an acquire either.
    fn acquire_and_clear(surface: &mut WinSurface, state: &Arc<Mutex<FakeState>>) -> Target {
        let (target, rect) = surface.acquire().expect("the fake acquires");
        assert_eq!(rect, game_rect());
        state.lock().unwrap().calls.clear();
        target
    }

    fn calls(state: &Arc<Mutex<FakeState>>) -> Vec<DriverCall> {
        state.lock().unwrap().calls.clone()
    }

    fn sent_events(state: &Arc<Mutex<FakeState>>) -> Vec<InputEvent> {
        calls(state)
            .into_iter()
            .filter_map(|call| match call {
                DriverCall::Send(event) => Some(event),
                _ => None,
            })
            .collect()
    }

    fn validation_calls() -> Vec<DriverCall> {
        vec![
            DriverCall::FindWindow,
            DriverCall::ClientRect(GAME_HWND),
            DriverCall::Foreground,
        ]
    }

    #[test]
    fn acquire_hands_back_the_exact_target_and_focuses_it() {
        let (mut surface, state) = fake_surface();
        state
            .lock()
            .unwrap()
            .foreground_results
            .extend([OTHER_HWND, GAME_HWND]);

        // The target is *returned* rather than stashed in the surface: the rect
        // the executor plans against and the window the inputs are aimed at come
        // out of the same call, so they cannot disagree.
        let (target, rect) = surface.acquire().expect("the fake acquires");
        assert_eq!(rect, game_rect());
        assert_eq!(target.hwnd, GAME_HWND);
        assert_eq!(target.rect, game_rect());
        assert_eq!(
            calls(&state),
            vec![
                DriverCall::FindWindow,
                // The preflight comes before the foreground steal: a window this
                // process may not drive is not worth pulling in front of the
                // player's own.
                DriverCall::Probe(GAME_HWND),
                DriverCall::Foreground,
                DriverCall::RequestForeground(GAME_HWND),
                DriverCall::Sleep(FOCUS_SETTLE_MS),
                DriverCall::Foreground,
                DriverCall::ClientRect(GAME_HWND),
            ]
        );
    }

    #[test]
    fn click_validates_before_every_normal_event() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);

        assert_eq!(surface.click(&target, (400, 500), 25), Ok(()));
        let mut expected = validation_calls();
        expected.push(DriverCall::Send(InputEvent::Move((400, 500))));
        expected.push(DriverCall::Sleep(MOVE_SETTLE_MS));
        expected.extend(validation_calls());
        expected.push(DriverCall::Send(InputEvent::LeftDown));
        expected.push(DriverCall::Sleep(25));
        expected.extend(validation_calls());
        expected.push(DriverCall::Send(InputEvent::LeftUp));
        assert_eq!(calls(&state), expected);
    }

    #[test]
    fn scroll_validates_before_move_and_again_before_wheel() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);

        assert_eq!(surface.scroll(&target, (300, 600), -2), Ok(()));
        let mut expected = validation_calls();
        expected.push(DriverCall::Send(InputEvent::Move((300, 600))));
        expected.push(DriverCall::Sleep(MOVE_SETTLE_MS));
        expected.extend(validation_calls());
        expected.push(DriverCall::Send(InputEvent::Wheel(-2)));
        assert_eq!(calls(&state), expected);
    }

    #[test]
    fn different_title_matched_window_is_fatal_before_input() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state.lock().unwrap().window = OTHER_HWND;

        assert!(matches!(
            surface.click(&target, (1, 2), 3),
            Err(SurfaceError::Fatal(reason)) if reason.contains("different window")
        ));
        assert_eq!(calls(&state), vec![DriverCall::FindWindow]);
    }

    #[test]
    fn missing_title_matched_window_is_fatal_before_input() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .find_results
            .push_back(Err(SurfaceError::Fatal("game window missing".to_owned())));

        assert!(matches!(
            surface.scroll(&target, (1, 2), 1),
            Err(SurfaceError::Fatal(reason)) if reason.contains("missing")
        ));
        assert_eq!(calls(&state), vec![DriverCall::FindWindow]);
    }

    #[test]
    fn moved_rect_is_recoverable_before_input() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state.lock().unwrap().rect = moved_rect();

        assert!(matches!(
            surface.click(&target, (1, 2), 3),
            Err(SurfaceError::Recoverable(reason)) if reason.contains("moved or resized")
        ));
        assert_eq!(
            calls(&state),
            vec![DriverCall::FindWindow, DriverCall::ClientRect(GAME_HWND)]
        );
    }

    #[test]
    fn minimized_rect_is_recoverable_before_input() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state.lock().unwrap().rect.width = 0;

        assert!(matches!(
            surface.scroll(&target, (1, 2), 1),
            Err(SurfaceError::Recoverable(reason)) if reason.contains("minimized")
        ));
        assert!(sent_events(&state).is_empty());
    }

    #[test]
    fn dead_stored_window_is_fatal_before_input() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .rect_results
            .push_back(Err(dead_window()));

        assert!(matches!(
            surface.click(&target, (1, 2), 3),
            Err(SurfaceError::Fatal(reason)) if reason.contains("client area")
        ));
        assert!(sent_events(&state).is_empty());
    }

    #[test]
    fn lost_focus_is_restored_and_verified_before_input() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .foreground_results
            .extend([OTHER_HWND, GAME_HWND]);

        assert_eq!(surface.scroll(&target, (30, 40), 1), Ok(()));
        let actual = calls(&state);
        assert_eq!(
            &actual[..7],
            &[
                DriverCall::FindWindow,
                DriverCall::ClientRect(GAME_HWND),
                DriverCall::Foreground,
                DriverCall::RequestForeground(GAME_HWND),
                DriverCall::Sleep(FOCUS_SETTLE_MS),
                DriverCall::Foreground,
                DriverCall::Send(InputEvent::Move((30, 40))),
            ]
        );
    }

    #[test]
    fn refused_focus_restoration_is_fatal_before_input() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .foreground_results
            .extend([OTHER_HWND, OTHER_HWND]);

        assert!(matches!(
            surface.scroll(&target, (30, 40), 1),
            Err(SurfaceError::Fatal(reason)) if reason.contains("could not focus")
        ));
        assert!(sent_events(&state).is_empty());
        assert_eq!(
            calls(&state),
            vec![
                DriverCall::FindWindow,
                DriverCall::ClientRect(GAME_HWND),
                DriverCall::Foreground,
                DriverCall::RequestForeground(GAME_HWND),
                DriverCall::Sleep(FOCUS_SETTLE_MS),
                DriverCall::Foreground,
            ]
        );
    }

    #[test]
    fn focus_loss_during_move_settle_blocks_left_down() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .foreground_results
            .extend([GAME_HWND, OTHER_HWND, OTHER_HWND]);

        assert!(matches!(
            surface.click(&target, (30, 40), 5),
            Err(SurfaceError::Fatal(_))
        ));
        assert_eq!(sent_events(&state), vec![InputEvent::Move((30, 40))]);
    }

    // `release_clears_target_and_actions_fail_closed` used to sit here, and its
    // twin `message_surface_input_without_acquire_is_fatal_not_a_panic` further
    // down. Both asserted that an input attempted after `release` — or with no
    // `acquire` at all — answered `SurfaceError::Fatal` rather than panicking.
    // Neither is a state either backend can be in any more: there is no
    // `Option<Target>` to be `None`, so the call cannot be written without a
    // `Target` to present (`api-004`). Deleted rather than weakened — a test for
    // an unrepresentable state is a test of nothing — and what it was really
    // guarding, that the executor never routes a window a job did not acquire, is
    // now pinned in `actuator::mod`'s
    // `every_input_and_the_release_see_the_window_that_job_acquired`.

    #[test]
    fn send_refusal_before_left_down_never_synthesizes_left_up() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .send_results
            .extend([Ok(()), Err(blocked_input())]);

        assert!(matches!(
            surface.click(&target, (30, 40), 5),
            Err(SurfaceError::Recoverable(_))
        ));
        assert_eq!(
            sent_events(&state),
            vec![InputEvent::Move((30, 40)), InputEvent::LeftDown]
        );
    }

    #[test]
    fn focus_is_restored_before_guarded_left_up() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .foreground_results
            .extend([GAME_HWND, GAME_HWND, OTHER_HWND, GAME_HWND]);

        assert_eq!(surface.click(&target, (30, 40), 5), Ok(()));
        let actual = calls(&state);
        let request = actual
            .iter()
            .position(|call| *call == DriverCall::RequestForeground(GAME_HWND))
            .unwrap();
        let up = actual
            .iter()
            .position(|call| *call == DriverCall::Send(InputEvent::LeftUp))
            .unwrap();
        assert!(request < up);
        assert_eq!(sent_events(&state).last(), Some(&InputEvent::LeftUp));
    }

    #[test]
    fn refused_focus_after_left_down_still_sends_unguarded_left_up() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .foreground_results
            .extend([GAME_HWND, GAME_HWND, OTHER_HWND, OTHER_HWND]);

        assert!(matches!(
            surface.click(&target, (30, 40), 5),
            Err(SurfaceError::Fatal(reason)) if reason.contains("could not focus")
        ));
        assert_eq!(
            sent_events(&state),
            vec![
                InputEvent::Move((30, 40)),
                InputEvent::LeftDown,
                InputEvent::LeftUp,
            ]
        );
    }

    #[test]
    fn moved_rect_after_left_down_still_sends_unguarded_left_up() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state.lock().unwrap().rect_results.extend([
            Ok(game_rect()),
            Ok(game_rect()),
            Ok(moved_rect()),
        ]);

        assert!(matches!(
            surface.click(&target, (30, 40), 5),
            Err(SurfaceError::Recoverable(reason)) if reason.contains("moved or resized")
        ));
        assert_eq!(sent_events(&state).last(), Some(&InputEvent::LeftUp));
    }

    #[test]
    fn minimized_rect_after_left_down_still_sends_unguarded_left_up() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        let mut minimized = game_rect();
        minimized.height = 0;
        state.lock().unwrap().rect_results.extend([
            Ok(game_rect()),
            Ok(game_rect()),
            Ok(minimized),
        ]);

        assert!(matches!(
            surface.click(&target, (30, 40), 5),
            Err(SurfaceError::Recoverable(reason)) if reason.contains("minimized")
        ));
        assert_eq!(sent_events(&state).last(), Some(&InputEvent::LeftUp));
    }

    #[test]
    fn refused_cleanup_left_up_is_retried_once() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        {
            let mut fake = state.lock().unwrap();
            fake.foreground_results
                .extend([GAME_HWND, GAME_HWND, OTHER_HWND, OTHER_HWND]);
            fake.send_results
                .extend([Ok(()), Ok(()), Err(blocked_input()), Ok(())]);
        }

        assert!(matches!(
            surface.click(&target, (30, 40), 5),
            Err(SurfaceError::Fatal(reason)) if reason.contains("could not focus")
        ));
        assert_eq!(
            sent_events(&state)
                .into_iter()
                .filter(|event| *event == InputEvent::LeftUp)
                .count(),
            2
        );
    }

    #[test]
    fn two_failed_cleanup_left_ups_are_fatal() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        {
            let mut fake = state.lock().unwrap();
            fake.foreground_results
                .extend([GAME_HWND, GAME_HWND, OTHER_HWND, OTHER_HWND]);
            fake.send_results
                .extend([Ok(()), Ok(()), Err(blocked_input()), Err(blocked_input())]);
        }

        assert!(matches!(
            surface.click(&target, (30, 40), 5),
            Err(SurfaceError::Fatal(reason)) if reason.contains("could not be proven released")
        ));
        assert_eq!(
            sent_events(&state)
                .into_iter()
                .filter(|event| *event == InputEvent::LeftUp)
                .count(),
            2
        );
    }

    #[test]
    fn refused_guarded_left_up_is_retried_and_returns_original_error() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .send_results
            .extend([Ok(()), Ok(()), Err(blocked_input()), Ok(())]);

        assert!(matches!(
            surface.click(&target, (30, 40), 5),
            Err(SurfaceError::Recoverable(reason)) if reason.contains("input blocked")
        ));
        assert_eq!(
            sent_events(&state)
                .into_iter()
                .filter(|event| *event == InputEvent::LeftUp)
                .count(),
            2
        );
    }

    #[test]
    fn sendinput_one_event_is_ok() {
        assert!(sendinput_result(1).is_ok());
    }

    #[test]
    fn sendinput_zero_events_is_recoverable() {
        assert!(matches!(
            sendinput_result(0),
            Err(SurfaceError::Recoverable(_))
        ));
    }

    /// What a UIPI-blocked call actually answers, measured on Windows 11 26200
    /// with a medium-integrity prober against a high-integrity window:
    /// `PostMessageW` of every mouse message and `SetWindowPos` in every form
    /// return FALSE with this code. Microsoft documents none of that, which is
    /// why the code chooses wording only and never the verdict.
    fn uipi_refusal() -> std::io::Error {
        std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32)
    }

    /// `ERROR_INVALID_WINDOW_HANDLE`: the window really did die.
    fn dead_handle() -> std::io::Error {
        std::io::Error::from_raw_os_error(1400)
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

    #[test]
    fn a_refused_preflight_stops_acquire_before_the_foreground_is_stolen() {
        let (mut surface, state) = fake_surface();
        state
            .lock()
            .unwrap()
            .probe_results
            .push_back(Err(uipi_refusal()));

        assert!(matches!(
            surface.acquire(),
            Err(SurfaceError::Fatal(reason)) if reason.contains("higher integrity level")
        ));
        // Nothing was focused and nothing was measured, so the job never becomes
        // clickable — and now for a stronger reason than an unset field: the `Err`
        // carries no `Target`, so there is no window for a later input to be
        // aimed at.
        assert_eq!(
            calls(&state),
            vec![DriverCall::FindWindow, DriverCall::Probe(GAME_HWND)]
        );
    }

    /// The whole reason the probe is a preflight rather than a better error
    /// message: with this backend the injection itself reports success.
    #[test]
    fn a_refused_preflight_is_the_only_signal_this_backend_gets() {
        let (mut surface, state) = fake_surface();
        state
            .lock()
            .unwrap()
            .probe_results
            .push_back(Err(uipi_refusal()));

        assert!(surface.acquire().is_err());
        // `sendinput_result` answers `Ok(())` for a UIPI-blocked injection —
        // documented, and unfixable at the injection site.
        assert!(sendinput_result(1).is_ok());
        assert!(sent_events(&state).is_empty());
    }

    // `message_surface_input_without_acquire_is_fatal_not_a_panic` stood here —
    // see the note where its `WinSurface` twin was, further up. Same reason: the
    // state it described cannot be constructed now that the window is a
    // parameter.

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

    /// The wheel path used to mask the delta into wParam's high word with
    /// `(delta as u32) << 16`, twelve lines below the doc comment that refuses
    /// exactly that for coordinates. Not reachable from the crate's own
    /// `±10` notches — which is the point: the trait boundary is what a future
    /// caller reaches, and a truncated delta scrolls the wrong distance in the
    /// opposite direction with every layer reporting success.
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
