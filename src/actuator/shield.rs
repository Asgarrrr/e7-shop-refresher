//! Invisible, non-activating window kept directly above the game while a job
//! posts its clicks: real mouse messages route to the window under the
//! cursor, so the shield absorbs the player's mouse over the game while the
//! posted messages — addressed to the game's handle — pass untouched.

use std::sync::Mutex;

use windows_sys::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{BLACK_BRUSH, GetStockObject};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GW_HWNDPREV, GetMessageW, GetWindow,
    IsWindow, IsWindowVisible, LWA_ALPHA, MSG, RegisterClassW, SW_HIDE, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetWindowPos, ShowWindow, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

use super::plan::ClientRect;
use super::win::wide;

const CLASS_NAME: &str = "arkyve-refresh-shop-shield";

/// Never caches a failure: a transient setup error must not brick the
/// backend for the process lifetime.
static WINDOW: Mutex<Option<isize>> = Mutex::new(None);

/// Ensures the shield sits directly above the game, covering `rect`;
/// `Ok(true)` means it was (re)placed — the game may still hold real moves.
pub(super) fn raise(game: HWND, rect: ClientRect) -> Result<bool, String> {
    let shield = handle()? as HWND;
    if unsafe { GetWindow(game, GW_HWNDPREV) } == shield && unsafe { IsWindowVisible(shield) } != 0
    {
        return Ok(false);
    }
    // Two-step swap into the game's own Z-slot: anchoring on the window
    // above it instead would catch Win32's topmost contagion.
    let placed = unsafe {
        SetWindowPos(
            shield,
            game,
            rect.left,
            rect.top,
            rect.width,
            rect.height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    };
    if placed == 0 {
        return Err("could not raise the input shield".to_owned());
    }
    let swapped = unsafe {
        SetWindowPos(
            game,
            shield,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    };
    if swapped == 0 {
        return Err("could not slot the game under the input shield".to_owned());
    }
    Ok(true)
}

/// Lowers the shield if it exists — never creates one.
pub(super) fn hide() {
    if let Some(shield) = *WINDOW.lock().expect("shield mutex poisoned") {
        unsafe { ShowWindow(shield as HWND, SW_HIDE) };
    }
}

/// Current window, recreated when missing or dead (a window dies with its
/// pump thread).
fn handle() -> Result<isize, String> {
    let mut window = WINDOW.lock().expect("shield mutex poisoned");
    if let Some(hwnd) = *window {
        if unsafe { IsWindow(hwnd as HWND) } != 0 {
            return Ok(hwnd);
        }
        *window = None;
    }
    let hwnd = spawn_window()?;
    *window = Some(hwnd);
    Ok(hwnd)
}

/// The window lives on its own pumping thread: unpumped, it would count as
/// hung precisely while it absorbs the player's input.
fn spawn_window() -> Result<isize, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("shield".to_owned())
        .spawn(move || {
            let created = create_window();
            let run = created.is_ok();
            let _ = tx.send(created);
            if run {
                pump();
            }
        });
    if spawned.is_err() {
        return Err("could not start the shield thread".to_owned());
    }
    rx.recv()
        .unwrap_or_else(|_| Err("the shield thread died during setup".to_owned()))
}

/// The class outlives its windows: already-registered is success on
/// recreation.
fn register_class(class: &[u16]) -> Result<(), String> {
    let wndclass = WNDCLASSW {
        lpfnWndProc: Some(shield_proc),
        hInstance: unsafe { GetModuleHandleW(std::ptr::null()) },
        lpszClassName: class.as_ptr(),
        // Hit-testing follows the painted content: unpainted, the layered
        // window would let the mouse fall through to the game.
        hbrBackground: unsafe { GetStockObject(BLACK_BRUSH) },
        ..Default::default()
    };
    if unsafe { RegisterClassW(&wndclass) } == 0
        && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS
    {
        return Err("could not register the shield window class".to_owned());
    }
    Ok(())
}

fn create_window() -> Result<isize, String> {
    let class = wide(CLASS_NAME);
    register_class(&class)?;
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class.as_ptr(),
            std::ptr::null(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err("could not create the shield window".to_owned());
    }
    // Alpha 1: invisible yet hit-testable; alpha 0 would be click-through.
    if unsafe { SetLayeredWindowAttributes(hwnd, 0, 1, LWA_ALPHA) } == 0 {
        return Err("could not set the shield transparency".to_owned());
    }
    Ok(hwnd as isize)
}

extern "system" fn shield_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn pump() {
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe { DispatchMessageW(&msg) };
    }
}
