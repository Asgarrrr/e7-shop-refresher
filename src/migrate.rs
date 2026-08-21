//! One-time cleanup of state older versions left on the player's machine.
//!
//! A `WinDivert` build extracted a driver into `%LOCALAPPDATA%\<app>\` and
//! locked that directory to Administrators with a *protected* DACL. The files
//! are litter; the DACL is not — `logs\` and `crash.log` inherit it, so a
//! machine that ever ran such a build silently loses its log file on every
//! unelevated run. Best-effort end to end: the app is elevated (for the
//! actuator, see `build.rs`), which is what lets it undo that DACL at all, but a
//! failure here must never stop the relay.
//!
//! # Why this module opens a handle before it does anything
//!
//! Four facts compose into an elevation-of-privilege primitive: an elevated
//! process inherits `LOCALAPPDATA` from whoever asked for the elevation, so the
//! launcher chooses it; every path-based Win32 or `std` API follows reparse
//! points; a directory junction needs no privilege to create, unlike a symlink;
//! and [`clean_windivert_leftovers`] runs at *every* launch, before anything
//! else in `main`. Point `LOCALAPPDATA` at an attacker-owned directory holding
//! an `arkyve-refresh-shop` junction to `C:\Windows` and this module strips that
//! tree's DACL and recursively deletes inside it.
//!
//! So: resolve to a *handle* once with `FILE_FLAG_OPEN_REPARSE_POINT`, refuse
//! anything that is not a plain directory, and address the DACL through that
//! handle (`GetSecurityInfo`, not `GetNamedSecurityInfoW`) — which is what
//! removes the check-then-act window. That is `open_directory_itself`; no
//! intra-doc link, because it is `#[cfg(windows)]` and this header is not, so a
//! link would break rustdoc on the dev machine.

use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::fs::File;

// The gate this module's elevated half goes through. Its own module because
// `install` needs the identical one and cannot reach into this file.
#[cfg(windows)]
use crate::dirhandle::open_directory_itself;

use tracing::{info, warn};

/// Files a `WinDivert` build self-extracted into the app-data root.
const EXTRACTED_FILES: [&str; 3] = ["WinDivert.dll", "WinDivert64.sys", "WinDivert-LICENSE.txt"];

/// Directory a late `WinDivert` build extracted into instead of the root, to keep
/// its admins-only DACL off `logs\`. Never shipped, but a developer machine can
/// have one, and it holds the same two binaries.
const EXTRACTED_SUBDIR: &str = "runtime";

/// What [`clean_windivert_leftovers`] did, so `main` can log it *after* the
/// subscriber exists.
///
/// A value rather than `warn!` calls in place, because of the ordering: the DACL
/// undone here is what stops `install_logging` from opening its file, so the
/// cleanup runs first — and anything logged directly would vanish into a process
/// with no subscriber.
#[derive(Default)]
pub struct Leftovers {
    reset_dacl: bool,
    removed: Vec<&'static str>,
    warnings: Vec<String>,
}

impl Leftovers {
    /// Emits what was found. Silent on machines that never ran a `WinDivert`
    /// build, which after the first cleaned launch is every machine.
    pub fn report(&self) {
        for warning in &self.warnings {
            warn!(target: "migrate", "{warning}");
        }
        if self.reset_dacl || !self.removed.is_empty() {
            info!(
                reset_dacl = self.reset_dacl,
                removed = %self.removed.join(", "),
                "cleaned up state left by a WinDivert build; if this run's log file is \
                 missing its first lines, that directory was admins-only until now"
            );
        }
    }
}

/// Deletes the extracted `WinDivert` runtime and puts the app-data directory back
/// on inherited permissions.
///
/// Steady state after the first run is one `CreateFileW`, one `GetSecurityInfo`
/// and four open-and-fail calls.
///
/// Call it before `main`'s logging setup (`src/main.rs`) and `report` the result
/// after — see [`Leftovers`]. No intra-doc link: `main` is in the binary target,
/// which the library's rustdoc cannot see.
#[must_use = "the findings are logged by `report` once the subscriber exists"]
pub fn clean_windivert_leftovers() -> Leftovers {
    match app_data_root() {
        Some(root) => clean_leftovers_in(&root),
        None => Leftovers::default(),
    }
}

/// The cleanup itself, against a directory named by the caller.
///
/// Split out because nothing in this crate mutates the environment (edition 2024
/// makes `set_var` `unsafe`: it races every concurrent `getenv` in the test
/// binary), so a test can reach the junction refusal only by handing over a path.
fn clean_leftovers_in(root: &Path) -> Leftovers {
    let mut found = Leftovers::default();

    #[cfg(windows)]
    {
        // The handle *is* the existence check. Held for the DACL work, dropped
        // before anything below deletes.
        let dir = match open_directory_itself(root) {
            Ok(dir) => dir,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return found,
            Err(err) => {
                found.warnings.push(format!(
                    "refusing to clean up {} ({err}) — nothing here is worth doing to a \
                     directory that is not the one it names",
                    root.display()
                ));
                return found;
            }
        };

        match dacl_is_protected(&dir) {
            // Nothing else ever protects this directory: a protected DACL here
            // is the extract-and-harden footprint.
            Ok(true) => match reset_dacl_to_inherited(&dir) {
                Ok(()) => found.reset_dacl = true,
                Err(err) => found.warnings.push(format!(
                    "could not restore inherited permissions on {} ({err}) — logs and crash.log \
                     may stay unwritable without administrator rights",
                    root.display()
                )),
            },
            Ok(false) => {}
            Err(err) => found.warnings.push(format!(
                "could not read the permissions on {} ({err}) — skipping the cleanup",
                root.display()
            )),
        }
    }

    // No handle validation on a dev machine (mac): no DACL to undo and no
    // elevation behind the deletes, so this is just "does it exist".
    #[cfg(not(windows))]
    if !root.is_dir() {
        return found;
    }

    for name in EXTRACTED_FILES {
        let stale = root.join(name);
        // `remove_file` deletes a symlink rather than its target, and the
        // directory these are looked up in has already been checked.
        match std::fs::remove_file(&stale) {
            Ok(()) => found.removed.push(name),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => found.warnings.push(format!(
                "could not delete the stale runtime file {} ({err})",
                stale.display()
            )),
        }
    }

    let subdir = root.join(EXTRACTED_SUBDIR);
    // The same gate as the root's: this is the elevated `remove_dir_all`, and
    // `runtime\` is a name anything with write access to the app-data directory
    // can claim, with a junction if it wants the delete to land elsewhere.
    #[cfg(windows)]
    let removable = match open_directory_itself(&subdir) {
        // Not bound: the handle has to be closed before `remove_dir_all` can
        // take the directory away.
        Ok(_) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            found.warnings.push(format!(
                "refusing to delete {} ({err}) — it is not the plain directory a WinDivert \
                 build would have left",
                subdir.display()
            ));
            false
        }
    };
    #[cfg(not(windows))]
    let removable = subdir.is_dir();

    if removable {
        match std::fs::remove_dir_all(&subdir) {
            Ok(()) => found.removed.push(EXTRACTED_SUBDIR),
            Err(err) => found.warnings.push(format!(
                "could not delete the extracted runtime directory {} ({err})",
                subdir.display()
            )),
        }
    }

    found
}

/// `%LOCALAPPDATA%\arkyve-refresh-shop`, and nothing else.
///
/// `None` when `LOCALAPPDATA` is unset. An exe-directory fallback is
/// deliberately *not* cleaned: rewriting a DACL in a folder the user chose (a
/// Desktop, a network share) is more surprising than leaving an unreachable
/// configuration alone.
///
/// `SHGetKnownFolderPath` was rejected: `FOLDERID_LocalAppData` resolves through
/// `HKCU\…\User Shell Folders`, which the same medium-integrity attacker can
/// write, so it closes the environment-block vector but not the one that
/// matters. Neither source is trustworthy enough to skip the check in
/// `open_directory_itself`, which is what carries the safety property for any
/// spelling of the path.
fn app_data_root() -> Option<PathBuf> {
    app_data_root_from(
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .as_deref(),
    )
}

/// Pure half of [`app_data_root`].
///
/// The absoluteness test is not decoration: a *relative* `LOCALAPPDATA` resolves
/// against the working directory the launching shortcut named, so refusing here
/// keeps a path whose meaning depends on where the app started from out of the
/// handle check entirely.
fn app_data_root_from(local_appdata: Option<&Path>) -> Option<PathBuf> {
    local_appdata
        .filter(|local| local.is_absolute())
        .map(|local| local.join(crate::APP_DIR))
}

/// True when the open directory's DACL carries `SE_DACL_PROTECTED` — inheritance
/// switched off, the `WinDivert` signature, and what keeps a non-elevated process
/// out of `logs\` and `crash.log`.
///
/// Takes the handle [`open_directory_itself`] validated rather than a path:
/// `GetSecurityInfo` and `GetNamedSecurityInfoW` differ in exactly that.
#[cfg(windows)]
fn dacl_is_protected(dir: &File) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, PSECURITY_DESCRIPTOR,
        SE_DACL_PROTECTED,
    };

    // One `LocalAlloc` block owning the ACL too, freed exactly once: the early
    // return below precedes the allocation, and the `LocalFree` further down is
    // the only one on any path past it.
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();

    // SAFETY: `dir` is an open directory handle carrying `READ_CONTROL`, the
    // right this information class needs, kept alive across the call by the
    // borrow. The four `null_mut()` out-parameters are documented as optional.
    // `descriptor` is a live stack slot, written only on `ERROR_SUCCESS`, which
    // is checked before any read; on failure nothing is allocated.
    let status = unsafe {
        GetSecurityInfo(
            dir.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(status as i32));
    }

    let mut control: u16 = 0;
    let mut revision: u32 = 0;
    // SAFETY: `descriptor` is the block the call above allocated and reported
    // success for, not yet freed. `control`/`revision` are stack slots that
    // outlive the call.
    let ok = unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) };
    // `GetLastError` is per-thread and the next Win32 call clobbers it: read it
    // before `LocalFree`, not after.
    let err = std::io::Error::last_os_error();
    // SAFETY: that same `LocalAlloc` block, freed exactly once here — the only
    // `LocalFree` in the function — and never touched after (`control` is a copy).
    unsafe { LocalFree(descriptor.cast()) };
    if ok == 0 {
        return Err(err);
    }
    Ok(control & SE_DACL_PROTECTED != 0)
}

/// Puts the open directory back on inherited permissions; auto-inherited
/// children (`logs\`, `crash.log`) are recomputed by the same call. Handle-based
/// for the reason on [`open_directory_itself`] — this is the subtree-wide
/// rewrite that must not be re-pointed by a name.
///
/// The ACL is deliberately **empty but not null**: a `NULL` `pDacl` installs a
/// *null DACL*, granting full access to everyone, so do not "simplify" the
/// zero-ACE ACL away.
#[cfg(windows)]
fn reset_dacl_to_inherited(dir: &File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use std::ptr;

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo};
    use windows_sys::Win32::Security::{
        ACL, ACL_REVISION, DACL_SECURITY_INFORMATION, InitializeAcl,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    };

    // An `ACL` header is 8 bytes and needs 4-byte alignment; a `u32` array gives
    // that alignment for free, and the extra room costs nothing on the stack.
    let mut acl_buf = [0u32; 16];

    // SAFETY: `acl_buf` is a `u32`-aligned stack buffer larger than the `ACL`
    // header, and its length is passed as the exact byte size of that same
    // buffer, so `InitializeAcl` cannot write out of bounds.
    let initialized = unsafe {
        InitializeAcl(
            acl_buf.as_mut_ptr().cast::<ACL>(),
            size_of_val(&acl_buf) as u32,
            ACL_REVISION,
        )
    };
    if initialized == 0 {
        return Err(std::io::Error::last_os_error());
    }

    // SAFETY: `acl_buf` holds an initialized zero-ACE `ACL` (the call above
    // reported success, the only way to reach here) and outlives
    // `SetSecurityInfo`, which does not retain the pointer. `dir` is an open
    // directory handle carrying `WRITE_DAC`, kept alive by the borrow. The null
    // owner/group/SACL pointers mean "do not change". Failure is a `WIN32_ERROR`
    // return, typically `ERROR_ACCESS_DENIED`, which the caller treats as
    // non-fatal.
    let result = unsafe {
        SetSecurityInfo(
            dir.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            acl_buf.as_ptr().cast::<ACL>(),
            ptr::null(),
        )
    };
    if result != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(result as i32));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cleanup_targets_the_app_data_root_and_never_its_parent() {
        // The reset propagates *downward*, so one level too high rewrites
        // permissions across the whole of `%LOCALAPPDATA%`.
        let Some(root) = app_data_root() else {
            return;
        };
        assert_eq!(
            root.file_name().and_then(|name| name.to_str()),
            Some(crate::APP_DIR)
        );
    }

    #[test]
    fn a_relative_local_appdata_is_not_a_location_this_cleanup_will_accept() {
        // Otherwise: an elevated delete and an elevated DACL rewrite aimed at
        // whatever directory the shortcut happened to start the app in.
        assert_eq!(app_data_root_from(None), None);
        assert_eq!(app_data_root_from(Some(Path::new("AppData/Local"))), None);
        assert_eq!(app_data_root_from(Some(Path::new(""))), None);

        let absolute = if cfg!(windows) {
            Path::new(r"C:\Users\x\AppData\Local")
        } else {
            Path::new("/Users/x/Library")
        };
        assert_eq!(
            app_data_root_from(Some(absolute)),
            Some(absolute.join(crate::APP_DIR))
        );
    }

    #[test]
    fn a_directory_with_nothing_left_in_it_reports_nothing() {
        let found = Leftovers::default();
        assert!(!found.reset_dacl);
        assert!(found.removed.is_empty());
        assert!(found.warnings.is_empty());
    }

    #[test]
    fn the_stale_files_are_the_three_a_windivert_build_extracted() {
        // The module that wrote them is gone and nothing else in the tree names
        // them, so a typo would silently clean nothing at all.
        assert_eq!(
            EXTRACTED_FILES,
            ["WinDivert.dll", "WinDivert64.sys", "WinDivert-LICENSE.txt"]
        );
    }

    /// RAII scratch directory: removed on drop including on an assertion panic,
    /// and named by test and pid so parallel tests cannot collide.
    #[cfg(windows)]
    struct TempDir(PathBuf);

    #[cfg(windows)]
    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "arkyve_migrate_{tag}_{}_{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    #[cfg(windows)]
    impl Drop for TempDir {
        fn drop(&mut self) {
            // The junctions go first, with `remove_dir`, which unlinks a mount
            // point without touching what is behind it. Leaving that to a
            // recursive delete, here of all places, would be careless.
            let root = self.0.join(crate::APP_DIR);
            let _ = std::fs::remove_dir(root.join(EXTRACTED_SUBDIR));
            let _ = std::fs::remove_dir(&root);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(windows)]
    use crate::dirhandle::junction;

    #[cfg(windows)]
    #[test]
    fn a_junction_in_place_of_the_app_data_directory_is_refused_not_followed() {
        // The header's attack, end to end. The scratch directory stands in for
        // `C:\Windows` and holds exactly what the cleanup deletes.
        let temp = TempDir::new("junction");
        let victim = temp.path().join("victim");
        std::fs::create_dir_all(victim.join(EXTRACTED_SUBDIR)).unwrap();
        for name in EXTRACTED_FILES {
            std::fs::write(victim.join(name), b"the target's own file").unwrap();
        }

        let root = temp.path().join(crate::APP_DIR);
        if !junction(&root, &victim) {
            // Group policy, or a filesystem without reparse points: asserting on
            // a machine that cannot host the attack proves nothing.
            eprintln!("skipped: mklink /J is unavailable here");
            return;
        }

        let found = clean_leftovers_in(&root);

        assert!(
            found.removed.is_empty(),
            "the cleanup deleted something through a junction: {:?}",
            found.removed
        );
        assert!(!found.reset_dacl, "a DACL was rewritten through a junction");
        assert!(
            found
                .warnings
                .iter()
                .any(|warning| warning.contains("reparse point")),
            "the refusal must be loud, and say why: {:?}",
            found.warnings
        );
        for name in EXTRACTED_FILES {
            assert!(
                victim.join(name).is_file(),
                "{name} was deleted through the junction"
            );
        }
        assert!(
            victim.join(EXTRACTED_SUBDIR).is_dir(),
            "the runtime directory was deleted through the junction"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_real_directory_is_still_cleaned() {
        // The other half of the pair: a check that refused everything would pass
        // the junction test just as well.
        let temp = TempDir::new("real");
        let root = temp.path().join(crate::APP_DIR);
        std::fs::create_dir_all(root.join(EXTRACTED_SUBDIR)).unwrap();
        for name in EXTRACTED_FILES {
            std::fs::write(root.join(name), b"stranded WinDivert runtime").unwrap();
        }

        let found = clean_leftovers_in(&root);

        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
        assert_eq!(found.removed.len(), EXTRACTED_FILES.len() + 1);
        assert!(found.removed.contains(&EXTRACTED_SUBDIR));
        assert!(!root.join(EXTRACTED_SUBDIR).exists());
        for name in EXTRACTED_FILES {
            assert!(!root.join(name).exists(), "{name} survived the cleanup");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_named_runtime_inside_a_real_directory_is_refused_too() {
        // The one a check on the root alone would miss: `runtime\` is a name
        // anything that can write to the app-data directory gets to choose, and
        // `remove_dir_all` is the dangerous end of this module.
        let temp = TempDir::new("subdir");
        let victim = temp.path().join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("keep-me.txt"), b"not yours to delete").unwrap();

        let root = temp.path().join(crate::APP_DIR);
        std::fs::create_dir_all(&root).unwrap();
        if !junction(&root.join(EXTRACTED_SUBDIR), &victim) {
            eprintln!("skipped: mklink /J is unavailable here");
            return;
        }

        let found = clean_leftovers_in(&root);

        assert!(!found.removed.contains(&EXTRACTED_SUBDIR));
        assert!(
            found
                .warnings
                .iter()
                .any(|warning| warning.contains("reparse point")),
            "{:?}",
            found.warnings
        );
        assert!(
            victim.join("keep-me.txt").is_file(),
            "remove_dir_all followed the junction"
        );
    }
}
