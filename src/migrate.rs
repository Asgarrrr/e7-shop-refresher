//! One-time cleanup of state older versions left on the player's machine.
//!
//! Nothing here serves a running feature. Until the `WinDivert` backend was
//! removed, the app embedded a kernel driver plus its user-mode DLL, extracted
//! both into `%LOCALAPPDATA%\arkyve-refresh-shop\`, and — because an elevated
//! process was about to load a driver out of that directory — locked the
//! directory itself down to Administrators and SYSTEM with a *protected* DACL.
//!
//! Both of those outlive the code that created them. The stranded files are
//! merely litter; the DACL is not. That directory is also the parent of `logs\`
//! and `crash.log`, and a protected admins-only DACL is inherited by both, so a
//! machine that ever ran one of those builds silently loses its log file on
//! every run that is not elevated. Measured on the development machine after
//! the fact: owner `BUILTIN\Administrateurs`, inheritance off, two ACEs
//! (SYSTEM, Administrators), and an unelevated build with no log at all.
//!
//! So the cleanup ships with the removal rather than after it. It is
//! best-effort from end to end — the app is elevated (for the actuator, see
//! `build.rs`), which is what makes it able to undo an admins-only DACL at all,
//! but a failure here must never stop the relay from running.

// `Path` is only ever taken by the two Win32 helpers below; on a dev machine
// (mac) the cleanup still deletes the files, there is simply no DACL to undo.
#[cfg(windows)]
use std::path::Path;
use std::path::PathBuf;

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
/// The ordering is the whole reason this is a value rather than a set of
/// `warn!` calls in place. The DACL being undone here is precisely what stops
/// `install_logging` from opening its file, so the cleanup has to run first —
/// and everything it has to say would then be emitted into a process with no
/// subscriber and vanish. One run's worth of findings is small enough to carry
/// across those few lines.
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
/// steady state is one `is_dir`, one `GetNamedSecurityInfoW` and three
/// `remove_file` calls that report "not found".
///
/// Call it before `main`'s logging setup (`src/main.rs`) and `report` the result
/// after — see [`Leftovers`]. No intra-doc link: `main` lives in the binary
/// target, which the library's rustdoc can never see.
#[must_use = "the findings are logged by `report` once the subscriber exists"]
pub fn clean_windivert_leftovers() -> Leftovers {
    let mut found = Leftovers::default();
    let Some(root) = app_data_root() else {
        return found;
    };
    if !root.is_dir() {
        return found;
    }

    #[cfg(windows)]
    match dacl_is_protected(&root) {
        // Not necessarily our doing — but nothing else ever protects this
        // directory, and the only way out is to undo it: a protected DACL here
        // is exactly the extract-and-harden footprint.
        Ok(true) => match reset_dacl_to_inherited(&root) {
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

    for name in EXTRACTED_FILES {
        let stale = root.join(name);
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
    if subdir.is_dir() {
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
fn app_data_root() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|local| PathBuf::from(local).join(crate::APP_DIR))
}

/// True when `dir`'s DACL carries `SE_DACL_PROTECTED`, i.e. inheritance from
/// its parent is switched off. That flag is the signature of a `WinDivert`
/// install: nothing in this app sets it any more, and it is what keeps a
/// non-elevated process out of `logs\` and `crash.log`.
#[cfg(windows)]
fn dacl_is_protected(dir: &Path) -> std::io::Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, PSECURITY_DESCRIPTOR,
        SE_DACL_PROTECTED,
    };

    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // The allocation lifetime spans the three calls below, and each of them gets
    // its own narrow `unsafe` block so that no single `// SAFETY:` has to cover
    // the whole function: on success `GetNamedSecurityInfoW` writes `descriptor`
    // with a single `LocalAlloc` block that owns the ACL as well, and it is freed
    // exactly once — the early return below happens before anything is allocated,
    // and the `LocalFree` further down is the only one on any path past it.
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();

    // SAFETY: `wide` is a valid null-terminated UTF-16 path, owned by this frame
    // and alive across the whole call. The four `null_mut()` out-parameters are
    // documented as optional (we want the descriptor, not the owner/group/ACL
    // pointers into it). `descriptor` is a live stack slot, written only when the
    // call returns `ERROR_SUCCESS`, which is checked before any read. Failure
    // mode: a missing or inaccessible directory returns a `WIN32_ERROR` and
    // nothing is allocated.
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
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

/// Puts `dir` back on inherited permissions: no explicit ACEs of its own, and
/// auto-inheritance from the parent switched back on. Children whose DACL is
/// auto-inherited (`logs\`, `crash.log`) are recomputed by the same call.
///
/// The ACL passed in is deliberately **empty but not null**, and that
/// distinction is the whole point of this function. `SetSecurityInfo`'s
/// documentation is explicit: `DACL_SECURITY_INFORMATION` with a `NULL` `pDacl`
/// does not mean "no DACL to set", it installs a *null DACL*, which grants FULL
/// ACCESS TO EVERYONE. Doing that here would leave the directory holding the
/// player's logs and crash log world-writable — strictly worse than the
/// over-broad DACL we are undoing. A zero-ACE ACL plus
/// `UNPROTECTED_DACL_SECURITY_INFORMATION` is the actual "reset to inherited"
/// spelling: nothing granted explicitly, everything granted by inheritance.
/// Do not "simplify" the ACL away.
#[cfg(windows)]
fn reset_dacl_to_inherited(dir: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
    use windows_sys::Win32::Security::{
        ACL, ACL_REVISION, DACL_SECURITY_INFORMATION, InitializeAcl,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    };

    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
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
    // `SetNamedSecurityInfoW`, which does not retain the pointer. `wide` is a
    // valid null-terminated UTF-16 path alive for the whole call. The owner, group
    // and SACL pointers are null, which is "do not change" for the information
    // bits we did not request. Failure mode: a `WIN32_ERROR` return (typically
    // `ERROR_ACCESS_DENIED` when not elevated), which the caller treats as
    // non-fatal.
    let result = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr(),
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
}
