//! Invisible, non-activating window kept directly above the game while a job
//! posts its clicks: real mouse messages route to the window under the
//! cursor, so the shield absorbs the player's mouse over the game while the
//! posted messages — addressed to the game's handle — pass untouched.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use windows_sys::Win32::Foundation::{ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM};
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

/// Poisoning carries no meaning here: the guarded state is a plain handle,
/// and a panic elsewhere must not turn every later click into a fatal — the
/// promise `WINDOW` makes right above.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Ensures the shield sits directly above the game, covering `rect`;
/// `Ok(true)` means it was (re)placed — the game may still hold real moves.
pub(super) fn raise(game: HWND, rect: ClientRect) -> Result<bool, String> {
    let shield = handle()? as HWND;
    // SAFETY: `game` is the handle the caller revalidated through
    // `Target::verify` just before this call, and `GetWindow` is defined over
    // any HWND — a window that died in between returns NULL, which reads as
    // "the shield is not directly above it" instead of faulting. Nothing is
    // borrowed past the call: the result is only compared.
    let directly_above = unsafe { GetWindow(game, GW_HWNDPREV) } == shield;
    // SAFETY: `shield` was proven alive by the `IsWindow` check inside
    // `handle()` on this same thread; `IsWindowVisible` only reports and
    // returns 0 for a handle it does not know.
    let visible = unsafe { IsWindowVisible(shield) } != 0;
    if directly_above && visible {
        return Ok(false);
    }
    // Two-step swap into the game's own Z-slot: anchoring on the window
    // above it instead would catch Win32's topmost contagion.
    // SAFETY: both handles are top-level windows owned by live threads, the
    // geometry is plain integers, and `SWP_NOACTIVATE` keeps this a Z-order
    // and placement edit only — no focus is stolen and nothing outlives the
    // call. A dead handle is reported as FALSE, checked right below.
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
        // Read before any other Win32 call: `GetLastError` is per-thread and
        // the very next call would overwrite it. UIPI refusals and dead
        // windows are indistinguishable without it.
        let error = std::io::Error::last_os_error();
        return Err(format!("could not raise the input shield ({error})"));
    }
    // SAFETY: same two live top-level handles, same no-activate contract;
    // `SWP_NOMOVE | SWP_NOSIZE` makes the zeroed geometry inert.
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
        let error = std::io::Error::last_os_error();
        return Err(format!(
            "could not slot the game under the input shield ({error})"
        ));
    }
    Ok(true)
}

/// Lowers the shield if it exists — never creates one.
pub(super) fn hide() {
    let window = lock(&WINDOW);
    if let Some(shield) = *window {
        // SAFETY: the cached handle may already have died with its pump
        // thread; `ShowWindow` is defined over arbitrary HWND values and
        // answers FALSE, which is exactly the best-effort semantics a
        // lowering wants. Nothing is dereferenced on this side.
        unsafe { ShowWindow(shield as HWND, SW_HIDE) };
    }
}

/// Current window, recreated when missing or dead (a window dies with its
/// pump thread).
fn handle() -> Result<isize, String> {
    let mut window = lock(&WINDOW);
    if let Some(hwnd) = *window {
        // SAFETY: asking whether a possibly-dead handle is still a window is
        // precisely what `IsWindow` is for — it validates the handle itself
        // and only reports.
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
    // The verdict travels in a shared slot and the channel only signals
    // readiness: that way a detailed `create_window` failure survives even
    // when the signal never arrives, instead of being flattened into the
    // generic "the thread died" below.
    let outcome: Arc<Mutex<Option<Result<isize, String>>>> = Arc::new(Mutex::new(None));
    let thread_outcome = Arc::clone(&outcome);
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let spawned = std::thread::Builder::new()
        .name("shield".to_owned())
        .spawn(move || {
            let created = create_window();
            let run = created.is_ok();
            *lock(&thread_outcome) = Some(created);
            let _ = tx.send(());
            if run {
                pump();
            }
        });
    if let Err(error) = spawned {
        // The OS reason (thread limit, out of memory) is the whole diagnosis.
        return Err(format!("could not start the shield thread ({error})"));
    }
    // Either the slot has been filled or the sender died with the thread;
    // both end the wait, and the slot tells which.
    let _ = rx.recv();
    lock(&outcome)
        .take()
        .unwrap_or_else(|| Err("the shield thread died during setup".to_owned()))
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

fn create_window() -> Result<isize, String> {
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
    Ok(hwnd as isize)
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
