//! Where `System32` really is, asked of Win32 rather than of the environment.
//!
//! One function, in its own file, for the same reason [`crate::wide`] is: two
//! independently-gated subsystems need it and neither can reach the other.
//! `capture::pcap` (`windows` + `pcap-backend`) resolves `wpcap.dll` here;
//! `install` (`gui`) resolves the `curl.exe` it downloads with. A `gui` build
//! without `pcap-backend` has the second and not the first.
//!
//! Unlike `wide`, the argument for one copy is not about drift in a performance
//! claim — it is that the *reason* not to read `%SystemRoot%` is a security
//! argument, and a second copy of a security argument is a second place for
//! someone to conclude the environment variable would be simpler. `Cargo.toml`
//! already states it beside the `Win32_System_SystemInformation` feature; this
//! is the code it points at.

use std::path::PathBuf;

use tracing::warn;

/// The system directory (`C:\Windows\System32` on a stock install), or the
/// conventional path if Win32 will not answer.
///
/// **Not `%SystemRoot%`.** A UAC-elevated process inherits its environment from
/// the medium-integrity process that requested the elevation, so that variable
/// is chosen by whoever launched this app and not by the OS. Both callers use
/// the result to pick an *executable or library to load*, and this process holds
/// an administrator token: an attacker who can set `SystemRoot` in the
/// environment of a launcher, and plant a file under the path it names, gets
/// their code run elevated by a player clicking through one consent prompt they
/// were going to click anyway. `GetSystemDirectoryW` cannot be redirected that
/// way.
///
/// `MAX_PATH` is documented as always sufficient for this one directory, so the
/// truncation branch is unreachable in practice; it falls back rather than
/// truncating, because half a path is a path that could resolve somewhere else.
#[must_use]
pub fn directory() -> PathBuf {
    use std::os::windows::ffi::OsStringExt;

    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    const MAX_PATH_WIDE: u32 = 260;
    let mut buffer = [0_u16; MAX_PATH_WIDE as usize];
    // SAFETY: the pointer and the count describe the same stack array, and the
    // count is in `u16`s, which is what this call wants. It writes at most that
    // many and returns how many it wrote, excluding the terminator; `0` is its
    // failure return and a value above the count means it wrote nothing and is
    // asking for a bigger buffer. Both are handled below.
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

    /// Only the shape, because the interesting property is not observable from
    /// inside the process: proving the answer ignores `%SystemRoot%` would mean
    /// setting it, and nothing in this crate mutates the environment (see
    /// `config_path_from` and `log_dirs_from`, both split out for exactly that
    /// reason) — in edition 2024 `set_var` is `unsafe` because it races every
    /// concurrent `getenv` in the test binary. What holds the property is that
    /// there is no environment read left in the function above.
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
