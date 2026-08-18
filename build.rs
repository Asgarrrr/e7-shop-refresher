//! Linker setup for the native capture backend.
//!
//! Two things are arranged here, and only on MSVC (both are MSVC linker
//! features):
//!
//! 1. **Non-elevation.** The exe used to carry a `requireAdministrator`
//!    manifest, which put the TOML parser, egui, the TLS uplink and the TCP
//!    reassembly of hostile network bytes in an administrator process for the
//!    whole session. Only opening the WinDivert handle ever needed the
//!    privilege, and that step now lives in a separate broker process the app
//!    launches with `runas` (`src/capture/elevate.rs`). So the image declares
//!    `asInvoker`: the window opens with no prompt, and the consent prompt
//!    appears when capture starts, for the one process that needs it.
//!
//! 2. **Delay-loading `WinDivert.dll`**, so the exe
//!    can ship as a single file. `WinDivert.dll` and `WinDivert64.sys` are
//!    both embedded in the exe (`include_bytes!` in `src/capture/windivert.rs`)
//!    and self-extracted on first run into `%LOCALAPPDATA%\arkyve-refresh-shop\`.
//!    That only works if the DLL is NOT resolved at process load (the loader
//!    runs before any of our code, so the file isn't on disk yet): delay-loaded,
//!    the import binds on the first WinDivert call — after the extraction. The
//!    extraction dir is not on the DLL search path, so `ensure_runtime_present`
//!    also `LoadLibrary`s the DLL by full path before that first call; the
//!    delay-load thunk then binds to the already mapped module.
//!
//! WinDivert.dll is LGPL. Embedding it keeps the shipped artifact a single exe;
//! the exe writes the DLL back out on first run (alongside its license), so a
//! user can replace it with their own build — the relink freedom the LGPL
//! requires.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if !msvc {
        return;
    }

    // Declare `asInvoker` explicitly, and keep the manifest embedded. Deleting
    // these two lines is NOT the same thing: `/MANIFEST` is on by default in
    // `link.exe`, so dropping `/MANIFEST:EMBED` does not produce "no manifest",
    // it produces an external `arkyve-refresh-shop.exe.manifest` sidecar that
    // Windows honours during dev runs from `target\` and that silently vanishes
    // when the lone exe ships. Dev and shipped builds would then differ in
    // exactly the property this whole design turns on, for no reason anyone
    // could find later. Stating the level is also documentation: a reader of
    // this file learns that running unelevated is a decision, not an omission.
    //
    // `uiAccess='false'` is the default, spelled out because the alternative
    // (a signed, `Program Files`-resident image driving higher-integrity UI) is
    // the one thing someone might reach for when the actuator hits UIPI, and it
    // is not what we do.
    //
    // Outside the backend guard on purpose: a `--no-default-features --features
    // gui,actuator` build used to get no manifest at all, so the two builds made
    // different declarations about the same exe.
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='asInvoker' uiAccess='false'");

    if std::env::var_os("CARGO_FEATURE_WINDIVERT_BACKEND").is_none() {
        return;
    }

    // Resolve WinDivert.dll on first use, not at load, so the embedded copy
    // can be extracted and preloaded first (see `ensure_runtime_present`).
    // `-bins` scopes this to the executable (tests and examples don't link
    // WinDivert).
    println!("cargo:rustc-link-arg-bins=/DELAYLOAD:WinDivert.dll");
    // The delay-load stubs call `__delayLoadHelper2`, which lives here.
    println!("cargo:rustc-link-arg-bins=delayimp.lib");
}
