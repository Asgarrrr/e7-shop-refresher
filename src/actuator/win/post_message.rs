//! The `PostMessageW` backend (the default): synthetic mouse messages posted to
//! the game window, so no focus is stolen and the player keeps the mouse.
//!
//! [`post`], [`pack_point`] and [`post_refusal`] are `pub(super)` — visible in
//! the `win` tree and nowhere else — solely because [`Hwnd`]'s doc comment is
//! written *about* them, and rustdoc will not link into a module-private item.

use std::time::Duration;

use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows_sys::Win32::System::SystemServices::MK_LBUTTON;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    PostMessageW, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
};

use crate::actuator::plan::ClientRect;
use crate::actuator::{Surface, SurfaceError, shield};

use super::dpi::ensure_dpi_awareness;
use super::{
    Hwnd, MOVE_SETTLE_MS, Target, WHEEL_DELTA, client_rect, find_game_window, preflight_refusal,
    probe_window_reachable, rect_change_error, release_twice,
};

/// Posted messages are retrieved before queued hardware input: a freshly
/// placed shield must let the game drain stale real moves before we post.
const SHIELD_DRAIN_MS: u64 = 50;

/// Why a refused `PostMessageW` is fatal or merely recoverable.
///
/// `ERROR_ACCESS_DENIED` here is UIPI. Do not classify it `Recoverable` under
/// "window gone or queue full": both causes are wrong for it, and the watchdog
/// would re-issue clicks forever against a condition that cannot heal. The
/// preflight at acquire catches it first, so this backstop — for an integrity
/// level that changed *mid-job* — names the cause in one clause and leaves the
/// fix to [`preflight_refusal`].
pub(super) fn post_refusal(error: &std::io::Error) -> SurfaceError {
    if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
        SurfaceError::Fatal(format!(
            "Windows refused the click: the game window is at a higher integrity level than this \
             app ({error})"
        ))
    } else {
        SurfaceError::Recoverable(format!(
            "PostMessageW failed — the game window may have closed, or its queue is full ({error})"
        ))
    }
}

/// The process-global calls used by [`MessageSurface`] (see the module docs
/// for why this trait exists).
trait MessageDriver: Send {
    fn find_game_window(&mut self) -> Result<Hwnd, SurfaceError>;
    /// [`probe_window_reachable`], handing back the thread's last-error
    /// untouched so the classification stays in the pure [`preflight_refusal`],
    /// which tests drive with a synthetic `ERROR_ACCESS_DENIED`.
    fn probe_reachable(&mut self, hwnd: Hwnd) -> std::io::Result<()>;
    fn client_rect(&mut self, hwnd: Hwnd) -> Result<ClientRect, SurfaceError>;
    fn post(
        &mut self,
        hwnd: Hwnd,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> Result<(), SurfaceError>;
    /// [`shield::raise`]: `Ok(true)` means the shield was (re)placed by this
    /// call, so the caller owes it the drain beat.
    fn shield_raise(&mut self, hwnd: Hwnd, rect: ClientRect) -> Result<bool, String>;
    fn shield_hide(&mut self);
    fn sleep(&mut self, duration: Duration);
}

struct SystemMessageDriver;

impl MessageDriver for SystemMessageDriver {
    fn find_game_window(&mut self) -> Result<Hwnd, SurfaceError> {
        ensure_dpi_awareness()?;
        find_game_window().map(Hwnd::new)
    }

    fn probe_reachable(&mut self, hwnd: Hwnd) -> std::io::Result<()> {
        probe_window_reachable(hwnd.raw())
    }

    fn client_rect(&mut self, hwnd: Hwnd) -> Result<ClientRect, SurfaceError> {
        client_rect(hwnd.raw())
    }

    fn post(
        &mut self,
        hwnd: Hwnd,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> Result<(), SurfaceError> {
        post(hwnd, msg, wparam, lparam)
    }

    fn shield_raise(&mut self, hwnd: Hwnd, rect: ClientRect) -> Result<bool, String> {
        shield::raise(hwnd, rect)
    }

    fn shield_hide(&mut self) {
        shield::hide();
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// `PostMessageW` backend (the default). The engine tracks its cursor through
/// move messages, so every input re-asserts the [`shield`] over the game until
/// [`release`](Surface::release).
///
/// The driver stays erased behind `Box<dyn MessageDriver>` even though
/// production only ever holds the one ZST, for the same reason as
/// `WinSurface`'s (`send_input.rs:117-121`): `MessageDriver`/
/// `SystemMessageDriver` are private to this module, so a generic parameter
/// such as `MessageSurface<D: MessageDriver = SystemMessageDriver>` would leak
/// a private type in a public API (`private_bounds`, `private_interfaces` —
/// red under `-D warnings`).
pub struct MessageSurface {
    driver: Box<dyn MessageDriver>,
}

impl Default for MessageSurface {
    fn default() -> Self {
        Self {
            driver: Box::new(SystemMessageDriver),
        }
    }
}

impl Drop for MessageSurface {
    fn drop(&mut self) {
        self.driver.shield_hide();
    }
}

impl Target {
    /// Screen → client pixels. Dropping the result means the input was about to
    /// be posted at the wrong coordinates.
    #[must_use]
    fn to_client(self, at: (i32, i32)) -> (i32, i32) {
        (at.0 - self.rect.left, at.1 - self.rect.top)
    }
}

impl MessageSurface {
    #[cfg(test)]
    fn with_driver(driver: impl MessageDriver + 'static) -> Self {
        Self {
            driver: Box::new(driver),
        }
    }

    /// Before every input: the window must be where the job planned it and
    /// the shield seated above it; a (re)placed shield gets the drain beat.
    /// A shield failure is fatal — never click shieldless.
    fn engage(&mut self, target: Target) -> Result<(), SurfaceError> {
        self.verify(target)?;
        if self
            .driver
            .shield_raise(target.hwnd, target.rect)
            .map_err(SurfaceError::Fatal)?
        {
            self.driver.sleep(Duration::from_millis(SHIELD_DRAIN_MS));
        }
        Ok(())
    }

    fn verify(&mut self, target: Target) -> Result<(), SurfaceError> {
        let rect = self.driver.client_rect(target.hwnd)?;
        if rect == target.rect {
            return Ok(());
        }
        Err(rect_change_error(rect))
    }
}

impl Surface for MessageSurface {
    type Window = Target;

    fn acquire(&mut self) -> Result<(Target, ClientRect), SurfaceError> {
        let hwnd = self.driver.find_game_window()?;
        // One probe per job: the alternative is `shield::raise` failing on the
        // first click of every job with a message naming the wrong cause.
        self.driver
            .probe_reachable(hwnd)
            .map_err(|error| preflight_refusal(&error))?;
        let rect = self.driver.client_rect(hwnd)?;
        let target = Target { hwnd, rect };
        Ok((target, rect))
    }

    /// Word for word `acquire`, and deliberately so: this backend posts to a
    /// window it never pulls forward, and the shield is raised per input by
    /// `engage`, so a dry run has nothing to skip. Spelled out rather than
    /// defaulted because the `input` backend's answer is not this one.
    fn measure(&mut self) -> Result<(Target, ClientRect), SurfaceError> {
        self.acquire()
    }

    fn click(
        &mut self,
        target: &Target,
        at: (i32, i32),
        press_ms: u64,
    ) -> Result<(), SurfaceError> {
        let target = *target;
        self.engage(target)?;
        let lparam = pack_point(target.to_client(at))?;
        self.driver.post(target.hwnd, WM_MOUSEMOVE, 0, lparam)?;
        self.driver.sleep(Duration::from_millis(MOVE_SETTLE_MS));
        self.driver
            .post(target.hwnd, WM_LBUTTONDOWN, MK_LBUTTON as usize, lparam)?;
        self.driver.sleep(Duration::from_millis(press_ms));
        // This backend re-verifies per post rather than up front, so there is
        // nothing to revalidate before the release.
        let driver = &mut self.driver;
        release_twice(
            || Ok(()),
            || driver.post(target.hwnd, WM_LBUTTONUP, 0, lparam),
            "WM_LBUTTONUP",
        )
    }

    fn scroll(
        &mut self,
        target: &Target,
        at: (i32, i32),
        notches: i32,
    ) -> Result<(), SurfaceError> {
        let target = *target;
        self.engage(target)?;
        self.driver.post(
            target.hwnd,
            WM_MOUSEMOVE,
            0,
            pack_point(target.to_client(at))?,
        )?;
        self.driver.sleep(Duration::from_millis(MOVE_SETTLE_MS));
        // `WM_MOUSEWHEEL` takes screen coordinates; the delta rides wParam.
        self.driver.post(
            target.hwnd,
            WM_MOUSEWHEEL,
            wheel_wparam(notches)?,
            pack_point(at)?,
        )?;
        Ok(())
    }

    /// Lowers the shield the inputs raised. `shield::hide` tolerates there being
    /// nothing up, which it must: this runs from the guard *and* from `Drop`.
    fn release(&mut self, _target: &Target) {
        self.driver.shield_hide();
    }
}

/// `WM_MOUSEWHEEL`'s wParam: the wheel delta in the high word, *signed 16-bit*.
///
/// Range-checked rather than shifted: `((delta as u32) << 16)` discards
/// everything above bit 15 with no diagnostic, so a large notch count would
/// scroll a wrong distance in the *opposite* direction while `PostMessageW`
/// reported success.
fn wheel_wparam(notches: i32) -> Result<usize, SurfaceError> {
    let delta = i16::try_from(notches.saturating_mul(WHEEL_DELTA)).map_err(|_| {
        SurfaceError::Recoverable(format!(
            "wheel delta for {notches} notches is out of wParam range"
        ))
    })?;
    Ok(usize::from(delta.cast_unsigned()) << 16)
}

/// `MAKELPARAM`: x in the low word, y in the high word, both signed 16-bit.
///
/// A coordinate that does not fit is refused rather than masked back inside the
/// window: `& 0xFFFF` would fold it silently onto some other pixel and the click
/// would still be posted. `Recoverable` because only an absurd client rect gets
/// here, and the next `acquire()` reads a fresh one.
///
/// The assembly goes through `u32`, not `i32`: shifting a high word past bit 31
/// in a signed integer sets the sign bit, and the `as isize` widening would then
/// sign-extend into the upper half of a 64-bit LPARAM.
pub(super) fn pack_point((x, y): (i32, i32)) -> Result<isize, SurfaceError> {
    let x = i16::try_from(x)
        .map_err(|_| SurfaceError::Recoverable(format!("x {x} out of LPARAM range")))?;
    let y = i16::try_from(y)
        .map_err(|_| SurfaceError::Recoverable(format!("y {y} out of LPARAM range")))?;
    let packed = (u32::from(y as u16) << 16) | u32::from(x as u16);
    Ok(packed as i32 as isize)
}

pub(super) fn post(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> Result<(), SurfaceError> {
    // SAFETY: `hwnd` may already be dead — `PostMessageW` reports that with
    // FALSE instead of faulting. Delivery is asynchronous, so nothing may be
    // borrowed by the queue: `wparam`/`lparam` carry packed coordinates and
    // button flags, never a pointer into this process.
    let ok = unsafe { PostMessageW(hwnd.raw(), msg, wparam, lparam) } != 0;
    if ok {
        return Ok(());
    }
    // Read before any other Win32 call: `GetLastError` is per-thread and the
    // very next call overwrites it. Here it decides whether the loop stops or
    // the watchdog retries.
    Err(post_refusal(&std::io::Error::last_os_error()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use windows_sys::Win32::Foundation::HWND;

    use crate::actuator::win::tests::{
        GAME_HWND, OTHER_HWND, dead_handle, game_rect, uipi_refusal,
    };

    // The specification the order tests below assert — read off the live
    // `click`/`scroll` above, not copied from the plan that asked for this
    // module, because the two are checked to agree independently:
    //
    // `click`:
    //   1. `verify`  — `client_rect(hwnd)`, compared to `target.rect`
    //   2. `shield::raise(hwnd, rect)` — `Sleep(SHIELD_DRAIN_MS)` follows
    //      *only* when it returned `true`
    //   3. `Post(WM_MOUSEMOVE, 0, lparam)`, `lparam` the **client** point
    //   4. `Sleep(MOVE_SETTLE_MS)`
    //   5. `Post(WM_LBUTTONDOWN, MK_LBUTTON, lparam)`
    //   6. `Sleep(press_ms)`
    //   7. `release_twice` → `Post(WM_LBUTTONUP, 0, lparam)`, retried once on
    //      a failed first attempt (see `release_twice`'s own contract)
    //
    // `scroll`:
    //   1. `verify` — same as `click`'s step 1
    //   2. `shield::raise` — same as `click`'s step 2
    //   3. `Post(WM_MOUSEMOVE, 0, lparam)`, `lparam` the **client** point
    //   4. `Sleep(MOVE_SETTLE_MS)`
    //   5. `Post(WM_MOUSEWHEEL, wheel_wparam(notches), lparam)` — this
    //      `lparam` is the **screen** point (`at`, unconverted): `WM_MOUSEWHEEL`
    //      takes screen coordinates by Win32 convention, unlike every other
    //      message this backend posts. Confirmed against the live code: this
    //      asymmetry is real, not a plan assumption.
    //
    // Verified against `MessageSurface::click`/`::scroll` above as they stood
    // before this test module existed: the live order matches what the plan
    // predicted, with no divergence.

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum MessageCall {
        FindWindow,
        Probe(Hwnd),
        ClientRect(Hwnd),
        Post(Hwnd, u32, usize, isize),
        ShieldRaise(Hwnd, ClientRect),
        ShieldHide,
        Sleep(u64),
    }

    struct FakeState {
        calls: Vec<MessageCall>,
        window: Hwnd,
        rect: ClientRect,
        find_results: VecDeque<Result<Hwnd, SurfaceError>>,
        probe_results: VecDeque<std::io::Result<()>>,
        rect_results: VecDeque<Result<ClientRect, SurfaceError>>,
        post_results: VecDeque<Result<(), SurfaceError>>,
        shield_raise_results: VecDeque<Result<bool, String>>,
    }

    impl Default for FakeState {
        fn default() -> Self {
            Self {
                calls: Vec::new(),
                window: GAME_HWND,
                rect: game_rect(),
                find_results: VecDeque::new(),
                probe_results: VecDeque::new(),
                rect_results: VecDeque::new(),
                post_results: VecDeque::new(),
                shield_raise_results: VecDeque::new(),
            }
        }
    }

    struct FakeMessageDriver {
        state: Arc<Mutex<FakeState>>,
    }

    impl MessageDriver for FakeMessageDriver {
        fn find_game_window(&mut self) -> Result<Hwnd, SurfaceError> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(MessageCall::FindWindow);
            if let Some(result) = state.find_results.pop_front() {
                result
            } else {
                Ok(state.window)
            }
        }

        fn probe_reachable(&mut self, hwnd: Hwnd) -> std::io::Result<()> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(MessageCall::Probe(hwnd));
            state.probe_results.pop_front().unwrap_or(Ok(()))
        }

        fn client_rect(&mut self, hwnd: Hwnd) -> Result<ClientRect, SurfaceError> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(MessageCall::ClientRect(hwnd));
            if let Some(result) = state.rect_results.pop_front() {
                result
            } else {
                Ok(state.rect)
            }
        }

        fn post(
            &mut self,
            hwnd: Hwnd,
            msg: u32,
            wparam: usize,
            lparam: isize,
        ) -> Result<(), SurfaceError> {
            let mut state = self.state.lock().unwrap();
            state
                .calls
                .push(MessageCall::Post(hwnd, msg, wparam, lparam));
            state.post_results.pop_front().unwrap_or(Ok(()))
        }

        fn shield_raise(&mut self, hwnd: Hwnd, rect: ClientRect) -> Result<bool, String> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(MessageCall::ShieldRaise(hwnd, rect));
            state.shield_raise_results.pop_front().unwrap_or(Ok(false))
        }

        fn shield_hide(&mut self) {
            self.state
                .lock()
                .unwrap()
                .calls
                .push(MessageCall::ShieldHide);
        }

        fn sleep(&mut self, duration: Duration) {
            self.state
                .lock()
                .unwrap()
                .calls
                .push(MessageCall::Sleep(duration.as_millis() as u64));
        }
    }

    fn moved_rect() -> ClientRect {
        ClientRect {
            left: 11,
            ..game_rect()
        }
    }

    fn fake_surface() -> (MessageSurface, Arc<Mutex<FakeState>>) {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let surface = MessageSurface::with_driver(FakeMessageDriver {
            state: state.clone(),
        });
        (surface, state)
    }

    /// Hands back the window every later call has to present, and clears the
    /// acquire's own calls out of the log so each test's assertion is about
    /// what it triggered, not `acquire`'s.
    fn acquire_and_clear(surface: &mut MessageSurface, state: &Arc<Mutex<FakeState>>) -> Target {
        let (target, rect) = surface.acquire().expect("the fake acquires");
        assert_eq!(rect, game_rect());
        state.lock().unwrap().calls.clear();
        target
    }

    fn calls(state: &Arc<Mutex<FakeState>>) -> Vec<MessageCall> {
        state.lock().unwrap().calls.clone()
    }

    fn posts(state: &Arc<Mutex<FakeState>>) -> Vec<MessageCall> {
        calls(state)
            .into_iter()
            .filter(|call| matches!(call, MessageCall::Post(..)))
            .collect()
    }

    /// `release` can be reached two or three times over one job with no shield
    /// ever raised, and `shield::hide` has to tolerate all of it.
    #[test]
    fn message_surface_cleanup_is_idempotent_without_a_shield() {
        let (mut surface, state) = fake_surface();
        let target = Target {
            hwnd: GAME_HWND,
            rect: game_rect(),
        };

        surface.release(&target);
        surface.release(&target);
        drop(surface);

        assert_eq!(
            calls(&state)
                .into_iter()
                .filter(|call| *call == MessageCall::ShieldHide)
                .count(),
            3,
            "the two explicit `release`s plus the one from `Drop` must each lower the shield"
        );
    }

    #[test]
    fn a_click_posts_move_then_down_then_up_in_that_order() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .shield_raise_results
            .push_back(Ok(true));

        assert_eq!(surface.click(&target, (400, 500), 25), Ok(()));

        let lparam = pack_point(target.to_client((400, 500))).unwrap();
        assert_eq!(
            calls(&state),
            vec![
                MessageCall::ClientRect(GAME_HWND),
                MessageCall::ShieldRaise(GAME_HWND, game_rect()),
                MessageCall::Sleep(SHIELD_DRAIN_MS),
                MessageCall::Post(GAME_HWND, WM_MOUSEMOVE, 0, lparam),
                MessageCall::Sleep(MOVE_SETTLE_MS),
                MessageCall::Post(GAME_HWND, WM_LBUTTONDOWN, MK_LBUTTON as usize, lparam),
                MessageCall::Sleep(25),
                MessageCall::Post(GAME_HWND, WM_LBUTTONUP, 0, lparam),
            ]
        );
    }

    #[test]
    fn the_shield_is_raised_before_any_message_is_posted() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);

        assert_eq!(surface.click(&target, (30, 40), 5), Ok(()));

        let recorded = calls(&state);
        let shield_at = recorded
            .iter()
            .position(|call| matches!(call, MessageCall::ShieldRaise(..)))
            .expect("the shield must be raised for every click");
        let first_post = recorded
            .iter()
            .position(|call| matches!(call, MessageCall::Post(..)))
            .expect("a click must post something");
        assert!(
            shield_at < first_post,
            "never click shieldless: {recorded:?}"
        );
    }

    #[test]
    fn a_freshly_placed_shield_gets_the_drain_beat() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .shield_raise_results
            .push_back(Ok(true));

        assert_eq!(surface.click(&target, (30, 40), 5), Ok(()));

        assert!(
            calls(&state).contains(&MessageCall::Sleep(SHIELD_DRAIN_MS)),
            "a (re)placed shield must drain before the move is posted"
        );
    }

    #[test]
    fn an_already_raised_shield_gets_no_drain_beat() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .shield_raise_results
            .push_back(Ok(false));

        assert_eq!(surface.click(&target, (30, 40), 5), Ok(()));

        assert!(
            !calls(&state).contains(&MessageCall::Sleep(SHIELD_DRAIN_MS)),
            "a shield already seated must not stretch the click with a drain beat"
        );
    }

    #[test]
    fn a_failed_shield_raise_is_fatal_and_posts_nothing() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .shield_raise_results
            .push_back(Err("raise the input shield: access denied".to_owned()));

        assert!(matches!(
            surface.click(&target, (30, 40), 5),
            Err(SurfaceError::Fatal(reason)) if reason.contains("access denied")
        ));
        assert!(posts(&state).is_empty());
    }

    #[test]
    fn a_moved_window_is_refused_before_the_shield_goes_up() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state
            .lock()
            .unwrap()
            .rect_results
            .push_back(Ok(moved_rect()));

        assert!(matches!(
            surface.click(&target, (30, 40), 5),
            Err(SurfaceError::Recoverable(reason)) if reason.contains("moved or resized")
        ));
        assert_eq!(
            calls(&state),
            vec![MessageCall::ClientRect(GAME_HWND)],
            "a moved window must be refused before the shield is touched or anything is posted"
        );
    }

    /// The invariant `engage`'s doc comment states plainly: never leave the
    /// left button held in the game window. `release_twice` is what enforces
    /// it, and this is the one scenario where enforcing it matters — the
    /// first `WM_LBUTTONUP` is refused.
    #[test]
    fn the_button_is_released_even_when_the_first_release_post_fails() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);
        state.lock().unwrap().post_results.extend([
            Ok(()),                                                  // MOUSEMOVE
            Ok(()),                                                  // LBUTTONDOWN
            Err(SurfaceError::Recoverable("queue full".to_owned())), // first LBUTTONUP
            Ok(()),                                                  // retried LBUTTONUP
        ]);

        // `release_twice` reports the *first* failure once the button is
        // provably up, so this is `Err`, not `Ok` — what matters here is that
        // the retry happened at all.
        assert!(matches!(
            surface.click(&target, (30, 40), 5),
            Err(SurfaceError::Recoverable(reason)) if reason.contains("queue full")
        ));

        let lparam = pack_point(target.to_client((30, 40))).unwrap();
        let up_posts = calls(&state)
            .into_iter()
            .filter(|call| *call == MessageCall::Post(GAME_HWND, WM_LBUTTONUP, 0, lparam))
            .count();
        assert_eq!(
            up_posts, 2,
            "a refused release must be retried, never left held"
        );
    }

    #[test]
    fn a_scroll_posts_move_then_wheel_with_screen_coordinates() {
        let (mut surface, state) = fake_surface();
        let target = acquire_and_clear(&mut surface, &state);

        assert_eq!(surface.scroll(&target, (300, 600), -2), Ok(()));

        let client_lparam = pack_point(target.to_client((300, 600))).unwrap();
        let screen_lparam = pack_point((300, 600)).unwrap();
        assert_ne!(
            client_lparam, screen_lparam,
            "the test target must actually distinguish client from screen coordinates"
        );
        assert_eq!(
            calls(&state),
            vec![
                MessageCall::ClientRect(GAME_HWND),
                MessageCall::ShieldRaise(GAME_HWND, game_rect()),
                MessageCall::Post(GAME_HWND, WM_MOUSEMOVE, 0, client_lparam),
                MessageCall::Sleep(MOVE_SETTLE_MS),
                MessageCall::Post(
                    GAME_HWND,
                    WM_MOUSEWHEEL,
                    wheel_wparam(-2).unwrap(),
                    screen_lparam
                ),
            ]
        );
    }

    #[test]
    fn an_access_denied_post_stops_the_loop_instead_of_being_retried_forever() {
        // It was `Recoverable`, so the watchdog re-issued clicks against a
        // condition that cannot heal while both processes keep running.
        let SurfaceError::Fatal(reason) = post_refusal(&uipi_refusal()) else {
            panic!("a UIPI refusal must be fatal");
        };
        assert!(reason.contains("integrity level"), "{reason}");
        // The two causes the old text named are both wrong for this one.
        assert!(!reason.contains("queue is full"), "{reason}");
        assert!(!reason.contains("closed"), "{reason}");
    }

    #[test]
    fn any_other_post_failure_stays_recoverable_and_keeps_its_plain_wording() {
        let SurfaceError::Recoverable(reason) = post_refusal(&dead_handle()) else {
            panic!("only a UIPI refusal is fatal here");
        };
        assert!(reason.contains("PostMessageW failed"), "{reason}");
        assert!(!reason.contains("integrity"), "{reason}");
    }

    // Do not re-add `message_surface_input_without_acquire_is_fatal_not_a_panic`:
    // the window is a parameter now, so the state it described cannot be
    // constructed. See the note where its `WinSurface` twin was, in
    // `send_input`'s test module.

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

    /// Win32 reads back the low 32 bits as two sign-extended words, and every
    /// accepted coordinate must survive that — high words past 0x8000 included.
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

    /// Not reachable from the crate's own `±10` notches — which is the point:
    /// the trait boundary is what a future caller reaches.
    #[test]
    fn a_wheel_delta_outside_the_signed_word_is_refused_instead_of_truncated() {
        // 300 notches × 120 = 36 000, past `i16::MAX`: the old form posted
        // 36 000 − 65 536 = −29 536, i.e. a scroll the other way.
        assert!(matches!(
            wheel_wparam(300),
            Err(SurfaceError::Recoverable(reason)) if reason.contains("out of wParam range")
        ));
        assert!(matches!(
            wheel_wparam(-300),
            Err(SurfaceError::Recoverable(_))
        ));
    }

    /// The notch counts the planner actually emits.
    #[test]
    fn wheel_wparam_carries_the_notches_as_a_signed_high_word() {
        assert_eq!(wheel_wparam(10).unwrap() >> 16, 1_200);
        let down = wheel_wparam(-10).unwrap();
        assert_eq!((down >> 16) as u16 as i16, -1_200);
        assert_eq!(wheel_wparam(0).unwrap(), 0);
        // The low word is wParam's button-state field and must stay clear.
        assert_eq!(wheel_wparam(10).unwrap() & 0xFFFF, 0);
    }

    /// The transposition the newtype exists to stop cannot be written here — a
    /// `#[test]` cannot assert a compile error — so what is pinned instead is
    /// that a handle and a packed point are no longer the same type.
    #[test]
    fn a_handle_survives_the_round_trip_to_the_abi_type_and_back() {
        let packed: isize = pack_point((0x1234, 0x5678)).unwrap();
        let hwnd = Hwnd::new(packed as HWND);
        assert_eq!(hwnd.raw() as isize, packed);
        assert_eq!(Hwnd::new(std::ptr::null_mut()), Hwnd(0));
        assert_ne!(GAME_HWND, OTHER_HWND);
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
