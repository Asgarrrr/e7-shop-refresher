//! Invisible input shield: a layered, non-activating window kept directly
//! above the game in the Z-order while a job posts its clicks.
//!
//! Windows routes real mouse messages to the window under the cursor, so the
//! shield absorbs the player's moves and clicks over the game area while the
//! posted messages — addressed to the game's handle — pass it entirely. The
//! engine's tracked cursor is therefore driven only by the job. The shield is
//! never anchored on a foreign window (that would catch Win32's topmost
//! contagion and float it above the player's own windows): it swaps into the
//! game's own Z-slot, so everything the player keeps above the game stays
//! fully usable.

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

/// The shield window, created on demand on its own pump thread. Nothing is
/// cached on failure and a dead window is dropped and recreated: a transient
/// setup failure must not brick the backend for the process lifetime.
static WINDOW: Mutex<Option<isize>> = Mutex::new(None);

/// Ensures the shield sits directly above the game, covering `rect`.
/// `Ok(true)` means it had to be (re)placed — until that instant the game was
/// still receiving the player's real mouse, and the caller must let it drain
/// before posting.
pub(super) fn raise(game: HWND, rect: ClientRect) -> Result<bool, String> {
    let shield = handle()? as HWND;
    // Already slotted right above the game and visible: nothing was routed
    // to the game since the last input, skip the swap and the drain.
    if unsafe { GetWindow(game, GW_HWNDPREV) } == shield && unsafe { IsWindowVisible(shield) } != 0
    {
        return Ok(false);
    }
    // Two-step swap anchored only on the game itself: shield first drops
    // directly below it, then the game slips under the shield. Anchoring on
    // whatever sits above the game instead would catch topmost contagion
    // (SetWindowPos makes a window topmost when its anchor is) — the shield
    // would then eat the mouse above ALL the player's windows.
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

/// Lowers the shield; a shield that was never created is a no-op, never a
/// creation attempt.
pub(super) fn hide() {
    if let Some(shield) = *WINDOW.lock().expect("shield mutex poisoned") {
        unsafe { ShowWindow(shield as HWND, SW_HIDE) };
    }
}

/// The current shield window, creating it if missing and dropping a handle
/// whose pump thread died (Windows destroys a thread's windows with it).
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

/// Creates the window on a dedicated thread that pumps its messages forever:
/// an unpumped window would count as hung precisely while it absorbs the
/// player's input.
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

/// Registers the window class; the class outlives any window (and its
/// thread), so a recreation finding it already registered is a success.
fn register_class(class: &[u16]) -> Result<(), String> {
    let wndclass = WNDCLASSW {
        lpfnWndProc: Some(shield_proc),
        hInstance: unsafe { GetModuleHandleW(std::ptr::null()) },
        lpszClassName: class.as_ptr(),
        // The surface must be painted: hit-testing a layered window follows
        // its content, and an unpainted one can read as fully transparent —
        // the player's mouse would fall straight through to the game.
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
    // Alpha 1 over the painted surface: visually nothing, yet hit-testable —
    // alpha 0 would let the player's mouse fall through to the game.
    if unsafe { SetLayeredWindowAttributes(hwnd, 0, 1, LWA_ALPHA) } == 0 {
        return Err("could not set the shield transparency".to_owned());
    }
    Ok(hwnd as isize)
}

/// The shield never reacts: every message — the absorbed mouse traffic
/// included — takes the default path.
extern "system" fn shield_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn pump() {
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe { DispatchMessageW(&msg) };
    }
}
