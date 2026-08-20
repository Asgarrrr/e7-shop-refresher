//! NUL-terminated UTF-16, as every `...W` entry point wants it.
//!
//! One function, in its own file, because `actuator::win` and `capture::pcap`
//! both need it and neither can reach the other. What made merging worth it is
//! not the five lines but the argument below: two copies of a performance claim
//! drift, and the day one is "simplified" to `collect` the other says why not.

/// `text` as a NUL-terminated UTF-16 buffer.
///
/// Returns an owned `Vec`, not a pointer: the buffer *is* the value, and
/// dropping it leaves the caller passing a dangling `as_ptr()` to Win32.
///
/// Sized up front rather than collected, because `EncodeUtf16::size_hint`'s
/// lower bound is `ceil(len / 3)` and that is what `collect` reserves — the
/// obvious spelling allocates a third of what an ASCII string needs and then
/// grows. `text.len() + 1` is exact for ASCII and never short otherwise.
///
/// For callers that run once per process. `actuator::win`'s hot path reads a
/// compile-time `GAME_WINDOW_TITLE_W` instead: `find_game_window` runs three
/// times inside a single click.
#[must_use]
pub fn wide(text: &str) -> Vec<u16> {
    let mut buffer = Vec::with_capacity(text.len() + 1);
    buffer.extend(text.encode_utf16());
    buffer.push(0);
    buffer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buffer_is_terminated_and_holds_no_other_nul() {
        let encoded = wide("NPF_{A1B2}");
        assert_eq!(encoded.last(), Some(&0), "a ...W call reads to the NUL");
        assert_eq!(
            encoded.iter().filter(|unit| **unit == 0).count(),
            1,
            "an interior NUL would truncate the string at the API boundary"
        );
        assert_eq!(encoded.len(), "NPF_{A1B2}".len() + 1);
    }

    #[test]
    fn the_capacity_claim_holds_for_non_ascii() {
        // A reservation that is ever *short* buys a reallocation instead of
        // avoiding one.
        for text in ["", "ascii", "é", "日本語", "🎮"] {
            let encoded = wide(text);
            assert!(
                encoded.len() <= text.len() + 1,
                "{text:?} encoded to {} units against a reservation of {}",
                encoded.len(),
                text.len() + 1
            );
        }
    }

    #[test]
    fn an_empty_string_is_just_the_terminator() {
        assert_eq!(wide(""), vec![0]);
    }
}
