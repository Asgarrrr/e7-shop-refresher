//! Stages the vendored WinDivert runtime files next to the built executable.
//!
//! WinDivert is linked dynamically (see `.cargo/config.toml`): the exe loads
//! `WinDivert.dll` at runtime, which in turn loads `WinDivert64.sys`, and both
//! are found beside the executable. `windivert-sys`'s own build script only
//! stages them into its `OUT_DIR`, which is not on the exe's DLL search path —
//! so `cargo run` and a copied-out release build would fail to load the driver.
//! This copies them into `target/<profile>/` so both just work, and the profile
//! dir is ready to ship as-is (exe + WinDivert.dll + WinDivert64.sys).

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

    for name in ["WinDivert.dll", "WinDivert64.sys"] {
        let src = vendor.join(name);
        if src.exists() {
            let _ = fs::copy(&src, profile_dir.join(name));
        }
    }
}
