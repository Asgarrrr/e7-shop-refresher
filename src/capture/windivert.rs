//! Backend de capture natif Windows via WinDivert, en mode SNIFF.
//!
//! `SNIFF` livre une **copie** des paquets pendant que les originaux poursuivent
//! leur route intacts ; `RECV_ONLY` interdit toute réinjection. La capture est
//! donc strictement passive : le trafic du jeu n'est jamais altéré.

use windivert::prelude::*;

use super::{parse_segment, PacketSource, Segment};
use crate::error::{Error, Result};

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
