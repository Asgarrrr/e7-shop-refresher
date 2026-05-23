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

/// Read-only surface for `ShopRunner`. Implemented by `WindowCapture`
/// for production; tests substitute a fake. `Send + Sync` because the
/// GUI thread shares this with the worker thread behind an `Arc`.
pub trait Capture: Send + Sync {
    fn snapshot_gray(&self) -> Result<GrayImage>;
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
    /// `Mutex` so `reattach()` can swap the live xcap handle through
    /// `&self` (capture is shared across threads behind `Arc`).
    window: Mutex<Window>,
    /// Lookup parameters retained so `reattach()` can re-find the game
    /// window after the HWND has been destroyed.
    title_contains: String,
    process_name: Option<String>,
    baseline_w: AtomicU32,
    baseline_h: AtomicU32,
    /// Zero = no HWND resolved.
    hwnd_raw: AtomicIsize,
}

impl WindowCapture {
    /// Errors with `MultipleWindowsMatched` if more than one non-minimized
    /// window matches — the user must narrow `title_contains` or set
    /// `process_name`.
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

    /// Re-run the lookup and swap the inner `Window` if a new match is
    /// found. The caller (the runner) catches `WindowNotFound` and retries
    /// with backoff.
    pub fn reattach(&self) -> Result<()> {
        let window = locate_window(&self.title_contains, self.process_name.as_deref())?;
        // Refresh the HWND first so any concurrent hwnd_is_valid() lines
        // up with the new window.
        {
            let title = window.title().unwrap_or_default();
            let raw = find_hwnd_raw_for_title(&title);
            self.hwnd_raw.store(raw, Ordering::Relaxed);
        }
        // Re-baseline so the next check_size_stable doesn't false-positive
        // on the relaunched window coming back at a slightly different size.
        if let (Ok(w), Ok(h)) = (window.width(), window.height()) {
            self.baseline_w.store(w, Ordering::Relaxed);
            self.baseline_h.store(h, Ordering::Relaxed);
        }
        let mut slot = self.window.lock().expect("window mutex poisoned");
        *slot = window;
        Ok(())
    }

    pub fn rect(&self) -> Result<WindowRect> {
        let window = self.window.lock().expect("window mutex poisoned");
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

    pub fn title(&self) -> Result<String> {
        let window = self.window.lock().expect("window mutex poisoned");
        Ok(window.title()?)
    }

    /// `tolerance_px` absorbs 1-2 px jitter from taskbar visibility / DPI
    /// rounding without bailing on a legitimate run.
    pub fn check_size_stable(&self, tolerance_px: u32) -> Result<()> {
        let r = self.rect()?;
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

    pub fn snapshot(&self) -> Result<RgbaImage> {
        let window = self.window.lock().expect("window mutex poisoned");
        Ok(window.capture_image()?)
    }

    pub fn snapshot_gray(&self) -> Result<GrayImage> {
        let rgba = self.snapshot()?;
        Ok(DynamicImage::ImageRgba8(rgba).into_luma8())
    }

    pub fn local_to_screen(&self, local_x: i32, local_y: i32) -> Result<(i32, i32)> {
        let r = self.rect()?;
        Ok((r.x + local_x, r.y + local_y))
    }

    /// Force outer size via `SetWindowPos`, then re-baseline to whatever
    /// the OS actually granted (can differ due to DPI / borders / min-size).
    /// `Ok(false)` if no HWND was resolved at construction.
    pub fn resize_to(&self, width: u32, height: u32) -> Result<bool> {
        use std::ffi::c_void;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{SWP_NOMOVE, SWP_NOZORDER, SetWindowPos};
        let raw = self.hwnd_raw.load(Ordering::Relaxed);
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
        std::thread::sleep(std::time::Duration::from_millis(300));
        if let Ok(r) = self.rect() {
            self.baseline_w.store(r.width, Ordering::Relaxed);
            self.baseline_h.store(r.height, Ordering::Relaxed);
        }
        Ok(true)
    }

    /// Snap back to the construction-time size (= the size templates were
    /// cropped against). Baseline is NOT updated: if SetWindowPos can't
    /// reach the target, the next `check_size_stable` still fails loudly.
    pub fn restore_to_baseline(&self) -> Result<bool> {
        use std::ffi::c_void;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{SWP_NOMOVE, SWP_NOZORDER, SetWindowPos};
        let raw = self.hwnd_raw.load(Ordering::Relaxed);
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
        let hwnd = HWND(self.hwnd_raw.load(Ordering::Relaxed) as *mut c_void);
        unsafe { GetForegroundWindow() == hwnd }
    }

    /// `IsWindow` check distinguishes "game crashed" from "foreground
    /// locked by another app" so callers can surface a clear error.
    pub fn hwnd_is_valid(&self) -> bool {
        use std::ffi::c_void;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::IsWindow;
        let raw = self.hwnd_raw.load(Ordering::Relaxed);
        if raw == 0 {
            return false;
        }
        let hwnd = HWND(raw as *mut c_void);
        unsafe { IsWindow(Some(hwnd)) }.as_bool()
    }

    /// `AttachThreadInput` is the standard workaround for Windows'
    /// foreground-lock rules (a process can only steal focus if it owns
    /// the current foreground OR the user has interacted recently).
    pub fn try_bring_foreground(&self) -> bool {
        use std::ffi::c_void;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
        use windows::Win32::UI::WindowsAndMessaging::{
            BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, SW_RESTORE,
            SetForegroundWindow, ShowWindow,
        };
        if !self.hwnd_is_valid() {
            return false;
        }
        let hwnd = HWND(self.hwnd_raw.load(Ordering::Relaxed) as *mut c_void);
        unsafe {
            if GetForegroundWindow() == hwnd {
                return true;
            }
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
            GetForegroundWindow() == hwnd
        }
    }
}

/// Shared lookup used by both `find` (boot) and `reattach` (recovery).
fn locate_window(title_contains: &str, process_name: Option<&str>) -> Result<Window> {
    let title_needle = title_contains.to_lowercase();
    let proc_needle = process_name.map(str::to_lowercase);
    let all = Window::all()?;

    let mut candidates = Vec::new();
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
        candidates.push(w);
    }

    match candidates.len() {
        0 => Err(Error::WindowNotFound(title_contains.into())),
        1 => Ok(candidates.into_iter().next().expect("len == 1")),
        _ => {
            let mut listed: Vec<String> = candidates
                .iter()
                .map(|w| {
                    format!(
                        "'{}' [{}]",
                        w.title().unwrap_or_default(),
                        w.app_name().unwrap_or_default()
                    )
                })
                .collect();
            listed.sort();
            Err(Error::MultipleWindowsMatched { candidates: listed })
        }
    }
}

/// Must run BEFORE `WindowCapture` is constructed: WGC ties its frame
/// pool to the window's size at session start, and resizing afterwards
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

    let lparam = LPARAM(&mut state as *mut _ as isize);
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
