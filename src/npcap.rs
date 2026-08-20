//! The one Npcap build this app names, in the one place both halves can see it.
//!
//! # Why this is its own module
//!
//! `install` (gui) and `capture::pcap` (windows + `pcap-backend`) both need
//! these facts and neither can reach the other — which is also why they are
//! named in plain code spans and not linked. The version was therefore spelled
//! in four independent literals, one of which the banner's hover text is
//! *parsed back out of* while the button beside it downloads [`INSTALLER_URL`].
//! Ungated here, with the version as a single token every item is built from
//! with `concat!`, that drift is unspellable rather than merely discouraged.
//!
//! # Bumping to a newer Npcap
//!
//! Change [`VERSION`], [`INSTALLER_SHA256`] and [`INSTALLER_BYTES`] together and
//! nothing else. The hash is not derivable: fetch the build twice from
//! independent networks, confirm the digests agree, and confirm the Authenticode
//! signature by hand (`Get-AuthenticodeSignature`) before writing it here.

/// The pinned Npcap version, as it appears in the vendor's file name.
///
/// A macro rather than a `const` because every item below is assembled with
/// `concat!`, which takes literal tokens and not constants.
macro_rules! version {
    () => {
        "1.88"
    };
}

/// The pinned version, for anything that wants to print it.
pub const VERSION: &str = version!();

/// The one build [`crate::install`] will download and run. Pinned with
/// [`INSTALLER_SHA256`]; change neither without the other.
///
/// Wireshark's build mirror rather than npcap.com, measured: npcap.com's own
/// installer URL failed at 19 s with the TLS handshake never completing, this
/// one answers in 0.27 s. The file served there was checked, not assumed —
/// Authenticode-signed `CN=Nmap Software LLC`, valid to 2027 — and we
/// redistribute nothing.
///
/// The version is pinned into the URL on purpose: the mirror keeps every build
/// back to 1.78, so a pinned link cannot rot into a 404 the way a "latest"
/// redirect can, and a bump per release is a smaller failure than a dead link in
/// the one message a stuck player ever sees.
pub const INSTALLER_URL: &str = concat!(
    "https://dev-libs.wireshark.org/windows/packages/Npcap/npcap-",
    version!(),
    ".exe"
);

/// SHA-256 of the file at [`INSTALLER_URL`], measured on two independent
/// downloads: `a2f4ec1e5ea353ff67efd24b2ebf081ba44532410fae8d5e146af0310aa4f56b`.
///
/// # Why a pinned hash rather than a signature check
///
/// An elevated process downloading and running an executable is the shape of the
/// thing this app is not, so the check is not optional — and `WinVerifyTrust`
/// would accept any file that vendor ever signed, where the URL already names
/// one exact build. Pinning rots at every release, but so does that URL: they
/// rot together, which is why they live in one file.
pub const INSTALLER_SHA256: [u8; 32] = [
    0xa2, 0xf4, 0xec, 0x1e, 0x5e, 0xa3, 0x53, 0xff, 0x67, 0xef, 0xd2, 0x4b, 0x2e, 0xbf, 0x08, 0x1b,
    0xa4, 0x45, 0x32, 0x41, 0x0f, 0xae, 0x8d, 0x5e, 0x14, 0x6a, 0xf0, 0x31, 0x0a, 0xa4, 0xf5, 0x6b,
];

/// Expected size, checked at both ends: handed to `curl --max-filesize` so a
/// redirect to an error page cannot become an unbounded response body, and
/// checked again before the file is read into memory to be hashed.
pub const INSTALLER_BYTES: u64 = 1_320_424;

/// Where the download lands. Deterministic so a second attempt replaces it
/// instead of littering, and version-stamped so a bump cannot reuse the previous
/// build's file — which would fail the hash and read as a corrupt download.
pub const TEMP_INSTALLER_NAME: &str = concat!("arkyve-npcap-", version!(), ".exe");

/// What to tell a player who has no Npcap at all.
///
/// The address in the middle has to be [`INSTALLER_URL`]: `ui::statusbar` parses
/// it back out of this sentence for the Download button's hover text while the
/// button fetches the constant, and `concat!` makes the two the same string by
/// construction rather than by review.
///
/// It ends on "then restart this app" because the capture source is opened once,
/// at startup: a player who installs Npcap and returns to the still-open window
/// is looking at a dead session with nothing to click.
pub const INSTALL_HINT: &str = concat!(
    "Npcap is missing, and the capture needs it. ",
    "https://dev-libs.wireshark.org/windows/packages/Npcap/npcap-",
    version!(),
    ".exe",
    " Keep the installer's defaults, then restart this app."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hint_carries_the_url_the_button_downloads() {
        // `concat!` already makes this unspellable; pinned anyway, because the
        // hint is prose and prose gets edited.
        assert!(
            INSTALL_HINT.contains(INSTALLER_URL),
            "the hint must quote the URL verbatim; hint: {INSTALL_HINT}"
        );
    }

    #[test]
    fn every_spelling_carries_the_pinned_version() {
        for (what, text) in [
            ("the URL", INSTALLER_URL),
            ("the temp file name", TEMP_INSTALLER_NAME),
            ("the hint", INSTALL_HINT),
        ] {
            assert!(
                text.contains(VERSION),
                "{what} lost the version {VERSION}: {text}"
            );
        }
    }

    #[test]
    fn the_pinned_hash_is_thirty_two_bytes_and_not_all_zero() {
        // A zeroed pin would match `install::sha256`'s own failure return, which
        // is how a failed hash could have read as a verified file.
        assert_eq!(INSTALLER_SHA256.len(), 32);
        assert!(INSTALLER_SHA256.iter().any(|byte| *byte != 0));
    }
}
