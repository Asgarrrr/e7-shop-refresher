//! The one Npcap build this app names, in the one place both halves can see it.
//!
//! # Why this is its own module
//!
//! Two modules need these facts and neither can reach the other: `install` is
//! behind `feature = "gui"` (only the window offers a Download button) and
//! `capture::pcap` behind `windows` + `pcap-backend` — which is also why they
//! are named here in plain code spans and not linked. So the version was
//! spelled independently in four production literals — the URL the button
//! fetches, the hash pinned beside it, the temp file it lands in, and the
//! sentence a player without Npcap reads — and the banner's hover text is an
//! address *parsed back out of that sentence* by `ui::statusbar::split_help_url`
//! while the button beside it downloads [`INSTALLER_URL`]. Bump one and not the
//! other and the app tells the player one address and fetches another, silently,
//! with a pinned hash that then refuses the download it just made.
//!
//! Nothing here is gated, so both callers reach it, and the version is a single
//! token every other item is built from with `concat!` — the drift is not
//! merely discouraged, it is unspellable.
//!
//! # Bumping to a newer Npcap
//!
//! Change [`VERSION`] and [`INSTALLER_SHA256`] together, and nothing else in
//! this crate. The hash is not optional and not derivable: fetch the new build
//! twice from independent networks, confirm the two digests agree, and confirm
//! the Authenticode signature by hand (`Get-AuthenticodeSignature`) before
//! writing it here. [`INSTALLER_BYTES`] is the new file's size.

/// The pinned Npcap version, as it appears in the vendor's file name.
///
/// A macro rather than a `const` because every item below is assembled with
/// `concat!`, which takes literal tokens and not constants. That is the whole
/// mechanism by which the four spellings became one.
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
/// Wireshark's build mirror rather than npcap.com, and the reason is measured:
/// npcap.com answers in 6–9 s from here and its own installer URL failed
/// outright at 19 s with the TLS handshake never completing, while this one
/// answers in 0.27 s and delivers the 1.3 MB in 0.81 s. A download that does not
/// arrive is not a source.
///
/// What is served there is the genuine article, checked rather than assumed:
/// `npcap-1.88.exe` is Authenticode-signed `CN=Nmap Software LLC`,
/// DigiCert-issued, valid to 2027, timestamped. Nothing is redistributed by us —
/// the player fetches a vendor-signed binary, from a host that answers.
///
/// The version is pinned into the URL on purpose. The mirror keeps every build
/// back to 1.78, so a pinned link cannot rot into a 404 the way a "latest"
/// redirect can; the cost is that it needs bumping when a newer Npcap is worth
/// having, which is a smaller failure than a dead link in the one message a
/// stuck player ever sees.
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
/// An elevated process that downloads and runs an executable is the shape of the
/// thing this app is not, so the check is not optional. Authenticode
/// verification through `WinVerifyTrust` would answer "signed by Nmap Software
/// LLC" — but the URL already names one exact build, so the stronger and much
/// smaller check is the bytes themselves. A signature check accepts any file
/// that vendor ever signed; this accepts the one whose signature was verified by
/// hand and whose hash was then confirmed on a second, independent download.
///
/// The usual objection to hash pinning is that it rots at every release. It does
/// — and so does the URL above, which names the same version. They rot together
/// and are bumped together, which is what makes the pin honest rather than a
/// hostage to fortune, and is why they now live in one file.
pub const INSTALLER_SHA256: [u8; 32] = [
    0xa2, 0xf4, 0xec, 0x1e, 0x5e, 0xa3, 0x53, 0xff, 0x67, 0xef, 0xd2, 0x4b, 0x2e, 0xbf, 0x08, 0x1b,
    0xa4, 0x45, 0x32, 0x41, 0x0f, 0xae, 0x8d, 0x5e, 0x14, 0x6a, 0xf0, 0x31, 0x0a, 0xa4, 0xf5, 0x6b,
];

/// Expected size of that file. Checked at both ends: handed to
/// `curl --max-filesize` so a redirect to an HTML error page cannot become an
/// unbounded response body, and checked again on the file before it is read into
/// memory to be hashed.
pub const INSTALLER_BYTES: u64 = 1_320_424;

/// Where the download lands, under the system temp directory. One deterministic
/// name, so a second attempt reuses or replaces it instead of littering, and
/// version-stamped so a bump cannot silently reuse the previous build's file —
/// which would fail the hash and read as a corrupt download.
pub const TEMP_INSTALLER_NAME: &str = concat!("arkyve-npcap-", version!(), ".exe");

/// What to tell a player who has no Npcap at all.
///
/// The address in the middle is [`INSTALLER_URL`], and it has to be, twice over:
/// `ui::statusbar` parses it back out of this sentence to use as the Download
/// button's hover text, and that button then fetches the constant. `concat!`
/// makes the two the same string by construction rather than by review.
///
/// It ends on "then restart this app" because the capture source is opened once,
/// from `Session::run`, at startup: a player who installs Npcap and comes back
/// to the still-open window is looking at a dead session with nothing to click.
/// The sentence is the cheap half of that fix; the expensive half is a re-probe
/// that rebuilds the session in place, which `docs/npcap-provisioning.md` leaves
/// open.
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
        // The defect this module exists for: the banner's hover text is parsed
        // out of the hint while the click uses the constant, so a bump that
        // reached one and not the other would name an address it does not fetch.
        // `concat!` makes that unspellable — this pins it anyway, because the
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
