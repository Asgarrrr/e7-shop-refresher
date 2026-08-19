//! The `SendInput` backend: the fallback, which drives the *real* cursor and
//! the real foreground window.
//!
//! Everything that reaches process-global input state sits behind the
//! [`InputDriver`] trait declared here, which is what lets the event-order tests
//! prove that validation happens before injection without moving the player's
//! mouse or stealing anyone's focus. The window itself, and the checks that
//! decide whether it may be driven at all, come from the module root.
//!
//! [`sendinput_result`] is `pub(super)` — visible in the `win` tree and nowhere
//! else — because it is the *evidence* the DPI preflight cites for why the
//! awareness has to be read back rather than inferred: this backend cannot report
//! a mis-scaled click at all. [`dpi`](super::dpi) links to it for that reason.

use std::time::Duration;

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SetForegroundWindow,
};

use crate::actuator::plan::ClientRect;
use crate::actuator::{Surface, SurfaceError};

use super::dpi::ensure_dpi_awareness;
use super::{
    Hwnd, MOVE_SETTLE_MS, Target, WHEEL_DELTA, client_rect, find_game_window, preflight_refusal,
    probe_window_reachable, rect_change_error, release_twice,
};

/// The foreground switch is asynchronous: give it a beat before verifying.
const FOCUS_SETTLE_MS: u64 = 100;

/// Full range of a `MOUSEEVENTF_ABSOLUTE` coordinate — a Win32 protocol
/// constant, like `WHEEL_DELTA` in the module root.
const ABSOLUTE_COORD_MAX: i64 = 65_535;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputEvent {
    Move((i32, i32)),
    LeftDown,
    LeftUp,
    Wheel(i32),
}

/// The process-global input calls used by [`WinSurface`] (see the module docs
/// for why this trait exists).
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

    /// The part of acquiring that both doors owe: find the window by title, then
    /// refuse one this process may not drive.
    ///
    /// The probe comes before the foreground is stolen and before any coordinate
    /// is planned — a window this process may not drive is not worth pulling
    /// forward, and `SendInput` will not report the refusal later, see
    /// [`probe_window_reachable`]. A dry run keeps
    /// it: it provably changes nothing, and "the real run would be refused" is
    /// the most useful sentence a rehearsal can print.
    fn locate(&mut self) -> Result<Hwnd, SurfaceError> {
        let hwnd = self.driver.find_game_window()?;
        self.driver
            .probe_reachable(hwnd)
            .map_err(|error| preflight_refusal(&error))?;
        Ok(hwnd)
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
        let hwnd = self.locate()?;
        self.ensure_foreground(hwnd)?;
        let rect = self.driver.client_rect(hwnd)?;
        Ok((Target { hwnd, rect }, rect))
    }

    /// `acquire` minus the one step that acts on the desktop, and *only* that
    /// step: `SendInput` aims at whatever owns the foreground, so a live job
    /// cannot skip the steal and a dry run must not perform it.
    ///
    /// The order in `acquire` is left alone rather than reused here, which is why
    /// the rect read is spelled twice. `ensure_foreground` restores a minimized
    /// window, so measuring after it is what makes a live job's rect the
    /// restored one; a dry run reads whatever is there and reports a minimized
    /// window as the recoverable abort it would be. Swapping the two lines to
    /// share them would quietly move a live job onto the second behaviour.
    fn measure(&mut self) -> Result<(Target, ClientRect), SurfaceError> {
        let hwnd = self.locate()?;
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
pub(super) fn sendinput_result(inserted: u32) -> Result<(), SurfaceError> {
    if inserted == 1 {
        Ok(())
    } else {
        Err(SurfaceError::Recoverable(format!(
            "SendInput injected {inserted}/1 events — input blocked ({})",
            std::io::Error::last_os_error()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use crate::actuator::win::tests::{GAME_HWND, OTHER_HWND, game_rect, uipi_refusal};

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

    /// The dry-run door, against the same fake as its live twin above. `Mode`
    /// promises to send nothing, and on this backend `acquire` broke that promise
    /// before a coordinate was even planned: `RequestForeground` is a real
    /// `SetForegroundWindow`, so rehearsing the plan yanked Epic Seven in front
    /// of whatever the player was doing, every tick, for no input at all.
    ///
    /// The call list is the assertion, not a count: `FindWindow` and `Probe` must
    /// stay (a dry run that cannot name a UIPI refusal has stopped rehearsing)
    /// and `ClientRect` must stay (it is what resolves the screen coordinates the
    /// journal prints). Only the three foreground calls and the focus settle go.
    #[test]
    fn measure_reads_the_window_without_pulling_it_forward() {
        let (mut surface, state) = fake_surface();
        // The foreground belongs to someone else — the case where `acquire` would
        // take it away.
        state
            .lock()
            .unwrap()
            .foreground_results
            .extend([OTHER_HWND, GAME_HWND]);

        let (target, rect) = surface.measure().expect("the fake measures");
        assert_eq!(rect, game_rect());
        assert_eq!(target.hwnd, GAME_HWND);
        assert_eq!(target.rect, game_rect());
        assert_eq!(
            calls(&state),
            vec![
                DriverCall::FindWindow,
                DriverCall::Probe(GAME_HWND),
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

    // Do not re-add a test for "input after release, or with no acquire at
    // all, is Fatal not a panic" (it used to live here and in `post_message`'s
    // test module): since `api-004` there is no `Option<Target>` to be `None`,
    // so the call cannot be written without a `Target` to present — the state
    // is unrepresentable, not merely untested. What it guarded is now pinned in
    // `actuator::mod`'s `every_input_and_the_release_see_the_window_that_job_acquired`.

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
}
