//! Réassemblage TCP.
//!
//! WinDivert opère sous TCP : on reçoit des segments potentiellement désordonnés,
//! dupliqués (retransmissions) ou chevauchants. Cette couche reconstitue, par
//! demi-flux, le flux d'octets ordonné exactement tel que la stack TCP le
//! livrerait — c'est ce flux que le serveur d'analyse s'attend à recevoir.
//!
//! Tout le travail se fait en **offsets relatifs** à l'origine du flux (le
//! premier segment observé). Les numéros de séquence TCP sont des `u32` qui
//! bouclent (une connexion dont l'ISN est proche de `2^32` boucle après quelques
//! centaines d'octets) ; raisonner en offsets `i64` calculés par `seq_diff`
//! élimine ce piège, l'ordre et la comparaison devenant monotones.

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
    /// partiel, ou attend encore un segment manquant. Le flux n'est jamais
    /// démonté sur FIN : un FIN réordonné arrivant avant un segment qui comble
    /// un trou ne doit pas jeter les données déjà bufferisées.
    pub fn push(&mut self, segment: &Segment) -> Vec<u8> {
        let half = self.halves.entry((segment.flow, segment.direction)).or_default();
        half.push(segment.seq, segment.syn, &segment.payload)
    }

    /// Réinitialise tout l'état : le prochain segment de chaque flux refixe une
    /// nouvelle origine. Utilisé après une pause Shop Watch pour repartir d'un
    /// point de resynchronisation propre plutôt que d'un `next_seq` périmé.
    pub fn clear(&mut self) {
        self.halves.clear();
    }
}

/// État de réassemblage d'un sens d'une connexion, en offsets relatifs.
#[derive(Default)]
struct HalfStream {
    /// Origine du flux (numéro de séquence du 1er octet), `None` avant la 1re obs.
    baseline: Option<u32>,
    /// Offset (depuis `baseline`) du prochain octet attendu.
    next_off: i64,
    /// Segments futurs bufferisés, clés par offset (ordre monotone, sans wrap).
    pending: BTreeMap<i64, Vec<u8>>,
    pending_bytes: usize,
}

impl HalfStream {
    fn push(&mut self, seq: u32, syn: bool, payload: &[u8]) -> Vec<u8> {
        // Le SYN consomme un numéro de séquence : les données démarrent à seq+1.
        let data_seq = if syn { seq.wrapping_add(1) } else { seq };
        // Première observation : on adopte ce point comme origine du flux.
        let baseline = *self.baseline.get_or_insert(data_seq);
        let offset = seq_diff(data_seq, baseline);

        let mut out = Vec::new();
        self.absorb(offset, payload, &mut out);
        self.drain(&mut out);
        out
    }

    /// Intègre un segment isolé : en ordre (append), futur (buffer), ancien (trim).
    fn absorb(&mut self, offset: i64, payload: &[u8], out: &mut Vec<u8>) {
        if payload.is_empty() {
            return;
        }
        if offset > self.next_off {
            self.buffer_future(offset, payload);
            return;
        }

        // offset <= next_off : le segment commence à/avant l'octet attendu.
        let already = (self.next_off - offset) as usize;
        if already < payload.len() {
            out.extend_from_slice(&payload[already..]);
            self.next_off += (payload.len() - already) as i64;
        }
        // sinon : entièrement déjà livré (retransmission) → ignoré.
    }

    fn buffer_future(&mut self, offset: i64, payload: &[u8]) {
        // Ne conserver que le plus grand segment vu à un offset donné.
        if self.pending.get(&offset).is_none_or(|v| v.len() < payload.len()) {
            if let Some(old) = self.pending.insert(offset, payload.to_vec()) {
                self.pending_bytes -= old.len();
            }
            self.pending_bytes += payload.len();
        }
        self.relieve_pressure();
    }

    /// Écoule les segments bufferisés devenus contigus après avancée de `next_off`.
    fn drain(&mut self, out: &mut Vec<u8>) {
        while let Some((&offset, _)) = self.pending.iter().next() {
            if offset > self.next_off {
                break; // trou toujours présent.
            }
            let payload = self.pending.remove(&offset).unwrap();
            self.pending_bytes -= payload.len();
            self.absorb(offset, &payload, out);
        }
    }

    /// Sous pression mémoire, on **abandonne le trou courant** : `next_off` saute
    /// jusqu'au plus proche segment en attente, qui devient alors livrable (le
    /// `drain` suivant l'écoule). Un octet manquant capté hors-ordre par un tap
    /// passif ne sera jamais retransmis — mieux vaut une discontinuité que le
    /// serveur resynchronise qu'un flux figé à jamais.
    fn relieve_pressure(&mut self) {
        if self.pending_bytes <= MAX_PENDING_BYTES {
            return;
        }
        if let Some((&offset, _)) = self.pending.iter().next() {
            if offset > self.next_off {
                self.next_off = offset;
            }
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
    fn fin_does_not_discard_buffered_data() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, false, b"AB")), b"AB");
        // Un FIN réordonné, en avance sur un trou, ne doit pas jeter son payload.
        assert!(r.push(&seg(1004, false, true, b"EF")).is_empty());
        // Le segment qui comble le trou écoule aussi les données du FIN.
        assert_eq!(r.push(&seg(1002, false, false, b"CD")), b"CDEF");
    }

    #[test]
    fn reassembles_across_sequence_wrap() {
        let mut r = Reassembler::new();
        // Baseline juste avant le rebouclage de l'espace de séquence u32.
        assert_eq!(r.push(&seg(0xFFFF_FFFE, false, false, b"AB")), b"AB");
        // Le segment suivant est à 0x0000_0000 (wrap) : il reste contigu.
        assert_eq!(r.push(&seg(0x0000_0000, false, false, b"CD")), b"CD");
    }

    #[test]
    fn reordering_across_wrap_is_ordered_correctly() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(0xFFFF_FFFE, false, false, b"AB")), b"AB");
        // Segment futur post-wrap bufferisé, puis comblement du trou.
        assert!(r.push(&seg(0x0000_0002, false, false, b"EF")).is_empty());
        assert_eq!(r.push(&seg(0x0000_0000, false, false, b"CD")), b"CDEF");
    }

    #[test]
    fn clear_resets_baseline_for_resync() {
        let mut r = Reassembler::new();
        assert_eq!(r.push(&seg(1000, false, false, b"AB")), b"AB");
        // Après une pause, l'état est vidé : un segment très en avant redevient
        // une nouvelle origine au lieu d'être bufferisé indéfiniment.
        r.clear();
        assert_eq!(r.push(&seg(9000, false, false, b"XY")), b"XY");
    }
}
