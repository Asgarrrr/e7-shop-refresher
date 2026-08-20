//! The `wpcap.dll` boundary: the transcribed ABI, the handle lifecycle, and the
//! per-adapter receive loop.
//!
//! # Why these three are one module
//!
//! They are separable on paper, and `docs/tech-debt/24-proj.md` proposed
//! separating them. They share one invariant only checkable in one place:
//! [`unsafe impl Send for Handle`](Handle) claims that nothing but the
//! owning `Handle` retains the `*mut PcapT`. Every line in this crate that
//! names a libpcap object pointer is in this file, and the field holding it
//! is private to this file — the only widened field is the `device` string
//! the parent uses for a thread name — so the claim is checkable against
//! this module rather than the whole `pcap` tree. Splitting `open_device`
//! from `capture_loop` would have made `Handle::handle` `pub(super)` and
//! spread that check over three files, the one cost the report named for
//! this move; not worth paying. [`super::link`] is the layer that carries
//! no pointer, and it is the one that left.

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, SyncSender, TrySendError};

use tracing::{debug, info_span, warn};

use super::link::{LinkStrip, UnsupportedDatalink};
use super::{AdapterFailure, short_device_name};
use crate::error::{Error, Result};

/// Size libpcap requires of every error buffer it is handed. Not negotiable:
/// the library writes up to this many bytes without asking.
const PCAP_ERRBUF_SIZE: usize = 256;

/// Bytes captured per packet.
///
/// Deliberately not the wire MTU, nor the 65 575 the removed `WinDivert`
/// backend used. Receive-side coalescing (RSC/LRO) hands the stack a single
/// "packet" made of many wire frames — the probe measured one of **48 870
/// bytes** on this machine, 32 times the MTU. 65 575 happened to be enough
/// here; there is no reason it is everywhere, and a too-small snaplen does
/// not fail, it silently truncates, reaching `parse_segment` as a malformed
/// packet that reads like a parser bug. 262 144 is libpcap's own documented
/// ceiling: as much as it will give.
pub(super) const SNAPLEN: c_int = 262_144;

/// Read timeout, in milliseconds. Bounds how long a capture thread can sit
/// inside `pcap_next_ex` without looking at the stop flag, and therefore how
/// long session teardown waits. Also how often each thread polls `pcap_stats`.
pub(super) const READ_TIMEOUT_MS: c_int = 200;

/// Promiscuous mode off: only traffic addressed to this host is wanted, and
/// asking the NIC for everything else would multiply the volume the kernel
/// filter has to chew through for no gain.
const PROMISCUOUS: c_int = 0;

/// `pcap_compile`'s optimizer flag. On: the filter is evaluated per packet in
/// the driver, so it is worth optimizing once at open time.
const OPTIMIZE_FILTER: c_int = 1;

/// Netmask handed to `pcap_compile`. No rung of [`super::filter_candidates`]
/// uses a broadcast-relative primitive (`ip broadcast` and friends), which is
/// the only thing the netmask feeds, so zero is correct rather than merely
/// tolerated — `vlan` shifts decoding offsets and reads none of it.
const FILTER_NETMASK: c_uint = 0;

/// `pcap_next_ex` return codes. Only these four are defined for a live handle.
const NEXT_EX_OK: c_int = 1;
const NEXT_EX_TIMEOUT: c_int = 0;

/// How many packets a capture thread delivers between two `pcap_stats` polls.
/// Stats are also polled on every read timeout, which covers an idle adapter;
/// this covers a busy one, where timeouts may never happen.
const STATS_EVERY_PACKETS: u64 = 512;

// Transcribed from `pcap.h` and, more importantly, **verified against a real
// run** of the feasibility probe against Npcap 1.75. Only the fields this
// module reads are named; the rest of each struct is present because the
// library writes through the pointer and the layout has to match.

#[repr(C)]
struct PcapIf {
    next: *mut PcapIf,
    name: *mut c_char,
    description: *mut c_char,
    addresses: *mut c_void,
    flags: c_uint,
}

/// libpcap's per-packet header.
///
/// The timestamp is a Windows `struct timeval`: two **32-bit** longs, so this
/// struct is 16 bytes, not the 24 a 64-bit `time_t` would give. Getting it
/// wrong is not a crash — `caplen` would read the tail of the timestamp and
/// yield absurd lengths — which is why [`is_plausible_caplen`] guards every read.
#[repr(C)]
struct PcapPktHdr {
    tv_sec: i32,
    tv_usec: i32,
    caplen: c_uint,
    len: c_uint,
}

#[repr(C)]
struct BpfProgram {
    bf_len: c_uint,
    bf_insns: *mut c_void,
}

/// `struct pcap_stat`.
///
/// libpcap defines three counters, and adds three Windows-only ones
/// (`ps_capt`, `ps_sent`, `ps_netdrop`) that only `pcap_stats_ex` ever fills.
/// All six are declared so the library can never write past this struct
/// whichever build answers; only the first two are read.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct PcapStat {
    ps_recv: c_uint,
    ps_drop: c_uint,
    ps_ifdrop: c_uint,
    ps_capt: c_uint,
    ps_sent: c_uint,
    ps_netdrop: c_uint,
}

// The two layouts `wpcap.dll` writes through a pointer, asserted where a wrong
// one would be *shipped* rather than where it would be tested: `mod pcap` is
// gated on `windows` + `pcap-backend`, and `cargo build --release` on that lane
// never evaluates a `#[cfg(test)]` assertion. The test below stays as well — it
// covers the same numbers for a reader who runs the suite, and costs nothing.
//
// `PcapPktHdr` is the single most dangerous constant in this file: at 24 bytes
// `caplen` would land on the low half of `tv_usec` and report nonsense lengths
// instead of crashing, which `is_plausible_caplen` then catches only ~3 packets
// in 4. `PcapStat` must have room for the Windows-only three-counter tail or the
// library writes past it. Both are `#[repr(C)]`, so these sizes are the ABI
// contract, not an implementation detail: a failure here means the transcription
// is wrong, never that the number should be updated.
const _: () = {
    assert!(size_of::<PcapPktHdr>() == 16);
    assert!(size_of::<PcapStat>() == 24);
};

/// Opaque `pcap_t`.
type PcapT = c_void;

/// The subset of `wpcap.dll` this backend uses, resolved once at open time.
///
/// Every entry point is a stable libpcap export whose signature has not changed
/// across the library's lifetime, so resolving them by name is safe in the sense
/// that matters: a missing symbol is caught here, at load, not at the call.
///
/// Every field is private to this module and stays that way: each of the thirteen
/// was verified against libpcap's ABI, and none of them is ever handed out. The
/// type itself is `pub(super)` only because the parent's `open` loads it and
/// passes it back in.
pub(super) struct Wpcap {
    /// Kept solely to pin the library in memory: the function pointers below
    /// are only valid while it stays loaded.
    _lib: libloading::Library,
    findalldevs: unsafe extern "C" fn(*mut *mut PcapIf, *mut c_char) -> c_int,
    freealldevs: unsafe extern "C" fn(*mut PcapIf),
    open_live: unsafe extern "C" fn(*const c_char, c_int, c_int, c_int, *mut c_char) -> *mut PcapT,
    close: unsafe extern "C" fn(*mut PcapT),
    datalink: unsafe extern "C" fn(*mut PcapT) -> c_int,
    datalink_val_to_name: unsafe extern "C" fn(c_int) -> *const c_char,
    compile:
        unsafe extern "C" fn(*mut PcapT, *mut BpfProgram, *const c_char, c_int, c_uint) -> c_int,
    setfilter: unsafe extern "C" fn(*mut PcapT, *mut BpfProgram) -> c_int,
    freecode: unsafe extern "C" fn(*mut BpfProgram),
    next_ex: unsafe extern "C" fn(*mut PcapT, *mut *mut PcapPktHdr, *mut *const u8) -> c_int,
    stats: unsafe extern "C" fn(*mut PcapT, *mut PcapStat) -> c_int,
    geterr: unsafe extern "C" fn(*mut PcapT) -> *mut c_char,
    lib_version: unsafe extern "C" fn() -> *const c_char,
}

/// Search flags for every candidate below, and the reason the bare name
/// `"wpcap.dll"` is no longer one of them.
///
/// `libloading::Library::new` is `LoadLibraryExW(name, NULL, 0)` — the standard
/// search order, whose **first** entry is the directory of the running
/// executable. This exe is manifested `requireAdministrator` (`build.rs`) and a
/// player runs the single downloaded file straight out of
/// `%USERPROFILE%\Downloads`, a directory any medium-integrity process of the
/// same user can write. A `wpcap.dll` dropped there would have had its
/// `DllMain` executed at high integrity before [`Wpcap::load`] returned: a
/// local privilege escalation reached by writing one file and waiting.
///
/// Absolute paths alone would fix the hijack but break the load, because
/// `wpcap.dll` in Npcap's private directory imports `Packet.dll` from beside
/// it, and a `LoadLibrary` of an absolute path searches the *application*
/// directory for dependencies, not the loaded module's own.
/// `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR` is what makes that import resolve, and
/// naming any `LOAD_LIBRARY_SEARCH_*` flag replaces the standard order outright
/// — the exe's directory is no longer in it. `LOAD_LIBRARY_SEARCH_SYSTEM32`
/// covers the WinPcap-compatible copy's own dependencies.
///
/// These flags require a fully qualified path, which is the other half of why
/// the bare name had to go.
/// Spelled `u32` because `libloading`'s own `LOAD_LIBRARY_FLAGS` alias for it is
/// private, while the constants below are not.
const SEARCH_FLAGS: u32 = libloading::os::windows::LOAD_LIBRARY_SEARCH_SYSTEM32
    | libloading::os::windows::LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR;

/// Where `wpcap.dll` is looked for, in order.
///
/// `Npcap\wpcap.dll` first: it is the private directory Npcap always writes to,
/// so it is present on every install. The System32 copy exists only when the
/// installer's WinPcap-compatible mode was kept (its default, though the player
/// may have unchecked it), so it is the fallback rather than the first ask —
/// the reverse of the order this list had while its first entry was a bare
/// name. Which one answered is logged, and the path says which install mode the
/// machine has, on a machine we cannot inspect.
///
/// The directory comes from `GetSystemDirectoryW` and not from `%SystemRoot%`:
/// a UAC-elevated process inherits its environment from the medium-integrity
/// process that requested the elevation, so that variable is exactly as
/// trustworthy as the attacker in [`SEARCH_FLAGS`].
fn dll_candidates() -> [PathBuf; 2] {
    let system = crate::system32::directory();
    [
        system.join("Npcap").join("wpcap.dll"),
        system.join("wpcap.dll"),
    ]
}

/// What to tell a player who has no Npcap at all.
///
/// Re-exported rather than spelled here: the sentence names one exact Npcap
/// build, the Download button in `ui::statusbar` fetches that same build from
/// [`crate::npcap::INSTALLER_URL`], and this module cannot see `install` (it is
/// behind `feature = "gui"`). [`crate::npcap`] carries both, and the argument
/// for the mirror, the pinned version and the closing sentence with them.
pub(super) use crate::npcap::INSTALL_HINT;

impl Wpcap {
    pub(super) fn load() -> Result<(Self, PathBuf)> {
        let mut failures = Vec::new();
        for path in dll_candidates() {
            // SAFETY: `load_with_flags` runs the DLL's entry point, which for
            // `wpcap.dll` only initializes the library's own state. The symbols
            // resolved just below are copied out as plain function pointers;
            // they stay valid exactly as long as `_lib` — stored in the same
            // struct, never separated from them — keeps the module loaded.
            // Failure mode: the DLL is absent or is not a valid image, which
            // surfaces as an `Err` here and is collected rather than fatal, so
            // the second candidate still gets its turn. The flags are argued at
            // [`SEARCH_FLAGS`]; the path is absolute, which they require.
            let lib = match unsafe {
                libloading::os::windows::Library::load_with_flags(&path, SEARCH_FLAGS)
            } {
                Ok(lib) => libloading::Library::from(lib),
                Err(err) => {
                    failures.push(format!("{}: {err}", path.display()));
                    continue;
                }
            };
            // Deliberately scope-local and unhygienic: the body reads `lib`,
            // `path` and `failures` from this loop, and its `continue`
            // abandons this DLL candidate — all twelve sibling resolutions
            // included — for the next one. That caller-scope `continue` is
            // why this is a macro, not a function, and why hoisting it to
            // module scope would not compile.
            macro_rules! sym {
                ($name:literal) => {
                    // SAFETY: `get` resolves one exported symbol and the
                    // transcribed signature is checked against `pcap.h` above; a
                    // name that does not exist returns `Err` and is reported
                    // rather than called. The returned `Symbol` borrows `lib`, but
                    // dereferencing it copies out a bare function pointer whose
                    // validity is tied to the module staying loaded, which `_lib`
                    // guarantees below.
                    match unsafe { lib.get($name) } {
                        Ok(symbol) => *symbol,
                        Err(err) => {
                            failures.push(format!(
                                "{}: {} is missing ({err})",
                                path.display(),
                                String::from_utf8_lossy($name).trim_end_matches('\0')
                            ));
                            continue;
                        }
                    }
                };
            }
            let resolved = Wpcap {
                findalldevs: sym!(b"pcap_findalldevs\0"),
                freealldevs: sym!(b"pcap_freealldevs\0"),
                open_live: sym!(b"pcap_open_live\0"),
                close: sym!(b"pcap_close\0"),
                datalink: sym!(b"pcap_datalink\0"),
                datalink_val_to_name: sym!(b"pcap_datalink_val_to_name\0"),
                compile: sym!(b"pcap_compile\0"),
                setfilter: sym!(b"pcap_setfilter\0"),
                freecode: sym!(b"pcap_freecode\0"),
                next_ex: sym!(b"pcap_next_ex\0"),
                stats: sym!(b"pcap_stats\0"),
                geterr: sym!(b"pcap_geterr\0"),
                lib_version: sym!(b"pcap_lib_version\0"),
                _lib: lib,
            };
            return Ok((resolved, path));
        }
        // The candidate paths and their OS errors go to the log, not to the
        // player. They only ever matter when Npcap *is* installed and the load
        // failed anyway — a case for whoever reads `logs\*.log`, not for the
        // banner, where they turned one actionable sentence into six lines of
        // red that nobody finishes.
        warn!(
            candidates = %failures.join("; "),
            "no wpcap.dll could be loaded"
        );
        Err(Error::Capture(INSTALL_HINT.to_owned()))
    }

    pub(super) fn version(&self) -> String {
        // SAFETY: `pcap_lib_version` takes no argument and returns a pointer to
        // a string constant owned by the library.
        let version = unsafe { (self.lib_version)() };
        // SAFETY: that constant is NUL-terminated, valid for reads for as long as
        // the module is loaded — which `_lib` guarantees across this call — and is
        // never written by anyone.
        unsafe { cstr(version) }
    }

    /// The library's account of the last failure on `handle`.
    ///
    /// # Safety
    ///
    /// `handle` must be a live `pcap_t` opened by this library.
    unsafe fn error_text(&self, handle: *mut PcapT) -> String {
        // SAFETY: delegated to the caller's contract.
        let text = unsafe { (self.geterr)(handle) };
        // SAFETY: `pcap_geterr` returns a pointer into the handle's own
        // NUL-terminated error buffer, valid until the next call on that handle;
        // this copies it out immediately and nothing else touches the handle in
        // between.
        unsafe { cstr(text) }
    }
}

/// Copies a NUL-terminated C string, treating null as empty.
///
/// # Safety
///
/// If `ptr` is non-null it must point at a NUL-terminated byte sequence that is
/// valid for reads up to and including the terminator, correctly aligned, and not
/// mutated for the duration of the call. Only the null case is checked here —
/// there is no other misuse this function can detect, which is why it is `unsafe`
/// and why a *bounded* buffer should go through [`errbuf_text`] instead.
unsafe fn cstr(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: delegated to the caller's contract, which is exactly
    // `CStr::from_ptr`'s. The result is copied before returning, so nothing
    // borrows the source past the call.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// A libpcap error buffer as text.
///
/// Safe, and that is the point: the scan is bounded by the array, so a libpcap
/// build that ever filled all [`PCAP_ERRBUF_SIZE`] bytes without terminating
/// yields a truncated message instead of a read past the end.
fn errbuf_text(buf: &[c_char; PCAP_ERRBUF_SIZE]) -> String {
    let text: Vec<u8> = buf
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte.cast_unsigned())
        .collect();
    String::from_utf8_lossy(&text).into_owned()
}

/// Rejects a `caplen` that cannot have come from a correctly-laid-out
/// [`PcapPktHdr`].
///
/// This is the canary for the one FFI mistake in this module that would not
/// crash: a `timeval` declared with 64-bit members would shift `caplen` onto the
/// low half of `tv_usec`, so every "length" would be a microsecond count instead
/// — nonsense, but nonsense that reads and slices without faulting. The bound is
/// the snaplen, because libpcap never reports more than it was asked to capture.
///
/// It is a canary, not a proof: a microsecond count happens to land inside
/// `0..=SNAPLEN` about a quarter of the time, so a wrong layout would be caught
/// within the first few packets rather than on the first one. That is enough —
/// the failure is systematic, and one caught packet ends the session with a
/// message naming the layout.
fn is_plausible_caplen(caplen: c_uint) -> bool {
    caplen != 0 && caplen <= SNAPLEN_CAPLEN
}

/// [`SNAPLEN`] on the side of the boundary that reports lengths back: `caplen` is
/// unsigned. One named conversion of a positive constant, so no call site has to
/// spell a cast.
const SNAPLEN_CAPLEN: c_uint = SNAPLEN.cast_unsigned();

// --- Handles ---------------------------------------------------------------

/// One opened, filtered adapter, owned by exactly one capture thread.
///
/// A `pcap_t` is not thread-safe; this type makes that safe by construction:
/// created on the opening thread, moved wholesale into its capture thread,
/// and closed by [`Drop`] on whichever thread ends up owning it last. No two
/// threads ever hold the same one.
pub(super) struct Handle {
    wpcap: Arc<Wpcap>,
    handle: *mut PcapT,
    /// The `\Device\NPF_{...}` name, kept for log lines.
    ///
    /// The one field visible outside this file — [`super::PcapSource::open`]
    /// names each capture thread after it — deliberately the only one: the
    /// raw pointer beside it stays private here, keeping the `unsafe impl
    /// Send` below auditable within one module.
    pub(super) device: String,
    strip: LinkStrip,
}

// SAFETY: `*mut PcapT` is not `Send` by default because libpcap makes no
// thread-safety promise about concurrent use of one handle. This wrapper does
// not make concurrent use possible: a `Handle` is an owning, non-`Clone`,
// non-`Sync` value, so at most one thread can name it at any instant, and the
// only transfer that happens is the single move into the capture thread at
// spawn time. Nothing else in this module retains the raw pointer — and since
// `handle` is private to this file and no other file in the crate so much as
// names a `*mut PcapT`, that last sentence is checkable here rather than
// against the whole `pcap` module tree.
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: `self.handle` was returned non-null by `pcap_open_live`, has
        // not been closed (this is the only close, and `Handle` is not `Clone`),
        // and no receive can be in flight because the thread that would issue it
        // is the thread running this drop.
        unsafe { (self.wpcap.close)(self.handle) };
    }
}

// --- Device enumeration and opening ----------------------------------------

/// Every capture device the driver will admit to, by name.
///
/// An empty list is not an error here — [`super::no_usable_device_error`]
/// turns it into one, since it is also the signature of `AdminOnly=1` and
/// deserves a message that says so.
pub(super) fn enumerate(wpcap: &Wpcap) -> Result<Vec<String>> {
    let mut errbuf = [0 as c_char; PCAP_ERRBUF_SIZE];
    let mut alldevs: *mut PcapIf = std::ptr::null_mut();
    // SAFETY: `alldevs` is a live out-parameter that receives a list owned by
    // the library; `errbuf` is exactly `PCAP_ERRBUF_SIZE` bytes as the API
    // requires and outlives the call. On failure nothing is allocated and
    // `alldevs` is untouched, so the early return below leaks nothing.
    let rc = unsafe { (wpcap.findalldevs)(&mut alldevs, errbuf.as_mut_ptr()) };
    if rc != 0 {
        return Err(Error::Capture(format!(
            "pcap_findalldevs: {}",
            errbuf_text(&errbuf)
        )));
    }

    let mut names = Vec::new();
    let mut cursor = alldevs;
    while !cursor.is_null() {
        // SAFETY: `cursor` walks the library-owned list returned above, which
        // stays valid until the `pcap_freealldevs` below; every `next` is either
        // null or the next node, and it is non-null here.
        let device = unsafe { &*cursor };
        // SAFETY: both fields are NUL-terminated strings owned by that same list
        // (or null, which `cstr` tolerates), unwritten for as long as the list
        // lives. Nothing read here outlives the copy `cstr` makes.
        let name = unsafe { cstr(device.name) };
        // SAFETY: as `name` just above.
        let description = unsafe { cstr(device.description) };
        if !name.is_empty() {
            debug!(device = %name, description = %description, "adapter enumerated");
            names.push(name);
        }
        cursor = device.next;
    }
    // SAFETY: `alldevs` is the list `pcap_findalldevs` allocated above, freed
    // exactly once, and no pointer into it is retained — every string was copied.
    unsafe { (wpcap.freealldevs)(alldevs) };
    Ok(names)
}

/// Opens `device`, checks its link type and installs the first kernel-side
/// filter it will accept.
///
/// The `Err` is the human-readable reason this adapter is unusable, not a fatal
/// error: the caller logs it and moves on to the next one.
pub(super) fn open_device(
    wpcap: &Arc<Wpcap>,
    device: &str,
    filters: &[String],
) -> std::result::Result<Handle, String> {
    let device_c = CString::new(device).map_err(|_| "device name contains a NUL".to_owned())?;
    let mut errbuf = [0 as c_char; PCAP_ERRBUF_SIZE];

    // SAFETY: `device_c` and `errbuf` outlive the call and are, respectively, a
    // NUL-terminated name and a `PCAP_ERRBUF_SIZE` buffer. A null return means
    // failure, with the reason in `errbuf`, and is handled without ever
    // dereferencing the returned pointer.
    let raw = unsafe {
        (wpcap.open_live)(
            device_c.as_ptr(),
            SNAPLEN,
            PROMISCUOUS,
            READ_TIMEOUT_MS,
            errbuf.as_mut_ptr(),
        )
    };
    if raw.is_null() {
        return Err(format!("pcap_open_live: {}", errbuf_text(&errbuf)));
    }
    // Owning wrapper first, so every failure below closes the handle by drop
    // rather than by a `pcap_close` that has to be repeated on each path. The
    // strip is provisional until `pcap_datalink` has answered — it cannot be
    // asked before the handle exists, and the handle must not exist unowned.
    let mut handle = Handle {
        wpcap: Arc::clone(wpcap),
        handle: raw,
        device: device.to_owned(),
        strip: LinkStrip::Fixed(0),
    };

    // SAFETY: `handle.handle` is the live, non-null `pcap_t` just opened, and
    // this thread is its only user until it is moved into a capture thread.
    let datalink = unsafe { (wpcap.datalink)(handle.handle) };
    // SAFETY: `datalink_val_to_name` reads no memory through a pointer; it only
    // takes an integer.
    let datalink_name = unsafe { (wpcap.datalink_val_to_name)(datalink) };
    // SAFETY: what it returns is a library-owned, NUL-terminated string constant
    // — or null, which `cstr` tolerates — never freed and never written.
    let datalink_name = unsafe { cstr(datalink_name) };
    let strip = LinkStrip::try_from(datalink).map_err(|UnsupportedDatalink(dlt)| {
        format!("link type {datalink_name} (DLT {dlt}) cannot be stripped to an IP packet")
    })?;
    handle.strip = strip;

    let installed =
        install_first_accepted(device, filters, |filter| install_filter(&handle, filter))?;

    debug!(
        device = %device,
        link = %datalink_name,
        datalink,
        ?strip,
        filter = %installed,
        "adapter opened and filtered"
    );
    Ok(handle)
}

/// Installs the first filter in `filters` that this adapter's libpcap accepts,
/// and returns it.
///
/// The ladder exists because the preferred filter names `vlan`, a keyword
/// [`super::filter_candidates`] argues for at length and that a libpcap old or
/// unusual enough could refuse. A refused *filter* must not cost the adapter:
/// [`open_device`]'s `Err` makes its caller skip the device entirely, so
/// treating "this build doesn't know `vlan`" as "this adapter is unusable"
/// would turn one blind spot into no capture at all. Falling back to the
/// untagged-only filter leaves such a machine exactly where it was before the
/// VLAN arms were added, which is the worst outcome this is willing to have.
///
/// Generic over the installer for the reason [`super::start_capture_threads`]
/// is generic over the spawner: the ladder is pure control flow, while the
/// thing it walks needs a live `pcap_t` and therefore a machine with Npcap.
fn install_first_accepted<'a>(
    device: &str,
    filters: &'a [String],
    install: impl Fn(&str) -> std::result::Result<(), String>,
) -> std::result::Result<&'a str, String> {
    let mut refused = Vec::new();
    for filter in filters {
        match install(filter) {
            Ok(()) => {
                if !refused.is_empty() {
                    // Only on a fallback, and only once per adapter. It is the
                    // one line that tells a reader of `logs\*.log` why a
                    // machine on tagged VLANs still sees nothing, which is
                    // otherwise indistinguishable from the bug this fixes.
                    warn!(
                        device = %short_device_name(device),
                        refused = %refused.join("; "),
                        installed = %filter,
                        "this adapter's libpcap refused the VLAN-aware kernel filter; \
                         capturing with the untagged-only one instead, so tagged frames \
                         on this adapter stay invisible"
                    );
                }
                return Ok(filter);
            }
            Err(reason) => refused.push(format!("{filter}: {reason}")),
        }
    }
    Err(format!(
        "no kernel filter could be installed — {}",
        refused.join("; ")
    ))
}

/// Compiles one filter expression and copies it into the driver.
///
/// Takes `&Handle` rather than the raw pointer so that its safety argument
/// stays the type's rather than this function's: a `Handle` exists only around
/// a live `pcap_t` that exactly one thread can name, which is what every
/// `unsafe` block below needs and all it needs.
fn install_filter(handle: &Handle, filter: &str) -> std::result::Result<(), String> {
    let wpcap = &handle.wpcap;
    let filter_c = CString::new(filter).map_err(|_| "filter contains a NUL".to_owned())?;
    let mut program = BpfProgram {
        bf_len: 0,
        bf_insns: std::ptr::null_mut(),
    };
    // The compiled program owns a heap allocation from here on, and the shape
    // below is what keeps `pcap_freecode` on *every* path out: nothing between
    // the successful compile and the free returns early. `pcap_compile` leaves
    // `program` untouched when it fails, which is why that one path returns
    // without freeing.
    //
    // SAFETY: `program` is a live, zeroed out-parameter that `pcap_compile` fills
    // on success; `filter_c` is a NUL-terminated expression alive for the call;
    // `handle.handle` is this thread's live `pcap_t`.
    let compiled = unsafe {
        (wpcap.compile)(
            handle.handle,
            &mut program,
            filter_c.as_ptr(),
            OPTIMIZE_FILTER,
            FILTER_NETMASK,
        )
    };
    if compiled != 0 {
        // SAFETY: the handle is live and exclusively this thread's.
        let reason = unsafe { wpcap.error_text(handle.handle) };
        return Err(format!("pcap_compile: {reason}"));
    }
    // SAFETY: the handle is live and exclusively this thread's, and `program` was
    // filled by the successful compile above. `pcap_setfilter` copies the program
    // into the driver, so freeing it immediately afterwards is correct.
    let installed = unsafe { (wpcap.setfilter)(handle.handle, &mut program) };
    let failure = (installed != 0).then(|| {
        // SAFETY: as the compile-failure path above.
        unsafe { wpcap.error_text(handle.handle) }
    });
    // SAFETY: `program` holds the allocation `pcap_compile` made, freed exactly
    // once — this is the only `pcap_freecode` on it, and it is unreachable when
    // the compile failed.
    unsafe { (wpcap.freecode)(&mut program) };
    match failure {
        Some(failure) => Err(format!("pcap_setfilter: {failure}")),
        None => Ok(()),
    }
}

// --- Capture thread --------------------------------------------------------

/// One adapter's receive loop: strip, forward, and watch the driver's drop
/// counter. Parses nothing — [`super::PacketSource::next_segment`] does that,
/// once, for every adapter, on the [`super::PcapSource`] side of the channel.
///
/// Returns when the stop flag is set, when the receiver has gone away, or when
/// the handle reports an error. An error kills only this adapter — the others
/// keep capturing, which matters precisely because this backend opens adapters
/// it has no reason to believe in — but it is no longer *silent*: the last act
/// of a thread ending on an error is an [`AdapterFailure`] to the parent, which
/// is the only thing that can tell an idle adapter's death apart from the death
/// of the one the game was talking through.
pub(super) fn capture_loop(
    handle: Handle,
    packets: &SyncSender<Vec<u8>>,
    failed: &Sender<AdapterFailure>,
    stop: &AtomicBool,
    capture_loss: &AtomicBool,
) {
    let wpcap: &Wpcap = &handle.wpcap;
    // One span per adapter thread, so every line this thread and `poll_drops`
    // emit carries the device without any of them repeating the field. Held as a
    // guard rather than an `.instrument()` because nothing in here is `async`.
    let _adapter = info_span!("adapter", device = %short_device_name(&handle.device)).entered();
    let mut delivered: u64 = 0;
    let mut unstrippable: u64 = 0;
    let mut dropped: c_uint = 0;
    let mut overflowed: u64 = 0;
    let mut error: Option<String> = None;

    while !stop.load(Ordering::Relaxed) {
        let mut header: *mut PcapPktHdr = std::ptr::null_mut();
        let mut data: *const u8 = std::ptr::null();
        // SAFETY: `handle.handle` is this thread's exclusive live `pcap_t`.
        // `header` and `data` are out-parameters the library points at its own
        // buffers; on return code 1 both are non-null and the bytes they
        // describe stay valid until the next call on this handle, which is why
        // the copy below happens before looping. Any other return code leaves
        // them unspecified and is handled without dereferencing.
        let rc = unsafe { (wpcap.next_ex)(handle.handle, &mut header, &mut data) };
        match rc {
            NEXT_EX_OK => {
                if header.is_null() || data.is_null() {
                    continue;
                }
                // SAFETY: return code 1 guarantees `header` points at a
                // fully-written `PcapPktHdr` owned by the library.
                let caplen = unsafe { (*header).caplen };
                if !is_plausible_caplen(caplen) {
                    error = Some(implausible_caplen_error(caplen));
                    break;
                }
                // Infallible on every target this ships to (`c_uint` and `usize`
                // are both 32-bit-or-wider), and bounded by `SNAPLEN` in any case
                // — but spelled as a conversion rather than a cast so the bound is
                // the code's rather than the reader's.
                let caplen = usize::try_from(caplen).unwrap_or(0);
                // SAFETY: return code 1 guarantees `data` points at `caplen`
                // readable bytes (checked plausible just above, and bounded by
                // the snaplen the handle was opened with). The slice is consumed
                // — copied into an owned `Vec` — before the next `pcap_next_ex`
                // on this handle invalidates it.
                let frame = unsafe { std::slice::from_raw_parts(data, caplen) };
                delivered += 1;
                let Some(ip) = handle.strip.ip_bytes(frame) else {
                    // A frame too short to hold its own link header, or with a
                    // VLAN stack deeper than this strips. Counted separately so
                    // that "the adapter delivers, but nothing survives the
                    // strip" is legible in this thread's closing line.
                    unstrippable += 1;
                    continue;
                };
                let forwarded = forward(packets, ip, capture_loss, &mut overflowed);
                if matches!(forwarded, Forwarded::SourceGone) {
                    break; // The source is gone; nothing left to feed.
                }
                if delivered.is_multiple_of(STATS_EVERY_PACKETS) {
                    poll_drops(wpcap, &handle, &mut dropped, capture_loss);
                }
            }
            // Read timeout. Normal, and the only moment an idle adapter gets to
            // look at the stop flag or at its drop counter.
            NEXT_EX_TIMEOUT => poll_drops(wpcap, &handle, &mut dropped, capture_loss),
            _ => {
                // SAFETY: the handle is still live and exclusively this thread's;
                // `pcap_geterr` reads its internal error buffer.
                error = Some(unsafe { wpcap.error_text(handle.handle) });
                break;
            }
        }
    }

    match error {
        Some(error) => {
            warn!(
                delivered,
                unstrippable,
                dropped,
                overflowed,
                error = %error,
                "adapter capture ended on an error"
            );
            // The last thing this thread does, and the only news of it the
            // parent can get: it is parked on a funnel that this thread's
            // siblings keep alive, so the disconnect it would otherwise wait
            // for never arrives. `delivered` travels with it because that is
            // what decides whether this death is the session's — every frame
            // counted there passed this adapter's kernel filter, which admits
            // the game server's source port and nothing else. A failed
            // send means the source is already gone, which is the one case
            // where nobody needs telling.
            let _ = failed.send(AdapterFailure {
                device: handle.device.clone(),
                delivered,
                error,
            });
        }
        None => debug!(
            delivered,
            unstrippable, dropped, overflowed, "adapter capture ended"
        ),
    }
    // `handle` drops here, on the thread that owned it, closing the `pcap_t`.
}

/// What [`forward`] did with a frame.
enum Forwarded {
    Queued,
    /// The funnel was full. Counted and flagged, never waited on.
    Dropped,
    SourceGone,
}

/// Hands one stripped IP packet to the funnel, and decides what happens when
/// the funnel is full.
///
/// `try_send`, and a dropped frame, rather than a blocking send, for two
/// separate reasons. The first is the one the old unbounded channel's comment
/// gave for having no bound at all: parking here parks this thread *outside*
/// `pcap_next_ex`, where the kernel ring behind it keeps filling, so a consumer
/// stall becomes driver-side loss — which is strictly worse, being unbounded
/// and invisible. The second is teardown: the receiver is a field of
/// [`super::PcapSource`], and a struct's fields drop *after* its [`Drop`] body,
/// which joins these threads. A producer parked in `send` would therefore be
/// joined by a thread that is holding the only thing that could ever wake it.
///
/// Dropping the newest frame costs nothing the resync would not discard
/// anyway, and it is reported through the same `capture_loss` flag the
/// driver's own `ps_drop` uses: a hole is a hole, and `app::ingest` already
/// turns that flag into a counted, lossless re-anchor rather than a stall.
fn forward(
    packets: &SyncSender<Vec<u8>>,
    ip: &[u8],
    capture_loss: &AtomicBool,
    overflowed: &mut u64,
) -> Forwarded {
    match packets.try_send(ip.to_vec()) {
        Ok(()) => Forwarded::Queued,
        Err(TrySendError::Full(frame)) => {
            *overflowed += 1;
            capture_loss.store(true, Ordering::Relaxed);
            if *overflowed == 1 {
                warn_funnel_full(frame.len());
            }
            Forwarded::Dropped
        }
        Err(TrySendError::Disconnected(_)) => Forwarded::SourceGone,
    }
}

/// Said once per thread rather than once per dropped frame, and out of line
/// like the other rare reports here: a full funnel is either absent or
/// sustained, and a log line per drop is the one thing guaranteed to make a
/// congested one worse. The running total goes out in the closing line.
#[cold]
#[inline(never)]
fn warn_funnel_full(bytes: usize) {
    warn!(
        bytes,
        "the capture funnel is full; dropping frames and asking the pipeline to resync — \
         the byte stream has a hole in it"
    );
}

/// The one FFI mistake in this module that would not crash, reported out of line:
/// it happens at most once per adapter, on the path that runs per captured packet.
#[cold]
#[inline(never)]
fn implausible_caplen_error(caplen: c_uint) -> String {
    format!(
        "pcap reported a {caplen}-byte capture, which is impossible at a snaplen of \
         {SNAPLEN} — the pcap_pkthdr layout is wrong, so this adapter's data cannot \
         be trusted"
    )
}

/// Reads the driver's counters and reports any *new* drop as capture loss.
///
/// `ps_drop` is packets the kernel had to throw away because the capture ring
/// was full. A passive tap never sees already-ACKed bytes again, so a hole left
/// this way can never be filled by a retransmission — which is exactly the
/// condition [`crate::capture::PacketSource::take_capture_loss`] exists to
/// report, and the capture loop turns into a resync instead of a permanent stall.
fn poll_drops(wpcap: &Wpcap, handle: &Handle, previous: &mut c_uint, capture_loss: &AtomicBool) {
    let mut stats = PcapStat::default();
    // SAFETY: `stats` is a live, fully-initialized `pcap_stat` of the layout the
    // library expects (with room for the Windows-only tail it may or may not
    // write), and `handle.handle` is this thread's exclusive live `pcap_t`. A
    // non-zero return means the counters were not written, so they are only read
    // on success.
    if unsafe { (wpcap.stats)(handle.handle, &mut stats) } != 0 {
        return;
    }
    // Wrapping: these are 32-bit counters that will roll over on a long-lived
    // handle, and a rollover must read as "some loss", never as a huge negative.
    let delta = stats.ps_drop.wrapping_sub(*previous);
    if delta == 0 {
        return;
    }
    *previous = stats.ps_drop;
    capture_loss.store(true, Ordering::Relaxed);
    warn_capture_loss(delta, stats);
}

/// The rare half of [`poll_drops`], out of line: `poll_drops` runs on every read
/// timeout and every 512th packet, and the counters usually have not moved.
/// The adapter is named by the enclosing span, not by a field here.
#[cold]
#[inline(never)]
fn warn_capture_loss(lost: c_uint, stats: PcapStat) {
    warn!(
        lost,
        total = stats.ps_drop,
        received = stats.ps_recv,
        "the capture driver dropped packets — the byte stream has a hole in it"
    );
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_full_funnel_drops_the_frame_and_flags_capture_loss_without_parking_the_thread() {
        let (sender, packets) = sync_channel(1);
        let sender = Arc::new(sender);
        let capture_loss = Arc::new(AtomicBool::new(false));
        let mut overflowed = 0;
        assert!(matches!(
            forward(&sender, b"first", &capture_loss, &mut overflowed),
            Forwarded::Queued
        ));
        assert_eq!(overflowed, 0);
        assert!(!capture_loss.load(Ordering::Relaxed));

        // The funnel is full and nothing is draining it. This is the moment
        // the unbounded channel grew instead — one frame is up to SNAPLEN,
        // and nothing charged any of it against the pipeline's byte budget
        // until the consumer had already dequeued it — and the moment a
        // blocking bounded send would park this thread outside the driver.
        // Run on its own thread so that parking fails the test instead of
        // hanging the suite.
        let (done, outcome) = std::sync::mpsc::channel();
        std::thread::spawn({
            let sender = Arc::clone(&sender);
            let capture_loss = Arc::clone(&capture_loss);
            move || {
                let mut overflowed = 0;
                let forwarded = forward(&sender, b"second", &capture_loss, &mut overflowed);
                let _ = done.send((matches!(forwarded, Forwarded::Dropped), overflowed));
            }
        });
        let (was_dropped, counted) = outcome
            .recv_timeout(Duration::from_secs(5))
            .expect("a full funnel must not park a capture thread between two pcap_next_ex calls");

        assert!(was_dropped, "the newest frame is what gives way");
        assert_eq!(counted, 1, "and it is counted, not silently gone");
        assert!(
            capture_loss.load(Ordering::Relaxed),
            "a dropped frame is a hole in the byte stream, and the pipeline must resync \
             rather than wait for a retransmission a passive tap never sees"
        );
        // Exactly what it accepted, and no more: the bound is a bound.
        assert_eq!(
            packets.try_recv().expect("the first frame"),
            b"first".to_vec()
        );
        assert!(packets.try_recv().is_err());

        drop(packets);
        assert!(matches!(
            forward(&sender, b"third", &capture_loss, &mut overflowed),
            Forwarded::SourceGone
        ));
    }

    #[test]
    fn a_libpcap_that_refuses_the_vlan_keyword_keeps_its_adapter_on_the_plain_filter() {
        // The shape of an older or unusual libpcap: `pcap_compile` rejects the
        // `vlan` keyword. Before the ladder, `open_device` returned that as the
        // adapter's refusal reason and `open` skipped the device — so a fix for
        // a blind spot on tagged frames would have cost such a machine its
        // capture entirely. Here it costs it only the VLAN arms.
        let filters = super::super::filter_candidates(NonZeroU16::new(3333).expect("not zero"));
        let attempted = std::cell::RefCell::new(Vec::new());
        let installed = install_first_accepted(r"\Device\NPF_{OLD}", &filters, |filter| {
            attempted.borrow_mut().push(filter.to_owned());
            if filter.contains("vlan") {
                return Err("syntax error".to_owned());
            }
            Ok(())
        })
        .expect("the plain filter is still installable, so the adapter is still usable");

        assert_eq!(installed, filters[1], "it falls back exactly one rung");
        assert_eq!(
            attempted.borrow().len(),
            2,
            "and only after the capable one was actually tried"
        );
        assert_eq!(attempted.borrow()[0], filters[0]);
    }

    #[test]
    fn an_adapter_that_takes_the_vlan_filter_never_sees_the_plain_one() {
        let filters = super::super::filter_candidates(NonZeroU16::new(3333).expect("not zero"));
        let attempted = std::cell::Cell::new(0usize);
        let installed = install_first_accepted(r"\Device\NPF_{MODERN}", &filters, |_| {
            attempted.set(attempted.get() + 1);
            Ok(())
        })
        .expect("a modern libpcap takes the first rung");
        assert_eq!(installed, filters[0]);
        assert_eq!(
            attempted.get(),
            1,
            "the ladder stops at the first rung that holds"
        );
    }

    #[test]
    fn an_adapter_that_refuses_every_filter_is_still_refused_and_says_why_for_each() {
        // The other half of the fail-safe: falling back must not become
        // swallowing. A device whose filters all fail is unusable, and the
        // caller's zero-usable-device message is built out of these reasons.
        let filters = super::super::filter_candidates(NonZeroU16::new(3333).expect("not zero"));
        let reason = install_first_accepted(r"\Device\NPF_{GONE}", &filters, |_| {
            Err("the handle is dead".to_owned())
        })
        .expect_err("no filter installed means no usable adapter");
        assert!(reason.contains("the handle is dead"), "{reason}");
        assert!(
            reason.contains("vlan"),
            "the rung that was tried first: {reason}"
        );
    }

    #[test]
    fn a_caplen_outside_the_snaplen_is_rejected_as_a_pkthdr_layout_error() {
        assert!(is_plausible_caplen(1));
        assert!(is_plausible_caplen(SNAPLEN_CAPLEN));
        assert!(!is_plausible_caplen(0));
        assert!(!is_plausible_caplen(SNAPLEN_CAPLEN + 1));
        // What a 64-bit `timeval` would produce: a microsecond count read as a
        // length, mostly out of bounds and caught here (see `plausible_caplen`
        // for why this is a canary, not a proof).
        assert!(!is_plausible_caplen(999_999));
        assert!(!is_plausible_caplen(0)); // a zero-length "packet" cannot exist
    }

    #[test]
    fn the_windows_pcap_pkthdr_is_sixteen_bytes_because_its_timeval_is_two_longs() {
        // The single most dangerous constant in this file: a 24-byte header
        // would put `caplen` where `tv_usec` is, reporting nonsense lengths
        // instead of crashing. The real gate is the `const _` beside the
        // struct (this build lane is feature- and OS-gated, so a release
        // build never evaluates a test); kept because it costs nothing and
        // states the fact where the test suite will show it.
        assert_eq!(size_of::<PcapPktHdr>(), 16);
        assert_eq!(size_of::<PcapStat>(), 24);
    }

    #[test]
    fn an_error_buffer_is_read_up_to_its_terminator_and_no_further() {
        let mut errbuf = [0 as c_char; PCAP_ERRBUF_SIZE];
        for (slot, byte) in errbuf.iter_mut().zip(b"pcap_open_live failed\0junk") {
            *slot = byte.cast_signed();
        }
        assert_eq!(errbuf_text(&errbuf), "pcap_open_live failed");
        // A buffer filled to the last byte without a terminator is truncated,
        // not read past — the reason this is safe taking the array by reference.
        let unterminated = [b'x'.cast_signed(); PCAP_ERRBUF_SIZE];
        assert_eq!(errbuf_text(&unterminated).len(), PCAP_ERRBUF_SIZE);
        assert_eq!(errbuf_text(&[0 as c_char; PCAP_ERRBUF_SIZE]), "");
    }

    #[test]
    fn no_wpcap_candidate_is_a_relative_name() {
        // The regression this pins is one character wide — a `"wpcap.dll"` back
        // in the list — and its consequence is arbitrary code at high
        // integrity, so it is asserted rather than left to the comment above
        // `SEARCH_FLAGS`. A relative name also makes `load_with_flags` fail
        // outright, but "the capture stopped working" is not the failure that
        // needs catching here.
        let candidates = dll_candidates();
        for path in &candidates {
            assert!(
                path.is_absolute(),
                "{} must be absolute: a relative name resolves against the exe's own directory",
                path.display()
            );
            assert!(
                path.starts_with(crate::system32::directory()),
                "{} must be under the system directory",
                path.display()
            );
        }
        assert_eq!(
            candidates[0].parent().and_then(|dir| dir.file_name()),
            Some(std::ffi::OsStr::new("Npcap")),
            "Npcap's private directory is present on every install, so it is asked first"
        );
    }

    #[test]
    fn the_search_flags_still_load_a_system_dll_by_absolute_path() {
        // The risk in narrowing the search order is that it narrows past the
        // real `wpcap.dll` too, and this machine cannot say — it has no Npcap,
        // so `the_tap_opens_on_this_machine_without_elevation` is unrunnable
        // here. `version.dll` stands in: same directory, same flags, same call.
        // It proves the combination resolves an absolute path under
        // `LOAD_LIBRARY_SEARCH_SYSTEM32`; it cannot prove the
        // `LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR` half, which only matters for
        // `Packet.dll` beside a real `wpcap.dll`.
        let path = crate::system32::directory().join("version.dll");
        // SAFETY: `version.dll` is a Windows system library already loaded into
        // most processes; its entry point initializes its own state only. The
        // handle is dropped immediately and no symbol is resolved from it.
        let loaded =
            unsafe { libloading::os::windows::Library::load_with_flags(&path, SEARCH_FLAGS) };
        assert!(
            loaded.is_ok(),
            "{} did not load under SEARCH_FLAGS: {:?}",
            path.display(),
            loaded.err()
        );
    }
}
