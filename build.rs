//! Linker setup for the shipped executable.
//!
//! One thing is arranged here, and only on MSVC (it is an MSVC linker
//! feature): **the exe asks for the administrator token — for the actuator,
//! not for the capture backend.** That reads backwards, so it is written down
//! in full.
//!
//! The capture backend needs no privilege at all. It is Npcap
//! (`src/capture/pcap.rs`): it taps every adapter through `wpcap.dll` from an
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
//! WinDivert broker so that only the driver handle ran with the token. It was
//! abandoned on the measurement above, not on taste: it solved a problem the
//! actuator has anyway, and the Npcap tap removed the only reason the split
//! existed.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

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
    // for no reason anyone could find later.
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
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'"
    );
}
