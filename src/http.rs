//! Minimal WinHTTP wrapper for the two outbound endpoints we use
//! (Discord webhook, GitHub releases API). Sync, blocking, HTTPS-only.
//! No external dependency — the windows crate is already pulled in and
//! WinHTTP handles TLS via the OS root store.

use std::ffi::c_void;

use anyhow::{Result, anyhow, bail};
use windows::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_STATUS_CODE, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetTimeouts,
};
use windows::core::{HSTRING, PCWSTR};

const USER_AGENT: &str = concat!("e7-shop-refresher/", env!("CARGO_PKG_VERSION"));
const INTERNET_DEFAULT_HTTPS_PORT: u16 = 443;
/// Hard cap on the response body we keep in memory. GitHub's release JSON
/// is ~30 KB; Discord returns ~200 bytes on success. Anything pathological
/// gets cut off rather than ballooning RAM.
const MAX_BODY_BYTES: usize = 256 * 1024;
/// Hard cap on streamed downloads (release binary). Real binary is in
/// the low MB; this bounds disk usage if a compromised host serves an
/// unbounded body, well before Windows starts struggling with a full
/// Program Files volume.
const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;
/// Per-phase WinHTTP timeout (ms). The completion webhook runs on the
/// worker thread between the loop exit and `suspend_to_sleep`, so the
/// default 30 s receive timeout would block the user's PC from sleeping
/// for that long if Discord is unreachable. 5 s × 4 phases = 20 s
/// absolute worst case (typical: < 200 ms total).
const TIMEOUT_MS: i32 = 5_000;

/// Closes the underlying HINTERNET on drop. Construction takes ownership
/// of a handle returned by a WinHTTP call; a null handle is treated as
/// "call failed" so the caller can `?` straight after construction.
struct Handle(*mut c_void);

// SAFETY: HINTERNET is opaque OS state that's safe to move across threads.
// We only ever touch one from one thread at a time.
unsafe impl Send for Handle {}

impl Handle {
    fn new(h: *mut c_void, what: &'static str) -> Result<Self> {
        if h.is_null() {
            // FormatMessage-decoded Win32 error (e.g. "the server name or address
            // could not be resolved") rather than a bare "returned null" so users
            // pasting logs into an issue have something to act on.
            Err(anyhow!(
                "{what} failed: {}",
                std::io::Error::last_os_error()
            ))
        } else {
            Ok(Self(h))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            let _ = unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

#[derive(Debug)]
struct Url<'a> {
    host: &'a str,
    path: &'a str,
}

/// Parses `https://host/path` — the only shape we actually call. `:port`
/// is rejected because our two endpoints are both on default 443, and
/// accepting a port would mean propagating it through `WinHttpConnect`.
fn parse_https(url: &str) -> Result<Url<'_>> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| anyhow!("only https:// URLs are supported"))?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if host.is_empty() {
        bail!("URL has empty host");
    }
    if host.contains(':') {
        bail!("URL must not include a port (only default 443 is supported)");
    }
    Ok(Url { host, path })
}

pub fn get_text(url: &str) -> Result<String> {
    request(url, "GET", None)
}

/// Fires a JSON POST with `Content-Type: application/json` and returns
/// `(status_code, body)`. Caller decides whether 2xx is success.
pub fn post_json(url: &str, body: &str) -> Result<(u32, String)> {
    request_with_status(
        url,
        "POST",
        Some(("Content-Type: application/json\r\n", body.as_bytes())),
    )
}

/// Streaming GET that writes the response body to `sink` chunk-by-chunk
/// without the 256 KB in-memory cap `get_text` enforces. Designed for
/// binary downloads (release exes, sized in the low MB).
///
/// `progress` is called with `(bytes_so_far, content_length_hint)` after
/// every chunk. `content_length_hint` is `None` when the server didn't
/// send `Content-Length` (rare on GitHub release assets but possible
/// after a redirect). Returns the number of bytes written.
pub fn download_to(
    url: &str,
    sink: &mut impl std::io::Write,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<u64> {
    let parsed = parse_https(url)?;
    let host_w = HSTRING::from(parsed.host);
    let path_w = HSTRING::from(parsed.path);
    let method_w = HSTRING::from("GET");
    let ua_w = HSTRING::from(USER_AGENT);

    let session = unsafe {
        WinHttpOpen(
            PCWSTR(ua_w.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        )
    };
    let session = Handle::new(session, "WinHttpOpen")?;
    // Downloads bigger than the JSON endpoints — bump the read timeout so
    // a slow connection on a ~10 MB binary doesn't trip on every chunk.
    // Connect/send keep the snappy default; only Receive is relaxed.
    let dl_recv_timeout: i32 = 60_000;
    if let Err(e) = unsafe {
        WinHttpSetTimeouts(
            session.0,
            TIMEOUT_MS,
            TIMEOUT_MS,
            TIMEOUT_MS,
            dl_recv_timeout,
        )
    } {
        tracing::debug!(error = %e, "WinHttpSetTimeouts(download) failed — falling back to OS defaults");
    }

    let connection = unsafe {
        WinHttpConnect(
            session.0,
            PCWSTR(host_w.as_ptr()),
            INTERNET_DEFAULT_HTTPS_PORT,
            0,
        )
    };
    let connection = Handle::new(connection, "WinHttpConnect")?;

    let request = unsafe {
        WinHttpOpenRequest(
            connection.0,
            PCWSTR(method_w.as_ptr()),
            PCWSTR(path_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        )
    };
    let request = Handle::new(request, "WinHttpOpenRequest")?;

    unsafe { WinHttpSendRequest(request.0, None, None, 0, 0, 0) }
        .map_err(|e| anyhow!("WinHttpSendRequest failed: {e}"))?;
    unsafe { WinHttpReceiveResponse(request.0, std::ptr::null_mut()) }
        .map_err(|e| anyhow!("WinHttpReceiveResponse failed: {e}"))?;

    let status = query_status(&request)?;
    if !(200..300).contains(&status) {
        bail!("HTTP {status} for {url}");
    }
    let content_length = query_content_length(&request);
    // Use Content-Length when honest, MAX_DOWNLOAD_BYTES otherwise.
    // Either way, never write more than the cap to disk.
    let cap = content_length
        .unwrap_or(MAX_DOWNLOAD_BYTES)
        .min(MAX_DOWNLOAD_BYTES);
    if let Some(len) = content_length
        && len > MAX_DOWNLOAD_BYTES
    {
        bail!(
            "refusing download: Content-Length {len} > {MAX_DOWNLOAD_BYTES} cap"
        );
    }

    let mut total: u64 = 0;
    let mut chunk_buf = vec![0u8; 64 * 1024];
    loop {
        let mut available: u32 = 0;
        unsafe { WinHttpQueryDataAvailable(request.0, &mut available) }
            .map_err(|e| anyhow!("WinHttpQueryDataAvailable failed: {e}"))?;
        if available == 0 {
            break;
        }
        let chunk = (available as usize).min(chunk_buf.len());
        let mut read: u32 = 0;
        unsafe {
            WinHttpReadData(
                request.0,
                chunk_buf.as_mut_ptr() as *mut _,
                chunk as u32,
                &mut read,
            )
        }
        .map_err(|e| anyhow!("WinHttpReadData failed: {e}"))?;
        if read == 0 {
            break;
        }
        if total.saturating_add(u64::from(read)) > cap {
            bail!(
                "download exceeded {cap} bytes — aborting (server sent more than \
                 declared / capped)"
            );
        }
        sink.write_all(&chunk_buf[..read as usize])
            .map_err(|e| anyhow!("write to sink failed: {e}"))?;
        total += u64::from(read);
        progress(total, content_length);
    }
    Ok(total)
}

fn query_content_length(request: &Handle) -> Option<u64> {
    use windows::Win32::Networking::WinHttp::WINHTTP_QUERY_CONTENT_LENGTH;
    let mut len_buf = [0u16; 32];
    let mut size: u32 = std::mem::size_of_val(&len_buf) as u32;
    unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_CONTENT_LENGTH,
            PCWSTR::null(),
            Some(len_buf.as_mut_ptr() as *mut _),
            &mut size,
            std::ptr::null_mut(),
        )
    }
    .ok()?;
    let chars = (size as usize) / 2;
    let s = String::from_utf16_lossy(&len_buf[..chars.min(len_buf.len())]);
    s.trim().parse::<u64>().ok()
}

fn request(url: &str, method: &str, body: Option<(&str, &[u8])>) -> Result<String> {
    let (status, body) = request_with_status(url, method, body)?;
    if !(200..300).contains(&status) {
        bail!("HTTP {status}: {body}");
    }
    Ok(body)
}

fn request_with_status(
    url: &str,
    method: &str,
    body: Option<(&str, &[u8])>,
) -> Result<(u32, String)> {
    let parsed = parse_https(url)?;
    let host_w = HSTRING::from(parsed.host);
    let path_w = HSTRING::from(parsed.path);
    let method_w = HSTRING::from(method);
    let ua_w = HSTRING::from(USER_AGENT);

    // Session: per-request rather than reused. Two endpoints, fired at
    // most once every few minutes — pooling is pure complexity here.
    let session = unsafe {
        WinHttpOpen(
            PCWSTR(ua_w.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        )
    };
    let session = Handle::new(session, "WinHttpOpen")?;
    // Best-effort: timeouts inherited by every request on this session.
    // Failure here is non-fatal (defaults still apply); a corporate
    // WinHTTP policy could in theory reject the call.
    if let Err(e) =
        unsafe { WinHttpSetTimeouts(session.0, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS) }
    {
        tracing::debug!(error = %e, "WinHttpSetTimeouts failed — falling back to OS defaults");
    }

    let connection = unsafe {
        WinHttpConnect(
            session.0,
            PCWSTR(host_w.as_ptr()),
            INTERNET_DEFAULT_HTTPS_PORT,
            0,
        )
    };
    let connection = Handle::new(connection, "WinHttpConnect")?;

    let request = unsafe {
        WinHttpOpenRequest(
            connection.0,
            PCWSTR(method_w.as_ptr()),
            PCWSTR(path_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        )
    };
    let request = Handle::new(request, "WinHttpOpenRequest")?;

    // WinHttpSendRequest takes the headers as a UTF-16 slice (length
    // derived from the slice). `headers_w` keeps the buffer alive across
    // the call.
    let headers_w: Option<Vec<u16>> = body.map(|(h, _)| h.encode_utf16().collect());
    let headers_slice: Option<&[u16]> = headers_w.as_deref();
    let body_bytes: &[u8] = body.map(|(_, b)| b).unwrap_or(&[]);

    unsafe {
        WinHttpSendRequest(
            request.0,
            headers_slice,
            Some(body_bytes.as_ptr() as *const _),
            body_bytes.len() as u32,
            body_bytes.len() as u32,
            0,
        )
    }
    .map_err(|e| anyhow!("WinHttpSendRequest failed: {e}"))?;

    unsafe { WinHttpReceiveResponse(request.0, std::ptr::null_mut()) }
        .map_err(|e| anyhow!("WinHttpReceiveResponse failed: {e}"))?;

    let status = query_status(&request)?;
    let body = read_body(&request)?;
    Ok((status, body))
}

fn query_status(request: &Handle) -> Result<u32> {
    let mut status: u32 = 0;
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut _ as *mut _),
            &mut size,
            std::ptr::null_mut(),
        )
    }
    .map_err(|e| anyhow!("WinHttpQueryHeaders(status) failed: {e}"))?;
    Ok(status)
}

fn read_body(request: &Handle) -> Result<String> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let mut available: u32 = 0;
        unsafe { WinHttpQueryDataAvailable(request.0, &mut available) }
            .map_err(|e| anyhow!("WinHttpQueryDataAvailable failed: {e}"))?;
        if available == 0 {
            break;
        }
        let remaining = MAX_BODY_BYTES.saturating_sub(buf.len());
        if remaining == 0 {
            break;
        }
        let chunk = (available as usize).min(remaining);
        let start = buf.len();
        buf.resize(start + chunk, 0);
        let mut read: u32 = 0;
        unsafe {
            WinHttpReadData(
                request.0,
                buf[start..].as_mut_ptr() as *mut _,
                chunk as u32,
                &mut read,
            )
        }
        .map_err(|e| anyhow!("WinHttpReadData failed: {e}"))?;
        buf.truncate(start + read as usize);
        if read == 0 {
            break;
        }
    }
    // Discord + GitHub both return UTF-8. Lossy keeps us moving on any
    // surprise non-UTF-8 byte instead of dropping the whole response.
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_https_splits_host_and_path() {
        let u = parse_https("https://example.com/foo/bar").unwrap();
        assert_eq!(u.host, "example.com");
        assert_eq!(u.path, "/foo/bar");
    }

    #[test]
    fn parse_https_defaults_path_to_slash() {
        let u = parse_https("https://example.com").unwrap();
        assert_eq!(u.host, "example.com");
        assert_eq!(u.path, "/");
    }

    #[test]
    fn parse_https_rejects_http() {
        assert!(parse_https("http://example.com").is_err());
    }

    #[test]
    fn parse_https_rejects_port() {
        assert!(parse_https("https://example.com:8443/foo").is_err());
    }

    #[test]
    fn parse_https_rejects_empty_host() {
        assert!(parse_https("https:///foo").is_err());
    }
}
