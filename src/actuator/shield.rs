//! Invisible input shield: a layered, non-activating window slotted into the
//! Z-order directly above the game while a job posts its clicks.
//!
//! Windows routes real mouse messages to the window under the cursor, so the
//! shield absorbs the player's moves and clicks over the game area while the
//! posted messages — addressed to the game's handle — pass it entirely. The
//! engine's tracked cursor is therefore driven only by the job. Not topmost:
//! the player's own windows above the game stay fully usable.

use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GW_HWNDPREV, GetMessageW, GetWindow,
    HWND_TOP, LWA_ALPHA, MSG, RegisterClassW, SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW,
    SetLayeredWindowAttributes, SetWindowPos, ShowWindow, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

use super::plan::ClientRect;

const CLASS_NAME: &str = "arkyve-refresh-shop-shield";

/// The shield window, created once per process on its own pump thread; the
/// stored error makes a failed setup fail every raise the same way.
static SHIELD: OnceLock<Result<isize, String>> = OnceLock::new();

/// Raises the shield over `rect`, directly above `game` in the Z-order.
pub(super) fn raise(game: HWND, rect: ClientRect) -> Result<(), String> {
    let shield = SHIELD.get_or_init(spawn_shield).clone()? as HWND;
    // SetWindowPos places the window *below* the anchor, so the window
    // currently above the game is the anchor; a game already on top anchors
    // to HWND_TOP. Anything later raised above the shield occludes the game
    // too — the "game receives no real mouse input" invariant survives.
    let above = unsafe { GetWindow(game, GW_HWNDPREV) };
    let anchor = if above.is_null() { HWND_TOP } else { above };
    let moved = unsafe {
        SetWindowPos(
            shield,
            anchor,
            rect.left,
            rect.top,
            rect.width,
            rect.height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    };
    if moved == 0 {
        return Err("could not raise the input shield".to_owned());
    }
    Ok(())
}

/// Lowers the shield; a shield that was never created is a no-op, never a
/// second creation attempt.
pub(super) fn hide() {
    if let Some(Ok(shield)) = SHIELD.get() {
        unsafe { ShowWindow(*shield as HWND, SW_HIDE) };
    }
}

/// Creates the window on a dedicated thread that pumps its messages forever:
/// an unpumped window would count as hung precisely while it absorbs the
/// player's input.
fn spawn_shield() -> Result<isize, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("shield".to_owned())
        .spawn(move || {
            let created = create_window();
            let ok = created.is_ok();
            let _ = tx.send(created);
            if ok {
                pump();
            }
        });
    if spawned.is_err() {
        return Err("could not start the shield thread".to_owned());
    }
    rx.recv()
        .unwrap_or_else(|_| Err("the shield thread died during setup".to_owned()))
}

fn create_window() -> Result<isize, String> {
    let class: Vec<u16> = CLASS_NAME.encode_utf16().chain([0]).collect();
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    let wndclass = WNDCLASSW {
        lpfnWndProc: Some(shield_proc),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&wndclass) } == 0 {
        return Err("could not register the shield window class".to_owned());
    }
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
    // Alpha 1: visually nothing, yet hit-testable — alpha 0 would let the
    // player's mouse fall through to the game.
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
