//! The `wpcap.dll` boundary: the transcribed ABI, the handle lifecycle, and the
//! per-adapter receive loop.
//!
//! # Why these three are one module
//!
//! One invariant, checkable in one place: the
//! [`unsafe impl Send for Handle`](Handle) below claims nothing but the owning
//! `Handle` retains the `*mut PcapT` — auditable only because that field, and
//! every `*mut PcapT` in the crate, is private to this file. Splitting
//! `open_device` from `capture_loop` (`docs/tech-debt/24-proj.md`) would make
//! it `pub(super)` and spread the check over three files. [`super::link`]
//! carries no pointer; it is the one that left.

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, SyncSender, TrySendError};

use tracing::{debug, info_span, warn};

use super::link::{LinkStrip, UnsupportedDatalink};
use super::{AdapterFailure, short_device_name};
use crate::error::{Error, Result};

/// Size libpcap requires of every error buffer: it writes up to this many
/// bytes without asking.
const PCAP_ERRBUF_SIZE: usize = 256;

/// Bytes captured per packet.
///
/// Not the wire MTU: RSC/LRO coalescing hands the stack one "packet" made of
/// many wire frames, measured here at **48 870 bytes**, 32 times the MTU. A
/// too-small snaplen silently truncates rather than failing, reaching
/// `parse_segment` as a malformed packet, so this is libpcap's documented
/// 262 144 ceiling.
pub(super) const SNAPLEN: c_int = 262_144;

/// Read timeout, in milliseconds: bounds how long a capture thread sits in
/// `pcap_next_ex` without seeing the stop flag, and so how long teardown
/// waits. Also the `pcap_stats` poll cadence.
pub(super) const READ_TIMEOUT_MS: c_int = 200;

/// Promiscuous mode off: traffic not addressed to this host would only
/// multiply what the kernel filter chews through, for no gain.
const PROMISCUOUS: c_int = 0;

/// `pcap_compile`'s optimizer flag. On, because the filter runs per packet in
/// the driver and is compiled once.
const OPTIMIZE_FILTER: c_int = 1;

/// Netmask for `pcap_compile`. Zero is correct, not merely tolerated: it feeds
/// only broadcast-relative primitives, which no rung of
/// [`super::filter_candidates`] uses.
const FILTER_NETMASK: c_uint = 0;

/// `pcap_next_ex` return codes. Anything else is an error on the handle.
const NEXT_EX_OK: c_int = 1;
const NEXT_EX_TIMEOUT: c_int = 0;

/// Packets between two `pcap_stats` polls. Read timeouts cover an idle
/// adapter; this covers a busy one, where timeouts may never happen.
const STATS_EVERY_PACKETS: u64 = 512;

// Transcribed from `pcap.h` and **verified against a real run** on Npcap 1.75.
// Fields never read here are present because the library writes through the
// pointer and the layout has to match.

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
/// The timestamp is a Windows `struct timeval`: two **32-bit** longs, so 16
/// bytes, not the 24 a 64-bit `time_t` would give. Wrong is not a crash —
/// `caplen` would read the timestamp's tail — hence [`is_plausible_caplen`].
#[repr(C)]
#[derive(Clone, Copy)]
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
/// The three Windows-only counters, which only `pcap_stats_ex` fills, are
/// declared so the library cannot write past this struct whichever build
/// answers.
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
// one would be *shipped*: a release build on this feature lane never evaluates
// a `#[cfg(test)]` assertion. At 24 bytes `PcapPktHdr`'s `caplen` would land on
// the low half of `tv_usec` and report nonsense instead of crashing; `PcapStat`
// must have room for the Windows-only counter tail or the library writes past
// it. Both are `#[repr(C)]`, so a failure here means the transcription is
// wrong, never that the number should be updated.
const _: () = {
    assert!(size_of::<PcapPktHdr>() == 16);
    assert!(size_of::<PcapStat>() == 24);
};

/// Opaque `pcap_t`.
type PcapT = c_void;

/// The subset of `wpcap.dll` this backend uses, resolved once at open time.
///
/// Every entry point is a stable libpcap export, and a missing symbol is caught
/// here, at load, not at the call. Every field stays private to this module:
/// each of the thirteen was verified against libpcap's ABI. `pub(super)` only
/// because the parent's `open` loads it and passes it back in.
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
/// `"wpcap.dll"` must never go back in that list.
///
/// A bare name means `LoadLibraryExW(name, NULL, 0)`, whose **first** search
/// entry is the running executable's directory. This exe is manifested
/// `requireAdministrator` and players run it from `%USERPROFILE%\Downloads`,
/// writable by any medium-integrity process of the same user: a `wpcap.dll`
/// dropped there gets its `DllMain` run at high integrity — privilege
/// escalation by writing one file and waiting.
///
/// Naming any `LOAD_LIBRARY_SEARCH_*` flag replaces that order outright;
/// `..._DLL_LOAD_DIR` also resolves `Packet.dll` beside Npcap's private
/// `wpcap.dll`, which an absolute path alone would look for in the
/// *application* directory. Both require a fully qualified path.
///
/// `u32` because `libloading`'s `LOAD_LIBRARY_FLAGS` alias is private.
const SEARCH_FLAGS: u32 = libloading::os::windows::LOAD_LIBRARY_SEARCH_SYSTEM32
    | libloading::os::windows::LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR;

/// Where `wpcap.dll` is looked for, in order.
///
/// Npcap's private directory first: present on every install, while the
/// System32 copy exists only if WinPcap-compatible mode was kept. Which one
/// answered is logged, naming the install mode of a machine we cannot inspect.
///
/// The directory comes from `GetSystemDirectoryW`, not `%SystemRoot%`: an
/// elevated process inherits its environment from the medium-integrity one that
/// requested elevation, so that variable is as trustworthy as the attacker in
/// [`SEARCH_FLAGS`].
fn dll_candidates() -> [PathBuf; 2] {
    let system = crate::system32::directory();
    [
        system.join("Npcap").join("wpcap.dll"),
        system.join("wpcap.dll"),
    ]
}

/// What to tell a player who has no Npcap at all.
///
/// Re-exported: the sentence names one exact build and `ui::statusbar`'s
/// Download button fetches that same build from
/// [`crate::npcap::INSTALLER_URL`], so [`crate::npcap`] carries both.
pub(super) use crate::npcap::INSTALL_HINT;

impl Wpcap {
    pub(super) fn load() -> Result<(Self, PathBuf)> {
        let mut failures = Vec::new();
        for path in dll_candidates() {
            // SAFETY: `load_with_flags` runs the DLL's entry point, which for
            // `wpcap.dll` only initializes its own state. The symbols below are
            // copied out as bare function pointers, valid exactly as long as
            // `_lib` — stored in the same struct — keeps the module loaded. An
            // absent or invalid image returns `Err`. Flags argued at
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
            // Scope-local and unhygienic on purpose: the body reads `lib`,
            // `path` and `failures` from this loop, and its `continue` abandons
            // the whole DLL candidate, all twelve sibling resolutions included.
            // That caller-scope `continue` is why this is a macro.
            macro_rules! sym {
                ($name:literal) => {
                    // SAFETY: `get` resolves one exported symbol against the
                    // signature transcribed above; a missing name returns `Err`
                    // rather than being called. The copied-out function pointer
                    // is valid while the module stays loaded, which `_lib` owns.
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
        // Candidate paths and OS errors go to the log, not the player: they
        // only matter when Npcap *is* installed and the load failed anyway, and
        // in the banner they buried the one actionable sentence.
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
        // SAFETY: that constant is NUL-terminated, never written, and valid
        // while the module is loaded — which `_lib` guarantees across this call.
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
        // NUL-terminated buffer, valid until the next call on that handle, and
        // this copies it out first.
        unsafe { cstr(text) }
    }
}

#[cfg(test)]
impl Wpcap {
    /// Builds a table from caller-supplied functions, pinned by a `Library` the
    /// caller already holds.
    ///
    /// The `_lib` field's contract is "these pointers are valid while this stays
    /// loaded", and a test satisfies it by handing over a real, already-loaded
    /// system library — see `dll_candidates`' own test
    /// ([`tests::the_search_flags_still_load_a_system_dll_by_absolute_path`]),
    /// which loads one for the same reason. The `unsafe extern "C" fn` items a
    /// test passes in are `'static`, so they outlive any `Library` regardless;
    /// the field is doing no work for them and that is fine — it is still the
    /// one true precondition, so it still has to be satisfied.
    ///
    /// Only `next_ex`, `stats`, `geterr` and `close` are parameters: those are
    /// the only four entry points [`capture_loop`] and [`Handle`]'s [`Drop`]
    /// reach. The other nine are wired to stubs that panic if called — see
    /// `tests::unreachable_*` — which is itself an assertion that this loop
    /// touches nothing else.
    ///
    /// Private, not `pub(super)`: every caller lives in `tests`, a descendant
    /// of this module, so a wider visibility would only leak `PcapPktHdr` and
    /// `PcapStat` — both deliberately private to this file — into the
    /// signature for no caller that needs it.
    fn from_fns(
        lib: libloading::Library,
        next_ex: unsafe extern "C" fn(*mut PcapT, *mut *mut PcapPktHdr, *mut *const u8) -> c_int,
        stats: unsafe extern "C" fn(*mut PcapT, *mut PcapStat) -> c_int,
        geterr: unsafe extern "C" fn(*mut PcapT) -> *mut c_char,
        close: unsafe extern "C" fn(*mut PcapT),
    ) -> Self {
        Self {
            _lib: lib,
            findalldevs: tests::unreachable_findalldevs,
            freealldevs: tests::unreachable_freealldevs,
            open_live: tests::unreachable_open_live,
            close,
            datalink: tests::unreachable_datalink,
            datalink_val_to_name: tests::unreachable_datalink_val_to_name,
            compile: tests::unreachable_compile,
            setfilter: tests::unreachable_setfilter,
            freecode: tests::unreachable_freecode,
            next_ex,
            stats,
            geterr,
            lib_version: tests::unreachable_lib_version,
        }
    }
}

/// Copies a NUL-terminated C string, treating null as empty.
///
/// # Safety
///
/// If `ptr` is non-null it must point at a NUL-terminated byte sequence, valid
/// for reads through the terminator, aligned, and unmutated for the call. Only
/// the null case is checkable here, which is why a *bounded* buffer goes
/// through [`errbuf_text`] instead.
unsafe fn cstr(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: delegated to the caller's contract, which is exactly
    // `CStr::from_ptr`'s. The result is copied, so nothing borrows the source
    // past the call.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

/// A libpcap error buffer as text.
///
/// Safe, and that is the point: the scan is bounded by the array, so a build
/// that filled all [`PCAP_ERRBUF_SIZE`] bytes without terminating yields a
/// truncated message instead of a read past the end.
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
/// The canary for the one FFI mistake here that would not crash: a `timeval`
/// with 64-bit members shifts `caplen` onto the low half of `tv_usec`, so every
/// "length" becomes a microsecond count — nonsense that reads and slices
/// without faulting. The bound is the snaplen, which libpcap never exceeds.
///
/// A canary, not a proof: such a count lands inside `0..=SNAPLEN` about a
/// quarter of the time, so a wrong layout is caught within the first few
/// packets rather than the first. Enough, because the failure is systematic.
fn is_plausible_caplen(caplen: c_uint) -> bool {
    caplen != 0 && caplen <= SNAPLEN_CAPLEN
}

/// [`SNAPLEN`] on the side of the boundary that reports lengths back, where it
/// is unsigned: one named conversion, so no call site spells a cast.
const SNAPLEN_CAPLEN: c_uint = SNAPLEN.cast_unsigned();

/// One opened, filtered adapter, owned by exactly one capture thread.
///
/// A `pcap_t` is not thread-safe, so this type makes that safe by construction:
/// created on the opening thread, moved wholesale into its capture thread,
/// closed by [`Drop`] on whichever thread owns it last. No two threads ever
/// hold the same one.
pub(super) struct Handle {
    wpcap: Arc<Wpcap>,
    handle: *mut PcapT,
    /// The `\Device\NPF_{...}` name, for log lines and the capture thread's
    /// name. Deliberately the only field visible outside this file: the raw
    /// pointer beside it stays private, which is what keeps the `unsafe impl
    /// Send` below auditable within one module.
    pub(super) device: String,
    strip: LinkStrip,
}

// SAFETY: `*mut PcapT` is not `Send` by default because libpcap makes no
// thread-safety promise about concurrent use of one handle, and this wrapper
// does not make concurrent use possible: a `Handle` is an owning, non-`Clone`,
// non-`Sync` value, so at most one thread can name it at any instant, and the
// only transfer is the single move into the capture thread at spawn time.
// Nothing else retains the raw pointer — checkable here, because `handle` is
// private to this file and no other file in the crate names a `*mut PcapT`.
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: `self.handle` was returned non-null by `pcap_open_live` and
        // is unclosed (this is the only close, and `Handle` is not `Clone`); no
        // receive can be in flight, because the thread that would issue it is
        // the one running this drop.
        unsafe { (self.wpcap.close)(self.handle) };
    }
}

/// Every capture device the driver will admit to, by name.
///
/// An empty list is not an error here: it is also the signature of
/// `AdminOnly=1`, so [`super::no_usable_device_error`] turns it into one that
/// says which.
pub(super) fn enumerate(wpcap: &Wpcap) -> Result<Vec<String>> {
    let mut errbuf = [0 as c_char; PCAP_ERRBUF_SIZE];
    let mut alldevs: *mut PcapIf = std::ptr::null_mut();
    // SAFETY: `alldevs` is a live out-parameter receiving a library-owned list;
    // `errbuf` is exactly `PCAP_ERRBUF_SIZE` bytes and outlives the call. On
    // failure nothing is allocated, so the early return leaks nothing.
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
        // SAFETY: `cursor` walks the library-owned list above, valid until the
        // `pcap_freealldevs` below, and is non-null here.
        let device = unsafe { &*cursor };
        // SAFETY: both fields are NUL-terminated strings owned by that list (or
        // null, which `cstr` tolerates), unwritten while it lives, and copied.
        let name = unsafe { cstr(device.name) };
        // SAFETY: as `name` just above.
        let description = unsafe { cstr(device.description) };
        if !name.is_empty() {
            debug!(device = %name, description = %description, "adapter enumerated");
            names.push(name);
        }
        cursor = device.next;
    }
    // SAFETY: `alldevs` is the list allocated above, freed exactly once, and no
    // pointer into it is retained — every string was copied.
    unsafe { (wpcap.freealldevs)(alldevs) };
    Ok(names)
}

/// Opens `device`, checks its link type and installs the first kernel-side
/// filter it will accept.
///
/// The `Err` is the reason this adapter is unusable, not a fatal error: the
/// caller logs it and moves on.
pub(super) fn open_device(
    wpcap: &Arc<Wpcap>,
    device: &str,
    filters: &[String],
) -> std::result::Result<Handle, String> {
    let device_c = CString::new(device).map_err(|_| "device name contains a NUL".to_owned())?;
    let mut errbuf = [0 as c_char; PCAP_ERRBUF_SIZE];

    // SAFETY: `device_c` and `errbuf` outlive the call and are, respectively, a
    // NUL-terminated name and a `PCAP_ERRBUF_SIZE` buffer. A null return is
    // failure, handled without dereferencing it.
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
    // rather than a `pcap_close` repeated on each path. The strip is
    // provisional because `pcap_datalink` cannot be asked before the handle
    // exists, and the handle must not exist unowned.
    let mut handle = Handle {
        wpcap: Arc::clone(wpcap),
        handle: raw,
        device: device.to_owned(),
        strip: LinkStrip::Fixed(0),
    };

    // SAFETY: `handle.handle` is the live `pcap_t` just opened, and this thread
    // is its only user until it moves into a capture thread.
    let datalink = unsafe { (wpcap.datalink)(handle.handle) };
    // SAFETY: `datalink_val_to_name` takes an integer and reads no memory
    // through a pointer.
    let datalink_name = unsafe { (wpcap.datalink_val_to_name)(datalink) };
    // SAFETY: what it returns is a library-owned, NUL-terminated string
    // constant — or null, which `cstr` tolerates — never freed, never written.
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
/// A refused *filter* must not cost the adapter: [`open_device`]'s `Err` makes
/// its caller skip the device entirely, so treating a libpcap that cannot
/// compile `vlan` as an unusable adapter would turn one blind spot into no
/// capture at all.
///
/// Generic over the installer so this is testable: the ladder is pure control
/// flow, while what it walks needs a live `pcap_t` and a machine with Npcap.
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
                    // Only on a fallback, once per adapter: the one line that
                    // tells a log reader why a machine on tagged VLANs still
                    // sees nothing.
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
/// Takes `&Handle` rather than the raw pointer so the safety argument stays the
/// type's: a `Handle` exists only around a live `pcap_t` that exactly one thread
/// can name, which is what every `unsafe` block below needs and all it needs.
fn install_filter(handle: &Handle, filter: &str) -> std::result::Result<(), String> {
    let wpcap = &handle.wpcap;
    let filter_c = CString::new(filter).map_err(|_| "filter contains a NUL".to_owned())?;
    let mut program = BpfProgram {
        bf_len: 0,
        bf_insns: std::ptr::null_mut(),
    };
    // Nothing between a successful compile and the free below returns early, so
    // `pcap_freecode` is on every path out. The compile-failure path returns
    // without freeing because `pcap_compile` leaves `program` untouched.
    //
    // SAFETY: `program` is a live, zeroed out-parameter `pcap_compile` fills on
    // success; `filter_c` is NUL-terminated and alive for the call;
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
    // SAFETY: the handle is live and exclusively this thread's, and `program`
    // was filled by the successful compile above. `pcap_setfilter` copies into
    // the driver, so freeing immediately afterwards is correct.
    let installed = unsafe { (wpcap.setfilter)(handle.handle, &mut program) };
    let failure = (installed != 0).then(|| {
        // SAFETY: as the compile-failure path above.
        unsafe { wpcap.error_text(handle.handle) }
    });
    // SAFETY: `program` holds the allocation `pcap_compile` made, freed exactly
    // once — the only `pcap_freecode` on it, unreachable if the compile failed.
    unsafe { (wpcap.freecode)(&mut program) };
    match failure {
        Some(failure) => Err(format!("pcap_setfilter: {failure}")),
        None => Ok(()),
    }
}

/// What one adapter's receive loop tallied before it ended.
///
/// Production discards this — the value already travels the closing
/// `warn!`/`debug!` line, [`capture_loop`]'s only caller ends the statement
/// with `;` — but a test needs `unstrippable` and `overflowed` without
/// scraping a log line, so [`capture_loop`] hands all three back instead of
/// only logging them. Constructing and returning this changes no behaviour.
#[allow(dead_code, reason = "read by capture::pcap::sys::tests only")]
pub(super) struct LoopCounters {
    pub(super) delivered: u64,
    pub(super) unstrippable: u64,
    pub(super) overflowed: u64,
}

/// One adapter's receive loop: strip, forward, and watch the driver's drop
/// counter. Parses nothing — [`super::PacketSource::next_segment`] does that
/// once for every adapter, on the other side of the channel.
///
/// Returns when the stop flag is set, the receiver has gone, or the handle
/// errors. An error kills only this adapter, but not silently: the last act of
/// a thread ending that way is an [`AdapterFailure`] to the parent, the only
/// thing that tells an idle adapter's death from that of the one the game was
/// talking through. The [`LoopCounters`] it hands back are for a test to read;
/// see that type's doc for why production ignores them.
pub(super) fn capture_loop(
    handle: Handle,
    packets: &SyncSender<Vec<u8>>,
    failed: &Sender<AdapterFailure>,
    stop: &AtomicBool,
    capture_loss: &AtomicBool,
) -> LoopCounters {
    let wpcap: &Wpcap = &handle.wpcap;
    // One span per adapter thread, so every line it and `poll_drops` emit
    // carries the device without any of them repeating the field.
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
        // `header` and `data` are out-parameters pointed at library-owned
        // buffers; on return code 1 both are non-null and valid until the next
        // call on this handle, which is why the copy below precedes the loop.
        // Any other code leaves them unspecified, and none is dereferenced.
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
                // Infallible on every target this ships to, but a conversion
                // rather than a cast so the bound is the code's, not the
                // reader's.
                let caplen = usize::try_from(caplen).unwrap_or(0);
                // SAFETY: return code 1 guarantees `data` points at `caplen`
                // readable bytes, checked plausible above and bounded by the
                // handle's snaplen. The slice is copied into an owned `Vec`
                // before the next `pcap_next_ex` invalidates it.
                let frame = unsafe { std::slice::from_raw_parts(data, caplen) };
                delivered += 1;
                let Some(ip) = handle.strip.ip_bytes(frame) else {
                    // Counted separately so that "the adapter delivers, but
                    // nothing survives the strip" is legible in the closing
                    // line.
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
            // Normal, and the only moment an idle adapter gets to look at the
            // stop flag or at its drop counter.
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
            // The only news of this death the parent can get: it is parked on a
            // funnel this thread's siblings keep alive, so the disconnect never
            // arrives. `delivered` travels with it because that decides whether
            // this death is the session's. A failed send means the source is
            // already gone, the one case where nobody needs telling.
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
    LoopCounters {
        delivered,
        unstrippable,
        overflowed,
    }
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
/// `try_send`, not a blocking send, for two reasons. Parking here parks this
/// thread *outside* `pcap_next_ex`, where the kernel ring keeps filling, so a
/// consumer stall would become unbounded driver-side loss. And the receiver is
/// a field of [`super::PcapSource`], whose fields drop *after* the [`Drop`]
/// body that joins these threads, so a producer parked in `send` would be
/// joined by the thread holding the only thing that could wake it.
///
/// The dropped frame is reported through the same `capture_loss` flag the
/// driver's `ps_drop` uses, which `app::ingest` turns into a counted re-anchor.
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

/// Once per thread, not per dropped frame: a full funnel is either absent or
/// sustained, and a log line per drop is the one thing guaranteed to make a
/// congested one worse. The total goes out in the thread's closing line.
#[cold]
#[inline(never)]
fn warn_funnel_full(bytes: usize) {
    warn!(
        bytes,
        "the capture funnel is full; dropping frames and asking the pipeline to resync — \
         the byte stream has a hole in it"
    );
}

/// Out of line: at most once per adapter, on the per-packet path.
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
/// `ps_drop` is packets the kernel threw away on a full capture ring. A passive
/// tap never sees already-ACKed bytes again, so that hole can never be filled
/// by a retransmission — the condition
/// [`crate::capture::PacketSource::take_capture_loss`] exists to report.
fn poll_drops(wpcap: &Wpcap, handle: &Handle, previous: &mut c_uint, capture_loss: &AtomicBool) {
    let mut stats = PcapStat::default();
    // SAFETY: `stats` is a live, fully-initialized `pcap_stat` of the layout
    // the library expects, with room for the Windows-only tail it may write,
    // and `handle.handle` is this thread's exclusive live `pcap_t`. A non-zero
    // return means the counters were not written, so they are read only on
    // success.
    if unsafe { (wpcap.stats)(handle.handle, &mut stats) } != 0 {
        return;
    }
    // Wrapping: 32-bit counters roll over on a long-lived handle, and that must
    // read as "some loss", never as a huge negative.
    let delta = stats.ps_drop.wrapping_sub(*previous);
    if delta == 0 {
        return;
    }
    *previous = stats.ps_drop;
    capture_loss.store(true, Ordering::Relaxed);
    warn_capture_loss(delta, stats);
}

/// The rare half of [`poll_drops`], out of line: that runs on every read
/// timeout and every 512th packet, and the counters usually have not moved.
/// The adapter is named by the enclosing span.
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
    use std::collections::VecDeque;
    use std::num::NonZeroU16;
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;

    // `std::cell::Cell`, `std::cell::RefCell` and `mpsc::channel` are used only fully-qualified below
    // (`std::cell::Cell::new(...)`, `std::sync::mpsc::channel()`), matching the
    // existing tests in this file (e.g. line ~1225's `std::sync::mpsc::channel()`,
    // line ~1265's `std::cell::RefCell::new(...)`) rather than importing them:
    // an unqualified `use` here would make those pre-existing call sites
    // "unnecessary qualification" under this crate's `unused_qualifications`
    // lint, and editing an existing test is out of scope for this plan.

    use super::*;

    // ---- The fake `wpcap.dll` table `capture_loop`'s tests drive it through ----
    //
    // `capture_loop` never runs against real Npcap in this suite (see the
    // module-level ban in `PLAN.md`): every scripted test below feeds it through
    // `Wpcap::from_fns`, four `unsafe extern "C" fn` items backed by thread-local
    // state, plus nine `unreachable_*` stubs for the entry points it must never
    // touch. `handle.handle: *mut PcapT` is never dereferenced by any of them —
    // it is carried around as an opaque token the way the real library treats an
    // opaque `pcap_t`.
    //
    // Each `#[test]` fn runs on its own native thread (the default test
    // harness), so `thread_local!` state below is naturally scoped to one test
    // at a time with no cross-test interference — `scripted_wpcap` resets it
    // anyway, so that holds even if a future harness change reuses threads.

    /// One scripted answer to a `pcap_next_ex` call.
    struct ScriptedCall {
        rc: c_int,
        /// `None` means the fake writes a null header out-parameter, exactly
        /// what a well-behaved caller must tolerate on some code paths and
        /// what a malicious or buggy library could hand back on any of them.
        header: Option<PcapPktHdr>,
        /// `None` means a null data out-parameter, as above.
        data: Option<Vec<u8>>,
        /// If true, the fake writes `true` to the shared stop flag right after
        /// this call is served. That is how a test ends the loop cleanly
        /// between two iterations, without a scripted call the loop would
        /// otherwise have to make and without forcing it through an error or a
        /// closed-funnel path that would assert something else entirely.
        stop_after: bool,
    }

    impl ScriptedCall {
        fn ok(header: PcapPktHdr, data: Vec<u8>) -> Self {
            Self {
                rc: NEXT_EX_OK,
                header: Some(header),
                data: Some(data),
                stop_after: false,
            }
        }

        fn null_header(data: Vec<u8>) -> Self {
            Self {
                rc: NEXT_EX_OK,
                header: None,
                data: Some(data),
                stop_after: false,
            }
        }

        fn null_data(header: PcapPktHdr) -> Self {
            Self {
                rc: NEXT_EX_OK,
                header: Some(header),
                data: None,
                stop_after: false,
            }
        }

        fn timeout() -> Self {
            Self {
                rc: NEXT_EX_TIMEOUT,
                header: None,
                data: None,
                stop_after: false,
            }
        }

        /// `-1` is libpcap's own `PCAP_ERROR`; any code outside `{0, 1}` lands
        /// on `capture_loop`'s `_ =>` arm, so the exact value is not the point.
        fn error() -> Self {
            Self {
                rc: -1,
                header: None,
                data: None,
                stop_after: false,
            }
        }

        fn stopping(mut self) -> Self {
            self.stop_after = true;
            self
        }
    }

    thread_local! {
        /// Consecutive `pcap_next_ex` outcomes, popped front-to-back.
        static SCRIPT: std::cell::RefCell<VecDeque<ScriptedCall>> =
            const { std::cell::RefCell::new(VecDeque::new()) };
        /// The same flag `capture_loop` was handed, shared so a
        /// [`ScriptedCall::stopping`] can end the loop from inside the fake.
        static STOP_FLAG: std::cell::RefCell<Option<Arc<AtomicBool>>> = const { std::cell::RefCell::new(None) };
        /// Backing storage for the header pointer handed out by the fake:
        /// thread-local storage does not move for the life of the thread, so a
        /// pointer into it stays valid across the return of `next_ex`, exactly
        /// as `pcap_next_ex`'s own contract promises the real one would.
        static LIVE_HEADER: std::cell::Cell<PcapPktHdr> = const {
            std::cell::Cell::new(PcapPktHdr {
                tv_sec: 0,
                tv_usec: 0,
                caplen: 0,
                len: 0,
            })
        };
        /// Same idea, for the data pointer.
        static LIVE_DATA: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
        static NEXT_EX_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        static STATS_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        static STATS_RESULT: std::cell::Cell<PcapStat> = const {
            std::cell::Cell::new(PcapStat {
                ps_recv: 0,
                ps_drop: 0,
                ps_ifdrop: 0,
                ps_capt: 0,
                ps_sent: 0,
                ps_netdrop: 0,
            })
        };
        static ERROR_TEXT: std::cell::RefCell<CString> = std::cell::RefCell::new(CString::new("").expect("no NUL"));
        static CLOSE_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    unsafe extern "C" fn scripted_next_ex(
        _handle: *mut PcapT,
        header_out: *mut *mut PcapPktHdr,
        data_out: *mut *const u8,
    ) -> c_int {
        NEXT_EX_CALLS.with(|calls| calls.set(calls.get() + 1));
        let call = SCRIPT
            .with(|script| script.borrow_mut().pop_front())
            .unwrap_or_else(|| {
                panic!("fake pcap_next_ex called more times than the test scripted")
            });

        if call.stop_after {
            STOP_FLAG.with(|flag| {
                if let Some(stop) = flag.borrow().as_ref() {
                    stop.store(true, Ordering::Relaxed);
                }
            });
        }

        match call.header {
            Some(header) => {
                LIVE_HEADER.with(|cell| cell.set(header));
                let ptr = LIVE_HEADER.with(std::cell::Cell::as_ptr);
                // SAFETY: `header_out` is `capture_loop`'s live out-parameter
                // for this one call. `ptr` points into `LIVE_HEADER`'s
                // thread-local storage, which outlives this call.
                unsafe { *header_out = ptr };
            }
            None => {
                // SAFETY: as above; a null header is the case under test.
                unsafe { *header_out = std::ptr::null_mut() };
            }
        }

        match call.data {
            Some(data) => {
                LIVE_DATA.with(|cell| *cell.borrow_mut() = data);
                let ptr = LIVE_DATA.with(|cell| cell.borrow().as_ptr());
                // SAFETY: as the header case above, backed by `LIVE_DATA`.
                unsafe { *data_out = ptr };
            }
            None => {
                // SAFETY: as above; a null data pointer is the case under test.
                unsafe { *data_out = std::ptr::null() };
            }
        }

        call.rc
    }

    unsafe extern "C" fn scripted_stats(_handle: *mut PcapT, out: *mut PcapStat) -> c_int {
        STATS_CALLS.with(|calls| calls.set(calls.get() + 1));
        let stat = STATS_RESULT.with(std::cell::Cell::get);
        // SAFETY: `out` is `poll_drops`'s live out-parameter for this call.
        unsafe { *out = stat };
        0
    }

    unsafe extern "C" fn scripted_geterr(_handle: *mut PcapT) -> *mut c_char {
        // SAFETY: none — no memory is touched here, only a pointer read out of
        // thread-local storage that outlives the call, matching what
        // `pcap_geterr`'s own contract promises for its internal buffer.
        ERROR_TEXT.with(|cell| cell.borrow().as_ptr().cast_mut())
    }

    unsafe extern "C" fn scripted_close(_handle: *mut PcapT) {
        CLOSE_CALLS.with(|calls| calls.set(calls.get() + 1));
    }

    // The nine entry points `capture_loop` and `Handle::drop` must never reach.
    // Panicking, not a silent no-op, so a future change that makes the loop
    // call a fourth or fifth entry point fails loudly here instead of shipping
    // unnoticed.

    pub(super) unsafe extern "C" fn unreachable_findalldevs(
        _: *mut *mut PcapIf,
        _: *mut c_char,
    ) -> c_int {
        panic!("capture_loop must not call pcap_findalldevs")
    }

    pub(super) unsafe extern "C" fn unreachable_freealldevs(_: *mut PcapIf) {
        panic!("capture_loop must not call pcap_freealldevs")
    }

    pub(super) unsafe extern "C" fn unreachable_open_live(
        _: *const c_char,
        _: c_int,
        _: c_int,
        _: c_int,
        _: *mut c_char,
    ) -> *mut PcapT {
        panic!("capture_loop must not call pcap_open_live")
    }

    pub(super) unsafe extern "C" fn unreachable_datalink(_: *mut PcapT) -> c_int {
        panic!("capture_loop must not call pcap_datalink")
    }

    pub(super) unsafe extern "C" fn unreachable_datalink_val_to_name(_: c_int) -> *const c_char {
        panic!("capture_loop must not call pcap_datalink_val_to_name")
    }

    pub(super) unsafe extern "C" fn unreachable_compile(
        _: *mut PcapT,
        _: *mut BpfProgram,
        _: *const c_char,
        _: c_int,
        _: c_uint,
    ) -> c_int {
        panic!("capture_loop must not call pcap_compile")
    }

    pub(super) unsafe extern "C" fn unreachable_setfilter(
        _: *mut PcapT,
        _: *mut BpfProgram,
    ) -> c_int {
        panic!("capture_loop must not call pcap_setfilter")
    }

    pub(super) unsafe extern "C" fn unreachable_freecode(_: *mut BpfProgram) {
        panic!("capture_loop must not call pcap_freecode")
    }

    pub(super) unsafe extern "C" fn unreachable_lib_version() -> *const c_char {
        panic!("capture_loop must not call pcap_lib_version")
    }

    /// A real, already-loaded system library, exactly the way
    /// `the_search_flags_still_load_a_system_dll_by_absolute_path` proves the
    /// combination works: `version.dll` ships with every Windows install, its
    /// entry point only initializes its own state, and no symbol is ever
    /// resolved from it — `Wpcap::from_fns` needs a `Library` only to satisfy
    /// `_lib`'s "stays loaded" contract, never to look anything up.
    fn a_real_loaded_library() -> libloading::Library {
        let path = crate::system32::directory().join("version.dll");
        // SAFETY: as `the_search_flags_still_load_a_system_dll_by_absolute_path`.
        let loaded =
            unsafe { libloading::os::windows::Library::load_with_flags(&path, SEARCH_FLAGS) }
                .expect("version.dll must load: it ships with every Windows install");
        libloading::Library::from(loaded)
    }

    /// Resets every piece of scripted state and returns a `Wpcap` wired to it.
    ///
    /// `stop` must be the same flag the test hands to `capture_loop`: a
    /// [`ScriptedCall::stopping`] writes `true` to *this* clone of it.
    fn scripted_wpcap(
        calls: impl IntoIterator<Item = ScriptedCall>,
        stop: &Arc<AtomicBool>,
    ) -> Wpcap {
        SCRIPT.with(|script| *script.borrow_mut() = calls.into_iter().collect());
        STOP_FLAG.with(|flag| *flag.borrow_mut() = Some(Arc::clone(stop)));
        NEXT_EX_CALLS.with(|calls| calls.set(0));
        STATS_CALLS.with(|calls| calls.set(0));
        STATS_RESULT.with(|cell| {
            cell.set(PcapStat {
                ps_recv: 0,
                ps_drop: 0,
                ps_ifdrop: 0,
                ps_capt: 0,
                ps_sent: 0,
                ps_netdrop: 0,
            });
        });
        ERROR_TEXT.with(|cell| *cell.borrow_mut() = CString::new("").expect("no NUL"));
        CLOSE_CALLS.with(|calls| calls.set(0));
        Wpcap::from_fns(
            a_real_loaded_library(),
            scripted_next_ex,
            scripted_stats,
            scripted_geterr,
            scripted_close,
        )
    }

    /// Overrides what `pcap_geterr` returns, for the tests that read it back
    /// off an [`AdapterFailure`]. Call after [`scripted_wpcap`], which resets it
    /// to empty.
    fn set_geterr_text(text: &str) {
        ERROR_TEXT
            .with(|cell| *cell.borrow_mut() = CString::new(text).expect("no NUL in test text"));
    }

    /// A `Handle` wrapping the scripted `wpcap`, never opened against a real
    /// adapter. `handle.handle` is a dangling-but-never-dereferenced sentinel:
    /// every fake above treats it as opaque, exactly as the real library's
    /// `pcap_t` is to everything outside `wpcap.dll`.
    fn scripted_handle(wpcap: Arc<Wpcap>, strip: LinkStrip) -> Handle {
        Handle {
            wpcap,
            handle: std::ptr::NonNull::<PcapT>::dangling().as_ptr(),
            device: r"\Device\NPF_{TEST}".to_owned(),
            strip,
        }
    }

    /// A one-byte-per-field header: `caplen` and `len` both equal the frame
    /// length the test hands alongside it.
    fn pkthdr(caplen: u32) -> PcapPktHdr {
        PcapPktHdr {
            tv_sec: 0,
            tv_usec: 0,
            caplen,
            len: caplen,
        }
    }

    /// A minimal untagged Ethernet frame carrying an IPv4 `EtherType`, for
    /// `LinkStrip::Ethernet` to strip. Rebuilt here rather than reused from
    /// `link::tests::ethernet_frame` because that helper is private to link's
    /// own test module (`PLAN.md` keeps `link.rs` out of this plan's scope).
    fn ethernet_frame_with_ip_payload(payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![0xAAu8; 12]; // dst + src MAC, unread by the strip
        frame.extend_from_slice(&0x0800u16.to_be_bytes()); // EtherType: IPv4
        frame.extend_from_slice(payload);
        frame
    }

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

        // The funnel is full and nothing is draining it: the moment a blocking
        // bounded send would park this thread outside the driver. Run on its
        // own thread so parking fails the test instead of hanging the suite.
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
        // Without the ladder, a `pcap_compile` that rejects `vlan` is the
        // adapter's refusal reason and `open` skips the device — a fix for a
        // blind spot on tagged frames costing such a machine all capture.
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
        // Falling back must not become swallowing: the caller's
        // zero-usable-device message is built out of these reasons.
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
        // length, mostly out of bounds and caught here.
        assert!(!is_plausible_caplen(999_999));
        assert!(!is_plausible_caplen(0)); // a zero-length "packet" cannot exist
    }

    #[test]
    fn the_windows_pcap_pkthdr_is_sixteen_bytes_because_its_timeval_is_two_longs() {
        // The real gate is the `const _` beside the struct, since a release
        // build on this lane never evaluates a test.
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
        // Filled to the last byte with no terminator: truncated, not read past.
        let unterminated = [b'x'.cast_signed(); PCAP_ERRBUF_SIZE];
        assert_eq!(errbuf_text(&unterminated).len(), PCAP_ERRBUF_SIZE);
        assert_eq!(errbuf_text(&[0 as c_char; PCAP_ERRBUF_SIZE]), "");
    }

    #[test]
    fn no_wpcap_candidate_is_a_relative_name() {
        // The regression is one character wide — a `"wpcap.dll"` back in the
        // list — and its consequence is arbitrary code at high integrity, so it
        // is asserted rather than left to the comment on `SEARCH_FLAGS`.
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
        // Narrowing the search order risks narrowing past the real
        // `wpcap.dll`, which this machine has no Npcap to say. `version.dll`
        // stands in: same directory, flags and call. It proves the combination
        // resolves an absolute path under `LOAD_LIBRARY_SEARCH_SYSTEM32`, not
        // the `..._DLL_LOAD_DIR` half, which only matters for `Packet.dll`
        // beside a real `wpcap.dll`.
        let path = crate::system32::directory().join("version.dll");
        // SAFETY: `version.dll` is a Windows system library whose entry point
        // initializes its own state only. The handle is dropped immediately and
        // no symbol is resolved from it.
        let loaded =
            unsafe { libloading::os::windows::Library::load_with_flags(&path, SEARCH_FLAGS) };
        assert!(
            loaded.is_ok(),
            "{} did not load under SEARCH_FLAGS: {:?}",
            path.display(),
            loaded.err()
        );
    }

    // ---- capture_loop, driven through the fake wpcap.dll table above ----

    #[test]
    fn a_delivered_frame_reaches_the_funnel_as_ip_bytes() {
        let ip_payload = b"the ip packet bytes";
        let frame = ethernet_frame_with_ip_payload(ip_payload);
        let caplen = u32::try_from(frame.len()).expect("test frame fits in u32");

        let (packets_tx, packets_rx) = sync_channel(1);
        let (failed_tx, failed_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let capture_loss = AtomicBool::new(false);

        let wpcap = Arc::new(scripted_wpcap(
            [ScriptedCall::ok(pkthdr(caplen), frame).stopping()],
            &stop,
        ));
        let handle = scripted_handle(wpcap, LinkStrip::Ethernet);

        let counters = capture_loop(handle, &packets_tx, &failed_tx, &stop, &capture_loss);

        assert_eq!(counters.delivered, 1);
        assert_eq!(counters.unstrippable, 0);
        assert_eq!(
            packets_rx
                .try_recv()
                .expect("the stripped ip bytes reached the funnel"),
            ip_payload.to_vec()
        );
        assert!(
            failed_rx.try_recv().is_err(),
            "a clean stop must not report a failure"
        );
    }

    #[test]
    fn an_implausible_caplen_ends_the_loop_with_a_layout_error() {
        let (packets_tx, packets_rx) = sync_channel(1);
        let (failed_tx, failed_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let capture_loss = AtomicBool::new(false);

        // What a 64-bit `timeval` would make `caplen` read as: past the
        // snaplen, the canary `is_plausible_caplen` exists to catch.
        let bogus_caplen = SNAPLEN_CAPLEN + 1;
        let wpcap = Arc::new(scripted_wpcap(
            [ScriptedCall::ok(pkthdr(bogus_caplen), vec![0u8; 4])],
            &stop,
        ));
        let handle = scripted_handle(wpcap, LinkStrip::Fixed(0));

        let counters = capture_loop(handle, &packets_tx, &failed_tx, &stop, &capture_loss);

        assert_eq!(
            counters.delivered, 0,
            "the frame is rejected before it is counted delivered"
        );
        assert!(
            packets_rx.try_recv().is_err(),
            "nothing survives an implausible caplen"
        );
        let failure = failed_rx.try_recv().expect("the layout error is reported");
        assert!(
            failure.error.contains(&bogus_caplen.to_string()),
            "{}",
            failure.error
        );
        assert_eq!(failure.delivered, 0);
    }

    #[test]
    fn a_frame_the_strip_rejects_is_counted_but_not_forwarded() {
        // The `unstrippable` counter exists so that "the adapter delivers, but
        // nothing survives the strip" is legible in the closing log line — this
        // asserts that diagnosis is actually produced.
        let (packets_tx, packets_rx) = sync_channel(1);
        let (failed_tx, failed_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let capture_loss = AtomicBool::new(false);

        // Shorter than `LinkStrip::Fixed(4)`'s header: `ip_bytes` returns `None`.
        let short_frame = vec![0xAA, 0xBB];
        let caplen = u32::try_from(short_frame.len()).expect("fits in u32");
        let wpcap = Arc::new(scripted_wpcap(
            [ScriptedCall::ok(pkthdr(caplen), short_frame).stopping()],
            &stop,
        ));
        let handle = scripted_handle(wpcap, LinkStrip::Fixed(4));

        let counters = capture_loop(handle, &packets_tx, &failed_tx, &stop, &capture_loss);

        assert_eq!(counters.delivered, 1, "the adapter did deliver a frame");
        assert_eq!(
            counters.unstrippable, 1,
            "but nothing in it survived the strip"
        );
        assert!(
            packets_rx.try_recv().is_err(),
            "an unstrippable frame must never reach the funnel"
        );
        assert!(
            failed_rx.try_recv().is_err(),
            "a clean stop is not a failure"
        );
    }

    #[test]
    fn a_null_header_or_data_on_rc_ok_is_skipped_not_dereferenced() {
        let (packets_tx, packets_rx) = sync_channel(2);
        let (failed_tx, failed_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let capture_loss = AtomicBool::new(false);

        // If either branch were dereferenced instead of skipped, this test
        // segfaults rather than fails an assertion — that is the point.
        let wpcap = Arc::new(scripted_wpcap(
            [
                ScriptedCall::null_header(vec![1, 2, 3]),
                ScriptedCall::null_data(pkthdr(4)).stopping(),
            ],
            &stop,
        ));
        let handle = scripted_handle(wpcap, LinkStrip::Fixed(0));

        let counters = capture_loop(handle, &packets_tx, &failed_tx, &stop, &capture_loss);

        assert_eq!(
            counters.delivered, 0,
            "neither null case counts as a delivered frame"
        );
        assert_eq!(counters.unstrippable, 0);
        assert!(packets_rx.try_recv().is_err());
        assert!(
            failed_rx.try_recv().is_err(),
            "two skipped frames and a stop is not a failure"
        );
        assert_eq!(
            NEXT_EX_CALLS.with(std::cell::Cell::get),
            2,
            "both scripted calls were actually made"
        );
    }

    #[test]
    fn a_negative_return_code_ends_the_loop_with_pcap_geterr_text() {
        let (packets_tx, packets_rx) = sync_channel(1);
        let (failed_tx, failed_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let capture_loss = AtomicBool::new(false);

        let wpcap = Arc::new(scripted_wpcap([ScriptedCall::error()], &stop));
        set_geterr_text("pcap_next_ex: the adapter vanished");
        let handle = scripted_handle(wpcap, LinkStrip::Fixed(0));

        let counters = capture_loop(handle, &packets_tx, &failed_tx, &stop, &capture_loss);

        assert_eq!(counters.delivered, 0);
        assert!(packets_rx.try_recv().is_err());
        let failure = failed_rx
            .try_recv()
            .expect("a negative rc reports a failure");
        assert_eq!(failure.error, "pcap_next_ex: the adapter vanished");
    }

    #[test]
    fn a_timeout_polls_drops_and_keeps_going() {
        let (packets_tx, packets_rx) = sync_channel(1);
        let (failed_tx, failed_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let capture_loss = AtomicBool::new(false);

        let wpcap = Arc::new(scripted_wpcap(
            [
                ScriptedCall::timeout(),
                ScriptedCall::timeout(),
                ScriptedCall::timeout().stopping(),
            ],
            &stop,
        ));
        let handle = scripted_handle(wpcap, LinkStrip::Fixed(0));

        let counters = capture_loop(handle, &packets_tx, &failed_tx, &stop, &capture_loss);

        assert_eq!(counters.delivered, 0);
        assert_eq!(
            STATS_CALLS.with(std::cell::Cell::get),
            3,
            "every read timeout polls the drop counter, not only some of them"
        );
        assert!(packets_rx.try_recv().is_err());
        assert!(
            failed_rx.try_recv().is_err(),
            "a stop flag after a timeout is the debug! path, not an AdapterFailure"
        );
    }

    #[test]
    fn the_stop_flag_ends_the_loop_between_packets() {
        let (packets_tx, packets_rx) = sync_channel(2);
        let (failed_tx, failed_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let capture_loss = AtomicBool::new(false);

        let frame = ethernet_frame_with_ip_payload(b"payload");
        let caplen = u32::try_from(frame.len()).expect("fits in u32");
        let wpcap = Arc::new(scripted_wpcap(
            [
                ScriptedCall::ok(pkthdr(caplen), frame),
                ScriptedCall::timeout().stopping(),
            ],
            &stop,
        ));
        let handle = scripted_handle(wpcap, LinkStrip::Ethernet);

        let counters = capture_loop(handle, &packets_tx, &failed_tx, &stop, &capture_loss);

        assert_eq!(
            counters.delivered, 1,
            "the packet before the stop was still processed"
        );
        assert_eq!(
            packets_rx.try_recv().expect("the frame reached the funnel"),
            b"payload".to_vec()
        );
        assert!(failed_rx.try_recv().is_err(), "a stop is not an error");
        assert_eq!(
            NEXT_EX_CALLS.with(std::cell::Cell::get),
            2,
            "the loop asked exactly as many times as scripted, not a third time \
             after the flag was seen between packets"
        );
    }

    #[test]
    fn a_closed_funnel_breaks_the_loop() {
        let (packets_tx, packets_rx) = sync_channel(1);
        drop(packets_rx); // the funnel is gone before capture ever runs
        let (failed_tx, failed_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let capture_loss = AtomicBool::new(false);

        let frame = ethernet_frame_with_ip_payload(b"payload");
        let caplen = u32::try_from(frame.len()).expect("fits in u32");
        let wpcap = Arc::new(scripted_wpcap(
            [ScriptedCall::ok(pkthdr(caplen), frame)],
            &stop,
        ));
        let handle = scripted_handle(wpcap, LinkStrip::Ethernet);

        let counters = capture_loop(handle, &packets_tx, &failed_tx, &stop, &capture_loss);

        assert_eq!(
            counters.delivered, 1,
            "the frame was pulled off the wire before the send found nobody there"
        );
        assert!(
            failed_rx.try_recv().is_err(),
            "SourceGone is not an AdapterFailure — nobody is left to read one"
        );
        assert_eq!(
            NEXT_EX_CALLS.with(std::cell::Cell::get),
            1,
            "the loop must not ask again once its sink is gone"
        );
    }
}
