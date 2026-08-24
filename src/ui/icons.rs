//! Gear-set icons: the server's base64 PNGs turned into egui textures.
//!
//! They ride the `{type:"catalog"}` message rather than being fetched from the
//! game's CDN. This relay talks to exactly one host, and a second endpoint would
//! be a second failure mode and a second set of TLS roots for a 2 KB picture —
//! the whole set is 22 pieces of 44x44, some 53 KB, sent once per connection.
//!
//! **Every failure in this module is silent by design.** A set with no texture
//! draws as a text chip, which is what the entire vocabulary already falls back
//! to when the server has no Catalog to read: a picture is a convenience, never
//! an authority, so a truncated blob, an unreadable PNG or a mistyped field
//! costs one icon and reports nothing to the player. That is also why the two
//! decoders answer `Option` rather than a typed error — there is no caller who
//! would do anything different with the reason.

use std::collections::HashMap;

use eframe::egui;

/// Textures for the gear sets the server pushed, keyed by the same set id
/// [`crate::domain::filter::Filter`] matches on.
#[derive(Default)]
pub(super) struct SetIcons {
    textures: HashMap<String, egui::TextureHandle>,
}

impl SetIcons {
    /// Replaces every texture wholesale, exactly as
    /// [`crate::uplink::vocabulary::VocabularyCell::set`] replaces the lists
    /// they belong to: a set the game dropped must not keep a picture nothing
    /// offers any more. An undecodable entry is skipped and the rest land.
    pub(super) fn load(&mut self, ctx: &egui::Context, icons: &HashMap<String, String>) {
        self.textures = icons
            .iter()
            .filter_map(|(id, encoded)| {
                let image = decode_base64_png(encoded)?;
                // Named per set so a texture inspector and a leak both point at
                // the id rather than at "image 47".
                let texture = ctx.load_texture(
                    format!("set-icon:{id}"),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                Some((id.clone(), texture))
            })
            .collect();
    }

    /// The texture for a set id, or `None` — which is the draw-a-text-chip case.
    pub(super) fn get(&self, id: &str) -> Option<&egui::TextureHandle> {
        self.textures.get(id)
    }
}

/// One wire icon: base64 in, an image ready for [`egui::Context::load_texture`]
/// out.
fn decode_base64_png(encoded: &str) -> Option<egui::ColorImage> {
    decode_png(&decode_base64(encoded)?)
}

/// Standard base64 (RFC 4648 §4) with required padding.
///
/// Hand-written rather than pulled from a crate: a closed 64-character alphabet
/// and a 4-into-3 regroup is the shape this project's dependency rule asks to be
/// written, and this repo counts its build time in crates.
///
/// The input crossed a network boundary, so every departure from the grammar is
/// refused outright instead of yielding a prefix of plausible bytes: a symbol
/// outside the alphabet, a length that is not a multiple of four, and `=`
/// anywhere but as the last one or two symbols of the last block.
fn decode_base64(encoded: &str) -> Option<Vec<u8>> {
    let symbols = encoded.as_bytes();
    if !symbols.len().is_multiple_of(4) {
        return None;
    }
    let blocks = symbols.len() / 4;
    let mut out = Vec::with_capacity(blocks * 3);
    for (index, block) in symbols.chunks_exact(4).enumerate() {
        // Only the final block may be padded, so an interior `=` falls through
        // to `sextet` below and is refused as the stray symbol it is.
        let padding = if index + 1 == blocks {
            block.iter().rev().take_while(|s| **s == b'=').count()
        } else {
            0
        };
        if padding > 2 {
            return None;
        }
        let mut packed = 0u32;
        for (position, symbol) in block.iter().enumerate() {
            let value = if position >= 4 - padding {
                0
            } else {
                sextet(*symbol)?
            };
            packed = (packed << 6) | value;
        }
        // 24 bits in the low three bytes; the padding is how many of them the
        // block never carried.
        out.extend_from_slice(&packed.to_be_bytes()[1..4 - padding]);
    }
    Some(out)
}

/// The alphabet as a lookup, `None` for everything outside it — `=` included,
/// which is what makes a misplaced pad a refusal.
fn sextet(symbol: u8) -> Option<u32> {
    let value = match symbol {
        b'A'..=b'Z' => symbol - b'A',
        b'a'..=b'z' => symbol - b'a' + 26,
        b'0'..=b'9' => symbol - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => return None,
    };
    Some(u32::from(value))
}

/// Decodes a PNG into the 8-bit RGBA egui wants.
///
/// The transformations are what make the output shape knowable: the icons are
/// RGBA today, but a palette or a grayscale one would otherwise arrive with a
/// different bytes-per-pixel and be read as garbage. `EXPAND` unpacks a palette
/// and a sub-byte grayscale, `ALPHA` adds the channel, `STRIP_16` folds a 16-bit
/// image down — and the color type is then *checked* rather than assumed,
/// because those three cover the PNG color types this app has reason to expect
/// and not the whole format.
fn decode_png(bytes: &[u8]) -> Option<egui::ColorImage> {
    // `Cursor`, because 0.18's `Decoder` wants `BufRead + Seek` and a `&[u8]` is
    // only the first of those.
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16,
    );
    let mut reader = decoder.read_info().ok()?;
    if reader.output_color_type() != (png::ColorType::Rgba, png::BitDepth::Eight) {
        return None;
    }
    let mut buffer = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buffer).ok()?;
    let size = [
        usize::try_from(info.width).ok()?,
        usize::try_from(info.height).ok()?,
    ];
    let frame = buffer.get(..info.buffer_size())?;
    // `from_rgba_unmultiplied` panics on a mismatch, and the numbers it compares
    // come from a decoder fed by the network. Checked here so the mismatch is a
    // dropped icon like every other failure in this module.
    if size[0].checked_mul(size[1])?.checked_mul(4)? != frame.len() {
        return None;
    }
    Some(egui::ColorImage::from_rgba_unmultiplied(size, frame))
}

/// A 1x1 opaque red RGBA PNG: signature, IHDR (1x1, 8-bit, color type 6), one
/// IDAT deflating to the scanline `00 ff 00 00 ff`, IEND.
///
/// Outside `mod tests` and `pub(super)` so the Setup tests can seed a wire icon
/// from the same bytes this module proves decodable — two hand-written PNGs
/// would be one fixture that can rot without anything comparing them.
#[cfg(test)]
pub(super) const RED_DOT: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x56, 0xc7, 0x2f, 0x0d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// [`RED_DOT`] in the form the wire carries it, which is what a vocabulary
/// fixture holds.
#[cfg(test)]
pub(super) fn red_dot_base64() -> String {
    encode_for_test(RED_DOT)
}

/// Test-only encoder, so the PNG fixture above can be stated once as bytes and
/// reused as a wire value. Deliberately not part of the module's contract —
/// nothing in this app ever encodes.
#[cfg(test)]
fn encode_for_test(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut block = [0u8; 3];
        block[..chunk.len()].copy_from_slice(chunk);
        let packed = u32::from(block[0]) << 16 | u32::from(block[1]) << 8 | u32::from(block[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                let sextet = (packed >> (18 - 6 * index)) & 0x3f;
                out.push(char::from(ALPHABET[sextet as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4648 §10, the canonical vectors: both padding lengths and the
    /// unpadded case.
    #[test]
    fn the_rfc_4648_vectors_decode() {
        for (encoded, decoded) in [
            ("", ""),
            ("Zg==", "f"),
            ("Zm8=", "fo"),
            ("Zm9v", "foo"),
            ("Zm9vYg==", "foob"),
            ("Zm9vYmE=", "fooba"),
            ("Zm9vYmFy", "foobar"),
        ] {
            assert_eq!(
                decode_base64(encoded).as_deref(),
                Some(decoded.as_bytes()),
                "{encoded}"
            );
        }
    }

    /// The case this actually ships: every icon on the wire opens with the PNG
    /// signature, so this is the first four bytes of one.
    #[test]
    fn a_png_signature_survives_the_round_trip() {
        assert_eq!(
            decode_base64("iVBORw==").as_deref(),
            Some(&[0x89, 0x50, 0x4e, 0x47][..])
        );
    }

    /// The input crossed a network boundary, so a malformed one answers rather
    /// than panics — and answers `None` rather than a prefix of plausible bytes.
    #[test]
    fn a_character_outside_the_alphabet_is_refused() {
        for encoded in ["Zm9-", "Zm9 ", "Zm9\n", "Zm9*", "Zm9\0"] {
            assert_eq!(decode_base64(encoded), None, "{encoded}");
        }
    }

    /// `=` is not a symbol, it is a length statement, so it is only legal as the
    /// last one or two characters of the last block. Every other placement would
    /// otherwise decode to a zero sextet and yield bytes nobody sent.
    #[test]
    fn misplaced_padding_is_refused() {
        for encoded in [
            "Z===",     // three pads: no block is one sextet long
            "====",     // four
            "Z=m9",     // interior
            "=m9v",     // leading
            "Zg==Zg==", // padded block followed by another block
            "Zg==Zm9v",
        ] {
            assert_eq!(decode_base64(encoded), None, "{encoded}");
        }
    }

    #[test]
    fn a_length_that_is_not_a_multiple_of_four_is_refused() {
        for encoded in ["Z", "Zm", "Zm9", "Zm9vY", "Zm9vYm"] {
            assert_eq!(decode_base64(encoded), None, "{encoded}");
        }
    }

    /// Checks the fixture before anything is built on it, and checks the colour
    /// too: a decoder handed the wrong bytes-per-pixel still answers a plausible
    /// size, so the size alone would not tell a working read from a lucky one.
    #[test]
    fn the_red_dot_is_a_one_pixel_image() {
        let image = decode_png(RED_DOT).expect("the fixture is a readable PNG");
        assert_eq!(image.size, [1, 1]);
        assert_eq!(
            image.pixels,
            vec![egui::Color32::from_rgba_unmultiplied(
                0xff, 0x00, 0x00, 0xff
            )]
        );
    }

    /// Corrupt bytes are the shape a truncated or re-encoded icon arrives in,
    /// and every failure here is silent by design: the caller draws a text chip.
    #[test]
    fn corrupt_bytes_answer_none_rather_than_panicking() {
        assert_eq!(decode_png(&[]), None);
        assert_eq!(decode_png(b"not a png at all"), None);
        // A valid signature and header, then nothing.
        assert_eq!(decode_png(&RED_DOT[..40]), None);
        // The signature intact, one byte of the header flipped.
        let mut flipped = RED_DOT.to_vec();
        flipped[20] ^= 0xff;
        assert_eq!(decode_png(&flipped), None);
    }

    /// The two halves compose, and a base64 failure costs the picture rather
    /// than reaching the decoder as garbage.
    #[test]
    fn the_two_decoders_compose() {
        let encoded = encode_for_test(RED_DOT);
        assert!(decode_base64_png(&encoded).is_some());
        assert_eq!(decode_base64_png("not base64 at all"), None);
        assert_eq!(decode_base64_png(&encode_for_test(b"not a png")), None);
    }

    /// A set the game dropped must not keep a picture nothing offers any more,
    /// so a second message replaces the whole table rather than merging into it
    /// — the same rule [`crate::uplink::vocabulary::VocabularyCell`] follows for
    /// the lists these belong to.
    #[test]
    fn a_later_catalog_replaces_every_texture() {
        let ctx = egui::Context::default();
        let mut icons = SetIcons::default();

        icons.load(&ctx, &table(&["set_speed", "set_retired"]));
        assert!(icons.get("set_retired").is_some());

        icons.load(&ctx, &table(&["set_speed"]));
        assert!(icons.get("set_speed").is_some());
        assert!(icons.get("set_retired").is_none());
    }

    /// One unreadable blob costs its own icon and nothing else — the module's
    /// whole failure policy, seen from the caller.
    #[test]
    fn an_undecodable_icon_costs_only_itself() {
        let ctx = egui::Context::default();
        let mut icons = SetIcons::default();
        let mut wire = table(&["set_speed"]);
        wire.insert("set_torn".to_owned(), "!!! not base64".to_owned());
        wire.insert("set_empty".to_owned(), String::new());

        icons.load(&ctx, &wire);

        assert!(icons.get("set_speed").is_some());
        assert!(icons.get("set_torn").is_none());
        assert!(icons.get("set_empty").is_none());
    }

    /// A wire table pairing each id with the red dot.
    fn table(ids: &[&str]) -> HashMap<String, String> {
        let encoded = red_dot_base64();
        ids.iter()
            .map(|id| ((*id).to_owned(), encoded.clone()))
            .collect()
    }
}
