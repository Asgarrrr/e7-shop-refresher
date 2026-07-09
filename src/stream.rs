//! Réassemblage TCP.
//!
//! WinDivert opère sous TCP : on reçoit des segments potentiellement désordonnés,
//! dupliqués (retransmissions) ou chevauchants. Cette couche reconstitue, par
//! demi-flux, le flux d'octets ordonné exactement tel que la stack TCP le
//! livrerait — c'est ce flux que le serveur d'analyse s'attend à recevoir.
//!
//! Hypothèse : l'espace des numéros de séquence ne boucle pas (`wrap`) au sein
//! d'une session de capture, ce qui tient tant que le flux reste < 4 Gio.

use std::collections::{BTreeMap, HashMap};

use crate::capture::{Direction, FlowKey, Segment};

/// Plafond d'octets hors-ordre bufferisés par demi-flux (garde-fou mémoire).
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;

/// Réassemble le trafic de plusieurs connexions, indexé par (flux, direction).
#[derive(Default)]
pub struct Reassembler {
    halves: HashMap<(FlowKey, Direction), HalfStream>,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intègre un segment et renvoie les octets nouvellement contigus (ordonnés).
    ///
    /// Renvoie un vecteur vide quand le segment est un doublon, comble un trou
    /// partiel, ou attend encore un segment manquant.
    pub fn push(&mut self, segment: &Segment) -> Vec<u8> {
        let key = (segment.flow, segment.direction);
        let half = self.halves.entry(key).or_default();
        let out = half.push(segment.seq, segment.syn, &segment.payload);
        if segment.fin {
            self.halves.remove(&key);
        }
        out
    }

    /// Oublie l'état d'une connexion (déconnexion observée en amont).
    pub fn forget(&mut self, flow: &FlowKey) {
        self.halves.retain(|(f, _), _| f != flow);
    }
}

/// État de réassemblage d'un sens d'une connexion.
#[derive(Default)]
struct HalfStream {
    /// Numéro de séquence du prochain octet attendu (`None` avant la 1re obs.).
    next_seq: Option<u32>,
    /// Segments futurs bufferisés, clés par numéro de séquence.
    pending: BTreeMap<u32, Vec<u8>>,
    pending_bytes: usize,
}

impl HalfStream {
    fn push(&mut self, seq: u32, syn: bool, payload: &[u8]) -> Vec<u8> {
        // Le SYN consomme un numéro de séquence : les données démarrent à seq+1.
        let data_seq = if syn { seq.wrapping_add(1) } else { seq };

        // Première observation : on adopte ce point comme origine du flux.
        if self.next_seq.is_none() {
            self.next_seq = Some(data_seq);
        }

        let mut out = Vec::new();
        self.absorb(data_seq, payload, &mut out);
        self.drain(&mut out);
        out
    }

    /// Intègre un segment isolé : en ordre (append), futur (buffer), ancien (trim).
    fn absorb(&mut self, seq: u32, payload: &[u8], out: &mut Vec<u8>) {
        if payload.is_empty() {
            return;
        }
        let next = self.next_seq.expect("next_seq initialisé par push");
        let offset = seq_diff(seq, next);

        if offset > 0 {
            self.buffer_future(seq, payload);
            return;
        }

        // offset <= 0 : le segment commence à `next` ou avant.
        let already = (-offset) as usize;
        if already < payload.len() {
            let fresh = &payload[already..];
            out.extend_from_slice(fresh);
            self.next_seq = Some(next.wrapping_add(fresh.len() as u32));
        }
        // sinon : entièrement déjà livré (retransmission) → ignoré.
    }

    fn buffer_future(&mut self, seq: u32, payload: &[u8]) {
        // Ne conserver que le plus grand segment vu à un seq donné.
        if self.pending.get(&seq).is_none_or(|v| v.len() < payload.len()) {
            if let Some(old) = self.pending.insert(seq, payload.to_vec()) {
                self.pending_bytes -= old.len();
            }
            self.pending_bytes += payload.len();
        }
        self.evict_if_over_budget();
    }

    /// Écoule les segments bufferisés devenus contigus après avancée de `next_seq`.
    fn drain(&mut self, out: &mut Vec<u8>) {
        while let Some((&seq, _)) = self.pending.iter().next() {
            let next = self.next_seq.expect("next_seq initialisé");
            if seq_diff(seq, next) > 0 {
                break; // trou toujours présent.
            }
            let payload = self.pending.remove(&seq).unwrap();
            self.pending_bytes -= payload.len();
            self.absorb(seq, &payload, out);
        }
    }

    fn evict_if_over_budget(&mut self) {
        // Sous pression mémoire, abandonner les segments futurs les plus lointains.
        while self.pending_bytes > MAX_PENDING_BYTES {
            let Some((&seq, _)) = self.pending.iter().next_back() else {
                break;
            };
            let removed = self.pending.remove(&seq).unwrap();
            self.pending_bytes -= removed.len();
        }
    }
}

/// Distance signée `a - b` sur l'espace circulaire des numéros de séquence.
fn seq_diff(a: u32, b: u32) -> i64 {
    (a.wrapping_sub(b) as i32) as i64
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};

    use super::*;

    fn flow() -> FlowKey {
        FlowKey {
            client: SocketAddr::from((Ipv4Addr::new(192, 168, 1, 10), 51000)),
            server: SocketAddr::from((Ipv4Addr::new(104, 116, 20, 111), 3333)),
        }
    }

    fn seg(seq: u32, syn: bool, fin: bool, payload: &[u8]) -> Segment {
        Segment {
            flow: flow(),
            direction: Direction::ServerToClient,
            seq,
            syn,
            fin,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn in_order() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, false, b"AB")), b"AB");
        assert_eq!(r.push(&seg(1002, false, false, b"CD")), b"CD");
    }

    #[test]
    fn reordering_flushes_multiple_buffered_segments() {
        let mut r = Reassembler::new();
        // La baseline est fixée par le premier segment observé.
        assert_eq!(r.push(&seg(1000, false, false, b"AB")), b"AB");
        // Deux segments futurs arrivent dans le désordre : rien de livrable.
        assert!(r.push(&seg(1006, false, false, b"GH")).is_empty());
        assert!(r.push(&seg(1004, false, false, b"EF")).is_empty());
        // Le trou comblé écoule tout ce qui était en attente, dans l'ordre.
        assert_eq!(r.push(&seg(1002, false, false, b"CD")), b"CDEFGH");
    }

    #[test]
    fn retransmission_is_ignored() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, false, b"AB")), b"AB");
        assert!(r.push(&seg(1000, false, false, b"AB")).is_empty());
    }

    #[test]
    fn overlapping_segment_keeps_only_fresh_tail() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, false, b"ABCD")), b"ABCD");
        // Chevauche "CD" (déjà vus) et apporte "EF".
        assert_eq!(r.push(&seg(1002, false, false, b"CDEF")), b"EF");
    }

    #[test]
    fn syn_sets_the_baseline() {
        let mut r = Reassembler::new();
        // Le SYN (seq 999, sans données) fixe l'origine à 1000.
        assert!(r.push(&seg(999, true, false, b"")).is_empty());
        assert_eq!(r.push(&seg(1000, false, false, b"AB")), b"AB");
    }

    #[test]
    fn gap_filled_out_of_order() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, false, b"AB")), b"AB");
        assert!(r.push(&seg(1004, false, false, b"EF")).is_empty()); // trou.
        assert_eq!(r.push(&seg(1002, false, false, b"CD")), b"CDEF");
    }

    #[test]
    fn fin_clears_flow_state() {
        let mut r = Reassembler::new();
        r.push(&seg(1000, false, true, b"AB"));
        assert!(r.halves.is_empty());
    }
}
