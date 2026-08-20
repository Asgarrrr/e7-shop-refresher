//! Opening a directory *as itself*, for the two places that then rewrite its
//! permissions.
//!
//! On Windows every path-based API — `SetNamedSecurityInfoW`, `remove_dir_all`,
//! `is_dir` — resolves reparse points on the way. A junction dropped in place of
//! a directory therefore silently redirects whatever the caller was about to do
//! onto a tree the attacker chose, and a junction needs no privilege at all to
//! create, unlike a symlink. When the caller is elevated, that is the whole ball
//! game.
//!
//! Testing the name first does not fix it: the name can be re-pointed between
//! the test and the act. The only binding form is to resolve the directory to a
//! *handle* once, refuse it if it is a reparse point, and then address the
//! object through that handle — `GetSecurityInfo`/`SetSecurityInfo` rather than
//! their `Named` counterparts — so that the thing checked and the thing acted on
//! are the same object by construction.
//!
//! Ungated and windows-only, for the same reason as [`crate::wide`] and
//! [`crate::system32`]: [`crate::migrate`] (always compiled) and
//! [`crate::install`] (gui) both hold an elevated token while rewriting a DACL,
//! neither can reach the other, and a security gate is the last thing that
//! should exist in two copies free to drift apart. One implementation, one place
//! to audit.

use std::fs::File;
use std::path::Path;

/// Opens `dir` *as itself*: never through a reparse point, and never as a file.
///
/// The one gate the elevated callers go through — see the module header for what
/// it is defending against. Three properties, in order:
///
/// * `FILE_FLAG_BACKUP_SEMANTICS`, without which `CreateFileW` cannot open a
///   directory at all.
/// * `FILE_FLAG_OPEN_REPARSE_POINT`, which makes the returned handle refer to
///   the junction/symlink itself instead of silently landing on its target.
/// * `READ_CONTROL | WRITE_DAC`, the two rights the caller needs, asked for up
///   front so that the DACL is read and rewritten through *this* handle. That
///   is the part that makes the check binding rather than advisory: a
///   name-based `SetNamedSecurityInfoW` after a name-based test would still
///   race a junction swapped in between the two.
///
/// **Any** reparse tag is refused, not just the two that `is_symlink` covers.
/// These are directories this app created under names it chose; a reparse point
/// on one is never something we put there, whatever the tag says, and the cost
/// of being wrong is the right way round — a false positive skips a cleanup or
/// a download and logs why, a false negative rewrites permissions on a tree the
/// attacker chose.
pub fn open_directory_itself(dir: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    // Spelled out rather than imported: every one of these lives in
    // `windows-sys`' `Win32_Storage_FileSystem` feature, which this crate does
    // not enable and which is a large module to pull in for seven integers. They
    // are ABI — `winnt.h` values that cannot change without breaking every
    // binary ever compiled against Windows.
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
