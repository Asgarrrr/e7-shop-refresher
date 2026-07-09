//! Backend de capture natif Windows via WinDivert, en mode SNIFF.
//!
//! `SNIFF` livre une **copie** des paquets pendant que les originaux poursuivent
//! leur route intacts ; `RECV_ONLY` interdit toute réinjection. La capture est
//! donc strictement passive : le trafic du jeu n'est jamais altéré.

use std::fs;
use std::path::Path;

use tracing::warn;
use windivert::prelude::*;

use super::{parse_segment, PacketSource, Segment};
use crate::error::{Error, Result};

/// Driver noyau signé, embarqué dans l'exécutable et extrait au runtime.
///
/// En lien statique, WinDivert charge le driver depuis le dossier de l'exe
/// (`GetModuleFileName(NULL)`) : on distribue ainsi un exe unique qui dépose
/// lui-même le `.sys` au premier run.
///
/// La version de ce `.sys` (WinDivert 2.2.2) doit rester alignée avec celle
/// des sources user-mode compilées par `windivert-sys`. WinDivert n'exige
/// qu'une compatibilité de version *majeure* (≥ 2), donc un drift mineur est
/// toléré, mais un saut de majeure imposerait de remplacer ce fichier.
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

/// Dépose le driver à côté de l'exe s'il est absent ou différent du binaire
/// embarqué. Idempotent et sûr en présence d'une autre instance.
fn ensure_driver_present() -> Result<()> {
    let exe = std::env::current_exe()
        .map_err(|err| Error::Capture(format!("chemin de l'exécutable : {err}")))?;
    let dir = exe
        .parent()
        .ok_or_else(|| Error::Capture("dossier de l'exécutable introuvable".to_owned()))?;
    let target = dir.join(DRIVER_FILE);

    // Comparaison du *contenu* (pas seulement de la taille) : un driver corrompu
    // ou d'une autre version de même taille est bien détecté et remplacé. Si le
    // contenu est identique, on ne réécrit pas — ce qui évite aussi de toucher
    // un fichier verrouillé car déjà chargé par une autre instance.
    if file_has_content(&target, DRIVER_SYS) {
        return Ok(());
    }

    // Écriture atomique (fichier temporaire puis renommage) : évite qu'une autre
    // instance ou WinDivert ne lise un `.sys` à moitié écrit, et rend le premier
    // lancement concurrent sûr.
    match atomic_replace(dir, &target, DRIVER_SYS) {
        Ok(()) => Ok(()),
        // Le remplacement a échoué mais un driver est déjà en place : il est
        // très probablement verrouillé parce que chargé par une instance en
        // cours. Le service driver tourne alors déjà et `WinDivertOpen` le
        // réutilisera — on continue plutôt que d'abandonner le démarrage.
        Err(err) if target.exists() => {
            warn!(error = %err, path = %target.display(),
                "driver en place non remplaçable (déjà chargé ?) — réutilisation");
            Ok(())
        }
        Err(err) => Err(Error::Capture(format!(
            "extraction du driver ({}) : {err} — placez l'exe dans un dossier accessible en écriture",
            target.display()
        ))),
    }
}

/// Vrai si `path` existe et contient exactement `expected`.
fn file_has_content(path: &Path, expected: &[u8]) -> bool {
    fs::read(path).is_ok_and(|content| content == expected)
}

/// Écrit `bytes` dans un fichier temporaire du même dossier puis le renomme sur
/// `target` (remplacement atomique via `MoveFileEx` côté Windows).
fn atomic_replace(dir: &Path, target: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Nom temporaire unique au process : deux premiers lancements simultanés
    // n'écrivent pas dans le même fichier intermédiaire.
    let tmp = dir.join(format!(".{DRIVER_FILE}.{}.tmp", std::process::id()));
    fs::write(&tmp, bytes)?;
    if let Err(err) = fs::rename(&tmp, target) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}
