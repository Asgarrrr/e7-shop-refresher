//! Backend de capture natif Windows via WinDivert, en mode SNIFF.
//!
//! `SNIFF` livre une **copie** des paquets pendant que les originaux poursuivent
//! leur route intacts ; `RECV_ONLY` interdit toute réinjection. La capture est
//! donc strictement passive : le trafic du jeu n'est jamais altéré.

use std::fs;
use std::path::PathBuf;

use windivert::prelude::*;

use super::{parse_segment, PacketSource, Segment};
use crate::error::{Error, Result};

/// Driver noyau signé, embarqué dans l'exécutable et extrait au runtime.
///
/// En lien statique, WinDivert charge le driver depuis le dossier de l'exe : on
/// distribue ainsi un exe unique qui dépose lui-même le `.sys` au premier run.
const DRIVER_SYS: &[u8] = include_bytes!("../../vendor/windivert/WinDivert64.sys");
const DRIVER_FILE: &str = "WinDivert64.sys";

pub struct WinDivertSource {
    handle: WinDivert<NetworkLayer>,
    buffer: Vec<u8>,
    game_port: u16,
}

impl WinDivertSource {
    /// Ouvre une poignée réseau en lecture seule pour `filter`.
    ///
    /// Nécessite les droits administrateur (chargement du driver).
    pub fn open(filter: &str, game_port: u16, buffer_size: usize) -> Result<Self> {
        ensure_driver_present()?;

        let flags = WinDivertFlags::new().set_sniff().set_recv_only();
        let handle = WinDivert::network(filter, 0, flags)
            .map_err(|err| Error::Capture(format!("ouverture WinDivert : {err}")))?;
        Ok(Self {
            handle,
            buffer: vec![0u8; buffer_size.max(1_500)],
            game_port,
        })
    }
}

impl PacketSource for WinDivertSource {
    fn next_segment(&mut self) -> Result<Segment> {
        loop {
            let packet = self
                .handle
                .recv(&mut self.buffer)
                .map_err(|err| Error::Capture(format!("réception : {err}")))?;

            if let Some(segment) = parse_segment(&packet.data[..], self.game_port) {
                return Ok(segment);
            }
        }
    }
}

/// Écrit le driver à côté de l'exe s'il est absent ou de taille différente.
fn ensure_driver_present() -> Result<()> {
    let exe = std::env::current_exe()
        .map_err(|err| Error::Capture(format!("chemin de l'exécutable : {err}")))?;
    let dir = exe
        .parent()
        .ok_or_else(|| Error::Capture("dossier de l'exécutable introuvable".to_owned()))?;
    let target: PathBuf = dir.join(DRIVER_FILE);

    // Si un driver de la bonne taille est déjà là (possiblement verrouillé car
    // chargé par une autre instance), on n'y touche pas.
    let up_to_date = fs::metadata(&target)
        .map(|meta| meta.len() == DRIVER_SYS.len() as u64)
        .unwrap_or(false);
    if up_to_date {
        return Ok(());
    }

    fs::write(&target, DRIVER_SYS)
        .map_err(|err| Error::Capture(format!("extraction du driver ({}) : {err}", target.display())))
}
