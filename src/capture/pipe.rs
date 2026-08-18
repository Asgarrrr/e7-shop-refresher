//! Frame protocol between the elevated capture broker and this process, and
//! the [`PacketSource`] that reads it.
//!
//! # Trust boundary
//!
//! Everything in this file runs **unelevated**. The broker is the only part of
//! the product that holds administrator rights, and it deliberately parses
//! nothing: it copies raw IP packets out of the driver and pushes them through
//! a pipe so that the hostile bytes are decoded down here, where a parser bug
//! costs a medium-integrity process rather than an elevated one.
//!
//! The direction of trust is therefore *not* symmetric with the privilege
//! direction. The framing arriving from the broker is treated as untrusted
//! input — a length is never believed past [`MAX_PACKET_BYTES`], an unknown
//! kind is a hard error, and no allocation is sized from the wire before that
//! check. The packet payloads themselves get no such vetting here because they
//! are not meant to be sane: they go straight to [`parse_segment`], which is
//! written against hostile input and answers `None` to anything it dislikes.
//!
//! Nothing in this module touches Win32. That is what lets its tests run over a
//! `Cursor<Vec<u8>>` in the portable lanes, which is where the protocol's
//! edge cases are actually pinned down — the elevated path has no test at all,
//! by construction.

use std::io::{self, Read, Write};

use tracing::{debug, info};

use super::{Direction, MAX_PACKET_BYTES, PacketSource, Segment, parse_segment};
use crate::error::{Error, Result};

/// Payload is one raw IP packet, exactly as the driver delivered it.
pub const FRAME_KIND_PACKET: u8 = 0;
/// Payload is UTF-8 text the broker wants recorded. The broker installs no
/// tracing subscriber (and the shipped build has no console), so its counters
/// can only reach the single log file by travelling through here.
pub const FRAME_KIND_DIAGNOSTIC: u8 = 1;
/// Payload is UTF-8 text describing a failure the broker cannot survive. It
/// exists because the most likely startup failure — the driver refusing to
/// open — would otherwise die silently with the elevated process, leaving the
/// player a banner that names no cause.
pub const FRAME_KIND_FATAL: u8 = 2;

/// `flags` bit 0 on a packet frame: the broker lost captured packets between
/// the previous frame and this one, so the byte stream has a hole in it.
pub const FRAME_FLAG_CAPTURE_LOSS: u8 = 0b1;

/// `u32` length, `u8` kind, `u8` flags — all little-endian, matching every
/// target this ships on, so no byte swapping is ever needed on either side.
const FRAME_HEADER_BYTES: usize = 6;

/// One decoded frame, kept in the same shape [`write_frame`] takes so the two
/// halves stay visibly symmetric. Interpreting `kind` is the reader's job, not
/// the codec's.
#[derive(Debug, PartialEq, Eq)]
struct Frame {
    kind: u8,
    flags: u8,
    payload: Vec<u8>,
}

/// Serializes one frame onto `out`.
///
/// Rejects an over-cap payload rather than emitting a frame the reader is
/// required to refuse: the cap is a property of the protocol, so both ends
/// enforce it and a violation is caught in the process that caused it.
pub fn write_frame<W: Write>(out: &mut W, kind: u8, flags: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_PACKET_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "frame payload of {} bytes exceeds the {MAX_PACKET_BYTES}-byte cap",
                payload.len()
            ),
        ));
    }
    let mut header = [0u8; FRAME_HEADER_BYTES];
    // Lossless: the cap above is far below `u32::MAX`.
    header[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    header[4] = kind;
    header[5] = flags;
    // One write per part rather than one buffer per packet: the broker's pipe
    // handle is not buffered, but a packet frame is a single ~1.5 KiB payload
    // and the header write is six bytes, so this costs one extra syscall per
    // packet and saves a copy of every packet.
    out.write_all(&header)?;
    out.write_all(payload)
}

/// Reads one frame off `src`, blocking until it is complete.
///
/// Every malformed-framing case is fatal, never a skip. The peer is our own
/// binary speaking a protocol defined in this file; a disagreement means the
/// stream is corrupt or the process on the other end is not the one we
/// launched, and resynchronizing on a corrupt stream would feed garbage to the
/// reassembler for the rest of the session.
fn read_frame<R: Read>(src: &mut R) -> Result<Frame> {
    let mut header = [0u8; FRAME_HEADER_BYTES];
    read_exact_or_gone(src, &mut header)?;

    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let kind = header[4];
    let flags = header[5];

    // Checked before the allocation below, so a corrupt length can never make
    // this process reserve four gigabytes on a stranger's say-so.
    if len > MAX_PACKET_BYTES {
        return Err(Error::Capture(format!(
            "capture broker sent a {len}-byte frame, over the {MAX_PACKET_BYTES}-byte cap"
        )));
    }
    if !matches!(
        kind,
        FRAME_KIND_PACKET | FRAME_KIND_DIAGNOSTIC | FRAME_KIND_FATAL
    ) {
        return Err(Error::Capture(format!(
            "capture broker sent an unknown frame kind {kind}"
        )));
    }

    let mut payload = vec![0u8; len];
    read_exact_or_gone(src, &mut payload)?;
    Ok(Frame {
        kind,
        flags,
        payload,
    })
}

/// `read_exact`, with end-of-stream reported as the broker being gone.
///
/// This is how a dead broker is detected, and it is not obvious that it works:
/// on Windows the std maps `ERROR_BROKEN_PIPE` — what a handle read returns
/// once the writing end closes — onto `Ok(0)`, i.e. a normal end of stream.
/// `read_exact` turns that `Ok(0)` into `UnexpectedEof`. So the single arm
/// below covers a clean exit at a frame boundary, a broker killed mid-frame,
/// and a broker that crashed before writing anything; no separate
/// broken-pipe check is needed, and adding one would be dead code.
fn read_exact_or_gone<R: Read>(src: &mut R, buf: &mut [u8]) -> Result<()> {
    match src.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
            Err(Error::Capture("capture broker exited".into()))
        }
        Err(err) => Err(Error::Capture(format!("capture broker read: {err}"))),
    }
}

/// Decodes broker text without ever failing on it.
///
/// Lossy on purpose: these strings exist to explain a problem, and turning
/// "the broker's explanation had a bad byte in it" into a second, less useful
/// error would lose the only account of the first one.
fn frame_text(payload: &[u8]) -> String {
    String::from_utf8_lossy(payload).into_owned()
}

/// How many packet frames between two funnel lines. Every captured packet
/// passes through here, so the line is emitted periodically rather than per
/// packet — plus once on the very first one, so a capture that is about to
/// reject everything says so immediately instead of after five hundred packets.
const FUNNEL_LOG_EVERY: u64 = 500;

/// Where packets that reach this process go to die.
///
/// The parsing half of the funnel the WinDivert backend used to carry on its
/// own. It lives here now because this is where the parsing happens: the
/// elevated broker keeps the raw half (`delivered` / `oversized`, which it ships
/// down the pipe as diagnostic frames) and does not decode a single byte, so a
/// verdict like "the packets arrive but none of them parse" can only be reached
/// on this side.
///
/// Two things can drop a packet between the pipe and the reassembler — the
/// parser returning `None`, and a segment that parses but travels the wrong way
/// — and each is individually plausible as the reason a healthy-looking session
/// yields nothing. `packets` staying at zero is itself the headline result: the
/// broker is connected but the driver's filter matches no traffic.
#[derive(Default)]
struct Funnel {
    packets: u64,
    unparsed: u64,
    admitted: u64,
    server_to_client: u64,
}

impl Funnel {
    /// Emits the funnel on the first packet frame, then once per
    /// [`FUNNEL_LOG_EVERY`]. Called on both paths that finish a packet — the
    /// unparsed skip and the admitted return — so every line reports a settled
    /// verdict on the packet that triggered it.
    fn report(&self) {
        if self.packets != 1 && !self.packets.is_multiple_of(FUNNEL_LOG_EVERY) {
            return;
        }
        debug!(
            packets = self.packets,
            admitted = self.admitted,
            server_to_client = self.server_to_client,
            unparsed = self.unparsed,
            "capture funnel"
        );
    }
}

/// A [`PacketSource`] fed by the elevated broker over `R`.
///
/// Generic over the reader rather than owning a pipe handle: the transport is
/// a Windows named pipe in the product, and a `Cursor` in the tests, and this
/// module has no reason to know which.
pub struct PipeSource<R> {
    reader: R,
    game_port: u16,
    /// Set by any packet frame carrying [`FRAME_FLAG_CAPTURE_LOSS`], and left
    /// set until the capture loop asks for it. Frames are consumed faster than
    /// the loop polls, so this accumulates across however many frames elapse
    /// between two calls to [`PacketSource::take_capture_loss`].
    capture_loss: bool,
    funnel: Funnel,
}

impl<R: Read> PipeSource<R> {
    pub fn new(reader: R, game_port: u16) -> Self {
        Self {
            reader,
            game_port,
            capture_loss: false,
            funnel: Funnel::default(),
        }
    }
}

impl<R: Read + Send> PacketSource for PipeSource<R> {
    fn next_segment(&mut self) -> Result<Segment> {
        loop {
            let frame = read_frame(&mut self.reader)?;
            match frame.kind {
                FRAME_KIND_DIAGNOSTIC => {
                    // The broker's counters, landing in the UI's log file as if
                    // they had been emitted here — which for the reader of that
                    // file is the only place they could usefully appear.
                    debug!(broker = %frame_text(&frame.payload), "capture broker diagnostic");
                }
                FRAME_KIND_FATAL => {
                    return Err(Error::Capture(frame_text(&frame.payload)));
                }
                // `read_frame` has already refused every kind but these three,
                // so this arm is the packet one; spelling it `_` keeps the
                // match exhaustive over a `u8` without an unreachable panic.
                _ => {
                    if frame.flags & FRAME_FLAG_CAPTURE_LOSS != 0 {
                        self.capture_loss = true;
                    }
                    self.funnel.packets += 1;
                    // The broker's network layer delivers whole IP packets, so
                    // there is no link-layer framing to decode and nothing about
                    // the adapter (Ethernet, WiFi, VPN) reaches this parser.
                    //
                    // A packet that decodes to nothing of interest (wrong port,
                    // pure ACK, malformed) is skipped, not an error — the same
                    // semantics the WinDivert backend applied to the same bytes
                    // before this side existed.
                    let Some(segment) = parse_segment(&frame.payload, self.game_port) else {
                        self.funnel.unparsed += 1;
                        self.funnel.report();
                        continue;
                    };

                    self.funnel.admitted += 1;
                    if segment.direction == Direction::ServerToClient {
                        self.funnel.server_to_client += 1;
                        if self.funnel.server_to_client == 1 {
                            // The shop response travels in this direction only,
                            // so this line is the proof that the filter, the
                            // port, the driver and the pipe all agree. Its
                            // *absence* in a session log means capture is open
                            // but sees nothing from the game server.
                            info!(
                                payload = segment.payload.len(),
                                syn = segment.syn,
                                server = %segment.flow.server,
                                client = %segment.flow.client,
                                "first server-to-client segment admitted"
                            );
                        }
                    }
                    self.funnel.report();
                    return Ok(segment);
                }
            }
        }
    }

    fn take_capture_loss(&mut self) -> bool {
        std::mem::take(&mut self.capture_loss)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use etherparse::PacketBuilder;

    use super::*;
    use crate::capture::Direction;

    const GAME_PORT: u16 = 3333;

    /// A raw IPv4/TCP packet, the exact shape the broker forwards: IP layer
    /// down, no link-layer framing.
    fn ipv4_tcp(src_port: u16, dst_port: u16, seq: u32, payload: &[u8]) -> Vec<u8> {
        let b = PacketBuilder::ipv4([104, 116, 20, 111], [192, 168, 1, 10], 64)
            .tcp(src_port, dst_port, seq, 64_240);
        let mut out = Vec::with_capacity(b.size(payload.len()));
        b.write(&mut out, payload).expect("write packet");
        out
    }

    /// A reader over the concatenation of `frames`, each `(kind, flags, payload)`.
    fn stream_of(frames: &[(u8, u8, &[u8])]) -> Cursor<Vec<u8>> {
        let mut bytes = Vec::new();
        for (kind, flags, payload) in frames {
            write_frame(&mut bytes, *kind, *flags, payload).expect("write frame");
        }
        Cursor::new(bytes)
    }

    fn source(frames: &[(u8, u8, &[u8])]) -> PipeSource<Cursor<Vec<u8>>> {
        PipeSource::new(stream_of(frames), GAME_PORT)
    }

    #[test]
    fn every_frame_kind_survives_an_encode_decode_round_trip() {
        for (kind, flags, payload) in [
            (FRAME_KIND_PACKET, 0, b"raw ip bytes".as_slice()),
            (
                FRAME_KIND_PACKET,
                FRAME_FLAG_CAPTURE_LOSS,
                b"raw ip bytes".as_slice(),
            ),
            (FRAME_KIND_DIAGNOSTIC, 0, b"delivered=500".as_slice()),
            (FRAME_KIND_FATAL, 0, b"WinDivert open: denied".as_slice()),
        ] {
            let mut bytes = Vec::new();
            write_frame(&mut bytes, kind, flags, payload).expect("write frame");
            assert_eq!(bytes.len(), FRAME_HEADER_BYTES + payload.len());

            let frame = read_frame(&mut Cursor::new(bytes)).expect("read frame");
            assert_eq!(
                frame,
                Frame {
                    kind,
                    flags,
                    payload: payload.to_vec(),
                }
            );
        }
    }

    #[test]
    fn an_empty_payload_round_trips_as_a_header_only_frame() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, FRAME_KIND_PACKET, 0, b"").expect("write frame");
        assert_eq!(bytes.len(), FRAME_HEADER_BYTES);
        let frame = read_frame(&mut Cursor::new(bytes)).expect("read frame");
        assert!(frame.payload.is_empty());
    }

    #[test]
    fn a_packet_frame_yields_the_segment_its_bytes_decode_to() {
        let packet = ipv4_tcp(GAME_PORT, 51_000, 1000, b"AB");
        let mut src = source(&[(FRAME_KIND_PACKET, 0, &packet)]);

        let segment = src.next_segment().expect("segment");
        assert_eq!(segment.direction, Direction::ServerToClient);
        assert_eq!(segment.seq, 1000);
        assert_eq!(segment.payload, b"AB");
        assert!(!src.take_capture_loss());
    }

    #[test]
    fn a_packet_that_decodes_to_nothing_is_skipped_rather_than_reported() {
        // Neither endpoint owns the game port: `parse_segment` answers `None`,
        // and the loop must move on to the next frame instead of erroring.
        let ignored = ipv4_tcp(4444, 51_000, 1, b"AB");
        let wanted = ipv4_tcp(GAME_PORT, 51_000, 2000, b"CD");
        let mut src = source(&[
            (FRAME_KIND_PACKET, 0, &ignored),
            (FRAME_KIND_PACKET, 0, &wanted),
        ]);

        assert_eq!(src.next_segment().expect("segment").seq, 2000);
    }

    #[test]
    fn a_diagnostic_frame_produces_no_segment_and_does_not_disturb_the_next_packet() {
        let packet = ipv4_tcp(GAME_PORT, 51_000, 3000, b"EF");
        let mut src = source(&[
            (FRAME_KIND_DIAGNOSTIC, 0, b"delivered=500 oversized=0"),
            (FRAME_KIND_PACKET, 0, &packet),
        ]);

        let segment = src.next_segment().expect("segment");
        assert_eq!(segment.seq, 3000);
        assert_eq!(segment.payload, b"EF");
    }

    #[test]
    fn a_fatal_frame_surfaces_its_text_as_the_capture_error() {
        let mut src = source(&[(FRAME_KIND_FATAL, 0, b"WinDivert open: access denied")]);

        let err = src.next_segment().expect_err("fatal frame must fail");
        assert!(
            matches!(&err, Error::Capture(text) if text == "WinDivert open: access denied"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_fatal_frame_wins_over_packets_queued_behind_it() {
        // The broker writes its cause and exits; whatever it managed to send
        // first must not delay the reason reaching the player's banner.
        let packet = ipv4_tcp(GAME_PORT, 51_000, 4000, b"GH");
        let mut src = source(&[
            (FRAME_KIND_FATAL, 0, b"broker gave up"),
            (FRAME_KIND_PACKET, 0, &packet),
        ]);

        assert!(src.next_segment().is_err());
    }

    #[test]
    fn a_length_above_the_packet_cap_is_a_capture_error_rather_than_a_skip() {
        // Hand-rolled: `write_frame` refuses to produce this frame at all.
        let mut bytes = ((MAX_PACKET_BYTES + 1) as u32).to_le_bytes().to_vec();
        bytes.push(FRAME_KIND_PACKET);
        bytes.push(0);
        let mut src = PipeSource::new(Cursor::new(bytes), GAME_PORT);

        let err = src.next_segment().expect_err("over-cap length must fail");
        assert!(
            matches!(&err, Error::Capture(text) if text.contains("over the")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn write_frame_refuses_a_payload_above_the_packet_cap() {
        let oversized = vec![0u8; MAX_PACKET_BYTES + 1];
        let err = write_frame(&mut Vec::new(), FRAME_KIND_PACKET, 0, &oversized)
            .expect_err("over-cap payload must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn an_unknown_frame_kind_is_a_capture_error_rather_than_a_skip() {
        let mut bytes = 0u32.to_le_bytes().to_vec();
        bytes.push(9); // no such kind
        bytes.push(0);
        let mut src = PipeSource::new(Cursor::new(bytes), GAME_PORT);

        let err = src.next_segment().expect_err("unknown kind must fail");
        assert!(
            matches!(&err, Error::Capture(text) if text.contains("unknown frame kind 9")),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_clean_end_of_stream_is_reported_as_the_broker_having_exited() {
        let mut src = PipeSource::new(Cursor::new(Vec::new()), GAME_PORT);

        let err = src.next_segment().expect_err("end of stream must fail");
        assert!(
            matches!(&err, Error::Capture(text) if text == "capture broker exited"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_truncated_header_is_reported_as_the_broker_having_exited() {
        let mut src = PipeSource::new(Cursor::new(vec![0u8, 0, 0]), GAME_PORT);

        let err = src.next_segment().expect_err("partial header must fail");
        assert!(
            matches!(&err, Error::Capture(text) if text == "capture broker exited"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_payload_cut_short_is_reported_as_the_broker_having_exited() {
        let packet = ipv4_tcp(GAME_PORT, 51_000, 5000, b"IJ");
        let mut bytes = stream_of(&[(FRAME_KIND_PACKET, 0, &packet)]).into_inner();
        bytes.truncate(bytes.len() - 1);
        let mut src = PipeSource::new(Cursor::new(bytes), GAME_PORT);

        let err = src.next_segment().expect_err("truncated payload must fail");
        assert!(
            matches!(&err, Error::Capture(text) if text == "capture broker exited"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn the_capture_loss_flag_is_reported_once_and_then_cleared() {
        let packet = ipv4_tcp(GAME_PORT, 51_000, 6000, b"KL");
        let next = ipv4_tcp(GAME_PORT, 51_000, 7000, b"MN");
        let mut src = source(&[
            (FRAME_KIND_PACKET, FRAME_FLAG_CAPTURE_LOSS, &packet),
            (FRAME_KIND_PACKET, 0, &next),
        ]);

        src.next_segment().expect("segment");
        assert!(src.take_capture_loss(), "the loss bit must be reported");
        assert!(!src.take_capture_loss(), "and cleared by that report");

        src.next_segment().expect("segment");
        assert!(
            !src.take_capture_loss(),
            "a later loss-free frame must not resurrect it"
        );
    }

    #[test]
    fn capture_loss_seen_on_a_skipped_packet_still_reaches_the_next_report() {
        // The flag describes the byte stream, not the frame carrying it, so a
        // frame dropped by `parse_segment` must not take the loss bit with it.
        let ignored = ipv4_tcp(4444, 51_000, 1, b"AB");
        let wanted = ipv4_tcp(GAME_PORT, 51_000, 8000, b"OP");
        let mut src = source(&[
            (FRAME_KIND_PACKET, FRAME_FLAG_CAPTURE_LOSS, &ignored),
            (FRAME_KIND_PACKET, 0, &wanted),
        ]);

        src.next_segment().expect("segment");
        assert!(src.take_capture_loss());
    }
}
