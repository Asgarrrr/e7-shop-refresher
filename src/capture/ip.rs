//! Décodage d'un paquet IP brut (couche réseau WinDivert) en [`Segment`].

use std::net::{IpAddr, SocketAddr};

use etherparse::{NetSlice, SlicedPacket, TransportSlice};

use super::{Direction, FlowKey, Segment};

/// Extrait un segment TCP d'un paquet IP.
///
/// Renvoie `None` si le paquet n'est pas du TCP concernant `game_port`, ou s'il
/// est malformé. La direction est déduite du port du serveur de jeu.
pub fn parse_segment(bytes: &[u8], game_port: u16) -> Option<Segment> {
    let sliced = SlicedPacket::from_ip(bytes).ok()?;

    let (src_ip, dst_ip) = match sliced.net? {
        NetSlice::Ipv4(ip) => {
            let header = ip.header();
            (
                IpAddr::V4(header.source_addr()),
                IpAddr::V4(header.destination_addr()),
            )
        }
        NetSlice::Ipv6(ip) => {
            let header = ip.header();
            (
                IpAddr::V6(header.source_addr()),
                IpAddr::V6(header.destination_addr()),
            )
        }
        _ => return None,
    };

    let TransportSlice::Tcp(tcp) = sliced.transport? else {
        return None;
    };

    // Ignorer les ACK purs (sans données ni SYN/FIN) : ils n'apportent aucun
    // octet au flux et sont ~la moitié des paquets d'une connexion active. Le
    // SYN et le FIN sont conservés (baseline / fin de flux).
    if tcp.payload().is_empty() && !tcp.syn() && !tcp.fin() {
        return None;
    }

    let src = SocketAddr::new(src_ip, tcp.source_port());
    let dst = SocketAddr::new(dst_ip, tcp.destination_port());

    let (direction, flow) = if dst.port() == game_port {
        (
            Direction::ClientToServer,
            FlowKey {
                client: src,
                server: dst,
            },
        )
    } else if src.port() == game_port {
        (
            Direction::ServerToClient,
            FlowKey {
                client: dst,
                server: src,
            },
        )
    } else {
        return None;
    };

    Some(Segment {
        flow,
        direction,
        seq: tcp.sequence_number(),
        syn: tcp.syn(),
        fin: tcp.fin(),
        payload: tcp.payload().to_vec(),
    })
}
