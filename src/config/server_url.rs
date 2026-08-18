//! The `server_url` key's type, and the cleartext rule it carries.
//!
//! This is a seam because of what the type promises rather than because of its
//! size: [`ServerUrl`]'s two fields are private *to this file*, so "the dial
//! string is reachable through exactly one `as_str()`, and `Debug`/`Display`
//! print the redacted form only" is a claim a reader can check by reading one
//! 170-line file instead of the whole schema module. Nothing in the config
//! schema touches these fields; it only calls [`ServerUrl::parse`].
//!
//! The authority-splitting helpers below are here rather than in the schema
//! because `parse` is their only caller and the userinfo subtlety they encode is
//! the reason the type exists.

use std::fmt;

use serde::Deserialize;

use crate::error::Result;

/// The authority of `rest` (everything after a `scheme://`), with any
/// `user:pass@` userinfo dropped: `host` or `host:port`, IPv6 in brackets.
///
/// The real host is what follows the *last* `@` — what `http::Uri`, and thus the
/// WebSocket client, actually connects to — so `127.0.0.1@evil.com` is correctly
/// seen as `evil.com`. That one line does double duty, which is why it lives in
/// exactly one place now: it is what stops a userinfo-embedded loopback from
/// leaking traffic in cleartext to a remote host, *and* what keeps a credential
/// out of the redacted form written to the log.
fn authority_of(rest: &str) -> &str {
    // The authority ends at the first path/query/fragment separator.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
}

/// `scheme://host[:port]` — userinfo, path, query and fragment all gone. The
/// only form of a server URL that may be written to a log or a journal line.
///
/// Deliberately lenient rather than fallible: it runs *after* the scheme check
/// inside [`ServerUrl::parse`] and only has to split an authority off text the
/// caller already accepted, so refusing would mean two ways to fail one parse.
/// Garbage reduces rather than errors (`"garbage"` becomes `"://garbage"`, which
/// is unmistakably not a URL and still carries no secret).
fn redacted_authority(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    format!("{scheme}://{}", authority_of(rest))
}

/// Strip a trailing `:port` from an authority. An IPv6 literal is bracketed, so
/// a trailing `:port` is only a port when it sits outside the brackets.
fn host_of(authority: &str) -> &str {
    if authority.starts_with('[') {
        // "[::1]:3001" -> "[::1]"
        authority
            .split_once(']')
            .map_or(authority, |(head, _)| &authority[..head.len() + 1])
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
    }
}

/// True for the hosts where cleartext never leaves the machine.
fn is_loopback_host(host: &str) -> bool {
    ["127.0.0.1", "localhost", "[::1]", "::1"]
        .iter()
        .any(|loopback| host.eq_ignore_ascii_case(loopback))
}

/// A `server_url` that has been proven safe to dial, carrying the proof.
///
/// The rule is a security property, not a spelling convention: `server_url`
/// receives the reassembled game stream, which can carry session tokens, so it
/// must be `wss://` — or `ws://` to a loopback host, where cleartext never
/// leaves the machine. [`ServerUrl::parse`] is the single place that rule and
/// the authority split it needs are written, and a `ServerUrl` that exists is
/// the evidence that they passed.
///
/// It also carries the redacted `scheme://host[:port]` form, because the same
/// split that defeats `ws://127.0.0.1@evil.com` is the one that keeps a
/// `user:pass@` credential out of the log the player is asked to send us. Those
/// two used to be written twice — here and in `app::redacted_server_url`, now
/// deleted — and only this copy had the userinfo tests, so the next parsing
/// subtlety had to be found twice.
///
/// `Debug` and `Display` print the redacted form **only**, so no `?url`, `%url`
/// or `#[instrument]` argument list can put a credential in the log. The dial
/// string — what the WebSocket client is actually given — comes out through
/// [`ServerUrl::as_str`] and nowhere else.
///
/// `Deserialize` goes through [`ServerUrl::parse`] via `#[serde(try_from =
/// "String")]`, so a `config.toml` cannot produce one that has not been checked.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct ServerUrl {
    dial: String,
    redacted: String,
}

impl ServerUrl {
    /// Parses `raw`, enforcing the cleartext rule. Surrounding whitespace is
    /// trimmed: a hand-edited `server_url = " wss://… "` dials the same server.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] — `raw` is empty, carries a scheme other than `ws://`
    /// or `wss://`, or is `ws://` to anywhere but loopback (which would forward
    /// the captured game stream, session tokens included, in cleartext).
    ///
    /// [`Error::Config`]: crate::Error::Config
    pub fn parse(raw: &str) -> Result<Self> {
        let dial = raw.trim();
        if dial.is_empty() {
            return Err(crate::Error::Config("server_url is empty".into()));
        }
        // URL schemes are case-insensitive, so match `WSS://` too.
        let (rest, tls) = if let Some(rest) = strip_scheme(dial, "wss://") {
            (rest, true)
        } else if let Some(rest) = strip_scheme(dial, "ws://") {
            (rest, false)
        } else {
            return Err(crate::Error::Config(
                "server_url must be a ws:// or wss:// URL".into(),
            ));
        };
        if !tls && !is_loopback_host(host_of(authority_of(rest))) {
            return Err(crate::Error::Config(
                "server_url uses ws:// to a non-loopback host — captured traffic \
                 would be sent in cleartext; use wss:// (or ws:// only for \
                 127.0.0.1/localhost)"
                    .into(),
            ));
        }
        Ok(Self {
            redacted: redacted_authority(dial),
            dial: dial.to_owned(),
        })
    }

    /// The dial string: what the WebSocket client connects to, userinfo and
    /// query intact. Never log this — see [`ServerUrl::redacted`].
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.dial
    }

    /// `scheme://host[:port]`, the only form safe to write to the log file the
    /// player is asked to send us. Also what `Debug`/`Display` print.
    #[must_use]
    pub fn redacted(&self) -> &str {
        &self.redacted
    }
}

/// Case-insensitive scheme prefix strip. `get` rather than a slice index because
/// `raw` is arbitrary player text and may not have a char boundary there.
fn strip_scheme<'a>(url: &'a str, scheme: &str) -> Option<&'a str> {
    url.get(..scheme.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(scheme))
        .map(|prefix| &url[prefix.len()..])
}

impl fmt::Debug for ServerUrl {
    /// The redacted form, deliberately — see the type's documentation.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ServerUrl").field(&self.redacted).finish()
    }
}

impl fmt::Display for ServerUrl {
    /// The redacted form, deliberately — see the type's documentation.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.redacted)
    }
}

/// The serde hook: `#[serde(try_from = "String")]` on a `ServerUrl` field parses
/// through [`ServerUrl::parse`], so the cleartext rule stops depending on
/// `Config::load` being the only constructor.
impl TryFrom<String> for ServerUrl {
    type Error = crate::Error;

    fn try_from(raw: String) -> Result<Self> {
        Self::parse(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The cleartext-rule tests below used to build a whole `Config` with the URL
    // overwritten and call `validate()`. They now call `ServerUrl::parse`
    // directly, because that *is* the load path: the field's type is the check,
    // and `Config::validate` has no `server_url` clause left to reach. Every
    // input is unchanged, including the two userinfo bypasses.

    #[test]
    fn wss_is_accepted() {
        assert!(ServerUrl::parse("wss://ingest.arkyve.dev/refresh-shop").is_ok());
    }

    #[test]
    fn ws_loopback_ipv4_accepted() {
        assert!(ServerUrl::parse("ws://127.0.0.1:3001/refresh-shop").is_ok());
    }

    #[test]
    fn ws_localhost_accepted() {
        assert!(ServerUrl::parse("ws://localhost:3001/x").is_ok());
    }

    #[test]
    fn ws_ipv6_loopback_accepted() {
        assert!(ServerUrl::parse("ws://[::1]:3001/x").is_ok());
    }

    #[test]
    fn ws_remote_host_rejected() {
        let err = ServerUrl::parse("ws://ingest.arkyve.dev/x").unwrap_err();
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn ws_example_com_rejected() {
        // Done-criteria spot check: a non-loopback ws:// host is refused.
        let err = ServerUrl::parse("ws://example.com/x").unwrap_err();
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn non_ws_scheme_rejected() {
        let err = ServerUrl::parse("http://example.com").unwrap_err();
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn empty_still_rejected() {
        let err = ServerUrl::parse("").unwrap_err();
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn ws_userinfo_loopback_is_rejected() {
        // The loopback text sits in the userinfo; the real host (evil.com) is
        // remote, so this must be refused — not accepted as loopback.
        let err = ServerUrl::parse("ws://127.0.0.1:3001@evil.com/refresh-shop").unwrap_err();
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn ws_bare_userinfo_loopback_is_rejected() {
        // The same bypass without a port in the userinfo.
        let err = ServerUrl::parse("ws://localhost@evil.com/x").unwrap_err();
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn an_uppercase_wss_scheme_is_accepted() {
        // URL schemes are case-insensitive; an uppercase scheme must not be
        // rejected when the WebSocket client would accept it.
        assert!(ServerUrl::parse("WSS://ingest.arkyve.dev/refresh-shop").is_ok());
    }

    #[test]
    fn an_uppercase_ws_scheme_to_loopback_is_accepted() {
        assert!(ServerUrl::parse("WS://127.0.0.1:3001/x").is_ok());
    }

    #[test]
    fn an_uppercase_ws_userinfo_bypass_is_still_rejected() {
        // The case-insensitive match must not become a way around the host check.
        assert!(ServerUrl::parse("WS://127.0.0.1@evil.com/x").is_err());
    }

    #[test]
    fn a_parsed_server_url_keeps_the_dial_string_and_redacts_the_credential() {
        // The two halves the crate used to write twice: what gets dialed, and
        // what is safe to log. One parse now produces both.
        let url = ServerUrl::parse("wss://token:secret@ingest.arkyve.dev:8443/path?key=abc")
            .expect("wss is accepted whatever the authority carries");
        assert_eq!(
            url.as_str(),
            "wss://token:secret@ingest.arkyve.dev:8443/path?key=abc"
        );
        assert_eq!(url.redacted(), "wss://ingest.arkyve.dev:8443");
    }

    #[test]
    fn a_server_urls_debug_and_display_cannot_leak_the_credential() {
        // The reason `Debug` is hand-written: the log file is what the player is
        // asked to email us, and `README.md` promises it carries no credential.
        let url = ServerUrl::parse("wss://token:secret@ingest.arkyve.dev/x").expect("accepted");
        for rendered in [format!("{url:?}"), format!("{url}")] {
            assert!(!rendered.contains("secret"), "{rendered}");
            assert!(rendered.contains("ingest.arkyve.dev"), "{rendered}");
        }
    }

    #[test]
    fn a_query_or_fragment_never_reaches_the_redacted_form() {
        // A fragment used to be authority text to the loopback check and not to
        // the log redactor; one parser now handles both.
        let url = ServerUrl::parse("ws://127.0.0.1:9000/?key=abc#frag").expect("loopback");
        assert_eq!(url.redacted(), "ws://127.0.0.1:9000");
    }

    #[test]
    fn a_surrounding_whitespace_server_url_is_accepted_and_dials_trimmed() {
        // A hand-edited `server_url = " wss://… "` names the same server; the
        // trim has to happen before the scheme match *and* before the dial
        // string is kept, or the client gets a URL with a leading space.
        let url = ServerUrl::parse("  wss://ingest.arkyve.dev/x  ").expect("accepted");
        assert_eq!(url.as_str(), "wss://ingest.arkyve.dev/x");
    }

    #[test]
    fn the_serde_hook_parses_through_the_same_rule() {
        // `#[serde(try_from = "String")]` on a `ServerUrl` field is what makes
        // the cleartext rule stop depending on `Config::load` being the only
        // constructor; the conversion must be the same parse, not a second one.
        assert!(ServerUrl::try_from("wss://ingest.arkyve.dev/x".to_owned()).is_ok());
        let error = ServerUrl::try_from("ws://evil.com/x".to_owned())
            .expect_err("a non-loopback ws:// must not become a ServerUrl");
        assert!(matches!(error, crate::Error::Config(_)));
    }
}
