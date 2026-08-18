# 04 — Unsafe Code (`unsafe-`)

**Category priority:** CRITICAL
**Rules audited:** 7 · **Files read:** 12 in full + 1 partial · **Findings:** 4 (P0 0 / P1 1 / P2 2 / P3 1)

Files read in full: `src/capture/pcap.rs`, `src/actuator/win.rs`, `src/actuator/shield.rs`,
`src/migrate.rs`, `src/crash.rs`, `src/render.rs`, `src/capture/mod.rs`, `src/lib.rs`,
`Cargo.toml`, `build.rs`, `.github/workflows/ci.yml`, `.claude/skills/rust-skills/rules/unsafe-*.md`
(7 files); `src/app/mod.rs` partially (the two regions that move a `PacketSource`/`CaptureStop`
across threads, to check the `Send` impl's premise).

Coverage note, stated plainly rather than claimed: instead of reading the 15 000 lines of GUI,
domain and config code that contain no unsafe, this audit proved their emptiness mechanically —
a crate-wide grep for `unsafe`, `unsafe impl`, `transmute`, `mem::zeroed`, `mem::uninitialized`,
`MaybeUninit`, `assume_init`, `from_raw_parts`, `set_len`, `*_unchecked`, `static mut`,
`no_mangle`, `export_name`, `link_section`, `repr(` and `extern "` returns hits in exactly four
`.rs` files, all four read in full; and `cargo clippy --all-targets -W clippy::undocumented_unsafe_blocks`
compiled every file of every target and reported **zero** omissions.

## Verdict

This is the best-documented unsafe code I have audited in a hobby-scale crate, and the audit's
headline result is a negative one: **there is no unsoundness here.** Clippy's
`undocumented_unsafe_blocks` fires on nothing — all ~35 `unsafe` blocks carry a `// SAFETY:`
comment — and I checked each claim against the real contract rather than taking it on trust. The
13 `wpcap.dll` signatures in `src/capture/pcap.rs` are an exact match for libpcap's ABI
(`pcap_findalldevs`, `pcap_open_live`, `pcap_next_ex`, `pcap_compile`, `pcap_stats`, … including
the `*mut *mut PcapPktHdr` / `*mut *const u8` mutability split, `bpf_u_int32` = `c_uint`, and the
Windows-only three-field tail of `struct pcap_stat`); `PcapPktHdr` is correctly 16 bytes because
Windows' `timeval` is two 32-bit longs; the `pcap_next_ex` buffer is copied into an owned `Vec`
before the loop can invalidate it; `unsafe impl Send for Handle` is genuinely justified and its
`Arc<Wpcap>` drop order keeps `wpcap.dll` loaded strictly past the last `pcap_close`; and
`shield_proc` is `extern "system"`, not `extern "C"`, which is the difference between correct and
32-bit-broken.

The worst offender file is `src/capture/pcap.rs`, and the single highest-value fix is
**`unsafe-001`**: `fn cstr(ptr: *const c_char) -> String` is a *safe* function that dereferences an
arbitrary raw pointer, and its `// SAFETY:` comment discharges the obligation onto "callers" that
the signature gives no way to bind. All seven call sites are correct today, so this is debt, not a
bug — but it is the one place in the crate where the unsafe boundary is drawn in the wrong place.

One durable gap is *not* filed here because it belongs to a sibling: neither `Cargo.toml` nor
`src/lib.rs` enables `clippy::undocumented_unsafe_blocks`, so the perfect record above is upheld by
discipline alone and CI's `-D warnings` would not catch the first omission. That is
[`lint-unsafe-doc`](../../.claude/skills/rust-skills/rules/lint-unsafe-doc.md), category `lint-`.
Whoever synthesises this audit should treat it as the cheapest possible insurance on this file.

## Findings

### unsafe-001 — `cstr` is a safe function with raw-pointer preconditions nothing enforces

- **Severity:** P1
- **Rule:** [`unsafe-safety-comment`](../../.claude/skills/rust-skills/rules/unsafe-safety-comment.md)
- **Site:** `src/capture/pcap.rs:300-311` (definition); callers at `:283`, `:295`, `:697`, `:709`, `:710`, `:751`, `:770`
- **What:** the helper is declared safe —

  ```rust
  fn cstr(ptr: *const c_char) -> String {
      if ptr.is_null() { return String::new(); }
      // SAFETY: callers pass either a library-owned string constant or a pointer
      // into a buffer that outlives this call; both are NUL-terminated, which is
      // the only thing `from_ptr` requires. ...
      unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
  }
  ```

  The comment is an accurate description of *what the current callers happen to do*, but it is
  written in the register of an invariant, and it is not one: nothing in `fn cstr(...)`'s signature
  obliges a caller to pass a NUL-terminated pointer. The null check makes this look defensive and
  hides that null is the *only* misuse it catches; a non-null pointer to a 256-byte buffer with no
  terminator reads past the end. The comment's closing clause is also slightly wrong on its own
  terms: `CStr::from_ptr` requires more than NUL-termination — validity for reads up to and
  including the terminator, alignment, and no mutation for the call's duration.
- **Why it matters here:** `cstr` is the module's universal C-string reader and it is called from
  both sides of the FFI boundary — on library-owned constants (`pcap_lib_version`,
  `pcap_datalink_val_to_name`), on a pointer into a live `pcap_t`'s internal error buffer
  (`pcap_geterr`), on `PcapIf` fields inside a walk of a library-owned linked list, and on two
  stack `[c_char; 256]` error buffers. Those five provenances have genuinely different validity
  windows, and a safe signature means the compiler will not stop the sixth caller from being wrong.
  Two of the seven call sites (`:697`, `:751`) do not need raw pointers at all, which is the tell.
- **Fix:** split it in two, so that the raw-pointer path is marked and the common path becomes
  actually safe.

  ```rust
  /// Copies a NUL-terminated C string, treating null as empty.
  ///
  /// # Safety
  ///
  /// If `ptr` is non-null it must point at a NUL-terminated byte sequence that
  /// is valid for reads up to and including the terminator, and must not be
  /// mutated for the duration of the call.
  unsafe fn cstr(ptr: *const c_char) -> String { /* body unchanged */ }

  /// A libpcap error buffer as text. Safe: the scan is bounded by the array.
  fn errbuf_text(buf: &[c_char; PCAP_ERRBUF_SIZE]) -> String {
      let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
      String::from_utf8_lossy(&buf[..end].iter().map(|&c| c as u8).collect::<Vec<_>>()).into_owned()
  }
  ```

  `:697` and `:751` become `errbuf_text(&errbuf)` — and gain a real guarantee, since a libpcap
  build that ever filled all 256 bytes without terminating would no longer read out of bounds.
  `:283`, `:295`, `:709`, `:710`, `:770` already sit inside `unsafe` blocks whose `// SAFETY:`
  comments already state the needed invariant, so they compile unchanged.
- **Effort:** small

### unsafe-002 — seven `unsafe` blocks wrap the surrounding safe code, five of them with multiple unsafe ops

- **Severity:** P2
- **Rule:** [`unsafe-minimize-scope`](../../.claude/skills/rust-skills/rules/unsafe-minimize-scope.md)
- **Site:** `src/capture/pcap.rs:239`, `:707`, `:768`, `:789`; `src/migrate.rs:183`, `:258`
- **What:** clippy confirms five of these mechanically — `cargo clippy --all-targets -W clippy::multiple_unsafe_ops_per_block`:

  ```
  src\capture\pcap.rs:239:28: this `unsafe` block contains 13 unsafe operations, expected only one
  src\capture\pcap.rs:768:37: this `unsafe` block contains 2 unsafe operations, expected only one
  src\capture\pcap.rs:789:5:  this `unsafe` block contains 5 unsafe operations, expected only one
  src\migrate.rs:183:5:       this `unsafe` block contains 3 unsafe operations, expected only one
  src\migrate.rs:258:5:       this `unsafe` block contains 2 unsafe operations, expected only one
  ```

  The worst is `pcap.rs:239-270`: a single `unsafe` block spanning 32 lines that contains a
  `macro_rules!` *definition*, thirteen `lib.get` calls, `format!`, `String::from_utf8_lossy`,
  `.trim_end_matches('\0')`, `failures.push`, a `continue` that escapes the block to the enclosing
  `for`, and the whole `Wpcap { .. }` struct literal. Only `lib.get(...)` is unsafe. `:707-717` and
  `:768-771` wrap calls to the safe `cstr` and a `debug!` invocation; `migrate.rs:183-210` wraps
  ~25 lines including `std::io::Error::from_raw_os_error`, two early returns and the final
  `control & SE_DACL_PROTECTED != 0` bit test.
- **Why it matters here:** this crate's SAFETY comments are its main soundness artefact, and the
  oversized blocks are precisely where that artefact degrades: one comment at `:236` has to cover
  thirteen operations plus the drop-order argument for `_lib`, and one at `:172` has to cover
  `GetNamedSecurityInfoW`, `GetSecurityDescriptorControl` and `LocalFree` with a
  freed-exactly-once claim spanning three control-flow paths. That is exactly the review surface
  the rule exists to shrink, and it is the review surface on which the crate's *only* remaining
  unsoundness risk lives.
- **Fix:** mechanical, no logic change. In `pcap.rs`, hoist the `macro_rules!` out of the block and
  put the `unsafe` inside it: `Ok(symbol) => unsafe { *symbol }` — the block then covers one op and
  the error arm's `format!`/`push`/`continue` leave unsafe context entirely. At `:768` and `:707`,
  narrow to `unsafe { (wpcap.datalink)(h) }` / `unsafe { &*cursor }` and leave the `cstr` and
  `debug!` calls outside. In `migrate.rs`, wrap each of the three (resp. two) Win32 calls
  individually and lift the `if`/`return` scaffolding out; each then gets a one-sentence
  `// SAFETY:` instead of one paragraph covering everything. `pcap.rs:789-809` is the one defensible
  large block — its five ops share the exact precondition ("this thread's live `pcap_t` plus the
  live `program`") and the visible `freecode`-on-every-path structure is load-bearing, so shrink
  only the `return Err(format!(...))` arms out of it rather than splitting the calls.
- **Effort:** small

### unsafe-003 — the FFI struct-layout guard is a runtime `#[test]`, not a compile-time assertion

- **Severity:** P2
- **Rule:** [`unsafe-safety-comment`](../../.claude/skills/rust-skills/rules/unsafe-safety-comment.md)
- **Site:** `src/capture/pcap.rs:1180-1187`
- **What:** the module correctly identifies its own most dangerous constant — the comment at
  `:135-140` says a mis-declared `timeval` "is not a crash — `caplen` would read the tail of the
  timestamp and yield absurd lengths" — and then checks it in a test:

  ```rust
  #[test]
  fn the_windows_pcap_pkthdr_is_sixteen_bytes_because_its_timeval_is_two_longs() {
      assert_eq!(size_of::<PcapPktHdr>(), 16);
      assert_eq!(size_of::<PcapStat>(), 24);
  }
  ```

  Both numbers are correct (I verified them against `pcap.h`: `struct timeval` is two 32-bit
  `long`s on Windows, and `struct pcap_stat`'s `_WIN32` tail adds `ps_capt`/`ps_sent`/`ps_netdrop`).
  The problem is only *when* they are checked. A `#[test]` gates `cargo test`; it does not gate
  `cargo build --release`, and `src/capture/mod.rs:77-83` already demonstrates the better idiom two
  files away:

  ```rust
  #[cfg(target_pointer_width = "64")]
  const _: () = { assert!(std::mem::size_of::<FlowKey>() == 64); ... };
  ```
- **Why it matters here:** these two sizes are the premise of the `// SAFETY:` comments at `:937`
  and `:948` — the ones that justify `(*header).caplen` and
  `slice::from_raw_parts(data, caplen as usize)`. A soundness premise should fail the *build*, on
  every target and every feature lane, not the test run. The consequence is bounded today because
  `plausible_caplen` is a genuine second line of defence and CI runs `cargo test` on four lanes, so
  this is a robustness gap rather than a live risk.
- **Fix:** move both assertions to a `const _: () = { ... };` item next to the struct definitions
  (they are `const`-evaluable exactly as written) and keep the explanatory comment. The `#[test]`
  can then be deleted, or kept as documentation with the `const` block as the real gate.
- **Effort:** trivial

### unsafe-004 — two SAFETY comments in `shield.rs` assert a window-aliveness invariant that does not hold and is not needed

- **Severity:** P3
- **Rule:** [`unsafe-safety-comment`](../../.claude/skills/rust-skills/rules/unsafe-safety-comment.md)
- **Site:** `src/actuator/shield.rs:46-48`, `:56-58`
- **What:** `:47` states "`shield` was proven alive by the `IsWindow` check inside `handle()` on
  this same thread". That is false on two counts: `handle()` reaches `IsWindow` only on the
  cache-hit path (the freshly-created path at `:152` never calls it), and in either case the shield
  window dies with its pump thread, which can exit at any moment after `handle()` returns —
  aliveness is not something a caller of `raise` can hold. `:57` has the same shape for the game
  window: "both handles are top-level windows owned by live threads". Both comments then state the
  argument that *is* true and *is* load-bearing — "`IsWindowVisible` only reports and returns 0 for
  a handle it does not know", "A dead handle is reported as FALSE, checked right below" — so
  nothing here is unsound.
- **Why it matters here:** in a crate whose SAFETY comments are the audit surface, a clause asserted
  in the same confident register as the true one costs a future reader the time to disprove it, and
  worse, invites them to *rely* on it: someone adding a call that genuinely does need a live HWND
  would read `:47` and conclude the precondition is already established.
- **Fix:** delete the aliveness clause from both and keep the tolerates-a-dead-handle argument,
  which is the whole justification. E.g. `:46` becomes "SAFETY: `IsWindowVisible` is defined over
  any HWND value — a shield whose pump thread has exited reports 0, which reads as 'not visible'
  and sends this through the (re)placement path below." Same edit at `:56`.
- **Effort:** trivial

## Clean areas

**`unsafe-safety-comment`**
- Every `unsafe` block in the crate carries a `// SAFETY:` comment —
  `clippy -W clippy::undocumented_unsafe_blocks --all-targets` reports zero omissions.
- The single `unsafe fn` in the crate (`Wpcap::error_text`, `src/capture/pcap.rs:286-296`) has a
  proper `# Safety` doc section *and* an inner `// SAFETY:` that explicitly delegates to the
  caller's contract — the exact two-level shape the rule prescribes.
- The comments are load-bearing, not ceremonial: `src/capture/pcap.rs:424-429` explains *why*
  `*mut PcapT` is not `Send` before asserting it is; `:948-952` names the invalidation window
  ("before the next `pcap_next_ex` on this handle invalidates it") rather than just saying "valid";
  `src/migrate.rs:172-182` tracks a `LocalAlloc` block across three control-flow paths and states
  the failure mode. I verified this last claim: `LocalFree` runs unconditionally after
  `GetSecurityDescriptorControl`, and the early return at `:195` precedes any allocation.
- Every FFI out-parameter is fully initialized before the call, and read only after the return code
  is checked: `errbuf` (`[0 as c_char; 256]`), `RECT`/`POINT` field-by-field, `PcapStat::default()`,
  `BpfProgram` with an explicit null `bf_insns`, `acl_buf = [0u32; 16]`, `MSG::default()`.

**ABI correctness (the substance behind the comments)**
- All 13 `wpcap.dll` signatures match libpcap exactly, including the pointer-mutability details
  that are easy to get wrong: `pcap_next_ex(pcap_t*, struct pcap_pkthdr**, const u_char**)` is
  transcribed as `(*mut PcapT, *mut *mut PcapPktHdr, *mut *const u8)`, and `bpf_u_int32` as
  `c_uint`. `extern "C"` is the correct convention for libpcap on both MSVC targets.
- `#[repr(C)]` on `PcapIf`, `PcapPktHdr`, `BpfProgram` and `PcapStat`; field-for-field matches of
  `pcap_if`, `pcap_pkthdr`, `bpf_program` and the `_WIN32` form of `pcap_stat`. `PcapStat` is
  deliberately over-declared with the three Windows-only counters so the library can never write
  past it — a correct instinct, and documented as such at `:155-160`.
- The `pcap_next_ex` buffer is never held past its validity window: `ip.to_vec()` at `:963` copies
  before the loop can call `pcap_next_ex` again, and the intervening `poll_drops` touches only
  `pcap_stats`.
- `libloading` usage is correct: `Symbol<T>` is dereferenced to a bare fn pointer, `_lib` is stored
  in the same struct to pin the module, and — the part that is easy to get wrong — the last
  `Arc<Wpcap>` clone is owned by a capture thread's `Handle`, so `FreeLibrary` cannot run before
  that thread's `pcap_close`. A partially-built `Wpcap` abandoned by `sym!`'s `continue` leaks
  nothing (fn pointers are `Copy` with no `Drop`) and drops `lib`.
- `shield_proc` is `extern "system"`, matching `WNDPROC`, not `extern "C"` — correct on i686 as
  well as x86-64. Under Rust ≥1.81 its definition also gets an abort-on-unwind shim, so a panic
  cannot unwind into Win32.
- `SendInput(1, &input, size_of::<INPUT>() as i32)` passes the *stride*, not the array length, and
  says so at `:496-501`. That is the classic `SendInput` bug, and it is not present.

**`unsafe-send-sync-manual`**
- Exactly one manual impl in the crate — `unsafe impl Send for Handle` (`src/capture/pcap.rs:430`)
  — and it is justified correctly. `Handle` is non-`Clone` and stays `!Sync` (the raw pointer
  withholds the auto-impl), so the single move into `std::thread::Builder::spawn` is the only
  transfer; the `Arc<Wpcap>` field is `Send + Sync` on its own. No `Sync` impl is added, which is
  the right call, and the type-level design (owning, non-`Clone`, `Drop`-closing) is what makes the
  claim true rather than the comment.
- No `unsafe impl Sync` anywhere; no `static mut`; the shield's process-global state is a
  `Mutex<Option<isize>>` (`src/actuator/shield.rs:27`), not a mutable static.

**`unsafe-maybeuninit`**
- Zero uses of `mem::uninitialized`, `mem::zeroed`, `MaybeUninit`, `assume_init` or `Vec::set_len`
  in the crate. The FFI-heavy modules would be the natural place for them and they are simply not
  needed here, because every out-parameter is a plain initialized value.

**`unsafe-minimize-scope` (where it is already honoured)**
- `src/actuator/win.rs` and `src/actuator/shield.rs` are exemplary: every one of their 21 `unsafe`
  blocks wraps exactly one Win32 call, several of them inline in an expression
  (`unsafe { IsWindowVisible(shield) } != 0`, `(unsafe { GetForegroundWindow() }) as isize`). Both
  files pass `clippy::multiple_unsafe_ops_per_block` clean.
- `src/capture/pcap.rs:875` (`RegGetValueW`) and `:931`/`:939`/`:953`/`:1016` are likewise
  single-op. Note `:939` and `:953` are split into two blocks with two different comments even
  though a single block would have compiled — the rule's preferred shape.
- No `unsafe fn` in the crate has unsafe operations relying implicitly on the function's own
  unsafety, so the 2024 `unsafe_op_in_unsafe_fn` requirement is met everywhere (verified: the one
  `unsafe fn` wraps its call in an explicit inner `unsafe {}`).

## Not applicable

- **`unsafe-miri-ci`** — Miri is genuinely meaningless for this crate, and I am not filing a finding
  demanding it. Every `unsafe` operation here is a foreign call: 21 Win32 calls through
  `windows-sys`, `libloading::Library::new` loading a real `wpcap.dll`, and 13 `pcap_*` function
  pointers. Miri cannot execute any of them, and the one test that would exercise the FFI path
  (`the_tap_opens_on_this_machine_without_elevation`, `src/capture/pcap.rs:1196-1203`) is
  `#[ignore]`d because it needs Npcap and a real adapter. The pure-Rust logic that Miri *could*
  interpret — `LinkStrip`, `ethernet_payload_offset`, `plausible_caplen`, `pack_point` — contains
  no unsafe at all and is already covered by twelve unit tests. Running `cargo miri test` here
  would either skip everything that matters or fail on the DLL load. The layout assertions of
  `unsafe-003` are the correct substitute, and they already exist (see that finding for making them
  compile-time).
- **`unsafe-extern-block`** — the crate declares no `extern "C" { }` blocks at all, so there is
  nothing to migrate to `unsafe extern`. Its FFI is (a) `windows-sys`, which emits its own 2024-
  correct declarations, and (b) function-pointer *types* in `struct Wpcap`
  (`src/capture/pcap.rs:184-197`), every one of which is already spelled
  `unsafe extern "C" fn(...)` — the correct and compiler-required form for a resolved symbol. The
  crate is `edition = "2024"` and compiles clean, which is itself proof no bare `extern` block
  survives.
- **`unsafe-no-mangle-unsafe`** — no `#[no_mangle]`, `#[export_name]` or `#[link_section]` anywhere
  in `src/`, `examples/` or `build.rs`. This is a binary that exports no symbols; `shield_proc` is
  handed to Win32 as a function pointer through `WNDCLASSW::lpfnWndProc`, never by symbol name, so
  it needs no export attribute. `build.rs` influences the linker only through
  `cargo:rustc-link-arg-bins` manifest flags.
- **`src/crash.rs` and `src/render.rs`** — both named in this reviewer's brief as containing
  unsafe. They do not: read in full, they contain zero `unsafe` blocks and no FFI. `crash.rs` is a
  panic hook built entirely on `std::panic`, `std::backtrace` and `std::fs`; `render.rs` is pure
  string formatting. Recording this so a later pass does not go looking.
