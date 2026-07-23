//! Native Windows capture backend via WinDivert, in SNIFF mode.
//!
//! `SNIFF` yields a *copy* of each packet while the originals continue intact;
//! `RECV_ONLY` forbids reinjection. Capture is therefore strictly passive — the
//! game's traffic is never altered.

use std::fs;
use std::path::Path;

use tracing::warn;
use windivert::prelude::*;

use super::{PacketSource, Segment, parse_segment};
use crate::error::{Error, Result};

/// Signed kernel driver, embedded in the executable and extracted at runtime.
///
/// WinDivert loads the driver from the module directory
/// (`GetModuleFileName`) — the exe's folder, where `WinDivert.dll` also sits —
/// so the exe drops the `.sys` there itself on first run. Distribution is thus
/// exe + `WinDivert.dll`; the `.sys` rides inside the exe.
///
/// This `.sys` (WinDivert 2.2.2) must stay aligned with `WinDivert.dll` and the
/// user-mode bindings in `windivert-sys` (both under `vendor/windivert/`).
/// WinDivert only requires a matching *major* version (>= 2), so a minor drift
/// is tolerated, but a major bump would force replacing all three together.
const DRIVER_SYS: &[u8] = include_bytes!("../../vendor/windivert/WinDivert64.sys");
const DRIVER_FILE: &str = "WinDivert64.sys";

/// Largest packet WinDivert can deliver (`WINDIVERT_MTU_MAX`). Coalesced
/// receives (RSC/LSO) routinely exceed the wire MTU, so anything smaller as a
/// buffer makes `recv` fail on the first bulk transfer.
const MAX_PACKET_BYTES: usize = 65_575;

pub struct WinDivertSource {
    handle: WinDivert<NetworkLayer>,
    buffer: Vec<u8>,
    game_port: u16,
}

impl WinDivertSource {
    /// Opens a read-only network handle for `filter`. Requires administrator
    /// rights (driver load).
    pub fn open(filter: &str, game_port: u16, buffer_size: usize) -> Result<Self> {
        ensure_driver_present()?;

        let flags = WinDivertFlags::new().set_sniff().set_recv_only();
        let handle = WinDivert::network(filter, 0, flags)
            .map_err(|err| Error::Capture(format!("WinDivert open: {err}")))?;
        Ok(Self {
            handle,
            // Floor at the driver's own maximum: a smaller buffer turns the
            // first oversized packet into a recv error.
            buffer: vec![0u8; buffer_size.max(MAX_PACKET_BYTES)],
            game_port,
        })
    }
}

impl PacketSource for WinDivertSource {
    fn next_segment(&mut self) -> Result<Segment> {
        loop {
            let packet = match self.handle.recv(&mut self.buffer) {
                Ok(packet) => packet,
                // The driver already dropped this copy: skipping one packet
                // leaves a reassembly gap, while propagating would kill the
                // capture for the rest of the session.
                Err(WinDivertError::Recv(WinDivertRecvError::InsufficientBuffer)) => {
                    warn!("packet larger than the capture buffer — skipped");
                    continue;
                }
                Err(err) => return Err(Error::Capture(format!("recv: {err}"))),
            };

            if let Some(segment) = parse_segment(&packet.data[..], self.game_port) {
                return Ok(segment);
            }
        }
    }
}

/// Drops the driver next to the exe when it is missing or differs from the
/// embedded binary. Idempotent and safe alongside another running instance.
fn ensure_driver_present() -> Result<()> {
    let exe =
        std::env::current_exe().map_err(|err| Error::Capture(format!("executable path: {err}")))?;
    let dir = exe
        .parent()
        .ok_or_else(|| Error::Capture("executable directory not found".to_owned()))?;
    let target = dir.join(DRIVER_FILE);

    // Compare *content*, not just length, so a corrupt or differently-versioned
    // driver of the same size is replaced. Identical content is left untouched,
    // which also avoids writing a file locked by an already-running instance.
    if file_has_content(&target, DRIVER_SYS) {
        return Ok(());
    }

    // Atomic write (temp file then rename) so no one reads a half-written `.sys`
    // and concurrent first launches stay safe.
    match atomic_replace(dir, &target, DRIVER_SYS) {
        Ok(()) => Ok(()),
        // Replacement failed but a driver is already present: most likely locked
        // because loaded by a running instance. The driver service is then
        // already up and `WinDivertOpen` will reuse it — continue rather than
        // abort startup.
        Err(err) if target.exists() => {
            warn!(error = %err, path = %target.display(),
                "driver present but not replaceable (already loaded?) — reusing it");
            Ok(())
        }
        Err(err) => Err(Error::Capture(format!(
            "driver extraction ({}): {err} — place the exe in a writable directory",
            target.display()
        ))),
    }
}

/// True if `path` exists and holds exactly `expected`.
fn file_has_content(path: &Path, expected: &[u8]) -> bool {
    fs::read(path).is_ok_and(|content| content == expected)
}

/// Writes `bytes` to a temp file in the same directory, then renames it onto
/// `target` (atomic replace via `MoveFileEx` on Windows).
fn atomic_replace(dir: &Path, target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Per-process temp name so two simultaneous first launches don't clash.
    let tmp = dir.join(format!(".{DRIVER_FILE}.{}.tmp", std::process::id()));
    fs::write(&tmp, bytes)?;
    if let Err(err) = fs::rename(&tmp, target) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}
