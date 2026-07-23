//! Delay-loads WinDivert.dll so the exe can ship as a single file.
//!
//! `WinDivert.dll` and `WinDivert64.sys` are both embedded in the exe
//! (`include_bytes!` in `src/capture/windivert.rs`) and self-extracted on first
//! run into `%LOCALAPPDATA%\arkyve-refresh-shop\`. That only works if the DLL is
//! NOT resolved at process load (the loader runs before any of our code, so the
//! file isn't on disk yet): we delay-load it, so the import binds on the first
//! WinDivert call — after the extraction. The extraction dir is not on the DLL
//! search path, so `ensure_runtime_present` also `LoadLibrary`s the DLL by full
//! path before that first call; the delay-load thunk then binds to the already
//! mapped module. This script emits the linker flags that arrange the delay.
//!
//! WinDivert.dll is LGPL. Embedding it keeps the shipped artifact a single exe;
//! the exe writes the DLL back out on first run (alongside its license), so a
//! user can replace it with their own build — the relink freedom the LGPL
//! requires.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Delay-loading is an MSVC linker feature (`/DELAYLOAD` + `delayimp.lib`),
    // and WinDivert is only linked when its backend feature is on.
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let windivert = std::env::var_os("CARGO_FEATURE_WINDIVERT_BACKEND").is_some();
    if msvc && windivert {
        // Resolve WinDivert.dll on first use, not at load, so the embedded copy
        // can be extracted and preloaded first (see `ensure_runtime_present`).
        // `-bins` scopes this to the executable (tests and examples don't link
        // WinDivert).
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:WinDivert.dll");
        // The delay-load stubs call `__delayLoadHelper2`, which lives here.
        println!("cargo:rustc-link-arg-bins=delayimp.lib");

        // Require elevation at launch. WinDivert installs and loads a kernel
        // driver, which needs administrator rights; without this the player
        // must right-click > "Run as administrator" or the first capture fails.
        // The linker bakes a UAC manifest into the exe so Windows shows the
        // consent prompt automatically on every launch. `/MANIFEST:EMBED` puts
        // it inside the exe (the default only emits an external `.manifest`).
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='requireAdministrator'");
    }
}
