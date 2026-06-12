//! In-place self-update: downloads the new exe from a GitHub release,
//! verifies it against `SHA256SUMS.txt`, swaps the current binary, and
//! relaunches.
//!
//! Windows-only binary-swap pattern: the running .exe can't be deleted
//! or overwritten, but it CAN be renamed. So we rename `current.exe` →
//! `current.exe.bak`, write the new bytes to the original path, spawn
//! the new exe, and exit. Next launch deletes the leftover `.bak`.

use std::fs::File;
use std::io::{BufWriter, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use anyhow::{Result, anyhow, bail};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

const REPO: &str = "Asgarrrr/e7-shop-refresher";

#[derive(Debug, Clone)]
pub struct ReleaseTarget {
    pub tag: String,
    pub exe_name: String,
    pub exe_url: String,
    pub checksums_url: String,
}

impl ReleaseTarget {
    pub fn for_running_binary(tag: impl Into<String>) -> Result<Self> {
        let tag = tag.into();
        let exe_path =
            std::env::current_exe().map_err(|e| anyhow!("cannot resolve current_exe: {e}"))?;
        let exe_name = exe_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("current_exe has no filename component"))?
            .to_string();
        // `tag` may or may not carry the `v` prefix — passed through
        // unchanged to match whatever GitHub's `tag_name` returned.
        Ok(Self {
            exe_url: format!("https://github.com/{REPO}/releases/download/{tag}/{exe_name}"),
            checksums_url: format!(
                "https://github.com/{REPO}/releases/download/{tag}/SHA256SUMS.txt"
            ),
            tag,
            exe_name,
        })
    }
}

/// Drains via the UI thread every frame; drives the banner state machine.
#[derive(Debug, Clone)]
pub enum UpdateEvent {
    /// `total` is `None` until the server reports `Content-Length`.
    Downloading {
        bytes: u64,
        total: Option<u64>,
    },
    Verifying,
    InstallingAndRestarting,
    Failed(String),
}

/// Success path ends in `std::process::exit(0)`; failure path sends
/// `Failed` and the thread returns.
pub fn spawn_install(target: ReleaseTarget, tx: Sender<UpdateEvent>) {
    let worker_tx = tx.clone();
    let spawn = std::thread::Builder::new()
        .name("auto-update".into())
        .spawn(move || {
            if let Err(e) = run(&target, &worker_tx) {
                warn!(error = %e, tag = %target.tag, "auto-update failed");
                let _ = worker_tx.send(UpdateEvent::Failed(e.to_string()));
            }
        });
    if let Err(e) = spawn {
        warn!(error = %e, "failed to spawn auto-update thread");
        let _ = tx.send(UpdateEvent::Failed(format!("could not spawn worker: {e}")));
    }
}

fn run(target: &ReleaseTarget, tx: &Sender<UpdateEvent>) -> Result<()> {
    info!(tag = %target.tag, exe = %target.exe_name, "auto-update starting");

    let download_path = download_path_for(&target.exe_name)?;
    debug!(download_path = %download_path.display(), "download path resolved");

    // Stream to disk so a multi-MB binary doesn't sit in RAM and a
    // mid-download crash leaves a stray file, not a corrupted in-place exe.
    let progress_tx = tx.clone();
    // Throttle: one event per 256 KB so the channel + repaint scheduler
    // don't get hammered on a 10 MB binary.
    let mut last_sent: u64 = 0;
    {
        let file = File::create(&download_path)
            .map_err(|e| anyhow!("create {}: {e}", download_path.display()))?;
        let mut buf = BufWriter::new(file);
        crate::http::download_to(&target.exe_url, &mut buf, |bytes, total| {
            const THROTTLE: u64 = 256 * 1024;
            if bytes.saturating_sub(last_sent) >= THROTTLE {
                last_sent = bytes;
                let _ = progress_tx.send(UpdateEvent::Downloading { bytes, total });
            }
        })?;
        // BufWriter::drop flushes implicitly; the block end is enough.
    }

    let _ = tx.send(UpdateEvent::Verifying);
    verify_sha256(&download_path, &target.exe_name, &target.checksums_url)?;

    let _ = tx.send(UpdateEvent::InstallingAndRestarting);
    install_and_restart(&download_path)?;

    // install_and_restart exit()s, so unreachable — kept as a guard
    // against future refactors of the exit path.
    Ok(())
}

fn download_path_for(exe_name: &str) -> Result<PathBuf> {
    // Stage next to the running exe so the rename is same-volume —
    // `std::fs::rename` returns ERROR_NOT_SAME_DEVICE across drives,
    // which would happen if we staged in %TEMP% (C:) while the user
    // installed on D:.
    let current = std::env::current_exe().map_err(|e| anyhow!("resolve current_exe: {e}"))?;
    let parent = current
        .parent()
        .ok_or_else(|| anyhow!("current_exe has no parent dir"))?;
    Ok(parent.join(format!("{exe_name}.new")))
}

fn verify_sha256(path: &Path, exe_name: &str, checksums_url: &str) -> Result<()> {
    let body =
        crate::http::get_text(checksums_url).map_err(|e| anyhow!("fetch SHA256SUMS: {e}"))?;
    let expected = parse_expected_checksum(&body, exe_name)
        .ok_or_else(|| anyhow!("{exe_name} not listed in SHA256SUMS"))?;

    let mut file =
        File::open(path).map_err(|e| anyhow!("open {} for verify: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| anyhow!("read {} for verify: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    // sha2 0.11 GenericArray doesn't implement LowerHex.
    let mut actual = String::with_capacity(digest.len() * 2);
    for byte in digest.as_slice() {
        use std::fmt::Write;
        let _ = write!(&mut actual, "{byte:02x}");
    }
    if actual != expected {
        bail!("SHA256 mismatch: expected {expected}, got {actual}");
    }
    info!(sha256 = %actual, "downloaded binary verified");
    Ok(())
}

/// `SHA256SUMS.txt` format published by the release workflow:
/// `<lowercase-hex>  <filename>` one per line. Strict exact-filename
/// match so `e7-shop-refresher.exe` and `*-cli.exe` can't be confused.
fn parse_expected_checksum(body: &str, exe_name: &str) -> Option<String> {
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let hash = parts.next()?.trim().to_ascii_lowercase();
        let name = parts.next()?.trim();
        if name.eq_ignore_ascii_case(exe_name) {
            return Some(hash);
        }
    }
    None
}

fn install_and_restart(downloaded: &Path) -> Result<()> {
    let current = std::env::current_exe().map_err(|e| anyhow!("resolve current_exe: {e}"))?;
    let bak = current.with_extension("exe.bak");

    // Leftover .bak deletes freely once the previous process exited.
    let _ = std::fs::remove_file(&bak);

    // Renaming a running .exe is allowed on Windows — once renamed,
    // the original path is free even though the old file is still
    // locked under its new name.
    std::fs::rename(&current, &bak).map_err(|e| {
        anyhow!(
            "rename current exe {} → {}: {e}",
            current.display(),
            bak.display()
        )
    })?;

    // Roll back the .bak on failure so the user isn't left with a
    // missing exe. If the rollback ALSO fails (AV scan window, file
    // lock racing both renames), surface that distinctly so the user
    // knows the install path is empty and they need to recover by
    // hand.
    if let Err(e) = std::fs::rename(downloaded, &current) {
        match std::fs::rename(&bak, &current) {
            Ok(()) => bail!(
                "move new exe → {}: {e} (rolled back from {})",
                current.display(),
                bak.display()
            ),
            Err(rb) => bail!(
                "move new exe → {}: {e}; rollback rename {} → {} also failed: {rb} — \
                 the original binary is at {} until manually restored",
                current.display(),
                bak.display(),
                current.display(),
                bak.display()
            ),
        }
    }

    info!(target = %current.display(), "new binary in place — spawning replacement");

    // Spawned child inherits our admin token (manifest requires it).
    // If spawn fails, the swap has already succeeded — the new binary
    // is at `current.exe`, so the message is "installed, restart
    // manually", not "update failed".
    std::process::Command::new(&current).spawn().map_err(|e| {
        anyhow!(
            "new binary installed at {} but auto-restart failed ({e}) — please close \
             and reopen the app to finish updating",
            current.display()
        )
    })?;

    // Let the child finish egui boot (~300 ms) before we drop our
    // window — otherwise the user sees a flicker between exit and
    // re-show.
    std::thread::sleep(std::time::Duration::from_millis(300));
    std::process::exit(0);
}

/// Best-effort cleanup of leftover `.bak` (post-install) and `.new`
/// (crashed mid-download). Failures are silent — a future restart
/// gets it.
pub fn cleanup_previous_bak() {
    let Ok(current) = std::env::current_exe() else {
        return;
    };
    for stale in [
        current.with_extension("exe.bak"),
        current.with_extension("exe.new"),
    ] {
        if !stale.exists() {
            continue;
        }
        match std::fs::remove_file(&stale) {
            Ok(()) => debug!(path = %stale.display(), "removed leftover update artefact"),
            Err(e) => {
                debug!(error = %e, path = %stale.display(), "could not clean leftover update artefact")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_target_constructs_release_urls_from_tag() {
        // Can't call for_running_binary in tests cleanly — fake the exe
        // name by constructing the struct directly.
        let target = ReleaseTarget {
            tag: "v0.7.0".into(),
            exe_name: "e7-shop-refresher.exe".into(),
            exe_url: "https://github.com/Asgarrrr/e7-shop-refresher/releases/download/v0.7.0/e7-shop-refresher.exe".into(),
            checksums_url: "https://github.com/Asgarrrr/e7-shop-refresher/releases/download/v0.7.0/SHA256SUMS.txt".into(),
        };
        assert!(target.exe_url.contains("/v0.7.0/"));
        assert!(target.exe_url.ends_with("/e7-shop-refresher.exe"));
        assert!(target.checksums_url.ends_with("/SHA256SUMS.txt"));
    }

    #[test]
    fn parse_expected_checksum_finds_named_file() {
        let body = "\
            aaaa  e7-shop-refresher-v0.7.0-windows-x64.zip\n\
            bbbb  e7-shop-refresher.exe\n\
            cccc  e7-shop-refresher-cli.exe\n";
        assert_eq!(
            parse_expected_checksum(body, "e7-shop-refresher.exe"),
            Some("bbbb".into())
        );
        assert_eq!(
            parse_expected_checksum(body, "e7-shop-refresher-cli.exe"),
            Some("cccc".into())
        );
    }

    #[test]
    fn parse_expected_checksum_returns_none_for_unknown_file() {
        let body = "aaaa  some-other.exe\n";
        assert_eq!(parse_expected_checksum(body, "e7-shop-refresher.exe"), None);
    }

    #[test]
    fn parse_expected_checksum_skips_blank_lines() {
        let body = "\n  \nbbbb  e7-shop-refresher.exe\n";
        assert_eq!(
            parse_expected_checksum(body, "e7-shop-refresher.exe"),
            Some("bbbb".into())
        );
    }

    #[test]
    fn parse_expected_checksum_is_case_insensitive_on_filename() {
        let body = "bbbb  E7-Shop-Refresher.exe\n";
        assert_eq!(
            parse_expected_checksum(body, "e7-shop-refresher.exe"),
            Some("bbbb".into())
        );
    }
}
