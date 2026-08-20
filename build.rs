//! Linker setup for the shipped executable.
//!
//! Two things, only on MSVC: the exe requests the administrator token and
//! declares itself per-monitor-v2 DPI aware. Windows decides both before any of
//! this crate's code runs, so both belong in the manifest and not in code.
//!
//! **Elevation is for the actuator, not the capture backend** — the Npcap tap
//! was measured working end to end unelevated. UIPI is the reason: Windows
//! silently refuses `SendInput` aimed at a window of higher integrity than the
//! sender (see `actuator::win::probe_window_reachable`), and Epic Seven inherits
//! **high** integrity from `STOVE.exe`, which always elevates — so at `asInvoker`
//! every click was refused and a player cannot fix it from the game's side.

/// Pins this process's DPI awareness, merged into the embedded manifest by
/// `/MANIFESTINPUT` below. `actuator::win::dpi` refuses to click unless the
/// process is per-monitor aware, and we cannot set that ourselves: `winit` calls
/// the same setter from `EventLoop::new` before the first acquire, and Windows
/// lets only the *first* setter win. The loader applies a manifest before any
/// code runs, so it outranks both.
///
/// `dpiAwareness` (2016) alone, no `dpiAware` (2005): the newer one wins on
/// 1607+ and is ignored below, which leaves nothing undeclared because
/// `windows-sys` statically imports three DPI entry points that appeared in 1607
/// and a missing export is a loader failure.
///
/// The value is a *list* because `permonitorv2` is only recognized from 1703,
/// and an unrecognized item falls back to "dpi unaware, and you cannot change it
/// programmatically" — on 1607 that would turn this fix into the halt it
/// prevents. Windows takes the leftmost item it recognizes, and v2 and v1 both
/// report the `DPI_AWARENESS_PER_MONITOR_AWARE` that `awareness_verdict` accepts.
///
/// A `__COMPAT_LAYER` shim applies *over* the manifest — measured,
/// `__COMPAT_LAYER=DPIUNAWARE` lands at `unaware` regardless — which is why
/// `dpi.rs` still checks rather than trusting this.
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

    // Dropping `/MANIFEST:EMBED` does not ship unmanifested — `/MANIFEST` is on
    // by default — it produces an external `.exe.manifest` sidecar honoured
    // during dev runs from `target\` that vanishes when the lone exe ships, so
    // dev and shipped builds would differ in exactly this property. The
    // `/MANIFESTINPUT` below is the floor: `link.exe` rejects it without
    // `/MANIFEST:EMBED` (`LNK1220`, measured), so deleting this line fails the
    // build instead of quietly producing a sidecar.
    //
    // `uiAccess='false'` is the default, spelled out because it is the *other*
    // answer to UIPI: `uiAccess='true'` needs an Authenticode-signed image under
    // a secure location such as `Program Files`, and neither holds for an exe a
    // player downloads.
    println!("cargo::rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo::rustc-link-arg-bins=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'"
    );

    // `/MANIFESTUAC` says only what its name says, so the DPI declaration
    // arrives as a manifest *input* the linker merges with the UAC fragment —
    // verified on the produced exe with `mt.exe -inputresource:…;#1`.
    //
    // Into `OUT_DIR`, never the source tree, and skipped when the bytes already
    // match so a no-op build touches no mtime: declaring `rerun-if-changed` on a
    // file this script writes is how a build script rebuilds forever.
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
