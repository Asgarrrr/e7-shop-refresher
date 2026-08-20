//! Opening a directory *as itself*, for the two places that then rewrite its
//! permissions.
//!
//! Every path-based Windows API — `SetNamedSecurityInfoW`, `remove_dir_all`,
//! `is_dir` — resolves reparse points on the way, and a junction needs no
//! privilege to create, unlike a symlink. So a junction dropped in place of a
//! directory redirects whatever an elevated caller was about to do onto a tree
//! the attacker chose. Testing the name first does not fix it: the name can be
//! re-pointed between the test and the act.
//!
//! The only binding form is to resolve to a *handle* once, refuse it if it is a
//! reparse point, and address the object through that handle
//! (`GetSecurityInfo`/`SetSecurityInfo`, not their `Named` counterparts), so
//! that the thing checked and the thing acted on are the same object by
//! construction.
//!
//! Ungated and windows-only for the same reason as [`crate::wide`] and
//! [`crate::system32`]: [`crate::migrate`] (always compiled) and
//! [`crate::install`] (gui) both rewrite a DACL under an elevated token and
//! neither can reach the other. A security gate must not exist in two copies.

use std::fs::File;
use std::path::Path;

/// Opens `dir` *as itself*: never through a reparse point, and never as a file.
///
/// The one gate the elevated callers go through; see the module header. Three
/// properties:
///
/// * `FILE_FLAG_BACKUP_SEMANTICS`, without which `CreateFileW` cannot open a
///   directory at all.
/// * `FILE_FLAG_OPEN_REPARSE_POINT`, so the handle refers to the junction itself
///   instead of landing on its target.
/// * `READ_CONTROL | WRITE_DAC` up front, so the DACL is rewritten through
///   *this* handle. That is what makes the check binding rather than advisory.
///
/// **Any** reparse tag is refused, not just the two `is_symlink` covers: a
/// reparse point on a directory this app created is never ours, and the cost of
/// being wrong is the right way round — a false positive skips a cleanup and
/// logs why, a false negative rewrites permissions on the attacker's tree.
pub fn open_directory_itself(dir: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    // Spelled out rather than imported: these live in `windows-sys`'
    // `Win32_Storage_FileSystem` feature, a large module to enable for seven
    // integers that are ABI and cannot change.
    const READ_CONTROL: u32 = 0x0002_0000;
    const WRITE_DAC: u32 = 0x0004_0000;
    const FILE_SHARE_READ_WRITE_DELETE: u32 = 0x0000_0007;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

    // Shared for read, write *and* delete: this handle is a probe, and holding
    // it must not be the reason some other process — or a `remove_dir_all` in
    // the caller — fails.
    let handle = std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL | WRITE_DAC)
        .share_mode(FILE_SHARE_READ_WRITE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(dir)?;

    // Asked of the handle, not of the path, so the answer describes the object
    // the caller is about to act on and not whatever the name resolves to next.
    let attributes = handle.metadata()?.file_attributes();
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} is a reparse point (junction or symlink), not a directory",
                dir.display()
            ),
        ));
    }
    if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!("{} is a file", dir.display()),
        ));
    }
    Ok(handle)
}

/// True when `dir` exists and is a plain directory this process may write into
/// — never a junction, never a symlink, never a file.
///
/// The write paths (`crash`, `lib`'s log and config roots) need the *answer*,
/// not the handle: they hand the path to `create_dir_all` and `OpenOptions`,
/// which resolve reparse points themselves. This narrows the window between
/// check and use to whatever those calls take; it does not close it. Closing it
/// needs handle-relative opens that `std` does not expose, and the trade is
/// deliberate — refusing a redirected root is worth far more than the residual
/// race, and the alternative shipped today is no check at all.
pub fn is_plain_directory(dir: &Path) -> bool {
    open_directory_itself(dir).is_ok()
}
