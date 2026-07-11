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
use super::{Surface, shield};

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
pub(super) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

/// Marks the process DPI-aware once, so client rects come back in physical
/// pixels. winit already sets this in gui builds; the console build needs it
/// here — a failed call means it was already set, which is what we want.
fn ensure_dpi_awareness() {
    static DPI: Once = Once::new();
    DPI.call_once(|| unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    });
}

pub struct WinSurface;

impl Surface for WinSurface {
    fn acquire(&mut self) -> Result<ClientRect, String> {
        ensure_dpi_awareness();
        let hwnd = find_game_window()?;
        focus(hwnd)?;
        client_rect(hwnd)
    }

    fn click(&mut self, at: (i32, i32), press_ms: u64) -> Result<(), String> {
        move_cursor(at);
        std::thread::sleep(Duration::from_millis(MOVE_SETTLE_MS));
        send_mouse(0, MOUSEEVENTF_LEFTDOWN);
        std::thread::sleep(Duration::from_millis(press_ms));
        send_mouse(0, MOUSEEVENTF_LEFTUP);
        Ok(())
    }

    fn scroll(&mut self, at: (i32, i32), notches: i32) -> Result<(), String> {
        move_cursor(at);
        std::thread::sleep(Duration::from_millis(MOVE_SETTLE_MS));
        send_mouse(notches.saturating_mul(WHEEL_DELTA), MOUSEEVENTF_WHEEL);
        Ok(())
    }
}

fn find_game_window() -> Result<HWND, String> {
    let title = wide(GAME_WINDOW_TITLE);
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if hwnd.is_null() {
        return Err(format!("no \"{GAME_WINDOW_TITLE}\" window found"));
    }
    Ok(hwnd)
}

/// The game must own the foreground: `SendInput` lands wherever the focus
/// is, and another window must never receive the game's clicks. A refused
/// foreground switch (Windows foreground lock) is therefore a hard error.
fn focus(hwnd: HWND) -> Result<(), String> {
    if unsafe { GetForegroundWindow() } == hwnd {
        return Ok(());
    }
    let _ = unsafe { SetForegroundWindow(hwnd) };
    std::thread::sleep(Duration::from_millis(FOCUS_SETTLE_MS));
    if unsafe { GetForegroundWindow() } != hwnd {
        return Err("could not focus the game window".to_owned());
    }
    Ok(())
}

fn client_rect(hwnd: HWND) -> Result<ClientRect, String> {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { GetClientRect(hwnd, &mut rect) } == 0 {
        return Err("could not read the game window's client area".to_owned());
    }
    let mut origin = POINT { x: 0, y: 0 };
    if unsafe { ClientToScreen(hwnd, &mut origin) } == 0 {
        return Err("could not locate the game window on screen".to_owned());
    }
    Ok(ClientRect {
        left: origin.x,
        top: origin.y,
        width: rect.right,
        height: rect.bottom,
    })
}

/// Absolute cursor move, normalized to the virtual desktop so multi-monitor
/// setups resolve the same physical pixel.
fn move_cursor(at: (i32, i32)) {
    let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(1);
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(1);
    let dx = i64::from(at.0 - left) * 65_535 / i64::from(width);
    let dy = i64::from(at.1 - top) * 65_535 / i64::from(height);
    send_input(MOUSEINPUT {
        dx: dx as i32,
        dy: dy as i32,
        mouseData: 0,
        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        time: 0,
        dwExtraInfo: 0,
    });
}

fn send_mouse(data: i32, flags: u32) {
    send_input(MOUSEINPUT {
        dx: 0,
        dy: 0,
        mouseData: data as _,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    });
}

fn send_input(mi: MOUSEINPUT) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi },
    };
    unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) };
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

#[derive(Clone, Copy)]
struct Target {
    hwnd: isize,
    /// Client area in screen pixels.
    rect: ClientRect,
}

impl Target {
    fn to_client(self, at: (i32, i32)) -> (i32, i32) {
        (at.0 - self.rect.left, at.1 - self.rect.top)
    }

    /// Before every input: the window must be where the job planned it and
    /// the shield seated above it; a (re)placed shield gets the drain beat.
    fn engage(self) -> Result<(), String> {
        self.verify()?;
        if shield::raise(self.hwnd as HWND, self.rect)? {
            std::thread::sleep(Duration::from_millis(SHIELD_DRAIN_MS));
        }
        Ok(())
    }

    fn verify(self) -> Result<(), String> {
        let rect = client_rect(self.hwnd as HWND)?;
        if rect == self.rect {
            return Ok(());
        }
        Err(if rect.width <= 0 || rect.height <= 0 {
            "the game window was minimized mid-job".to_owned()
        } else {
            "the game window moved or resized mid-job".to_owned()
        })
    }
}

impl Surface for MessageSurface {
    fn acquire(&mut self) -> Result<ClientRect, String> {
        ensure_dpi_awareness();
        let hwnd = find_game_window()?;
        let rect = client_rect(hwnd)?;
        self.target = Some(Target {
            hwnd: hwnd as isize,
            rect,
        });
        Ok(rect)
    }

    fn click(&mut self, at: (i32, i32), press_ms: u64) -> Result<(), String> {
        let target = self.target.expect("acquire() before click");
        target.engage()?;
        let lparam = pack_point(target.to_client(at));
        post(target.hwnd, WM_MOUSEMOVE, 0, lparam);
        std::thread::sleep(Duration::from_millis(MOVE_SETTLE_MS));
        post(target.hwnd, WM_LBUTTONDOWN, MK_LBUTTON as usize, lparam);
        std::thread::sleep(Duration::from_millis(press_ms));
        post(target.hwnd, WM_LBUTTONUP, 0, lparam);
        Ok(())
    }

    fn scroll(&mut self, at: (i32, i32), notches: i32) -> Result<(), String> {
        let target = self.target.expect("acquire() before scroll");
        target.engage()?;
        post(
            target.hwnd,
            WM_MOUSEMOVE,
            0,
            pack_point(target.to_client(at)),
        );
        std::thread::sleep(Duration::from_millis(MOVE_SETTLE_MS));
        // WM_MOUSEWHEEL takes screen coordinates; the delta rides wParam's
        // high word.
        let delta = notches.saturating_mul(WHEEL_DELTA);
        post(
            target.hwnd,
            WM_MOUSEWHEEL,
            ((delta as u32) << 16) as usize,
            pack_point(at),
        );
        Ok(())
    }

    fn release(&mut self) {
        shield::hide();
    }
}

/// `MAKELPARAM`: x in the low word, y in the high word, both signed 16-bit.
fn pack_point((x, y): (i32, i32)) -> isize {
    (((y & 0xFFFF) << 16) | (x & 0xFFFF)) as isize
}

fn post(hwnd: isize, msg: u32, wparam: usize, lparam: isize) {
    unsafe { PostMessageW(hwnd as HWND, msg, wparam, lparam) };
}
