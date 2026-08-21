//! Where `System32` really is, asked of Win32 rather than of the environment.
//!
//! One function, in its own file, for the same reason [`crate::wide`] is: two
//! independently-gated subsystems need it and neither can reach the other —
//! `capture::pcap` resolves `wpcap.dll` here, `install` (gui) the `curl.exe` it
//! downloads with.
//!
//! Unlike `wide`, the argument for one copy is not a drifting performance claim:
//! the reason not to read `%SystemRoot%` is a security argument, and a second
//! copy of one is a second place to conclude the variable would be simpler.

use std::path::PathBuf;

use tracing::warn;

/// The system directory (`C:\Windows\System32` on a stock install), or the
/// conventional path if Win32 will not answer.
///
/// **Not `%SystemRoot%`.** A UAC-elevated process inherits its environment from
/// whoever requested the elevation, both callers use this result to pick an
/// *executable or library to load*, and this process holds an administrator
/// token — so an attacker who sets `SystemRoot` in a launcher's environment and
/// plants a file under it gets their code run elevated. `GetSystemDirectoryW`
/// cannot be redirected that way.
///
/// `MAX_PATH` is not documented as always sufficient here; the contract is only
/// that a too-small buffer is reported, as the `SAFETY` note below spells out.
/// A real system directory is short, so that branch is very unlikely to fire —
/// it is kept because the call reports the case for free, and it falls back
/// rather than truncating, because half a path is a path that could resolve
/// somewhere else.
#[must_use]
pub fn directory() -> PathBuf {
    use std::os::windows::ffi::OsStringExt;

    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    const MAX_PATH_WIDE: u32 = 260;
    let mut buffer = [0_u16; MAX_PATH_WIDE as usize];
    // SAFETY: the pointer and the count describe the same stack array, and the
    // count is in `u16`s as this call wants. It writes at most that many and
    // returns how many, excluding the terminator; `0` is failure and a value
    // above the count means "wrote nothing, need a bigger buffer". Both handled
    // below.
    let written = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), MAX_PATH_WIDE) };
    match buffer.get(..written as usize) {
        Some(path) if written > 0 => PathBuf::from(std::ffi::OsString::from_wide(path)),
        _ => {
            warn!(
                written,
                "GetSystemDirectoryW did not answer; assuming the conventional path"
            );
            PathBuf::from(r"C:\Windows\System32")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the shape: proving the answer ignores `%SystemRoot%` would mean
    /// setting it, and `set_var` is `unsafe` in edition 2024 because it races
    /// every concurrent `getenv`. What holds the property is that there is no
    /// environment read left in the function above.
    #[test]
    fn the_answer_is_an_absolute_directory_that_exists() {
        let dir = directory();
        assert!(dir.is_absolute(), "{dir:?}");
        assert!(
            dir.is_dir(),
            "{dir:?} — both callers join an executable onto this"
        );
        // Not `ends_with("System32")`: a machine can be configured otherwise,
        // and asserting the stock layout would make this a test of the machine.
        assert!(dir.join("kernel32.dll").is_file(), "{dir:?}");
    }
}
