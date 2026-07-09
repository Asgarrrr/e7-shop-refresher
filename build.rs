//! Copie le runtime WinDivert (DLL + driver signé) à côté de l'exécutable.
//!
//! `WinDivert.dll` charge le driver `WinDivert64.sys` depuis son propre dossier
//! au premier `WinDivertOpen`. Les deux fichiers doivent donc voisiner l'exe.

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=vendor/windivert");

    // Seul le backend WinDivert a besoin du runtime.
    if env::var_os("CARGO_FEATURE_WINDIVERT_BACKEND").is_none() {
        return;
    }

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest.join("vendor").join("windivert");

    // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out → remonter au dossier de profil.
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let Some(profile_dir) = out_dir.ancestors().nth(3) else {
        return;
    };

    for file in ["WinDivert.dll", "WinDivert64.sys"] {
        let src = vendor.join(file);
        if src.exists() {
            let _ = fs::copy(&src, profile_dir.join(file));
        }
    }
}
