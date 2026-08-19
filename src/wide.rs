//! NUL-terminated UTF-16, as every `...W` entry point wants it.
//!
//! One function, in its own file, because it had two identical implementations —
//! `actuator::win` and `capture::pcap` — carrying the same sizing argument in
//! two different wordings. Neither subsystem can reach the other (`actuator` and
//! `pcap-backend` are independent features), so the copy was not laziness; it
//! was the only spelling available. What made it worth merging is not the five
//! lines, it is the argument below: two copies of a performance claim drift, and
//! the day one of them is "simplified" to `collect` the other still says why not.

/// `text` as a NUL-terminated UTF-16 buffer.
///
/// The buffer *is* the value: dropping it leaves the caller passing a dangling
/// `as_ptr()` to Win32, so it has to outlive the call it is handed to. This is
/// why it returns an owned `Vec` and not a pointer.
///
/// Sized up front rather than collected. `EncodeUtf16::size_hint`'s lower bound
/// is `ceil(len / 3)` — one unit per three bytes, the worst case for a string of
/// three-byte characters — and that lower bound is what `collect` and
/// `Vec::from_iter` reserve. So the obvious spelling allocates a third of what
/// an ASCII string needs and then grows. `text.len() + 1` is exact for ASCII and
/// never short for anything else: at most one UTF-16 unit per byte, plus the
/// terminator.
///
/// Callers that run once per process — a window class name, a device path —
/// use this directly. `actuator::win`'s hot path does not: `find_game_window`
/// runs before *every* injected event, three times inside a single click, so it
/// reads a compile-time `GAME_WINDOW_TITLE_W` instead of re-encoding a constant
/// on each call.
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
        // The reason for `len + 1` over `collect`: it must never be *short*, or
        // the reservation argument buys a reallocation instead of avoiding one.
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
