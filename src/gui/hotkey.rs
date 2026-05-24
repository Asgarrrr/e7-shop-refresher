//! Global keyboard shortcut for emergency Stop.
//!
//! Win32 `RegisterHotKey` lets a key combo fire regardless of which app
//! has focus — important for a bot whose primary failure mode is "I
//! can't reach the GUI because the game stole my mouse". The pressed
//! flag is polled by the GUI thread in its update loop and translated
//! into a normal `stop_bot()` call, so the same code path handles
//! mouse-button Stop and hotkey Stop.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use tracing::{info, warn};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{MOD_CONTROL, RegisterHotKey, UnregisterHotKey};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

/// Arbitrary positive ID. Must be unique within the process.
const STOP_HOTKEY_ID: i32 = 1;

/// `VK_7` — the digit `7` on the main row. Combined with `MOD_CONTROL`
/// = Ctrl+7. Picked because Epic Seven does not bind Ctrl+digit and
/// it nods to the game name (mostly for ergonomics: easy to remember,
/// reachable one-handed, unlikely to clash with system shortcuts).
const VK_7: u32 = 0x37;

/// Spawns a side thread that owns a Win32 message queue, registers
/// Ctrl+7 as a system-wide hotkey, and flips the returned flag every
/// time the combo is pressed. The GUI polls this flag in `update()`.
///
/// Registration can fail if another process already owns the same
/// combo — we log a warning and return a flag that never flips. The
/// GUI Stop button stays usable, so the loss is just the hotkey
/// convenience, not core functionality.
pub fn spawn_stop_hotkey(ctx: egui::Context) -> Arc<AtomicBool> {
    let pressed = Arc::new(AtomicBool::new(false));
    let pressed_thread = Arc::clone(&pressed);

    thread::Builder::new()
        .name("stop-hotkey".into())
        .spawn(move || run_hotkey_loop(ctx, pressed_thread))
        .expect("spawn stop-hotkey thread");

    pressed
}

fn run_hotkey_loop(ctx: egui::Context, pressed: Arc<AtomicBool>) {
    // SAFETY: RegisterHotKey is a thread-affine call — the messages
    // are delivered to the thread that registered, so this MUST run
    // in the same thread that pumps GetMessageW below.
    let registered =
        unsafe { RegisterHotKey(Some(HWND::default()), STOP_HOTKEY_ID, MOD_CONTROL, VK_7) };
    if registered.is_err() {
        warn!("could not register Ctrl+7 stop hotkey — another process likely owns it");
        return;
    }
    info!("Ctrl+7 stop hotkey registered");

    let mut msg = MSG::default();
    loop {
        // SAFETY: GetMessageW blocks until a message arrives. Returns
        // 0 on WM_QUIT, -1 on error, otherwise positive. We never post
        // WM_QUIT to this thread (the OS reaps the thread on process
        // exit), so the loop only exits on the -1 error path.
        let rc = unsafe { GetMessageW(&mut msg, Some(HWND::default()), 0, 0) };
        if rc.0 <= 0 {
            break;
        }
        if msg.message == WM_HOTKEY && msg.wParam.0 as i32 == STOP_HOTKEY_ID {
            pressed.store(true, Ordering::Relaxed);
            // Wake the GUI immediately — without this the flag would
            // only be observed at the next natural repaint, which can
            // be 250ms+ when the bot isn't actively repainting.
            ctx.request_repaint();
        }
    }

    let _ = unsafe { UnregisterHotKey(Some(HWND::default()), STOP_HOTKEY_ID) };
}
