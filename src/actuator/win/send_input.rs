//! The `SendInput` backend: the fallback, which drives the *real* cursor and
//! the real foreground window.
//!
//! Everything that reaches process-global input state sits behind the
//! [`InputDriver`] trait, which is what lets the event-order tests prove that
//! validation happens before injection without moving the player's mouse or
//! stealing anyone's focus.
//!
//! [`sendinput_result`] is `pub(super)` only so [`dpi`](super::dpi) can link to
//! it: this backend cannot report a mis-scaled click at all, which is why the
//! DPI awareness has to be read back rather than inferred.

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

use super::{
    Hwnd, MOVE_SETTLE_MS, SystemWindowDriver, Target, WHEEL_DELTA, WindowDriver, preflight_refusal,
    release_twice, verify_identity_of,
};

/// The foreground switch is asynchronous: give it a beat before verifying.
///
/// The timing budget, stated once for the whole tree: a planned hold is 40–90 ms
/// (`Jitter::press_ms`) and this settle is 100 ms, so anything that blocks
/// between the `LEFTDOWN` and the `LEFTUP` more than doubles the press the game
/// measures. Nothing may — see [`WinSurface::release_after_down`], the one place
/// that could have reached this from there and deliberately does not.
const FOCUS_SETTLE_MS: u64 = 100;

/// Full range of a `MOUSEEVENTF_ABSOLUTE` coordinate.
const ABSOLUTE_COORD_MAX: i64 = 65_535;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputEvent {
    Move((i32, i32)),
    LeftDown,
    LeftUp,
    Wheel(i32),
}

/// What this backend adds to [`WindowDriver`]: injecting input, and the
/// foreground `SendInput` aims at.
///
/// The five window calls it shares with `post_message`'s `MessageDriver` live
/// on the supertrait — see [`WindowDriver`] for why they are not declared here.
trait InputDriver: WindowDriver {
    fn foreground_window(&mut self) -> Hwnd;
    fn request_foreground(&mut self, hwnd: Hwnd);
    fn send(&mut self, event: InputEvent) -> Result<(), SurfaceError>;
}

impl InputDriver for SystemWindowDriver {
    fn foreground_window(&mut self) -> Hwnd {
        // SAFETY: no arguments, no ownership — the returned HWND (possibly
        // NULL) is only ever compared here, never dereferenced.
        Hwnd::new(unsafe { GetForegroundWindow() })
    }

    fn request_foreground(&mut self, hwnd: Hwnd) {
        // The refusal is dropped on purpose: `SetForegroundWindow` reports FALSE
        // both when the switch is denied and when it merely has not happened
        // yet. `ensure_foreground`'s re-read after the settle is the authority.
        // SAFETY: `hwnd` is the handle `acquire` found; the call validates it
        // itself and answers FALSE for a window that died in between.
        let _ = unsafe { SetForegroundWindow(hwnd.raw()) };
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
}

/// `SendInput` backend: real cursor, real foreground.
///
/// The driver stays erased behind `Box<dyn InputDriver>` even though production
/// only ever holds the one ZST: `InputDriver` is private to this module, so
/// `WinSurface<D: InputDriver = SystemWindowDriver>` would leak a private type
/// (`private_bounds`, `private_interfaces` — red under `-D warnings`).
/// Measured, not assumed; the allocation is one per session.
pub struct WinSurface {
    driver: Box<dyn InputDriver>,
}

impl Default for WinSurface {
    fn default() -> Self {
        Self {
            driver: Box::new(SystemWindowDriver),
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

    /// The part of acquiring that both doors owe. The probe comes before the
    /// foreground is stolen and before any coordinate is planned — an
    /// unreachable window is not worth pulling forward, and `SendInput` will not
    /// report the refusal later (see [`probe_window_reachable`](super::probe_window_reachable)). A dry run
    /// keeps it: it provably changes nothing, and "the real run would be
    /// refused" is the most useful sentence a rehearsal can print.
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

    /// Everything [`validate_target`](Self::validate_target) checks that is a
    /// pure *read*, split out because
    /// [`release_after_down`](Self::release_after_down) needs exactly this half
    /// and must not have the other. `FindWindowW`, `GetClientRect` and
    /// `ClientToScreen` are answered out of window-manager state and cannot wait
    /// on anything; `SetWindowPos` does enter Epic Seven's message loop, which is
    /// why [`probe_window_reachable`](super::probe_window_reachable) needed a hang check and this does not.
    fn verify_placement(&mut self, target: Target) -> Result<(), SurfaceError> {
        verify_identity_of(&mut *self.driver, target)
    }

    /// The placement reads, plus the demand that the window own the foreground —
    /// restoring it if it does not. What every event is guarded by *before* it is
    /// injected, since `SendInput` aims at whoever holds the foreground.
    fn validate_target(&mut self, target: Target) -> Result<(), SurfaceError> {
        self.verify_placement(target)?;
        self.ensure_foreground(target.hwnd)
    }

    /// [`validate_target`](Self::validate_target) with the acting taken out: a
    /// bare look at who owns the foreground in place of a demand to own it. This
    /// is what a check is allowed to be while the left button is down — no
    /// `SetForegroundWindow`, no [`FOCUS_SETTLE_MS`] sleep, so it cannot stretch
    /// the press.
    ///
    /// `Recoverable`, where [`ensure_foreground`](Self::ensure_foreground)'s
    /// refusal is `Fatal`: there the app *asked* for the foreground and Windows
    /// refused after a full settle, which does not heal while both processes
    /// keep running, whereas here nothing was asked and the next job's `acquire`
    /// takes the foreground back — producing the `Fatal` itself if it genuinely
    /// cannot.
    fn observe_target(&mut self, target: Target) -> Result<(), SurfaceError> {
        self.verify_placement(target)?;
        if self.driver.foreground_window() != target.hwnd {
            return Err(SurfaceError::Recoverable(
                "the game window lost the foreground while the left button was held, so the click \
                 did not land in it"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    fn send_guarded(&mut self, target: Target, event: InputEvent) -> Result<(), SurfaceError> {
        self.validate_target(target)?;
        self.driver.send(event)
    }

    /// This backend's half of [`release_twice`]: *look*, then `LEFTUP`.
    ///
    /// Look, not validate, and the word is the whole decision. Nothing here may
    /// block, sleep, or put anything right: this used to call the full
    /// [`validate_target`](Self::validate_target), whose `SetForegroundWindow`
    /// plus [`FOCUS_SETTLE_MS`] sleep turned the planned press into a 140–190 ms
    /// one, a long-press rather than a click — and activated a window that would
    /// then receive a button pressed somewhere else. Recovery belongs before the
    /// press, where `send_guarded` refuses the `LEFTDOWN` if it cannot pull the
    /// window forward, and after the release, in the next job's `acquire`.
    ///
    /// What is *not* dropped is the finding: a click released over a window that
    /// no longer owned the foreground did not land where the job aimed it, so
    /// releasing promptly must not become releasing quietly. That verdict goes in
    /// `release_twice`'s first slot, returned once the button is provably up.
    ///
    /// The `observed` binding exists because both closures want `&mut self`.
    fn release_after_down(&mut self, target: Target) -> Result<(), SurfaceError> {
        let observed = self.observe_target(target);
        let driver = &mut self.driver;
        release_twice(|| observed, || driver.send(InputEvent::LeftUp), "LEFTUP")
    }
}

impl Surface for WinSurface {
    type Window = Target;

    fn acquire(&mut self) -> Result<(Target, ClientRect), SurfaceError> {
        let hwnd = self.locate()?;
        self.ensure_foreground(hwnd)?;
        let rect = self.driver.client_rect(hwnd)?;
        let pid = self.driver.owning_pid(hwnd)?;
        Ok((Target { hwnd, rect, pid }, rect))
    }

    /// `acquire` minus the one step that acts on the desktop, and *only* that
    /// step: `SendInput` aims at whatever owns the foreground, so a live job
    /// cannot skip the steal and a dry run must not perform it.
    ///
    /// The rect read is spelled twice rather than shared: `ensure_foreground`
    /// restores a minimized window, so measuring after it is what makes a live
    /// job's rect the restored one, while a dry run reads whatever is there.
    /// Sharing the two lines would quietly move a live job onto the second
    /// behaviour.
    fn measure(&mut self) -> Result<(Target, ClientRect), SurfaceError> {
        let hwnd = self.locate()?;
        let rect = self.driver.client_rect(hwnd)?;
        let pid = self.driver.owning_pid(hwnd)?;
        Ok((Target { hwnd, rect, pid }, rect))
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
    // sent, and the window is the executor guard's, dropping with the job.
}

/// One `SM_*` metric. Win32 has no error channel here: an unknown index (or a
/// metric the session cannot answer) simply reads back 0.
fn system_metric(index: i32) -> i32 {
    // SAFETY: the call takes no pointer or handle, and any index it does not
    // recognize returns 0 rather than faulting.
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
        // leaves i32 entirely — a wrapped `as i32` would aim anywhere, while the
        // desktop edge is at least bounded. An unreachable path, not a live one:
        // getting here means `acquire` already measured a client rect
        // `Viewport::of` accepted, which a session with no virtual screen cannot
        // have.
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
        // `mouseData` is a `u32` holding a signed wheel delta in its low word;
        // spelled out so the reinterpretation is visible rather than inferred.
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
    // call, the count of 1 matches the single-element pointer, and the third
    // argument is the stride of *one* structure — passing the array's total size
    // there is the classic mistake that makes `SendInput` reject everything.
    let inserted = unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) };
    sendinput_result(inserted)
}

/// `SendInput` returns the number of events inserted; we always send 1, so
/// anything else means the input was blocked (foreground lock, full queue).
/// Recoverable: the watchdog re-issues against a fresh acquire.
///
/// Note what is *not* in that list: UIPI. "Neither `GetLastError` nor the return
/// value will indicate the failure was caused by UIPI blocking", so an
/// out-of-reach window makes this answer `Ok(())` while nothing moves in the
/// game — a blind spot only [`probe_window_reachable`](super::probe_window_reachable) can cover.
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

    use crate::actuator::win::tests::{
        GAME_HWND, GAME_PID, OTHER_HWND, OTHER_PID, game_rect, uipi_refusal,
    };

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum DriverCall {
        FindWindow,
        Probe(Hwnd),
        Foreground,
        RequestForeground(Hwnd),
        ClientRect(Hwnd),
        OwningPid(Hwnd),
        Send(InputEvent),
        Sleep(u64),
    }

    struct FakeState {
        calls: Vec<DriverCall>,
        window: Hwnd,
        foreground: Hwnd,
        rect: ClientRect,
        pid: u32,
        find_results: VecDeque<Result<Hwnd, SurfaceError>>,
        /// Raw: an `Err` here is the thread's last-error as Win32 would have
        /// left it, so the tests exercise the real classification rather than a
        /// pre-classified verdict.
        probe_results: VecDeque<std::io::Result<()>>,
        foreground_results: VecDeque<Hwnd>,
        rect_results: VecDeque<Result<ClientRect, SurfaceError>>,
        pid_results: VecDeque<Result<u32, SurfaceError>>,
        send_results: VecDeque<Result<(), SurfaceError>>,
    }

    impl Default for FakeState {
        fn default() -> Self {
            Self {
                calls: Vec::new(),
                window: GAME_HWND,
                foreground: GAME_HWND,
                rect: game_rect(),
                pid: GAME_PID,
                find_results: VecDeque::new(),
                probe_results: VecDeque::new(),
                foreground_results: VecDeque::new(),
                rect_results: VecDeque::new(),
                pid_results: VecDeque::new(),
                send_results: VecDeque::new(),
            }
        }
    }

    struct FakeInputDriver {
        state: Arc<Mutex<FakeState>>,
    }

    impl WindowDriver for FakeInputDriver {
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

        fn client_rect(&mut self, hwnd: Hwnd) -> Result<ClientRect, SurfaceError> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(DriverCall::ClientRect(hwnd));
            if let Some(result) = state.rect_results.pop_front() {
                result
            } else {
                Ok(state.rect)
            }
        }

        fn owning_pid(&mut self, hwnd: Hwnd) -> Result<u32, SurfaceError> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(DriverCall::OwningPid(hwnd));
            if let Some(result) = state.pid_results.pop_front() {
                result
            } else {
                Ok(state.pid)
            }
        }

        fn sleep(&mut self, duration: Duration) {
            self.state
                .lock()
                .unwrap()
                .calls
                .push(DriverCall::Sleep(duration.as_millis() as u64));
        }
    }

    impl InputDriver for FakeInputDriver {
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

        fn send(&mut self, event: InputEvent) -> Result<(), SurfaceError> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(DriverCall::Send(event));
            state.send_results.pop_front().unwrap_or(Ok(()))
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

    /// Hands back the window every later call has to present: under `api-004` a
    /// test cannot reach an input without the proof of an acquire either.
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

    /// Where a call landed in the recorded sequence. Panics rather than
    /// returning an `Option`: every caller asserts *about* the call, so its
    /// absence is the test failing, not a case to handle.
    fn index_of(calls: &[DriverCall], wanted: &DriverCall) -> usize {
        calls
            .iter()
            .position(|call| call == wanted)
            .unwrap_or_else(|| panic!("{wanted:?} was never recorded in {calls:?}"))
    }

    fn validation_calls() -> Vec<DriverCall> {
        vec![
            DriverCall::FindWindow,
            DriverCall::OwningPid(GAME_HWND),
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

        // The rect the executor plans against and the window the inputs are
        // aimed at come out of the same call, so they cannot disagree.
        let (target, rect) = surface.acquire().expect("the fake acquires");
        assert_eq!(rect, game_rect());
        assert_eq!(target.hwnd, GAME_HWND);
        assert_eq!(target.rect, game_rect());
        assert_eq!(
            calls(&state),
            vec![
                DriverCall::FindWindow,
                // Before the foreground steal: an unreachable window is not
                // worth pulling in front of the player's own.
                DriverCall::Probe(GAME_HWND),
                DriverCall::Foreground,
                DriverCall::RequestForeground(GAME_HWND),
                DriverCall::Sleep(FOCUS_SETTLE_MS),
                DriverCall::Foreground,
                DriverCall::ClientRect(GAME_HWND),
                DriverCall::OwningPid(GAME_HWND),
            ]
        );
    }

    /// `RequestForeground` is a real `SetForegroundWindow`, so a dry run through
    /// `acquire` yanked Epic Seven in front of the player every tick, for no
    /// input at all. The call list is the assertion, not a count: `FindWindow`
    /// and `Probe` must stay (a dry run that cannot name a UIPI refusal has
    /// stopped rehearsing) and so must `ClientRect` (it resolves the screen
    /// coordinates the journal prints).
    #[test]
    fn measure_reads_the_window_without_pulling_it_forward() {
        let (mut surface, state) = fake_surface();
        // The foreground belongs to someone else: the case where `acquire` would
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
                DriverCall::OwningPid(GAME_HWND),
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

    /// Named for the parity this backend and `post_message.rs` share through
    /// [`verify_identity`](super::verify_identity): the same scenario as
    /// `different_title_matched_window_is_fatal_before_input` below, under the
    /// name both backends' test modules use for it.
    #[test]
    fn a_title_resolving_to_another_window_mid_job_is_fatal() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state.lock().unwrap().window = OTHER_HWND;

        assert!(matches!(
            surface.click(&target, (1, 2), 3),
            Err(SurfaceError::Fatal(reason)) if reason.contains("different window")
        ));
        assert_eq!(calls(&state), vec![DriverCall::FindWindow]);
        assert!(sent_events(&state).is_empty());
    }

    /// Parity with `post_message.rs`'s equivalent: a recycled `HWND` value now
    /// owned by a different process must be refused exactly like a changed
    /// title, even though the title and the `HWND` integer both still match.
    #[test]
    fn a_changed_owning_process_is_fatal() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state.lock().unwrap().pid_results.push_back(Ok(OTHER_PID));

        assert!(matches!(
            surface.click(&target, (1, 2), 3),
            Err(SurfaceError::Fatal(reason)) if reason.contains("different process")
        ));
        assert_eq!(
            calls(&state),
            vec![
                DriverCall::FindWindow,
                // `verify_identity` reads the pid and the rect together, in one
                // round trip, once the title has matched — see its doc comment
                // for why that is not a correctness gap: both are cheap reads,
                // unlike the desktop-affecting calls the title check guards.
                DriverCall::OwningPid(GAME_HWND),
                DriverCall::ClientRect(GAME_HWND),
            ]
        );
        assert!(sent_events(&state).is_empty());
    }

    /// The negative control for the two checks above: an unchanged target
    /// must still click end to end, so the title and pid re-resolution
    /// cannot pass by refusing everything.
    #[test]
    fn an_unchanged_target_still_clicks() {
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
            vec![
                DriverCall::FindWindow,
                DriverCall::OwningPid(GAME_HWND),
                DriverCall::ClientRect(GAME_HWND),
            ]
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
            &actual[..8],
            &[
                DriverCall::FindWindow,
                DriverCall::OwningPid(GAME_HWND),
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
                DriverCall::OwningPid(GAME_HWND),
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

    // Do not re-add a test for "input after release, or with no acquire at all,
    // is Fatal not a panic": since `api-004` the call cannot be written without
    // a `Target` to present, so the state is unrepresentable rather than
    // untested. What it guarded is pinned in `actuator::mod`'s
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

    /// Do not re-add `focus_is_restored_before_guarded_left_up`, which this
    /// replaces: it asserted that `RequestForeground` came before the `LEFTUP`,
    /// which is the defect — `SetForegroundWindow` plus a `FOCUS_SETTLE_MS`
    /// sleep *inside* the press makes 90 + 100 ms, a long-press rather than a
    /// click. Recovery belongs before the `LEFTDOWN`
    /// (`focus_loss_during_move_settle_blocks_left_down`) and at the next
    /// `acquire`.
    #[test]
    fn a_foreground_lost_mid_press_does_not_stretch_the_hold() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        // Focus survives the move and the press, then goes elsewhere while the
        // button is held.
        state
            .lock()
            .unwrap()
            .foreground_results
            .extend([GAME_HWND, GAME_HWND, OTHER_HWND]);

        // 90 ms is the top of `Jitter::press_ms`'s band: the worst case.
        let verdict = surface.click(&target, (30, 40), 90);

        let actual = calls(&state);
        let held = &actual[index_of(&actual, &DriverCall::Send(InputEvent::LeftDown))
            ..index_of(&actual, &DriverCall::Send(InputEvent::LeftUp))];
        let held_ms: u64 = held
            .iter()
            .filter_map(|call| match *call {
                DriverCall::Sleep(ms) => Some(ms),
                _ => None,
            })
            .sum();
        assert_eq!(held_ms, 90, "the press must be the planned one: {held:?}");
        assert!(
            !held.contains(&DriverCall::RequestForeground(GAME_HWND)),
            "nothing may act on the desktop while the button is down: {held:?}"
        );
        // Released promptly is not released quietly: the click did not land in
        // the game, and the caller is told so.
        assert!(matches!(
            verdict,
            Err(SurfaceError::Recoverable(reason)) if reason.contains("lost the foreground")
        ));
        assert_eq!(sent_events(&state).last(), Some(&InputEvent::LeftUp));
    }

    #[test]
    fn a_foreground_lost_after_left_down_still_sends_unguarded_left_up() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .foreground_results
            .extend([GAME_HWND, GAME_HWND, OTHER_HWND]);

        // Recoverable, not Fatal: nothing refused this app the foreground, it
        // merely observed someone else holding it, and the next `acquire` takes
        // it back.
        assert!(matches!(
            surface.click(&target, (30, 40), 5),
            Err(SurfaceError::Recoverable(reason)) if reason.contains("lost the foreground")
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
                .extend([GAME_HWND, GAME_HWND, OTHER_HWND]);
            fake.send_results
                .extend([Ok(()), Ok(()), Err(blocked_input()), Ok(())]);
        }

        // The retry got the button up, so the fault worth reporting is the one
        // that says the click missed, not the first `LEFTUP`'s refusal.
        assert!(matches!(
            surface.click(&target, (30, 40), 5),
            Err(SurfaceError::Recoverable(reason)) if reason.contains("lost the foreground")
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
                .extend([GAME_HWND, GAME_HWND, OTHER_HWND]);
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
        // Nothing was focused and nothing was measured: the `Err` carries no
        // `Target`, so there is no window for a later input to be aimed at.
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
