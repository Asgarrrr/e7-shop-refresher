//! Input backends driving the Epic Seven window: [`MessageSurface`]
//! (`PostMessageW`, background, shielded — the default) and [`WinSurface`]
//! (`SendInput`, real cursor, foreground — the fallback). No window is ever
//! resized: `to_screen` covers any aspect from 16:9 up, refuses narrower.

use std::sync::Once;
use std::time::Duration;

use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
use windows_sys::Win32::System::SystemServices::MK_LBUTTON;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetClientRect, GetForegroundWindow, GetSystemMetrics, PostMessageW,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SetForegroundWindow, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
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

/// Null-terminated UTF-16, the shape W-suffixed Win32 calls want.
///
/// The buffer *is* the value: dropping it leaves the caller passing a
/// dangling `as_ptr()` to Win32.
#[must_use]
pub(super) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

/// Marks the process DPI-aware once, so client rects come back in physical
/// pixels. winit already sets this in gui builds; the console build needs it
/// here — a failed call means it was already set, which is what we want.
fn ensure_dpi_awareness() {
    static DPI: Once = Once::new();
    DPI.call_once(|| {
        // SAFETY: the argument is a well-known Win32 constant and the call
        // only flips process-global DPI state — it borrows nothing and hands
        // back nothing to keep alive. A refusal means the mode was already
        // set, which is the outcome this function wants anyway.
        unsafe {
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
    });
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
    fn find_game_window(&mut self) -> Result<isize, SurfaceError>;
    fn foreground_window(&mut self) -> isize;
    fn request_foreground(&mut self, hwnd: isize);
    fn client_rect(&mut self, hwnd: isize) -> Result<ClientRect, SurfaceError>;
    fn send(&mut self, event: InputEvent) -> Result<(), SurfaceError>;
    fn sleep(&mut self, duration: Duration);
}

struct SystemInputDriver;

impl InputDriver for SystemInputDriver {
    fn find_game_window(&mut self) -> Result<isize, SurfaceError> {
        ensure_dpi_awareness();
        find_game_window().map(|hwnd| hwnd as isize)
    }

    fn foreground_window(&mut self) -> isize {
        // SAFETY: no arguments, no ownership — the returned HWND (possibly
        // NULL) is only ever compared here, never dereferenced.
        (unsafe { GetForegroundWindow() }) as isize
    }

    fn request_foreground(&mut self, hwnd: isize) {
        // The refusal is dropped on purpose: `SetForegroundWindow` reports
        // FALSE both when the switch is denied and when it merely has not
        // happened yet, so its verdict is worthless. `ensure_foreground`
        // sleeps and then re-reads the actual foreground window — that read
        // is the authority, and it is the one that produces the error.
        // SAFETY: `hwnd` is the handle `acquire` found; the call validates it
        // itself and answers FALSE for a window that died in between.
        let _ = unsafe { SetForegroundWindow(hwnd as HWND) };
    }

    fn client_rect(&mut self, hwnd: isize) -> Result<ClientRect, SurfaceError> {
        client_rect(hwnd as HWND)
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

pub struct WinSurface {
    driver: Box<dyn InputDriver>,
    target: Option<Target>,
}

impl Default for WinSurface {
    fn default() -> Self {
        Self {
            driver: Box::new(SystemInputDriver),
            target: None,
        }
    }
}

impl WinSurface {
    #[cfg(test)]
    fn with_driver(driver: impl InputDriver + 'static) -> Self {
        Self {
            driver: Box::new(driver),
            target: None,
        }
    }

    fn target(&self) -> Result<Target, SurfaceError> {
        self.target.ok_or_else(|| {
            SurfaceError::Fatal("input attempted without an acquired game window".to_owned())
        })
    }

    fn ensure_foreground(&mut self, hwnd: isize) -> Result<(), SurfaceError> {
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
    fn validate_target(&mut self) -> Result<(), SurfaceError> {
        let target = self.target()?;
        let titled = self.driver.find_game_window()?;
        if titled != target.hwnd {
            return Err(SurfaceError::Fatal(
                "the game window title now identifies a different window".to_owned(),
            ));
        }

        let rect = self.driver.client_rect(target.hwnd)?;
        if rect.width <= 0 || rect.height <= 0 {
            return Err(SurfaceError::Recoverable(
                "the game window was minimized mid-job".to_owned(),
            ));
        }
        if rect != target.rect {
            return Err(SurfaceError::Recoverable(
                "the game window moved or resized mid-job".to_owned(),
            ));
        }

        self.ensure_foreground(target.hwnd)
    }

    fn send_guarded(&mut self, event: InputEvent) -> Result<(), SurfaceError> {
        self.validate_target()?;
        self.driver.send(event)
    }

    /// A successful down must always be paired with a release attempt. If the
    /// target has become unsafe, LEFTUP is the one permitted unguarded event:
    /// it cannot initiate a click and is safer than leaving the global button
    /// state held. Refusal is retried exactly once.
    fn release_after_down(&mut self) -> Result<(), SurfaceError> {
        let original = match self.validate_target() {
            Ok(()) => match self.driver.send(InputEvent::LeftUp) {
                Ok(()) => return Ok(()),
                Err(error) => error,
            },
            Err(error) => match self.driver.send(InputEvent::LeftUp) {
                Ok(()) => return Err(error),
                Err(_) => error,
            },
        };

        if self.driver.send(InputEvent::LeftUp).is_ok() {
            return Err(original);
        }
        Err(SurfaceError::Fatal(
            "left button state could not be proven released after two failed LEFTUP attempts"
                .to_owned(),
        ))
    }
}

impl Surface for WinSurface {
    fn acquire(&mut self) -> Result<ClientRect, SurfaceError> {
        self.target = None;
        let hwnd = self.driver.find_game_window()?;
        self.ensure_foreground(hwnd)?;
        let rect = self.driver.client_rect(hwnd)?;
        self.target = Some(Target { hwnd, rect });
        Ok(rect)
    }

    fn click(&mut self, at: (i32, i32), press_ms: u64) -> Result<(), SurfaceError> {
        self.send_guarded(InputEvent::Move(at))?;
        self.driver.sleep(Duration::from_millis(MOVE_SETTLE_MS));
        self.send_guarded(InputEvent::LeftDown)?;
        self.driver.sleep(Duration::from_millis(press_ms));
        self.release_after_down()
    }

    fn scroll(&mut self, at: (i32, i32), notches: i32) -> Result<(), SurfaceError> {
        self.send_guarded(InputEvent::Move(at))?;
        self.driver.sleep(Duration::from_millis(MOVE_SETTLE_MS));
        self.send_guarded(InputEvent::Wheel(notches))
    }

    fn release(&mut self) {
        self.target = None;
    }
}

/// No window at all: nothing to retry against — fatal.
fn find_game_window() -> Result<HWND, SurfaceError> {
    let title = wide(GAME_WINDOW_TITLE);
    // SAFETY: `title` is a NUL-terminated UTF-16 buffer that outlives the
    // call (`wide` appends the terminator), and a null class filter means
    // "any class". The returned handle is borrowed, not owned: nothing to
    // free, and NULL is the documented not-found answer.
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
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
    let dx = (i64::from(at.0) - i64::from(left)) * 65_535 / i64::from(width);
    let dy = (i64::from(at.1) - i64::from(top)) * 65_535 / i64::from(height);
    send_input(MOUSEINPUT {
        // Clamped, not truncated: a failed `GetSystemMetrics` reads 0, the
        // `.max(1)` above turns that into a width of 1, and the ratio then
        // leaves i32 entirely. Landing on the desktop edge is wrong but
        // bounded; a wrapped `as i32` would aim anywhere.
        dx: dx.clamp(0, 65_535) as i32,
        dy: dy.clamp(0, 65_535) as i32,
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
        mouseData: data as _,
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
    let inserted = unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) };
    sendinput_result(inserted)
}

/// `SendInput` returns the number of events inserted; we always send 1, so
/// anything else means the input was blocked (UIPI, foreground lock, full
/// queue). Recoverable: the watchdog re-issues against a fresh acquire.
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

/// `PostMessageW` returns a BOOL; false means the post failed (window gone,
/// queue full). Recoverable for the same reason.
fn postmessage_result(ok: bool) -> Result<(), SurfaceError> {
    if ok {
        Ok(())
    } else {
        Err(SurfaceError::Recoverable(format!(
            "PostMessageW failed — window gone or queue full ({})",
            std::io::Error::last_os_error()
        )))
    }
}

/// `PostMessageW` backend (the default): posts synthetic mouse messages to
/// the game window — no focus stolen, the player keeps the mouse. The engine
/// tracks its cursor through move messages, so every input re-asserts the
/// [`shield`](super::shield) over the game until [`release`](Surface).
#[derive(Default)]
pub struct MessageSurface {
    /// Job-scoped; the handle is stored as an integer so the executor's
    /// future stays `Send` across awaits.
    target: Option<Target>,
}

impl MessageSurface {
    /// The job-scoped target. An input attempted without a prior `acquire()`
    /// is a contract violation, not a fault of the world: it answers with the
    /// same [`SurfaceError::Fatal`] `WinSurface::target` raises rather than
    /// panicking — a panic here would kill the actuator task and take the
    /// whole session down, where `Fatal` stops the loop with a real reason.
    fn target(&self) -> Result<Target, SurfaceError> {
        self.target.ok_or_else(|| {
            SurfaceError::Fatal("input attempted without an acquired game window".to_owned())
        })
    }

    fn cleanup(&mut self) {
        self.target = None;
        shield::hide();
    }
}

impl Drop for MessageSurface {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[derive(Clone, Copy)]
struct Target {
    hwnd: isize,
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
        if shield::raise(self.hwnd as HWND, self.rect).map_err(SurfaceError::Fatal)? {
            std::thread::sleep(Duration::from_millis(SHIELD_DRAIN_MS));
        }
        Ok(())
    }

    fn verify(self) -> Result<(), SurfaceError> {
        let rect = client_rect(self.hwnd as HWND)?;
        if rect == self.rect {
            return Ok(());
        }
        // The window is alive, just elsewhere: the next job's acquire()
        // re-reads a fresh rect, so the watchdog's retry self-heals this.
        Err(SurfaceError::Recoverable(
            if rect.width <= 0 || rect.height <= 0 {
                "the game window was minimized mid-job".to_owned()
            } else {
                "the game window moved or resized mid-job".to_owned()
            },
        ))
    }
}

impl Surface for MessageSurface {
    fn acquire(&mut self) -> Result<ClientRect, SurfaceError> {
        ensure_dpi_awareness();
        let hwnd = find_game_window()?;
        let rect = client_rect(hwnd)?;
        self.target = Some(Target {
            hwnd: hwnd as isize,
            rect,
        });
        Ok(rect)
    }

    fn click(&mut self, at: (i32, i32), press_ms: u64) -> Result<(), SurfaceError> {
        let target = self.target()?;
        target.engage()?;
        let lparam = pack_point(target.to_client(at))?;
        post(target.hwnd, WM_MOUSEMOVE, 0, lparam)?;
        std::thread::sleep(Duration::from_millis(MOVE_SETTLE_MS));
        post(target.hwnd, WM_LBUTTONDOWN, MK_LBUTTON as usize, lparam)?;
        std::thread::sleep(Duration::from_millis(press_ms));
        // Button is down: always post the release, retrying once on failure so a
        // refused click never leaves the game seeing a held left button.
        if let Err(original) = post(target.hwnd, WM_LBUTTONUP, 0, lparam) {
            // The retry's own verdict decides which error is told: same rule
            // as `WinSurface::release_after_down`. If both posts fail the
            // game is left holding the button, and *that* — not the first
            // refusal — is the fault worth reporting.
            if post(target.hwnd, WM_LBUTTONUP, 0, lparam).is_ok() {
                return Err(original);
            }
            return Err(SurfaceError::Fatal(
                "left button state could not be proven released after two failed WM_LBUTTONUP posts"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    fn scroll(&mut self, at: (i32, i32), notches: i32) -> Result<(), SurfaceError> {
        let target = self.target()?;
        target.engage()?;
        post(
            target.hwnd,
            WM_MOUSEMOVE,
            0,
            pack_point(target.to_client(at))?,
        )?;
        std::thread::sleep(Duration::from_millis(MOVE_SETTLE_MS));
        // WM_MOUSEWHEEL takes screen coordinates; the delta rides wParam's
        // high word.
        let delta = notches.saturating_mul(WHEEL_DELTA);
        post(
            target.hwnd,
            WM_MOUSEWHEEL,
            ((delta as u32) << 16) as usize,
            pack_point(at)?,
        )?;
        Ok(())
    }

    fn release(&mut self) {
        self.cleanup();
    }
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
/// only reads the low 32 bits, so the old form worked by accident; this one
/// builds the value the doc comment promises.
fn pack_point((x, y): (i32, i32)) -> Result<isize, SurfaceError> {
    let x = i16::try_from(x)
        .map_err(|_| SurfaceError::Recoverable(format!("x {x} out of LPARAM range")))?;
    let y = i16::try_from(y)
        .map_err(|_| SurfaceError::Recoverable(format!("y {y} out of LPARAM range")))?;
    let packed = (u32::from(y as u16) << 16) | u32::from(x as u16);
    Ok(packed as i32 as isize)
}

fn post(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> Result<(), SurfaceError> {
    // SAFETY: `hwnd` may already be dead — `PostMessageW` reports that with
    // FALSE instead of faulting. Delivery is asynchronous, so nothing may be
    // borrowed by the queue: `wparam`/`lparam` carry packed coordinates and
    // button flags only, never a pointer into this process.
    let ok = unsafe { PostMessageW(hwnd as HWND, msg, wparam, lparam) } != 0;
    postmessage_result(ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    const GAME_HWND: isize = 101;
    const OTHER_HWND: isize = 202;

    #[test]
    fn message_surface_cleanup_is_idempotent_without_a_window() {
        let mut surface = MessageSurface::default();

        surface.release();
        surface.release();

        assert!(surface.target.is_none());
        drop(surface);
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum DriverCall {
        FindWindow,
        Foreground,
        RequestForeground(isize),
        ClientRect(isize),
        Send(InputEvent),
        Sleep(u64),
    }

    struct FakeState {
        calls: Vec<DriverCall>,
        window: isize,
        foreground: isize,
        rect: ClientRect,
        find_results: VecDeque<Result<isize, SurfaceError>>,
        foreground_results: VecDeque<isize>,
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
        fn find_game_window(&mut self) -> Result<isize, SurfaceError> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(DriverCall::FindWindow);
            if let Some(result) = state.find_results.pop_front() {
                result
            } else {
                Ok(state.window)
            }
        }

        fn foreground_window(&mut self) -> isize {
            let mut state = self.state.lock().unwrap();
            state.calls.push(DriverCall::Foreground);
            if let Some(hwnd) = state.foreground_results.pop_front() {
                hwnd
            } else {
                state.foreground
            }
        }

        fn request_foreground(&mut self, hwnd: isize) {
            self.state
                .lock()
                .unwrap()
                .calls
                .push(DriverCall::RequestForeground(hwnd));
        }

        fn client_rect(&mut self, hwnd: isize) -> Result<ClientRect, SurfaceError> {
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

    fn acquire_and_clear(surface: &mut WinSurface, state: &Arc<Mutex<FakeState>>) {
        assert_eq!(surface.acquire(), Ok(game_rect()));
        state.lock().unwrap().calls.clear();
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
    fn acquire_stores_the_exact_target_and_focuses_it() {
        let (mut surface, state) = fake_surface();
        state
            .lock()
            .unwrap()
            .foreground_results
            .extend([OTHER_HWND, GAME_HWND]);

        assert_eq!(surface.acquire(), Ok(game_rect()));
        let target = surface.target.expect("target stored");
        assert_eq!(target.hwnd, GAME_HWND);
        assert_eq!(target.rect, game_rect());
        assert_eq!(
            calls(&state),
            vec![
                DriverCall::FindWindow,
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
        acquire_and_clear(&mut surface, &state);

        assert_eq!(surface.click((400, 500), 25), Ok(()));
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
        acquire_and_clear(&mut surface, &state);

        assert_eq!(surface.scroll((300, 600), -2), Ok(()));
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
        acquire_and_clear(&mut surface, &state);
        state.lock().unwrap().window = OTHER_HWND;

        assert!(matches!(
            surface.click((1, 2), 3),
            Err(SurfaceError::Fatal(reason)) if reason.contains("different window")
        ));
        assert_eq!(calls(&state), vec![DriverCall::FindWindow]);
    }

    #[test]
    fn missing_title_matched_window_is_fatal_before_input() {
        let (mut surface, state) = fake_surface();
        acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .find_results
            .push_back(Err(SurfaceError::Fatal("game window missing".to_owned())));

        assert!(matches!(
            surface.scroll((1, 2), 1),
            Err(SurfaceError::Fatal(reason)) if reason.contains("missing")
        ));
        assert_eq!(calls(&state), vec![DriverCall::FindWindow]);
    }

    #[test]
    fn moved_rect_is_recoverable_before_input() {
        let (mut surface, state) = fake_surface();
        acquire_and_clear(&mut surface, &state);
        state.lock().unwrap().rect = moved_rect();

        assert!(matches!(
            surface.click((1, 2), 3),
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
        acquire_and_clear(&mut surface, &state);
        state.lock().unwrap().rect.width = 0;

        assert!(matches!(
            surface.scroll((1, 2), 1),
            Err(SurfaceError::Recoverable(reason)) if reason.contains("minimized")
        ));
        assert!(sent_events(&state).is_empty());
    }

    #[test]
    fn dead_stored_window_is_fatal_before_input() {
        let (mut surface, state) = fake_surface();
        acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .rect_results
            .push_back(Err(dead_window()));

        assert!(matches!(
            surface.click((1, 2), 3),
            Err(SurfaceError::Fatal(reason)) if reason.contains("client area")
        ));
        assert!(sent_events(&state).is_empty());
    }

    #[test]
    fn lost_focus_is_restored_and_verified_before_input() {
        let (mut surface, state) = fake_surface();
        acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .foreground_results
            .extend([OTHER_HWND, GAME_HWND]);

        assert_eq!(surface.scroll((30, 40), 1), Ok(()));
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
        acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .foreground_results
            .extend([OTHER_HWND, OTHER_HWND]);

        assert!(matches!(
            surface.scroll((30, 40), 1),
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
        acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .foreground_results
            .extend([GAME_HWND, OTHER_HWND, OTHER_HWND]);

        assert!(matches!(
            surface.click((30, 40), 5),
            Err(SurfaceError::Fatal(_))
        ));
        assert_eq!(sent_events(&state), vec![InputEvent::Move((30, 40))]);
    }

    #[test]
    fn release_clears_target_and_actions_fail_closed() {
        let (mut surface, state) = fake_surface();
        acquire_and_clear(&mut surface, &state);
        surface.release();

        assert!(matches!(
            surface.click((1, 2), 3),
            Err(SurfaceError::Fatal(reason)) if reason.contains("without an acquired")
        ));
        assert!(matches!(
            surface.scroll((1, 2), 1),
            Err(SurfaceError::Fatal(reason)) if reason.contains("without an acquired")
        ));
        assert!(calls(&state).is_empty());
    }

    #[test]
    fn send_refusal_before_left_down_never_synthesizes_left_up() {
        let (mut surface, state) = fake_surface();
        acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .send_results
            .extend([Ok(()), Err(blocked_input())]);

        assert!(matches!(
            surface.click((30, 40), 5),
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
        acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .foreground_results
            .extend([GAME_HWND, GAME_HWND, OTHER_HWND, GAME_HWND]);

        assert_eq!(surface.click((30, 40), 5), Ok(()));
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
        acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .foreground_results
            .extend([GAME_HWND, GAME_HWND, OTHER_HWND, OTHER_HWND]);

        assert!(matches!(
            surface.click((30, 40), 5),
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
        acquire_and_clear(&mut surface, &state);
        state.lock().unwrap().rect_results.extend([
            Ok(game_rect()),
            Ok(game_rect()),
            Ok(moved_rect()),
        ]);

        assert!(matches!(
            surface.click((30, 40), 5),
            Err(SurfaceError::Recoverable(reason)) if reason.contains("moved or resized")
        ));
        assert_eq!(sent_events(&state).last(), Some(&InputEvent::LeftUp));
    }

    #[test]
    fn minimized_rect_after_left_down_still_sends_unguarded_left_up() {
        let (mut surface, state) = fake_surface();
        acquire_and_clear(&mut surface, &state);
        let mut minimized = game_rect();
        minimized.height = 0;
        state.lock().unwrap().rect_results.extend([
            Ok(game_rect()),
            Ok(game_rect()),
            Ok(minimized),
        ]);

        assert!(matches!(
            surface.click((30, 40), 5),
            Err(SurfaceError::Recoverable(reason)) if reason.contains("minimized")
        ));
        assert_eq!(sent_events(&state).last(), Some(&InputEvent::LeftUp));
    }

    #[test]
    fn refused_cleanup_left_up_is_retried_once() {
        let (mut surface, state) = fake_surface();
        acquire_and_clear(&mut surface, &state);
        {
            let mut fake = state.lock().unwrap();
            fake.foreground_results
                .extend([GAME_HWND, GAME_HWND, OTHER_HWND, OTHER_HWND]);
            fake.send_results
                .extend([Ok(()), Ok(()), Err(blocked_input()), Ok(())]);
        }

        assert!(matches!(
            surface.click((30, 40), 5),
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
        acquire_and_clear(&mut surface, &state);
        {
            let mut fake = state.lock().unwrap();
            fake.foreground_results
                .extend([GAME_HWND, GAME_HWND, OTHER_HWND, OTHER_HWND]);
            fake.send_results
                .extend([Ok(()), Ok(()), Err(blocked_input()), Err(blocked_input())]);
        }

        assert!(matches!(
            surface.click((30, 40), 5),
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
        acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .send_results
            .extend([Ok(()), Ok(()), Err(blocked_input()), Ok(())]);

        assert!(matches!(
            surface.click((30, 40), 5),
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

    #[test]
    fn postmessage_true_is_ok() {
        assert!(postmessage_result(true).is_ok());
    }

    #[test]
    fn postmessage_false_is_recoverable() {
        assert!(matches!(
            postmessage_result(false),
            Err(SurfaceError::Recoverable(_))
        ));
    }

    /// The default backend must fail closed exactly like `WinSurface`: a
    /// panic here would kill the actuator task and end the session.
    #[test]
    fn message_surface_input_without_acquire_is_fatal_not_a_panic() {
        let mut surface = MessageSurface::default();

        assert!(matches!(
            surface.click((1, 2), 3),
            Err(SurfaceError::Fatal(reason)) if reason.contains("without an acquired")
        ));
        assert!(matches!(
            surface.scroll((1, 2), 1),
            Err(SurfaceError::Fatal(reason)) if reason.contains("without an acquired")
        ));
    }

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
