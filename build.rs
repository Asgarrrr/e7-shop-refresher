//! Linker setup for the shipped executable.
//!
//! Two things are arranged here, only on MSVC: the exe requests the
//! administrator token, and it declares itself per-monitor-v2 DPI aware.
//! Both are properties of the *process* that Windows decides before any of
//! this crate's code runs, so both belong in the manifest, not in code.
//!
//! **Elevation is for the actuator, not the capture backend.** Capture is
//! Npcap (`src/capture/pcap/`): it taps every adapter through `wpcap.dll`
//! from an ordinary process, and the whole pipeline — capture, reassembly,
//! uplink, decoded snapshot, refresh job — was measured working end to end
//! unelevated. We ship no driver; the player installs Npcap themselves.
//!
//! The actuator needs the token because of UIPI: Windows silently refuses
//! `SendInput` aimed at a window whose integrity level is higher than the
//! sender's (see `actuator::win::probe_window_reachable`). Epic Seven is
//! launched through `C:\ProgramData\Smilegate\STOVE\STOVE.exe`, whose
//! manifest declares `requestedExecutionLevel
//! level='requireAdministrator'`, so the game inherits **high** integrity
//! from its launcher. Measured live: `EpicSeven.exe` high, this app (then
//! `asInvoker`) medium — every click was refused, and the journal said so
//! ("the game window runs at a higher integrity level than this app …").
//! STOVE always elevates, so a player cannot fix this from the game's side.
//!
//! So the exe declares `requireAdministrator`. One side effect of the
//! medium-integrity run: `install_logging` could not open its log file
//! under `%LOCALAPPDATA%`, and fell back to an inert stdout.
//!
//! An earlier design split the app into an unelevated UI plus an elevated
//! `WinDivert` broker so only the driver handle held the token; abandoned
//! once the Npcap tap removed the reason it existed.
//!
//! The DPI declaration exists because
//! `actuator::win::dpi::ensure_dpi_awareness` refuses to click otherwise,
//! and the manifest is the only place that declaration can be made early
//! enough to be ours — see `DPI_MANIFEST` below.

/// The manifest fragment that pins this process's DPI awareness, merged into
/// the embedded manifest by `/MANIFESTINPUT` below.
///
/// # Why the manifest and not a `Set…` call
///
/// The actuator's coordinates are physical-pixel arithmetic, and only a
/// per-monitor-aware process is told the truth about a window's rect —
/// `actuator::win::dpi` reads the awareness back and refuses the acquire
/// with a `Fatal` for anything else. Calling `SetProcessDpiAwarenessContext`
/// ourselves cannot establish that value: `winit` calls the same setter
/// from `EventLoop::new`, before the first `acquire`, and Windows lets only
/// the *first* setter win. Today that means per-monitor-v2 (`winit-0.30.13`
/// `platform_impl/windows/dpi.rs::become_dpi_aware`), but it is an
/// undocumented internal detail one dependency bump from changing.
///
/// A manifest declaration is applied by the loader before any code runs, so
/// it wins over winit and our own setter. Both then fail harmlessly: winit
/// ignores its return, and `dpi.rs` treats a refusal as "someone else got
/// there first" and reads the value back instead.
///
/// # Why `dpiAwareness` alone, with a fallback item, and no `dpiAware`
///
/// `dpiAware` (2005 `windowsSettings` namespace) and `dpiAwareness` (2016)
/// are different declarations: on Windows 10 1607+ `dpiAwareness` overrides
/// `dpiAware`; below 1607 it is ignored. That looks like it leaves older
/// Windows undeclared, but this exe cannot run there: `windows-sys` imports
/// `SetProcessDpiAwarenessContext`, `GetThreadDpiAwarenessContext` and
/// `GetAwarenessFromDpiAwarenessContext` statically from `user32.dll`
/// (verified with `dumpbin /imports:USER32.dll`), all three appeared in
/// 1607, and a missing export is a loader failure, not a runtime fallback.
///
/// The value is a *list* because `permonitorv2` is only recognized from
/// Windows 10 1703; an unrecognized item falls back to "dpi unaware, and you
/// cannot change it programmatically" — which on 1607 would turn this fix
/// into the halt it prevents. Windows takes the leftmost item it
/// recognizes, so 1703+ gets per-monitor-v2 and 1607 gets per-monitor v1.
/// Both report `DPI_AWARENESS_PER_MONITOR_AWARE`, which is what
/// `awareness_verdict` accepts: the `DPI_AWARENESS` enum has no separate v2
/// value, and `GetAwarenessFromDpiAwarenessContext` maps the v2 *context*
/// onto it — measured on a manifested build, at process entry, before
/// `main`.
///
/// One thing the manifest cannot outrank: a `__COMPAT_LAYER` shim (the
/// "Override high DPI scaling behavior" checkbox, or its environment
/// variable) applies *over* the manifest. Measured:
/// `__COMPAT_LAYER=DPIUNAWARE` lands at `unaware` regardless — exactly the
/// case `ensure_dpi_awareness`'s refusal exists for, and why `dpi.rs` still
/// checks rather than trusting this.
const DPI_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <asmv3:application xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
    <asmv3:windowsSettings>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">permonitorv2, permonitor</dpiAwareness>
    </asmv3:windowsSettings>
  </asmv3:application>
</assembly>
"#;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");

    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if !msvc {
        return;
    }

    // Ask for the administrator token, and keep the manifest embedded.
    // Deleting these two lines is not the same as shipping unmanifested:
    // `/MANIFEST` is on by default in `link.exe`, so dropping
    // `/MANIFEST:EMBED` produces an external
    // `arkyve-refresh-shop.exe.manifest` sidecar that Windows honours during
    // dev runs from `target\` but that vanishes when the lone exe ships —
    // dev and shipped builds would then differ in exactly the property this
    // design turns on. The `/MANIFESTINPUT` below gives this a floor:
    // `link.exe` rejects `/MANIFESTINPUT` without `/MANIFEST:EMBED` outright
    // (`LNK1220`, measured), so deleting this line fails the build instead
    // of quietly producing a sidecar.
    //
    // `uiAccess='false'` is the default, spelled out because it's the
    // *other* answer to UIPI: `uiAccess='true'` would let a medium-integrity
    // process drive higher-integrity UI, but only for an Authenticode-signed
    // image installed under a secure location such as `Program Files`.
    // Neither holds for a single exe a player downloads.
    println!("cargo::rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo::rustc-link-arg-bins=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'"
    );

    // `/MANIFESTUAC` can only say the one thing its name says, so the DPI
    // declaration arrives as a manifest *input* that the linker merges with
    // the UAC fragment it generates — verified on the produced exe:
    // `mt.exe -inputresource:…;#1` shows one manifest holding both
    // `trustInfo` and `dpiAwareness`.
    //
    // Written into `OUT_DIR`, never the source tree, since this is
    // generated. The write is skipped when the bytes already match so a
    // no-op build touches no mtime — `rerun-if-changed=build.rs` above
    // already covers the only input; declaring `rerun-if-changed` on a file
    // this script writes is how a build script rebuilds forever.
    let out_dir = std::env::var("OUT_DIR").expect("cargo always sets OUT_DIR for a build script");
    let manifest = std::path::Path::new(&out_dir).join("dpi-awareness.manifest");
    let unchanged =
        std::fs::read(&manifest).is_ok_and(|current| current == DPI_MANIFEST.as_bytes());
    if !unchanged {
        std::fs::write(&manifest, DPI_MANIFEST)
            .unwrap_or_else(|error| panic!("writing {}: {error}", manifest.display()));
    }
    // Unquoted on purpose: cargo and rustc each hand this to the next tool
    // as one argument, so a path containing a space survives — quoting here
    // would make the quotes part of the file name.
    println!(
        "cargo::rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
}
