//! The `wpcap.dll` boundary: the transcribed ABI, the handle lifecycle, and the
//! per-adapter receive loop.
//!
//! # Why these three are one module
//!
//! They are separable on paper, and `docs/tech-debt/24-proj.md` proposed
//! separating them. They share one invariant that is only worth anything if a
//! reader can check it in one place: [`unsafe impl Send for Handle`](Handle)
//! claims that nothing but the owning `Handle` retains the `*mut PcapT`. Every
//! line in this crate that names a libpcap object pointer is in this file, and
//! the field holding it is private to this file — the only widened field is the
//! `device` string the parent uses for a thread name — so the claim is checkable
//! against this module rather than against the whole `pcap` tree. Splitting
//! `open_device` from `capture_loop` would have made `Handle::handle`
//! `pub(super)` and spread that check over three files, which is the one cost the
//! report named for this move; it is not worth paying. [`super::link`] is the
//! layer that carries no pointer, and it is the one that left.

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use tracing::{debug, info_span, warn};

use super::link::{LinkStrip, UnsupportedDatalink};
use super::short_device_name;
use crate::error::{Error, Result};

/// Size libpcap requires of every error buffer it is handed. Not negotiable:
/// the library writes up to this many bytes without asking.
const PCAP_ERRBUF_SIZE: usize = 256;

/// Bytes captured per packet.
///
/// Deliberately not the wire MTU, and deliberately not the 65 575 the removed
/// `WinDivert` backend used either. Receive-side coalescing (RSC/LRO) hands the
/// stack a single "packet" made of many wire frames, and the probe measured one
/// of **48 870 bytes** on this machine — 32 times the MTU. 65 575 happened to be
/// above that here; there is no reason it is above it everywhere, and a
/// too-small snaplen does not fail, it silently truncates, which reaches
/// `parse_segment` as a malformed packet and reads as a parser bug. 262 144 is
/// libpcap's own documented ceiling, so this is simply "as much as it will
/// give".
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

/// Netmask handed to `pcap_compile`. The filter below uses no broadcast-relative
/// primitive (`ip broadcast` and friends), which is the only thing the netmask
/// feeds, so zero is correct rather than merely tolerated.
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

/// Where `wpcap.dll` is looked for, in order.
///
/// The plain name resolves only when Npcap was installed in WinPcap-compatible
/// mode (the installer default, but the player may have unchecked it); the
/// second path is the private directory Npcap always writes to and is not on
/// any DLL search path. Which one answered is logged, because "the plain name
/// did not resolve" is the difference between a working and a broken install on
/// a machine we cannot inspect.
pub(super) const DLL_CANDIDATES: [&str; 2] = ["wpcap.dll", r"C:\Windows\System32\Npcap\wpcap.dll"];

/// What to tell a player who has no Npcap at all.
pub(super) const INSTALL_HINT: &str = "install Npcap from https://npcap.com/#download and leave \
     \"Restrict Npcap driver's access to Administrators\" UNCHECKED";

impl Wpcap {
    pub(super) fn load() -> Result<(Self, &'static str)> {
        let mut failures = Vec::new();
        for path in DLL_CANDIDATES {
            // SAFETY: `Library::new` runs the DLL's entry point, which for
            // `wpcap.dll` only initializes the library's own state. The symbols
            // resolved just below are copied out as plain function pointers;
            // they stay valid exactly as long as `_lib` — stored in the same
            // struct, never separated from them — keeps the module loaded.
            // Failure mode: the DLL is absent or is not a valid image, which
            // surfaces as an `Err` here and is collected rather than fatal, so
            // the second candidate still gets its turn.
            let lib = match unsafe { libloading::Library::new(path) } {
                Ok(lib) => lib,
                Err(err) => {
                    failures.push(format!("{path}: {err}"));
                    continue;
                }
            };
            // Deliberately scope-local and unhygienic by design: the body reads
            // `lib`, `path` and `failures` from this loop body, and its `continue`
            // abandons this DLL candidate — all twelve sibling resolutions
            // included — for the next one. That caller-scope `continue` is the
            // reason this is a macro and not a function, and it is also why
            // hoisting it to module scope will not compile.
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
                                "{path}: {} is missing ({err})",
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
        Err(Error::Capture(format!(
            "could not load wpcap.dll ({}) — {INSTALL_HINT}",
            failures.join("; ")
        )))
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
/// A `pcap_t` is not thread-safe, and this type is what makes that safe by
/// construction: it is created on the opening thread, moved wholesale into its
/// capture thread, and closed by [`Drop`] on whichever thread ends up owning it
/// last. No two threads ever hold the same one.
pub(super) struct Handle {
    wpcap: Arc<Wpcap>,
    handle: *mut PcapT,
    /// The `\Device\NPF_{...}` name, kept for log lines.
    ///
    /// The one field visible outside this file — [`super::PcapSource::open`] names
    /// each capture thread after it — and deliberately the only one: the raw
    /// pointer beside it stays private here, which is what keeps the `unsafe impl
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
/// An empty list is not an error here — [`super::no_usable_device_error`] is what
/// turns it into one, because it is also the signature of `AdminOnly=1` and
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

/// Opens `device`, checks its link type and installs the kernel-side filter.
///
/// The `Err` is the human-readable reason this adapter is unusable, not a fatal
/// error: the caller logs it and moves on to the next one.
pub(super) fn open_device(
    wpcap: &Arc<Wpcap>,
    device: &str,
    filter: &str,
) -> std::result::Result<Handle, String> {
    let device_c = CString::new(device).map_err(|_| "device name contains a NUL".to_owned())?;
    let filter_c = CString::new(filter).map_err(|_| "filter contains a NUL".to_owned())?;
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
        return Err(format!("pcap_compile({filter}): {reason}"));
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
    if let Some(failure) = failure {
        return Err(format!("pcap_setfilter: {failure}"));
    }

    debug!(
        device = %device,
        link = %datalink_name,
        datalink,
        ?strip,
        "adapter opened and filtered"
    );
    Ok(handle)
}

// --- Capture thread --------------------------------------------------------

/// One adapter's receive loop: strip, forward, and watch the driver's drop
/// counter. Parses nothing — [`super::PacketSource::next_segment`] does that,
/// once, for every adapter, on the [`super::PcapSource`] side of the channel.
///
/// Returns when the stop flag is set, when the receiver has gone away, or when
/// the handle reports an error. An error kills only this adapter: the others
/// keep capturing, which matters precisely because this backend opens adapters
/// it has no reason to believe in.
pub(super) fn capture_loop(
    handle: Handle,
    packets: &Sender<Vec<u8>>,
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
                if packets.send(ip.to_vec()).is_err() {
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
        Some(error) => warn!(
            delivered,
            unstrippable,
            dropped,
            error = %error,
            "adapter capture ended on an error"
        ),
        None => debug!(delivered, unstrippable, dropped, "adapter capture ended"),
    }
    // `handle` drops here, on the thread that owned it, closing the `pcap_t`.
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
    use super::*;

    #[test]
    fn a_caplen_outside_the_snaplen_is_rejected_as_a_pkthdr_layout_error() {
        assert!(is_plausible_caplen(1));
        assert!(is_plausible_caplen(SNAPLEN_CAPLEN));
        assert!(!is_plausible_caplen(0));
        assert!(!is_plausible_caplen(SNAPLEN_CAPLEN + 1));
        // What a 64-bit `timeval` would produce: a microsecond count read as a
        // length. Most of that range is out of bounds and caught here; the part
        // below the snaplen is not, which is why this is a canary rather than a
        // proof (see `plausible_caplen`).
        assert!(!is_plausible_caplen(999_999));
        assert!(!is_plausible_caplen(0)); // a zero-length "packet" cannot exist
    }

    #[test]
    fn the_windows_pcap_pkthdr_is_sixteen_bytes_because_its_timeval_is_two_longs() {
        // The single most dangerous constant in this file: a 24-byte header
        // would put `caplen` where `tv_usec` is and report nonsense lengths
        // instead of crashing. The real gate is the `const _` beside the struct
        // — this build lane is feature- and OS-gated, so a release build here
        // would never have evaluated a test. Kept because it costs nothing and
        // states the same fact where a reader running the suite will see it.
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
        // A buffer libpcap filled to the last byte without terminating it is
        // truncated rather than read past — which is the whole reason this is a
        // safe function taking the array by reference.
        let unterminated = [b'x'.cast_signed(); PCAP_ERRBUF_SIZE];
        assert_eq!(errbuf_text(&unterminated).len(), PCAP_ERRBUF_SIZE);
        assert_eq!(errbuf_text(&[0 as c_char; PCAP_ERRBUF_SIZE]), "");
    }
}
