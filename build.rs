//! Linker setup for the shipped executable.
//!
//! Two things are arranged here, and only on MSVC (both are MSVC linker
//! features), and both are properties of the *process* that Windows decides
//! before any of this crate's code runs:
//!
//! 1. **The exe asks for the administrator token — for the actuator, not for
//!    the capture backend.** That reads backwards, so it is written down in
//!    full below.
//! 2. **The exe declares itself per-monitor-v2 DPI aware**, because
//!    `actuator::win::dpi::ensure_dpi_awareness` refuses to click otherwise and
//!    the manifest is the only place that declaration can be made early enough
//!    to be *ours*. See `DPI_MANIFEST` below.
//!
//! The capture backend needs no privilege at all. It is Npcap
//! (`src/capture/pcap/`): it taps every adapter through `wpcap.dll` from an
//! ordinary process, and the whole pipeline — capture, reassembly, uplink,
//! decoded snapshot, refresh job — was measured working end to end from an
//! unelevated run. Nothing below is for it. We ship no driver and embed
//! nothing; the player installs Npcap once, themselves.
//!
//! The *actuator* is what needs the token, because of UIPI: Windows refuses
//! input aimed at a window whose integrity level is higher than the sending
//! process's, and it refuses it silently for `SendInput` (see
//! `actuator::win::probe_window_reachable`). Epic Seven is launched through
//! `C:\ProgramData\Smilegate\STOVE\STOVE.exe`, whose manifest declares
//! `requestedExecutionLevel level='requireAdministrator'`, so the game
//! inherits **high** integrity from its launcher. Measured live on a real
//! install: `EpicSeven.exe` high, this app (then `asInvoker`) medium — every
//! click was refused, and the journal said so ("the game window runs at a
//! higher integrity level than this app … — stopping the loop"). A player
//! cannot fix that from the game's side: STOVE is the launcher Epic Seven
//! ships with, and it always elevates.
//!
//! So the exe declares `requireAdministrator`. That is the *product* talking:
//! driving another window's input is the feature, and the feature requires the
//! privilege. A second, smaller consequence of the medium-integrity run:
//! `install_logging` could not even open its log file under `%LOCALAPPDATA%`,
//! and fell back to an inert stdout.
//!
//! An earlier design split the app into an unelevated UI plus an elevated
//! `WinDivert` broker so that only the driver handle ran with the token. It was
//! abandoned on the measurement above, not on taste: it solved a problem the
//! actuator has anyway, and the Npcap tap removed the only reason the split
//! existed.

/// The manifest fragment that pins this process's DPI awareness, merged into the
/// embedded manifest by `/MANIFESTINPUT` below.
///
/// # Why the manifest and not a `Set…` call
///
/// Every coordinate the actuator computes is physical-pixel arithmetic, and only
/// a per-monitor-aware process is told the truth about a window's rect — see
/// `actuator::win::dpi`, which reads the awareness back and refuses the acquire
/// with a `Fatal` (halting the watch) for anything else. Its
/// `SetProcessDpiAwarenessContext` call cannot be what establishes the value in
/// the shipped GUI build: `winit` calls the same setter from
/// `EventLoop::new`, long before the first `acquire`, and Windows lets only the
/// *first* setter win. So the awareness would be whatever winit chose —
/// per-monitor-v2 today (`winit-0.30.13`
/// `platform_impl/windows/dpi.rs::become_dpi_aware`), but that is an
/// undocumented internal detail one dependency bump away from changing, and
/// nothing in this crate would notice except a player whose watch halts.
///
/// A manifest declaration is applied by the loader *before any code runs*, so it
/// wins over winit, over our own setter, and over the ordering question
/// entirely. Both then fail harmlessly — winit ignores its return, and `dpi.rs`
/// already treats a refusal as "someone else got there first" and reads the
/// value back rather than inferring it.
///
/// # Why `dpiAwareness` alone, with a fallback item, and no `dpiAware`
///
/// `dpiAware` (the 2005 `windowsSettings` namespace) and `dpiAwareness` (the
/// 2016 one) are different declarations: on Windows 10 1607 and later
/// `dpiAwareness` overrides `dpiAware`, and below 1607 `dpiAwareness` is the one
/// that is ignored. Shipping only the 2016 element therefore *looks* like it
/// leaves older Windows undeclared — but this exe cannot run there at all:
/// `windows-sys` imports `SetProcessDpiAwarenessContext`,
/// `GetThreadDpiAwarenessContext` and `GetAwarenessFromDpiAwarenessContext`
/// statically from `user32.dll` (verified with `dumpbin /imports:USER32.dll` on
/// the built exe), all three appeared in 1607, and a missing export is a loader
/// failure, not a runtime fallback. A `dpiAware` line could only take effect on
/// an OS where this process never reaches `main`, so it is left out rather than
/// shipped as decoration.
///
/// The value is a *list* because `permonitorv2` is only recognized from Windows
/// 10 1703, and the documented behaviour for an unrecognized item is "dpi
/// unaware, and you cannot change it programmatically" — which on 1607 would
/// have turned this fix into the very halt it prevents. Windows takes the
/// leftmost item it recognizes, so 1703+ gets per-monitor-v2 and 1607 gets
/// per-monitor v1. Both report `DPI_AWARENESS_PER_MONITOR_AWARE`, which is what
/// `awareness_verdict` accepts: the `DPI_AWARENESS` enum has no separate v2
/// value, and `GetAwarenessFromDpiAwarenessContext` maps the v2 *context* onto
/// the per-monitor awareness — measured on a manifested build, not assumed: the
/// context reads back as the v2 handle and the awareness as
/// `DPI_AWARENESS_PER_MONITOR_AWARE`, at process entry, before `main`.
///
/// One thing the manifest still cannot outrank, and the reason `dpi.rs` keeps
/// checking rather than trusting this: a `__COMPAT_LAYER` shim — the
/// "Override high DPI scaling behavior" checkbox, or the environment variable
/// behind it — is applied *over* the manifest. Measured on a manifested build:
/// `__COMPAT_LAYER=DPIUNAWARE` lands at `unaware` regardless of everything above.
/// That is exactly the case `ensure_dpi_awareness`'s refusal names, and it stays
/// a refusal.
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

    // Ask for the administrator token, and keep the manifest embedded. Deleting
    // these two lines is NOT the same thing: `/MANIFEST` is on by default in
    // `link.exe`, so dropping `/MANIFEST:EMBED` does not produce "no manifest",
    // it produces an external `arkyve-refresh-shop.exe.manifest` sidecar that
    // Windows honours during dev runs from `target\` and that silently vanishes
    // when the lone exe ships. Dev and shipped builds would then differ in
    // exactly the property this whole design turns on — the run from `target\`
    // would drive the game and the shipped exe would be refused every click —
    // for no reason anyone could find later. Since the `/MANIFESTINPUT` below
    // joined it that trap has a floor: `link.exe` rejects `/MANIFESTINPUT`
    // without `/MANIFEST:EMBED` outright (`LNK1220`, measured), so deleting this
    // line now fails the build instead of quietly producing a sidecar.
    //
    // `uiAccess='false'` is the default, spelled out because it is the *other*
    // answer to UIPI and we deliberately do not take it: `uiAccess='true'` would
    // let a medium-integrity process drive higher-integrity UI, but only for an
    // Authenticode-signed image installed under a secure location such as
    // `Program Files`. Neither holds for a single exe a player downloads, so
    // elevation is the only door left.
    //
    // Unconditional on purpose — no feature gate. Every build of this exe is
    // the same exe as far as Windows is concerned, and a lane that produced an
    // unmanifested binary would be making a different declaration about it.
    println!("cargo::rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo::rustc-link-arg-bins=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'"
    );

    // `/MANIFESTUAC` can only say the one thing its name says, so the DPI
    // declaration arrives as a manifest *input* that the linker merges with the
    // UAC fragment it generates — verified on the produced exe, not inferred
    // from this line: `mt.exe -inputresource:…;#1` shows one manifest holding
    // both `trustInfo` and the `dpiAwareness` element.
    //
    // Into `OUT_DIR`, never the source tree: this is generated, and a tracked
    // copy would be one more file that has to be kept in step with the constant
    // above. The write is skipped when the bytes already match so that a no-op
    // build does not touch a single mtime — `DPI_MANIFEST` is a literal, so
    // `rerun-if-changed=build.rs` above already covers the only input, and
    // declaring `rerun-if-changed` on a file this script *writes* is how a build
    // script rebuilds forever.
    let out_dir = std::env::var("OUT_DIR").expect("cargo always sets OUT_DIR for a build script");
    let manifest = std::path::Path::new(&out_dir).join("dpi-awareness.manifest");
    let unchanged =
        std::fs::read(&manifest).is_ok_and(|current| current == DPI_MANIFEST.as_bytes());
    if !unchanged {
        std::fs::write(&manifest, DPI_MANIFEST)
            .unwrap_or_else(|error| panic!("writing {}: {error}", manifest.display()));
    }
    // Unquoted on purpose: cargo hands this to rustc as one argument and rustc
    // hands it to `link.exe` as one argument, so a path containing a space
    // survives — adding quotes here would make them part of the file name.
    println!(
        "cargo::rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
}
