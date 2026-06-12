use std::sync::Mutex;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};

use image::{DynamicImage, GrayImage, RgbaImage};
use xcap::Window;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Send + Sync because the GUI thread shares this with the worker
/// behind an Arc. Production impl is `WindowCapture`; tests fake it.
pub trait Capture: Send + Sync {
    fn snapshot_gray(&self) -> Result<GrayImage>;
    /// Default impl synthesises from `snapshot_gray` so existing fakes
    /// don't have to change. Real `WindowCapture` overrides with the
    /// actual RGBA WGC capture; colour-dependent paths (FP filter) on
    /// a gray fake just reject everything.
    fn snapshot_rgba(&self) -> Result<RgbaImage> {
        let gray = self.snapshot_gray()?;
        Ok(image::DynamicImage::ImageLuma8(gray).into_rgba8())
    }
    fn rect(&self) -> Result<WindowRect>;
    fn check_size_stable(&self, tolerance_px: u32) -> Result<()>;
    fn restore_to_baseline(&self) -> Result<bool>;
    fn local_to_screen(&self, local_x: i32, local_y: i32) -> Result<(i32, i32)>;
    fn is_foreground(&self) -> bool;
    fn try_bring_foreground(&self) -> bool;
    fn hwnd_is_valid(&self) -> bool;
    fn reattach(&self) -> Result<()>;
}

impl Capture for WindowCapture {
    fn snapshot_gray(&self) -> Result<GrayImage> {
        WindowCapture::snapshot_gray(self)
    }
    fn snapshot_rgba(&self) -> Result<RgbaImage> {
        WindowCapture::snapshot(self)
    }
    fn rect(&self) -> Result<WindowRect> {
        WindowCapture::rect(self)
    }
    fn check_size_stable(&self, tolerance_px: u32) -> Result<()> {
        WindowCapture::check_size_stable(self, tolerance_px)
    }
    fn restore_to_baseline(&self) -> Result<bool> {
        WindowCapture::restore_to_baseline(self)
    }
    fn local_to_screen(&self, x: i32, y: i32) -> Result<(i32, i32)> {
        WindowCapture::local_to_screen(self, x, y)
    }
    fn is_foreground(&self) -> bool {
        WindowCapture::is_foreground(self)
    }
    fn try_bring_foreground(&self) -> bool {
        WindowCapture::try_bring_foreground(self)
    }
    fn hwnd_is_valid(&self) -> bool {
        WindowCapture::hwnd_is_valid(self)
    }
    fn reattach(&self) -> Result<()> {
        WindowCapture::reattach(self)
    }
}

pub struct WindowCapture {
    /// `Mutex` so `reattach` can swap the xcap handle through `&self`
    /// (capture is shared across threads behind `Arc`).
    window: Mutex<Window>,
    /// Retained so `reattach` can re-find the window after HWND was
    /// destroyed.
    title_contains: String,
    process_name: Option<String>,
    baseline_w: AtomicU32,
    baseline_h: AtomicU32,
    /// Zero = no HWND resolved.
    hwnd_raw: AtomicIsize,
}

impl WindowCapture {
    /// `MultipleWindowsMatched` when more than one non-minimized window
    /// matches — user must narrow `title_contains` or set `process_name`.
    pub fn find(title_contains: &str, process_name: Option<&str>) -> Result<Self> {
        let window = locate_window(title_contains, process_name)?;
        let (w, h) = (window.width()?, window.height()?);
        let hwnd_raw = {
            let title = window.title().unwrap_or_default();
            find_hwnd_raw_for_title(&title)
        };
        Ok(Self {
            window: Mutex::new(window),
            title_contains: title_contains.to_string(),
            process_name: process_name.map(str::to_string),
            baseline_w: AtomicU32::new(w),
            baseline_h: AtomicU32::new(h),
            hwnd_raw: AtomicIsize::new(hwnd_raw),
        })
    }

    /// Poison means an earlier panic mid-FFI, not corrupt state — the slot
    /// only ever holds a whole `Window`, so the last value is safe to reuse.
    fn window_lock(&self) -> std::sync::MutexGuard<'_, Window> {
        self.window.lock().unwrap_or_else(|e| {
            tracing::debug!("window mutex poisoned by an earlier panic — recovering");
            e.into_inner()
        })
    }

    /// Caller (runner) catches `WindowNotFound` and retries with backoff.
    pub fn reattach(&self) -> Result<()> {
        let window = locate_window(&self.title_contains, self.process_name.as_deref())?;
        let title = window.title().unwrap_or_default();
        let raw = find_hwnd_raw_for_title(&title);
        // Re-baseline so check_size_stable doesn't false-positive on a
        // relaunched window coming back at a slightly different size.
        if let (Ok(w), Ok(h)) = (window.width(), window.height()) {
            self.baseline_w.store(w, Ordering::Relaxed);
            self.baseline_h.store(h, Ordering::Relaxed);
        }
        // Swap Window and HWND under one lock so readers (snapshot / rect)
        // see a consistent pair — otherwise an old WGC frame could be
        // cropped against the new window's client rect. Release pairs
        // with the Acquire load in client_offset_and_size.
        let mut slot = self.window_lock();
        *slot = window;
        self.hwnd_raw.store(raw, Ordering::Release);
        Ok(())
    }

    /// Client-area rect (excludes OS title bar + borders). Layout
    /// ratios in `crate::layout` are calibrated against this so they
    /// stay stable across window sizes — the title bar is a fixed pixel
    /// height, not a fixed proportion. Falls back to the raw window
    /// rect if Win32 client-rect query fails.
    pub fn rect(&self) -> Result<WindowRect> {
        let window = self.window_lock();
        if window.is_minimized().unwrap_or(false) {
            return Err(Error::WindowGone);
        }
        let win_x = window.x()?;
        let win_y = window.y()?;
        let win_w = window.width()?;
        let win_h = window.height()?;
        let client = self.client_offset_and_size();
        drop(window);
        match client {
            Some((off_x, off_y, cw, ch)) if cw > 0 && ch > 0 => Ok(WindowRect {
                x: win_x + off_x,
                y: win_y + off_y,
                width: cw,
                height: ch,
            }),
            _ => Ok(WindowRect {
                x: win_x,
                y: win_y,
                width: win_w,
                height: win_h,
            }),
        }
    }

    /// Raw OS window rect including title bar + borders. Used by
    /// `check_size_stable` and `restore_to_baseline` because `SetWindowPos`
    /// takes window dims, not client dims.
    fn window_rect(&self) -> Result<WindowRect> {
        let window = self.window_lock();
        if window.is_minimized().unwrap_or(false) {
            return Err(Error::WindowGone);
        }
        Ok(WindowRect {
            x: window.x()?,
            y: window.y()?,
            width: window.width()?,
            height: window.height()?,
        })
    }

    /// `(client_x_in_window, client_y_in_window, client_w, client_h)`.
    /// `None` if HWND unresolved or Win32 calls fail — callers fall
    /// back to treating the window rect as the client rect. Cheap
    /// (~3 syscalls), per-snapshot so resizes pick up without reattach.
    fn client_offset_and_size(&self) -> Option<(i32, i32, u32, u32)> {
        use std::ffi::c_void;
        use windows::Win32::Foundation::{HWND, POINT, RECT};
        use windows::Win32::Graphics::Gdi::ClientToScreen;
        use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowRect};

        let raw = self.hwnd_raw.load(Ordering::Acquire);
        if raw == 0 {
            return None;
        }
        let hwnd = HWND(raw as *mut c_void);

        let mut win_rect = RECT::default();
        let mut client_rect = RECT::default();
        unsafe {
            if GetWindowRect(hwnd, &mut win_rect).is_err() {
                return None;
            }
            if GetClientRect(hwnd, &mut client_rect).is_err() {
                return None;
            }
        }
        let mut origin = POINT { x: 0, y: 0 };
        if !unsafe { ClientToScreen(hwnd, &mut origin) }.as_bool() {
            return None;
        }

        let off_x = origin.x - win_rect.left;
        let off_y = origin.y - win_rect.top;
        let w = (client_rect.right - client_rect.left).max(0) as u32;
        let h = (client_rect.bottom - client_rect.top).max(0) as u32;
        Some((off_x, off_y, w, h))
    }

    pub fn title(&self) -> Result<String> {
        let window = self.window_lock();
        Ok(window.title()?)
    }

    /// `tolerance_px` absorbs 1-2 px jitter from taskbar visibility /
    /// DPI rounding. Compares raw OS window dims (not client dims) —
    /// stays consistent with what `restore_to_baseline` would push via
    /// `SetWindowPos`.
    pub fn check_size_stable(&self, tolerance_px: u32) -> Result<()> {
        let r = self.window_rect()?;
        let iw = self.baseline_w.load(Ordering::Relaxed);
        let ih = self.baseline_h.load(Ordering::Relaxed);
        let dw = r.width.abs_diff(iw);
        let dh = r.height.abs_diff(ih);
        if dw > tolerance_px || dh > tolerance_px {
            return Err(Error::WindowResized {
                initial_w: iw,
                initial_h: ih,
                current_w: r.width,
                current_h: r.height,
            });
        }
        Ok(())
    }

    /// Crops the WGC frame to the client area so it matches what
    /// `rect()` reports. Falls back to the uncropped frame if Win32
    /// client-rect query fails.
    pub fn snapshot(&self) -> Result<RgbaImage> {
        let window = self.window_lock();
        let full = window.capture_image()?;
        let client = self.client_offset_and_size();
        drop(window);
        let Some((off_x, off_y, cw, ch)) = client else {
            return Ok(full);
        };
        if cw == 0 || ch == 0 {
            return Ok(full);
        }
        match client_crop_rect(full.width(), full.height(), off_x, off_y, cw, ch) {
            None => {
                tracing::warn!(
                    off_x,
                    off_y,
                    img_w = full.width(),
                    img_h = full.height(),
                    "client offset outside frame, using full"
                );
                Ok(full)
            }
            Some((x, y, w, h)) => Ok(image::imageops::crop_imm(&full, x, y, w, h).to_image()),
        }
    }

    pub fn snapshot_gray(&self) -> Result<GrayImage> {
        let rgba = self.snapshot()?;
        Ok(DynamicImage::ImageRgba8(rgba).into_luma8())
    }

    pub fn local_to_screen(&self, local_x: i32, local_y: i32) -> Result<(i32, i32)> {
        let r = self.rect()?;
        Ok((r.x + local_x, r.y + local_y))
    }

    /// Force client size to `(client_w, client_h)` via `SetWindowPos`,
    /// converting to outer dims via the current chrome delta. Templates
    /// and `base_resolution` are calibrated against client area, so the
    /// caller passes client dims even though SetWindowPos takes outer.
    /// `Ok(false)` if no HWND was resolved.
    pub fn resize_to(&self, client_w: u32, client_h: u32) -> Result<bool> {
        use std::ffi::c_void;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{SWP_NOMOVE, SWP_NOZORDER, SetWindowPos};
        let raw = self.hwnd_raw.load(Ordering::Acquire);
        if raw == 0 {
            return Ok(false);
        }
        let hwnd = HWND(raw as *mut c_void);
        let (chrome_w, chrome_h) = self.window_chrome_size().unwrap_or((0, 0));
        let outer_w = client_w.saturating_add(chrome_w);
        let outer_h = client_h.saturating_add(chrome_h);
        unsafe {
            SetWindowPos(
                hwnd,
                None,
                0,
                0,
                outer_w as i32,
                outer_h as i32,
                SWP_NOMOVE | SWP_NOZORDER,
            )?;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        // Baseline tracks outer dims so check_size_stable and
        // restore_to_baseline (both SetWindowPos consumers) stay aligned.
        if let Ok(r) = self.window_rect() {
            self.baseline_w.store(r.width, Ordering::Relaxed);
            self.baseline_h.store(r.height, Ordering::Relaxed);
        }
        Ok(true)
    }

    /// `(outer_w - client_w, outer_h - client_h)` — the title-bar +
    /// border padding added by the OS to translate client dims into
    /// `SetWindowPos`-friendly outer dims.
    fn window_chrome_size(&self) -> Option<(u32, u32)> {
        let outer = self.window_rect().ok()?;
        let (_, _, client_w, client_h) = self.client_offset_and_size()?;
        Some((
            outer.width.saturating_sub(client_w),
            outer.height.saturating_sub(client_h),
        ))
    }

    /// Snap back to the construction-time size. Baseline is NOT updated
    /// so the next `check_size_stable` still fails loudly if
    /// `SetWindowPos` couldn't reach the target.
    pub fn restore_to_baseline(&self) -> Result<bool> {
        use std::ffi::c_void;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{SWP_NOMOVE, SWP_NOZORDER, SetWindowPos};
        let raw = self.hwnd_raw.load(Ordering::Acquire);
        if raw == 0 {
            return Ok(false);
        }
        let w = self.baseline_w.load(Ordering::Relaxed);
        let h = self.baseline_h.load(Ordering::Relaxed);
        let hwnd = HWND(raw as *mut c_void);
        unsafe {
            SetWindowPos(
                hwnd,
                None,
                0,
                0,
                w as i32,
                h as i32,
                SWP_NOMOVE | SWP_NOZORDER,
            )?;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        Ok(true)
    }

    pub fn is_foreground(&self) -> bool {
        use std::ffi::c_void;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        if !self.hwnd_is_valid() {
            return false;
        }
        let stored = HWND(self.hwnd_raw.load(Ordering::Acquire) as *mut c_void);
        let fg = unsafe { GetForegroundWindow() };
        if fg == stored {
            return true;
        }
        // E7 on STOVE wraps the game inside a launcher HWND, so
        // `stored` is on the wrapper while the visible window is a
        // child. Same-process matching counts those as foreground.
        let stored_pid = pid_of_hwnd(stored);
        let fg_pid = pid_of_hwnd(fg);
        stored_pid != 0 && stored_pid == fg_pid
    }

    /// `IsWindow` check distinguishes "game crashed" from "foreground
    /// locked by another app" so callers can surface a clear error.
    pub fn hwnd_is_valid(&self) -> bool {
        use std::ffi::c_void;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::IsWindow;
        let raw = self.hwnd_raw.load(Ordering::Acquire);
        if raw == 0 {
            return false;
        }
        let hwnd = HWND(raw as *mut c_void);
        unsafe { IsWindow(Some(hwnd)) }.as_bool()
    }

    /// `AttachThreadInput` works around Windows' foreground-lock rules
    /// (a process can only steal focus if it owns the current foreground
    /// OR the user has interacted recently).
    pub fn try_bring_foreground(&self) -> bool {
        use std::ffi::c_void;
        use std::thread;
        use std::time::{Duration, Instant};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
        use windows::Win32::UI::WindowsAndMessaging::{
            BringWindowToTop, GetWindowThreadProcessId, IsIconic, SW_RESTORE, SetForegroundWindow,
            ShowWindow,
        };
        if self.is_foreground() {
            return true;
        }
        let hwnd = HWND(self.hwnd_raw.load(Ordering::Acquire) as *mut c_void);
        unsafe {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let target_thread = GetWindowThreadProcessId(hwnd, None);
            let current_thread = GetCurrentThreadId();
            let attached = target_thread != 0
                && target_thread != current_thread
                && AttachThreadInput(current_thread, target_thread, true).as_bool();
            let _ = SetForegroundWindow(hwnd);
            let _ = BringWindowToTop(hwnd);
            if attached {
                let _ = AttachThreadInput(current_thread, target_thread, false);
            }
        }
        // SetForegroundWindow is async — the immediate
        // GetForegroundWindow may still report the old window. Poll up
        // to 200 ms for the focus to actually land on hwnd.
        const POLL_DEADLINE_MS: u64 = 200;
        const POLL_TICK_MS: u64 = 15;
        let deadline = Instant::now() + Duration::from_millis(POLL_DEADLINE_MS);
        while Instant::now() < deadline {
            if self.is_foreground() {
                return true;
            }
            thread::sleep(Duration::from_millis(POLL_TICK_MS));
        }
        // Last gasp + diagnostic: log what's actually foreground so a
        // user reproducing this can paste the WHAT-vs-EXPECTED.
        let now_foreground = self.is_foreground();
        if !now_foreground {
            log_foreground_mismatch(self.hwnd_raw.load(Ordering::Acquire));
        }
        now_foreground
    }
}

fn log_foreground_mismatch(stored_raw: isize) {
    use std::ffi::c_void;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW};
    let fg = unsafe { GetForegroundWindow() };
    let fg_raw = fg.0 as isize;
    let stored_hwnd = HWND(stored_raw as *mut c_void);
    let stored_pid = pid_of_hwnd(stored_hwnd);
    let fg_pid = pid_of_hwnd(fg);
    let mut buf = [0u16; 256];
    let len = unsafe { GetWindowTextW(fg, &mut buf) };
    let fg_title = if len > 0 {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        String::new()
    };
    tracing::debug!(
        stored_hwnd = stored_raw,
        stored_pid,
        foreground_hwnd = fg_raw,
        foreground_pid = fg_pid,
        foreground_title = %fg_title,
        "foreground bring failed — diagnostic snapshot"
    );
}

fn pid_of_hwnd(hwnd: windows::Win32::Foundation::HWND) -> u32 {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    let mut pid: u32 = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    pid
}

/// Epic Seven uses GLFW for windowing — no other common Windows app
/// uses this class, so it's the strongest fingerprint we have. Confirmed
/// across multiple setups (Global, see PR notes).
const GAME_WINDOW_CLASS: &str = "GLFW30";
const GAME_PROCESS_NAME: &str = "EpicSeven";
/// Below this in either dim, the candidate is almost certainly a
/// tooltip, a minimized restore, or some chrome popup that happens to
/// contain "Epic Seven" in the title.
const MIN_GAME_WINDOW_DIM: u32 = 400;

/// Shared lookup used by both `find` (boot) and `reattach` (recovery).
/// Scores candidates matching the title filter so the real game window
/// wins over title-substring false positives (eg. a browser tab titled
/// "Epic Seven — Codex…"). True ties (same top score) still raise
/// `MultipleWindowsMatched` so the user can disambiguate.
fn locate_window(title_contains: &str, process_name: Option<&str>) -> Result<Window> {
    let title_needle = title_contains.to_lowercase();
    let proc_needle = process_name.map(str::to_lowercase);
    let all = Window::all()?;

    let mut scored: Vec<(Window, String, i32)> = Vec::new();
    for w in all {
        if w.is_minimized().unwrap_or(false) {
            continue;
        }
        let title = w.title().unwrap_or_default();
        if !title.to_lowercase().contains(&title_needle) {
            continue;
        }
        if let Some(proc) = &proc_needle {
            let app = w.app_name().unwrap_or_default().to_lowercase();
            if !app.contains(proc) {
                continue;
            }
        }
        let score = score_candidate(&w, &title, title_contains);
        scored.push((w, title, score));
    }

    if scored.is_empty() {
        return Err(Error::WindowNotFound(title_contains.into()));
    }

    scored.sort_by_key(|c| std::cmp::Reverse(c.2));
    let top_score = scored[0].2;
    let tied_count = scored.iter().filter(|(_, _, s)| *s == top_score).count();

    if tied_count == 1 {
        let (best, title, score) = scored.into_iter().next().expect("scored non-empty");
        tracing::debug!(title = %title, score, "window locate: picked best candidate");
        Ok(best)
    } else {
        let mut listed: Vec<String> = scored
            .iter()
            .filter(|(_, _, s)| *s == top_score)
            .map(|(w, t, _)| format!("'{}' [{}]", t, w.app_name().unwrap_or_default()))
            .collect();
        listed.sort();
        Err(Error::MultipleWindowsMatched { candidates: listed })
    }
}

fn score_candidate(w: &Window, title: &str, title_needle: &str) -> i32 {
    let mut score = 0;

    let hwnd_raw = find_hwnd_raw_for_title(title);
    if class_name_of_hwnd_raw(hwnd_raw).as_deref() == Some(GAME_WINDOW_CLASS) {
        score += 100;
    }

    let app = w.app_name().unwrap_or_default();
    if app.eq_ignore_ascii_case(GAME_PROCESS_NAME) {
        score += 50;
    }

    if title.eq_ignore_ascii_case(title_needle) {
        score += 20;
    }

    let ww = w.width().unwrap_or(0);
    let wh = w.height().unwrap_or(0);
    if ww >= MIN_GAME_WINDOW_DIM && wh >= MIN_GAME_WINDOW_DIM {
        score += 5;
    }

    score
}

fn class_name_of_hwnd_raw(raw: isize) -> Option<String> {
    use std::ffi::c_void;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    if raw == 0 {
        return None;
    }
    let hwnd = HWND(raw as *mut c_void);
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len <= 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// Must run BEFORE `WindowCapture` is constructed — WGC ties its frame
/// pool to the window size at session start, and a resize afterwards
/// makes `capture_image()` block indefinitely.
pub fn ensure_window_size(title_contains: &str, width: u32, height: u32) -> Result<bool> {
    use std::ffi::c_void;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{SWP_NOMOVE, SWP_NOZORDER, SetWindowPos};

    let raw = find_hwnd_raw_for_title_contains(title_contains);
    if raw == 0 {
        return Ok(false);
    }
    let hwnd = HWND(raw as *mut c_void);
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            width as i32,
            height as i32,
            SWP_NOMOVE | SWP_NOZORDER,
        )?;
    }
    std::thread::sleep(std::time::Duration::from_millis(400));
    Ok(true)
}

pub fn preflight_resize_if_enabled(window: &crate::config::WindowConfig) {
    use tracing::{info, warn};
    if !window.auto_resize {
        return;
    }
    let [bw, bh] = window.base_resolution;
    match ensure_window_size(&window.title_contains, bw, bh) {
        Ok(true) => info!(target = format!("{bw}x{bh}"), "pre-flight resize issued"),
        Ok(false) => warn!("pre-flight resize: window not found yet"),
        Err(e) => warn!(error = %e, "pre-flight resize failed"),
    }
}

#[derive(Copy, Clone)]
enum TitleMatch {
    Contains,
    Exact,
}

fn enum_visible_windows_find(needle: &str, mode: TitleMatch) -> isize {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, IsWindowVisible};
    use windows::core::BOOL;

    struct State {
        needle: String,
        case_insensitive: bool,
        substring: bool,
        found: isize,
    }
    let mut state = State {
        needle: match mode {
            TitleMatch::Contains => needle.to_lowercase(),
            TitleMatch::Exact => needle.to_string(),
        },
        case_insensitive: matches!(mode, TitleMatch::Contains),
        substring: matches!(mode, TitleMatch::Contains),
        found: 0,
    };

    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let state = unsafe { &mut *(lparam.0 as *mut State) };
        if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            return BOOL(1);
        }
        let mut buf = [0u16; 512];
        let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
        if len <= 0 {
            return BOOL(1);
        }
        let title = String::from_utf16_lossy(&buf[..len as usize]);
        let title = if state.case_insensitive {
            title.to_lowercase()
        } else {
            title
        };
        let hit = if state.substring {
            title.contains(&state.needle)
        } else {
            title == state.needle
        };
        if hit {
            state.found = hwnd.0 as isize;
            return BOOL(0);
        }
        BOOL(1)
    }

    let lparam = LPARAM(&raw mut state as isize);
    unsafe {
        let _ = EnumWindows(Some(cb), lparam);
    }
    state.found
}

fn find_hwnd_raw_for_title_contains(needle: &str) -> isize {
    enum_visible_windows_find(needle, TitleMatch::Contains)
}

fn find_hwnd_raw_for_title(needle: &str) -> isize {
    enum_visible_windows_find(needle, TitleMatch::Exact)
}

/// Crop rect for the client area within a captured frame; `None` = use the
/// full frame (degenerate client dims or offset outside the frame).
fn client_crop_rect(
    img_w: u32,
    img_h: u32,
    off_x: i32,
    off_y: i32,
    cw: u32,
    ch: u32,
) -> Option<(u32, u32, u32, u32)> {
    if cw == 0 || ch == 0 {
        return None;
    }
    let x = off_x.max(0) as u32;
    let y = off_y.max(0) as u32;
    if x >= img_w || y >= img_h {
        return None;
    }
    Some((x, y, cw.min(img_w - x), ch.min(img_h - y)))
}

#[cfg(test)]
mod tests {
    use super::client_crop_rect;

    #[test]
    fn crop_rect_normal_case() {
        assert_eq!(
            client_crop_rect(1920, 1080, 8, 31, 1904, 1041),
            Some((8, 31, 1904, 1041))
        );
    }

    #[test]
    fn crop_rect_negative_offsets_clamp_to_zero() {
        assert_eq!(
            client_crop_rect(1920, 1080, -5, -3, 1920, 1080),
            Some((0, 0, 1920, 1080))
        );
    }

    #[test]
    fn crop_rect_offset_outside_frame_returns_none() {
        assert_eq!(client_crop_rect(800, 600, 800, 0, 100, 100), None);
        assert_eq!(client_crop_rect(800, 600, 0, 600, 100, 100), None);
    }

    #[test]
    fn crop_rect_zero_client_dims_return_none() {
        assert_eq!(client_crop_rect(1920, 1080, 8, 31, 0, 1041), None);
        assert_eq!(client_crop_rect(1920, 1080, 8, 31, 1904, 0), None);
    }

    #[test]
    fn crop_rect_clips_to_frame_bounds() {
        assert_eq!(
            client_crop_rect(800, 600, 700, 500, 400, 400),
            Some((700, 500, 100, 100))
        );
    }
}
