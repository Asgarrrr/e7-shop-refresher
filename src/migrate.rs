//! One-time cleanup of state older versions left on the player's machine.
//!
//! Nothing here serves a running feature. Until the `WinDivert` backend was
//! removed, the app embedded a kernel driver plus its user-mode DLL,
//! extracted both into `%LOCALAPPDATA%\arkyve-refresh-shop\`, and — because
//! an elevated process was about to load a driver from that directory —
//! locked it down to Administrators and SYSTEM with a *protected* DACL.
//!
//! Both outlive the code that created them. The stranded files are litter;
//! the DACL is not: that directory is also the parent of `logs\` and
//! `crash.log`, and a protected admins-only DACL is inherited by both, so a
//! machine that ever ran one of those builds silently loses its log file on
//! every unelevated run. Measured on the dev machine: owner
//! `BUILTIN\Administrateurs`, inheritance off, two ACEs (SYSTEM,
//! Administrators), and an unelevated build with no log at all.
//!
//! So the cleanup ships with the removal, and is best-effort end to end —
//! the app is elevated (for the actuator, see `build.rs`), which is what
//! makes it able to undo an admins-only DACL at all, but a failure here must
//! never stop the relay from running.
//!
//! # Why this module opens a handle before it does anything
//!
//! Everything above describes an *elevated* DACL rewrite and an elevated
//! recursive delete, aimed at a path assembled from `%LOCALAPPDATA%`. That is
//! `crate::system32`'s argument arriving one directory over: a UAC-elevated
//! process inherits its environment from whoever asked for the elevation, so
//! `LOCALAPPDATA` is chosen by the launcher and not by the OS, and every Win32
//! or `std` API that takes a *path* follows reparse points.
//!
//! Composed, those two facts are an elevation-of-privilege primitive.
//! `LOCALAPPDATA` pointed at a directory the attacker owns, holding an
//! `arkyve-refresh-shop` junction to `C:\Windows`, turns
//! [`clean_windivert_leftovers`] — which runs at *every* launch, before
//! anything else in `main` — into "strip the explicit DACL off `C:\Windows`
//! and switch inheritance back on across the tree", followed by a
//! `remove_dir_all` in there. A directory junction needs no privilege at all
//! to create, unlike a symlink; the attacker only has to be able to start this
//! app with an environment block of their choosing, which is one shortcut.
//!
//! The defence is `open_directory_itself` — no intra-doc link, because that
//! function is `#[cfg(windows)]` and this header is not, so a link would break
//! rustdoc on the dev machine: resolve the directory to a
//! *handle* first, with `FILE_FLAG_OPEN_REPARSE_POINT` so that a reparse point
//! is reported rather than traversed, refuse loudly if what came back is not a
//! plain directory, and then address the DACL through that handle
//! (`GetSecurityInfo`/`SetSecurityInfo`) rather than through the name — which
//! removes the check-then-act window from the half of this module that can do
//! real damage.

use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::fs::File;

// The gate this module's elevated half goes through. It lives in its own module
// because `install` needs the identical one and cannot reach into this file; see
// that module's header.
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
/// A value rather than `warn!` calls in place, because of the ordering: the
/// DACL being undone here is precisely what stops `install_logging` from
/// opening its file, so the cleanup has to run first, and anything logged
/// directly would be emitted into a process with no subscriber and vanish.
#[derive(Default)]
pub struct Leftovers {
    reset_dacl: bool,
    removed: Vec<&'static str>,
    warnings: Vec<String>,
}

impl Leftovers {
    /// Emits what was found. Silent — not even a debug line — on the machines
    /// that never ran a `WinDivert` build, which after the first cleaned launch
    /// is every machine.
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
/// Shaped to run once and cost nothing afterwards: a directory that has no
/// stranded file and an unprotected DACL is left completely alone, so the
/// steady state is one `CreateFileW`, one `GetSecurityInfo` and four
/// open-and-fail calls that report "not found".
///
/// Call it before `main`'s logging setup (`src/main.rs`) and `report` the result
/// after — see [`Leftovers`]. No intra-doc link: `main` lives in the binary
/// target, which the library's rustdoc can never see.
#[must_use = "the findings are logged by `report` once the subscriber exists"]
pub fn clean_windivert_leftovers() -> Leftovers {
    match app_data_root() {
        Some(root) => clean_leftovers_in(&root),
        None => Leftovers::default(),
    }
}

/// The cleanup itself, against a directory named by the caller.
///
/// Split out for the same reason `config_path_from` and `log_dirs_from` are:
/// nothing in this crate mutates the environment (in edition 2024 `set_var` is
/// `unsafe`, because it races every concurrent `getenv` in the test binary), so
/// a test can only reach this code by handing it a path. That matters more here
/// than there — the refusal this function makes when handed a junction is the
/// one behaviour in the module that has to keep working.
fn clean_leftovers_in(root: &Path) -> Leftovers {
    let mut found = Leftovers::default();

    #[cfg(windows)]
    {
        // The handle *is* the existence check: `CreateFileW` on a missing
        // directory is the same "not found" `is_dir` used to answer, and one
        // call now does both that and the validation. Held for as long as the
        // DACL work, then dropped before anything below deletes.
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
            // Not necessarily our doing — but nothing else ever protects this
            // directory, and the only way out is to undo it: a protected DACL here
            // is exactly the extract-and-harden footprint.
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

    // No handle validation to do on a dev machine (mac): there is no DACL to
    // undo there and no elevation behind the deletes, so this is the plain
    // "does it exist" question it always was.
    #[cfg(not(windows))]
    if !root.is_dir() {
        return found;
    }

    for name in EXTRACTED_FILES {
        let stale = root.join(name);
        // `remove_file` deletes a symlink rather than its target, so the three
        // named files need no reparse check of their own — and the directory
        // they are looked up in has already had one.
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
    // The same gate as the root's, because this is the elevated `remove_dir_all`
    // and `runtime\` is a name anything with write access to the app-data
    // directory can claim — with a junction, if it wants the delete to land
    // somewhere else.
    #[cfg(windows)]
    let removable = match open_directory_itself(&subdir) {
        // Not bound: the handle has done its job, and it has to be closed
        // before `remove_dir_all` can take the directory away.
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
/// `None` when `LOCALAPPDATA` is unset. The old code also had an exe-directory
/// fallback, and it is deliberately *not* cleaned: deleting files and rewriting
/// a DACL in a folder the user chose (a Desktop, a network share) is a far more
/// surprising action than leaving an exotic, effectively unreachable
/// configuration alone.
///
/// Still the environment variable, and not `SHGetKnownFolderPath`. The known
/// folder would close the one vector the variable opens — an environment block
/// chosen by the launcher of an elevated process — but it would *not* close the
/// vector that matters, because `FOLDERID_LocalAppData` resolves through
/// `HKCU\…\User Shell Folders`, which the same medium-integrity attacker can
/// write. Neither source is trustworthy enough to skip the check in
/// `open_directory_itself`, and once that check is there it is what carries
/// the safety property, for any spelling of the path. (It also needs
/// `windows-sys` features this crate does not enable: `Win32_UI_Shell` for the
/// call, `Win32_System_Com` for the `CoTaskMemFree` that pairs with it.)
fn app_data_root() -> Option<PathBuf> {
    app_data_root_from(
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .as_deref(),
    )
}

/// Pure half of [`app_data_root`].
///
/// The absoluteness test is not decoration. `LOCALAPPDATA` is a string an
/// attacker can choose, and a *relative* one resolves against this process's
/// working directory — which is whatever directory the shortcut that launched
/// it named. Refusing here means the caller never even gets to the handle
/// check with a path whose meaning depends on where the app was started from.
fn app_data_root_from(local_appdata: Option<&Path>) -> Option<PathBuf> {
    local_appdata
        .filter(|local| local.is_absolute())
        .map(|local| local.join(crate::APP_DIR))
}

/// True when the open directory's DACL carries `SE_DACL_PROTECTED`, i.e.
/// inheritance from its parent is switched off. That flag is the signature of a
/// `WinDivert` install: nothing in this app sets it any more, and it is what
/// keeps a non-elevated process out of `logs\` and `crash.log`.
///
/// Takes the handle [`open_directory_itself`] validated rather than a path —
/// `GetSecurityInfo` and `GetNamedSecurityInfoW` differ in exactly that, and
/// the difference is the whole point.
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

    // The allocation lifetime spans the three calls below, and each of them gets
    // its own narrow `unsafe` block so that no single `// SAFETY:` has to cover
    // the whole function: on success `GetSecurityInfo` writes `descriptor`
    // with a single `LocalAlloc` block that owns the ACL as well, and it is freed
    // exactly once — the early return below happens before anything is allocated,
    // and the `LocalFree` further down is the only one on any path past it.
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();

    // SAFETY: `dir` is an open directory handle carrying `READ_CONTROL`, which
    // is the right this information class needs, and the borrow keeps it alive
    // across the call. The four `null_mut()` out-parameters are documented as
    // optional (we want the descriptor, not the owner/group/ACL pointers into
    // it). `descriptor` is a live stack slot, written only when the call returns
    // `ERROR_SUCCESS`, which is checked before any read. Failure mode: a
    // `WIN32_ERROR` return with nothing allocated.
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
    // success for, and nothing has freed it yet. `control`/`revision` are stack
    // slots that outlive the call.
    let ok = unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) };
    // GetLastError is per-thread and the very next Win32 call clobbers it:
    // read it before `LocalFree`, not after.
    let err = std::io::Error::last_os_error();
    // SAFETY: `descriptor` is that same `LocalAlloc` block, freed exactly once
    // here — the only `LocalFree` in the function, on the only path that reaches
    // it — and never touched afterwards (`control` is a copy).
    unsafe { LocalFree(descriptor.cast()) };
    if ok == 0 {
        return Err(err);
    }
    Ok(control & SE_DACL_PROTECTED != 0)
}

/// Puts the open directory back on inherited permissions: no explicit ACEs of
/// its own, and auto-inheritance from the parent switched back on. Children
/// whose DACL is auto-inherited (`logs\`, `crash.log`) are recomputed by the
/// same call.
///
/// Handle-based for the reason given on [`open_directory_itself`]: this is the
/// call that rewrites permissions across a whole subtree, so it is the one that
/// must not be re-pointed by a name resolving somewhere else than it did a
/// moment ago.
///
/// The ACL passed in is deliberately **empty but not null** — the whole
/// point of this function. `SetSecurityInfo`'s documentation is explicit:
/// `DACL_SECURITY_INFORMATION` with a `NULL` `pDacl` does not mean "no DACL
/// to set", it installs a *null DACL*, granting FULL ACCESS TO EVERYONE.
/// That would leave the directory holding the player's logs and crash log
/// world-writable — strictly worse than the over-broad DACL being undone. A
/// zero-ACE ACL plus `UNPROTECTED_DACL_SECURITY_INFORMATION` is the actual
/// "reset to inherited" spelling. Do not "simplify" the ACL away.
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

    // SAFETY: `acl_buf` is a `u32`-aligned stack buffer, larger than the `ACL`
    // header `InitializeAcl` writes into it, and its length is passed as the
    // exact byte size of that same buffer — so `InitializeAcl` cannot write out
    // of bounds.
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

    // SAFETY: `acl_buf` holds an initialized zero-ACE `ACL` — the call above
    // reported success, which is the only way to reach here — and it outlives
    // `SetSecurityInfo`, which does not retain the pointer. `dir` is an open
    // directory handle carrying `WRITE_DAC`, alive for the whole call through
    // the borrow. The owner, group and SACL pointers are null, which is "do not
    // change" for the information bits we did not request. Failure mode: a
    // `WIN32_ERROR` return (typically `ERROR_ACCESS_DENIED` when not elevated),
    // which the caller treats as non-fatal.
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
        // The DACL reset propagates *downward* into auto-inherited children, so
        // aiming one level too high would rewrite permissions across the whole
        // of `%LOCALAPPDATA%`.
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
        // An elevated delete and an elevated DACL rewrite, aimed at whatever
        // directory the shortcut happened to start the app in. Absolute or
        // nothing.
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
        // The steady state on every machine, one launch after the cleanup: no
        // findings, so `report` stays completely silent.
        let found = Leftovers::default();
        assert!(!found.reset_dacl);
        assert!(found.removed.is_empty());
        assert!(found.warnings.is_empty());
    }

    #[test]
    fn the_stale_files_are_the_three_a_windivert_build_extracted() {
        // Spelled out here because the module that wrote them is gone: nothing
        // else in the tree still names these files, so a typo would silently
        // clean nothing at all.
        assert_eq!(
            EXTRACTED_FILES,
            ["WinDivert.dll", "WinDivert64.sys", "WinDivert-LICENSE.txt"]
        );
    }

    /// RAII scratch directory, in the shape `crash.rs`'s `TempFile` uses and for
    /// the same reasons: removed on drop including on an assertion panic, and
    /// named by test and pid so parallel tests cannot collide.
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
            // The junctions go first, by name and with `remove_dir`, which
            // unlinks a mount point without touching what is behind it. Leaving
            // that to a recursive delete in the one test file that exists
            // because of reparse points would be careless even if it worked.
            let root = self.0.join(crate::APP_DIR);
            let _ = std::fs::remove_dir(root.join(EXTRACTED_SUBDIR));
            let _ = std::fs::remove_dir(&root);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A directory junction, which — unlike a symlink — an ordinary user can
    /// create, which is exactly why it is the interesting attack. `mklink` is a
    /// `cmd` builtin, so it cannot be spawned directly.
    #[cfg(windows)]
    fn junction(link: &Path, target: &Path) -> bool {
        std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .output()
            .is_ok_and(|out| out.status.success())
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_in_place_of_the_app_data_directory_is_refused_not_followed() {
        // The finding this module's header describes, reproduced end to end:
        // `%LOCALAPPDATA%` under the attacker's control, and the app-data
        // directory a junction onto a tree they want an elevated process to
        // touch. Standing in for `C:\Windows` is a scratch directory holding
        // exactly what the cleanup deletes — if the junction is followed, these
        // are gone, and on a real machine so is `C:\Windows`' DACL.
        let temp = TempDir::new("junction");
        let victim = temp.path().join("victim");
        std::fs::create_dir_all(victim.join(EXTRACTED_SUBDIR)).unwrap();
        for name in EXTRACTED_FILES {
            std::fs::write(victim.join(name), b"the target's own file").unwrap();
        }

        let root = temp.path().join(crate::APP_DIR);
        if !junction(&root, &victim) {
            // Group policy or a filesystem without reparse points. Skipping is
            // honest; asserting on a machine that cannot host the attack is not.
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
        // The other half of the pair: the refusal above is only worth having if
        // the ordinary case still works, and a check that refused everything
        // would pass that test just as well.
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
        // The second elevated operation, and the one a plain check on the root
        // would miss: `runtime\` is a name anything that can write to the
        // app-data directory gets to choose, and `remove_dir_all` is the
        // dangerous end of this module.
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
