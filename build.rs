//! Stages the vendored WinDivert runtime files next to the built executable.
//!
//! WinDivert is linked dynamically (see `.cargo/config.toml`): the exe imports
//! `WinDivert.dll` at load time, so the DLL must sit beside the executable —
//! `windivert-sys`'s own build script only stages it into its `OUT_DIR`, which
//! is not on the exe's DLL search path, so `cargo run` and a copied-out release
//! build would fail to load it. This copies `WinDivert.dll` into
//! `target/<profile>/` so both just work.
//!
//! The `WinDivert64.sys` driver is NOT copied here: it is embedded in the exe
//! (`include_bytes!`) and self-extracted next to the exe on first run (see
//! `src/capture/windivert.rs`). WinDivert.dll is LGPL, so its license is staged
//! alongside to keep the profile dir ready to ship (exe + WinDivert.dll +
//! WinDivert-LICENSE.txt).

use std::path::PathBuf;
use std::{env, fs};

fn main() {
    println!("cargo:rerun-if-changed=vendor/windivert");

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest.join("vendor").join("windivert");

    // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out; three levels up is the
    // profile dir (target/<profile>), where the executable lands.
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let Some(profile_dir) = out_dir.ancestors().nth(3) else {
        return;
    };

    for (src_name, dst_name) in [
        ("WinDivert.dll", "WinDivert.dll"),
        ("LICENSE", "WinDivert-LICENSE.txt"),
    ] {
        let src = vendor.join(src_name);
        if src.exists() {
            let _ = fs::copy(&src, profile_dir.join(dst_name));
        }
    }
}
