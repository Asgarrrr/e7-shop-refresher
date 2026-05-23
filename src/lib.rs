#[cfg(not(windows))]
compile_error!(
    "e7-shop-refresher is Windows-only: xcap WGC + Win32 SetForegroundWindow \
     have no portable equivalent. See README."
);

pub mod capture;
pub mod config;
pub mod detector;
pub mod error;
pub mod gui;
pub mod input;
pub mod power;
pub mod shop;

/// Per-monitor DPI awareness + capped rayon pool. Called by every entry
/// point (CLI, GUI).
pub fn init() {
    enable_per_monitor_dpi();
    init_thread_pool();
}

/// Caps NCC parallelism so the bot doesn't pin every core while the user
/// is also playing the game. `build_global` only succeeds once; later
/// calls are silently ignored.
fn init_thread_pool() {
    let n = std::thread::available_parallelism().map_or(2, |p| (p.get() / 2).clamp(2, 4));
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(n)
        .thread_name(|i| format!("e7-rayon-{i}"))
        .build_global();
}

fn enable_per_monitor_dpi() {
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}
