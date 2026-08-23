//! Invisible, non-activating window kept directly above the game while a job
//! posts its clicks: real mouse messages route to the window under the
//! cursor, so the shield absorbs the player's mouse over the game while the
//! posted messages — addressed to the game's handle — pass untouched.

use std::sync::Mutex;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{BLACK_BRUSH, GetStockObject};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GW_HWNDPREV, GetMessageW, GetWindow,
    IsWindow, IsWindowVisible, LWA_ALPHA, MSG, RegisterClassW, SW_HIDE, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetWindowPos, ShowWindow, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

use super::lock;
use super::plan::ClientRect;
use super::win::{Hwnd, wide};

const CLASS_NAME: &str = "arkyve-refresh-shop-shield";

/// Never caches a failure: a transient setup error must not brick the
/// backend for the process lifetime.
static WINDOW: Mutex<Option<Hwnd>> = Mutex::new(None);

// Poisoning carries no meaning here: the guarded state is a plain handle, so a
// panic elsewhere must not turn every later click into a fatal. That is the
// actuator-wide policy, hence `super::lock` rather than a second copy here.

/// Ensures the shield sits directly above the game, covering `rect`;
/// `Ok(true)` means it was (re)placed — the game may still hold real moves.
///
/// # Errors
///
/// The message a refused `SetWindowPos` deserves, from
/// [`placement_refusal`] — an integrity-level mismatch named as one, anything
/// else as the plain action that failed plus the OS error. Also carries the
/// reason the shield window could not be created or its pump thread started.
/// Every one of them is fatal to the caller: never click shieldless.
pub(super) fn raise(game: Hwnd, rect: ClientRect) -> Result<bool, String> {
    let shield = handle()?;
    // SAFETY: `game` is the handle the caller revalidated through
    // `Target::verify` just before this call, and `GetWindow` is defined over
    // any HWND — a window that died in between returns NULL, which reads as
    // "the shield is not directly above it" instead of faulting. Nothing is
    // borrowed past the call: the result is only compared.
    let directly_above = unsafe { GetWindow(game.raw(), GW_HWNDPREV) } == shield.raw();
    // SAFETY: `IsWindowVisible` is defined over any HWND — it validates the
    // handle itself, only reports, and answers 0 for one it does not know. The
    // shield's aliveness is deliberately *not* claimed here: it dies with its
    // pump thread, which can exit at any moment after `handle()` returns, so no
    // caller of `raise` could hold that precondition.
    let visible = unsafe { IsWindowVisible(shield.raw()) } != 0;
    if directly_above && visible {
        return Ok(false);
    }
    // Two-step swap into the game's own Z-slot: anchoring on the window
    // above it instead would catch Win32's topmost contagion.
    // SAFETY: `SetWindowPos` validates both handles itself, the geometry is
    // plain integers, and `SWP_NOACTIVATE` keeps this a Z-order and placement
    // edit only — no focus is stolen and nothing outlives the call. Neither
    // window is claimed to be alive: a dead handle is reported as FALSE, which
    // is checked right below, and that tolerance is the whole justification.
    let placed = unsafe {
        SetWindowPos(
            shield.raw(),
            game.raw(),
            rect.left,
            rect.top,
            rect.width,
            rect.height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    };
    if placed == 0 {
        // Read before any other Win32 call: `GetLastError` is per-thread and
        // the very next call would overwrite it. UIPI refusals and dead
        // windows are indistinguishable without it.
        let error = std::io::Error::last_os_error();
        return Err(placement_refusal("raise the input shield", &error));
    }
    // SAFETY: same two live top-level handles, same no-activate contract;
    // `SWP_NOMOVE | SWP_NOSIZE` makes the zeroed geometry inert.
    let swapped = unsafe {
        SetWindowPos(
            game.raw(),
            shield.raw(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    };
    if swapped == 0 {
        // Same rule, same reason: read it before anything else touches Win32.
        let error = std::io::Error::last_os_error();
        return Err(placement_refusal(
            "slot the game under the input shield",
            &error,
        ));
    }
    Ok(true)
}

/// Why a `SetWindowPos` refusal reads the way it does.
///
/// UIPI refusals and dead windows want opposite advice: a dead window is
/// nothing to do, an integrity-level mismatch is "restart this app as
/// administrator" (the game cannot go the other way — Epic Seven inherits high
/// integrity from STOVE's `requireAdministrator` launcher).
///
/// The "restart as administrator" fix deliberately does **not** appear here.
/// `MessageSurface::acquire` probes the window once per job and stops the loop
/// with the full explanation (`actuator::win::preflight_refusal`) before a
/// single click is planned, so by the time this line can fire the game has
/// changed integrity level *mid-job* — a real but rare case. Naming the cause
/// is what this owes the log; repeating the paragraph on every click would
/// bury it.
fn placement_refusal(action: &str, error: &std::io::Error) -> String {
    if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
        format!(
            "Windows now refuses to {action}: the game window sits at a higher integrity level \
             than this app ({error})"
        )
    } else {
        format!("could not {action} ({error})")
    }
}

/// Lowers the shield if it exists — never creates one.
pub(super) fn hide() {
    let window = lock(&WINDOW);
    if let Some(shield) = *window {
        // SAFETY: the cached handle may already have died with its pump
        // thread; `ShowWindow` is defined over arbitrary HWND values and
        // answers FALSE, which is exactly the best-effort semantics a
        // lowering wants. Nothing is dereferenced on this side.
        unsafe { ShowWindow(shield.raw(), SW_HIDE) };
    }
}

/// Current window, recreated when missing or dead (a window dies with its
/// pump thread).
fn handle() -> Result<Hwnd, String> {
    // The lock deliberately spans the probe, the recreation *and* the store,
    // even though `spawn_window` — a thread start plus a blocking `recv` — is
    // by far the slowest thing under it. That width is the "at most one shield
    // exists" invariant. Tightened around the probe alone, two callers can both
    // read an empty slot, both spawn a window and a pump thread, and the second
    // store overwrites the first handle; `hide` lowers only the handle it finds
    // in `WINDOW`, so the orphan stays raised over the game and swallows the
    // player's mouse for the rest of the process, with nothing left able to
    // reach it. Only the tail below is outside the critical section.
    let mut window = lock(&WINDOW);
    if let Some(hwnd) = *window {
        // SAFETY: asking whether a possibly-dead handle is still a window is
        // precisely what `IsWindow` is for — it validates the handle itself
        // and only reports.
        if unsafe { IsWindow(hwnd.raw()) } != 0 {
            return Ok(hwnd);
        }
        *window = None;
    }
    let hwnd = spawn_window()?;
    *window = Some(hwnd);
    drop(window);
    Ok(hwnd)
}

/// The window lives on its own pumping thread: unpumped, it would count as
/// hung precisely while it absorbs the player's input.
fn spawn_window() -> Result<Hwnd, String> {
    // The channel carries the verdict itself, not a bare readiness signal: a
    // successful `recv` *is* the detailed `create_window` answer, and a
    // `RecvError` — the sender dropped without sending — is exactly "the thread
    // died during setup". Do not reintroduce an `Arc<Mutex<Option<…>>>` here to
    // hold that failure instead: the two statements between filling the slot
    // and sending cannot panic, so the mutex would guard nothing.
    let (tx, rx) = std::sync::mpsc::channel::<Result<Hwnd, String>>();
    let spawned = std::thread::Builder::new()
        .name("shield".to_owned())
        .spawn(move || {
            let created = create_window();
            let run = created.is_ok();
            // A send failure means the requester is already gone; `run` was read
            // before the move, so the pump decision does not depend on it.
            let _ = tx.send(created);
            if run {
                pump();
            }
        });
    if let Err(error) = spawned {
        // The OS reason (thread limit, out of memory) is the whole diagnosis.
        return Err(format!("could not start the shield thread ({error})"));
    }
    rx.recv()
        .unwrap_or_else(|_| Err("the shield thread died during setup".to_owned()))
}

/// The class outlives its windows: already-registered is success on
/// recreation.
fn register_class(class: &[u16]) -> Result<(), String> {
    let wndclass = WNDCLASSW {
        lpfnWndProc: Some(shield_proc),
        // SAFETY: `GetModuleHandleW(NULL)` names the calling process's own
        // module. It takes no reference count, cannot fail for NULL, and the
        // handle stays valid for the process lifetime.
        hInstance: unsafe { GetModuleHandleW(std::ptr::null()) },
        lpszClassName: class.as_ptr(),
        // Hit-testing follows the painted content: unpainted, the layered
        // window would let the mouse fall through to the game.
        // SAFETY: stock objects are owned by GDI and shared process-wide —
        // valid forever, and this side must never delete the returned brush.
        hbrBackground: unsafe { GetStockObject(BLACK_BRUSH) },
        ..Default::default()
    };
    // SAFETY: `wndclass` is fully initialized and outlives the call;
    // `lpszClassName` points into `class`, which the caller keeps alive, and
    // Win32 copies the name into its own class table.
    let registered = unsafe { RegisterClassW(&wndclass) };
    if registered == 0 {
        // Read immediately: the atom-zero return is the only signal, and the
        // benign "already registered" case is told apart by this code alone.
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_CLASS_ALREADY_EXISTS as i32) {
            return Err(format!(
                "could not register the shield window class ({error})"
            ));
        }
    }
    Ok(())
}

fn create_window() -> Result<Hwnd, String> {
    let class = wide(CLASS_NAME);
    register_class(&class)?;
    // SAFETY: as in `register_class` — the process's own module handle, no
    // ownership taken.
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    // SAFETY: `class` is a NUL-terminated UTF-16 buffer that outlives the
    // call and names the class registered just above; the null window name,
    // parent, menu and creation parameter are the documented encodings for
    // "none". Failure returns NULL, checked before the handle is used.
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
        let error = std::io::Error::last_os_error();
        return Err(format!("could not create the shield window ({error})"));
    }
    // Alpha 1: invisible yet hit-testable; alpha 0 would be click-through.
    // SAFETY: `hwnd` was just created with `WS_EX_LAYERED`, the single
    // precondition of this call; the colour key is unused with `LWA_ALPHA`.
    if unsafe { SetLayeredWindowAttributes(hwnd, 0, 1, LWA_ALPHA) } == 0 {
        let error = std::io::Error::last_os_error();
        return Err(format!("could not set the shield transparency ({error})"));
    }
    Ok(Hwnd::new(hwnd))
}

extern "system" fn shield_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: the four arguments are the ones Win32 just handed to this
    // window procedure, forwarded unchanged to the documented fallback
    // handler, which validates them itself.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn pump() {
    let mut msg = MSG::default();
    // SAFETY: `msg` is an initialized `MSG` owned exclusively by this thread
    // and alive for the whole loop; a null window filter means "every message
    // of this thread", which is what a pump wants. The `> 0` guard excludes
    // both the -1 error and the WM_QUIT zero before `msg` is dispatched.
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        // SAFETY: `msg` was filled by the `GetMessageW` that gated this
        // branch and is still owned by this thread.
        unsafe { DispatchMessageW(&msg) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Measured, not assumed: a medium-integrity process calling `SetWindowPos`
    /// against a high-integrity window is refused with this exact code.
    fn uipi_refusal() -> std::io::Error {
        std::io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32)
    }

    #[test]
    fn an_access_denied_placement_names_the_integrity_level_instead_of_a_dead_window() {
        let message = placement_refusal("raise the input shield", &uipi_refusal());
        assert!(message.contains("integrity level"), "{message}");
        assert!(message.contains("raise the input shield"), "{message}");
    }

    #[test]
    fn an_access_denied_placement_leaves_the_fix_to_the_preflight() {
        // The preflight stops the loop with the "restart it as administrator"
        // line before any click is planned. Repeating it per click would bury
        // the one line that matters.
        let message = placement_refusal("slot the game under the input shield", &uipi_refusal());
        assert!(!message.contains("restart"), "{message}");
        assert!(!message.contains("relaunch"), "{message}");
    }

    #[test]
    fn any_other_placement_failure_keeps_its_plain_wording() {
        // ERROR_INVALID_WINDOW_HANDLE: the window really did die mid-job, and
        // an integrity-level lecture would be the wrong diagnosis.
        let error = std::io::Error::from_raw_os_error(1400);
        let message = placement_refusal("raise the input shield", &error);
        assert!(
            message.starts_with("could not raise the input shield"),
            "{message}"
        );
        assert!(!message.contains("integrity"), "{message}");
    }
}
