# 03 — Memory Optimization (`mem-`)

**Category priority:** CRITICAL
**Rules audited:** 17 · **Files read:** 41 (+ `Cargo.toml`) · **Findings:** 9 (P0 0 / P1 1 / P2 7 / P3 1)

## Verdict

This category is in better shape than "CRITICAL" usually implies: the crate already
applies `mem-assert-type-size`, `mem-with-capacity`, `mem-take-replace`,
`mem-smaller-integers` and `mem-drop-order` deliberately, with the reasoning
written down beside the code, and the GUI avoids per-frame `format!` in three
places *on purpose*. The worst offender is the hot path's entry, `src/capture/`:
every captured packet is copied **twice** — once out of the Npcap ring into a
channel `Vec` (unavoidable), then again in `parse_segment` to carve out the TCP
payload (avoidable), with a measured RSC frame size of 48 870 bytes and a snaplen
of 262 144. That single redundant copy (mem-001) is the highest-value fix, and it
also removes the one per-packet buffer that `PipelineBudget` cannot see. Everything
else is P2 idiom work; two items (mem-003, mem-007) are trivial-to-small and
protect against future regressions rather than fixing present cost.

## Findings

### mem-001 — Every captured packet is copied twice: frame into a channel `Vec`, then payload into a second `Vec`

- **Severity:** P1
- **Rule:** [`mem-zero-copy`](../../.claude/skills/rust-skills/rules/mem-zero-copy.md)
- **Site:** `src/capture/ip.rs:66` (+ `src/capture/pcap.rs:963`)
- **What:** the capture thread copies the stripped IP bytes out of libpcap's ring into
  an owned buffer — `packets.send(ip.to_vec())` — which is mandatory, because the
  next `pcap_next_ex` on that handle invalidates the slice (documented at
  `pcap.rs:948-953`). `parse_segment` then copies the TCP payload *out of that
  buffer* into a second allocation:

  ```rust
  payload: tcp.payload().to_vec(),
  ```

  and the frame `Vec` is dropped immediately afterwards. Two allocations and two
  memcpys per admitted packet, where one of each would do.
- **Why it matters here:** the module's own measurements make the size real rather
  than theoretical — `pcap.rs:62-73` records an RSC/LRO-coalesced "packet" of
  **48 870 bytes** on the development machine and sets `SNAPLEN = 262_144`
  precisely because that ceiling is not knowable in advance. So the redundant copy
  is tens of kilobytes per packet, and it doubles the transient footprint exactly
  when the pipeline is under pressure. Worse for a crate that built
  `PipelineBudget` to bound memory: the frame `Vec` is created *before* admission
  and sits in the deliberately-unbounded `Receiver<Vec<u8>>` (`pcap.rs:492`,
  "Unbounded on purpose"), so the one per-packet buffer the byte budget does not
  account for is the one being duplicated. Steady-state packet rate is low (one
  kernel-filtered TCP port), so this is burst-shaped cost, not a throughput
  crisis — but it is gratuitous work on the only hot path in the program.
- **Fix:** make the payload a view into the buffer that is already owned instead of
  a fresh copy. Have `parse_segment` return the payload's offsets rather than a
  slice-to-be-copied — e.g. `Option<(FlowKey, u32, bool, Range<usize>)>`, or a
  borrowing `SegmentView<'_>` the caller turns into a `Segment` — and then either
  (a) store `Segment { buffer: Vec<u8>, payload: Range<usize> }`, letting the
  capture thread's buffer *become* the segment payload with zero copies, or
  (b) as the smaller change, keep `payload: Vec<u8>` and build it in place from
  the frame buffer with `frame.drain(..start); frame.truncate(len);` — one
  memmove, no allocation. Note (a) also removes the `payload.bytes.drain(..already)`
  memmove at `stream.rs:739`, and would let the budget account for the real buffer.
  Whichever is chosen, re-measure and update the `Segment` size canary at
  `capture/mod.rs:77-83` deliberately (see mem-003: shrinking `Segment` makes
  `Error`'s 96 bytes the binding constraint on `Result<Segment>`).
- **Effort:** medium

### mem-002 — `HalfStream::push` allocates a fresh out-`Vec` for every packet to carry, usually, one chunk

- **Severity:** P2
- **Rule:** [`mem-smallvec`](../../.claude/skills/rust-skills/rules/mem-smallvec.md) (also [`mem-with-capacity`](../../.claude/skills/rust-skills/rules/mem-with-capacity.md))
- **Site:** `src/stream.rs:696` — `let mut out = Vec::new();`
- **What:** every reassembled segment builds a `Vec<BudgetedChunk>` that is handed
  up through `HalfOutcome::Chunks` → `ReassemblyOutcome::Chunks` → `forward_chunks`
  and consumed immediately. `Vec::new()` does not allocate, but the first `push`
  does — and for a 48-byte element type `RawVec` jumps straight to capacity 4, so
  the common in-order case pays one 192-byte allocation and free per packet to
  hold a single chunk. More than one chunk only happens when a buffered gap fills
  (`drain`, `stream.rs:777-789`).
- **Why it matters here:** it is a per-packet allocation on the reassembly path,
  and nothing downstream needs the heap indirection — `forward_chunks` iterates the
  vector once and drops it.
- **Fix:** `SmallVec<[BudgetedChunk; 1]>` for `out` and both `Chunks` variants —
  one inline slot covers the common case and keeps the enum small (48-byte
  element). If adding `smallvec` is not wanted, `Vec::with_capacity(1)` at least
  turns allocate-then-grow into one exact allocation. `mem-smallvec` asks for
  measurement before committing; the thing being removed is one malloc/free per
  packet.
- **Effort:** small

### mem-003 — `Error` is 96 bytes because two cold config-parse variants are unboxed

- **Severity:** P2
- **Rule:** [`mem-box-large-variant`](../../.claude/skills/rust-skills/rules/mem-box-large-variant.md)
- **Site:** `src/error.rs:18` (`ConfigParse(#[from] toml::de::Error)`) and `src/error.rs:49` (`ConfigReparse(#[from] toml_edit::TomlError)`)
- **What:** measured on this toolchain — `size_of::<Error>() == 96`, driven entirely
  by `toml::de::Error` (88 bytes) and `toml_edit::TomlError` (88 bytes). The next
  largest variant is `ConfigRead { path: PathBuf, source: io::Error }` at 40. Two
  variants that can only ever be constructed once, at startup, from reading
  `config.toml`, set the size of the error type that *every* fallible function in
  the crate returns.
- **Why it matters here:** `Error` is the `E` of `crate::Result<T>`, including
  `PacketSource::next_segment() -> Result<Segment>` on the capture loop.
  It happens to cost nothing today — `Segment` is also 96, so `Result<Segment>` is
  104 either way — but that is coincidence, and it becomes a live cost the moment
  mem-001 shrinks `Segment`. Clippy will not flag it: `result_large_err` triggers
  at 128 bytes and `large_enum_variant` at a 200-byte difference between variants,
  so 96-vs-40 sits under both thresholds (verified: `cargo clippy --all-targets`
  is clean).
- **Fix:** box the two TOML payloads — `ConfigParse(Box<toml::de::Error>)`,
  `ConfigReparse(Box<toml_edit::TomlError>)` — which takes `Error` to 40 bytes.
  `#[from]` must be dropped and replaced with hand-written conversions, because
  `#[from] Box<T>` generates `From<Box<T>>` and would break the `?` on
  `toml::from_str`:

  ```rust
  impl From<toml::de::Error> for Error {
      fn from(source: toml::de::Error) -> Self { Self::ConfigParse(Box::new(source)) }
  }
  ```

  `Display` and `#[source]` behaviour are unchanged (`Box<E>` derefs to `E`).
  Consider a `const _: () = assert!(size_of::<Error>() <= 48);` alongside, in the
  style the crate already uses (mem-007).
- **Effort:** small

### mem-004 — `wide()` re-encodes a compile-time-constant window title, through a growing `Vec`, before every injected input event

- **Severity:** P2
- **Rule:** [`mem-with-capacity`](../../.claude/skills/rust-skills/rules/mem-with-capacity.md) (also [`mem-avoid-format`](../../.claude/skills/rust-skills/rules/mem-avoid-format.md) — "allocates every time, even for static text")
- **Site:** `src/actuator/win.rs:45` (called from `win.rs:281`), `src/capture/pcap.rs:890`, `src/migrate.rs:166`, `src/migrate.rs:239`
- **What:** all four sites are `text.encode_utf16().chain(once(0)).collect()`.
  `EncodeUtf16::size_hint` returns a *lower* bound of `ceil(len / 3)`, and
  `Vec::from_iter` reserves the lower bound — so the vector allocates small and
  then reallocates as it fills. Measured:

  ```
  wide("Epic Seven")                 -> len 11, capacity 20   (size_hint (4, Some(10)))
  wide("arkyve-refresh-shop-shield") -> len 27, capacity 44
  with_capacity(text.len() + 1)      -> len 11, capacity 11   (one allocation)
  ```

- **Why it matters here:** `GAME_WINDOW_TITLE` is a `&'static str` const, and
  `WinSurface::validate_target` calls `find_game_window` — hence `wide` — **before
  every single injected event**, which the module's own test pins: `validation_calls()`
  appears three times inside one `click` (`win.rs:964-972`), twice inside one
  `scroll`. A two-slot buy job is ~20 events, so ~60 allocations (each with growth
  reallocations) per job, all to re-encode the same 10 bytes. The `migrate.rs` and
  `pcap.rs` sites are cold (startup only) and matter only for consistency.
- **Fix:** two independent parts.
  1. In both `wide` helpers and both `migrate` sites: `Vec::with_capacity(text.len() + 1)`
     then `extend(text.encode_utf16())` and `push(0)`. `text.len()` is an exact bound
     for ASCII and a safe over-estimate otherwise (`Some(len)` is the size_hint's
     upper bound). One allocation, no growth.
  2. For the hot one, remove the allocation entirely: hoist the encoded title to a
     `static GAME_WINDOW_TITLE_W: [u16; 11]` (or a small `const fn` encoder) and pass
     `.as_ptr()` to `FindWindowW`, so `validate_target` allocates nothing at all.
  The two duplicate `wide` helpers (`win.rs`, `pcap.rs`) could also collapse into one.
- **Effort:** small

### mem-005 — The dedup fingerprint deep-clones every substat string of every slot, then clones the whole structure again

- **Severity:** P2
- **Rule:** [`mem-zero-copy`](../../.claude/skills/rust-skills/rules/mem-zero-copy.md), [`mem-clone-from`](../../.claude/skills/rust-skills/rules/mem-clone-from.md)
- **Site:** `src/domain/control/dedup.rs:23-37` (+ `src/domain/control/mod.rs:470`)
- **What:** `fingerprint` builds a `Vec<SlotIdentity>` in which each slot owns
  `set: Option<String>` and `substats: Vec<SubStat>` (each `SubStat` owning a
  `String`) — all cloned out of the snapshot. For a 6-slot shop with ~4 substats
  each that is ~30 heap allocations per shop message. The result is only ever
  *compared for equality and cloned*; not one of those strings is ever read. Then
  `mod.rs:470` clones the whole structure a second time:

  ```rust
  self.bought_fingerprint = self.acted_fingerprint.clone();
  ```

  which is the `mem-clone-from` Bad example verbatim.
- **Why it matters here:** it runs on the decode path for every shop message, and
  it keeps two extra owned copies of the current roll's substat strings alive
  inside `Controller` for as long as the roll lasts — on top of the copy
  `last_snapshot` already holds.
- **Fix:** in increasing order of payoff.
  1. `self.bought_fingerprint.clone_from(&self.acted_fingerprint);` — reuses the
     outer `Vec` buffer, no semantic change. Be aware it does *not* save the inner
     `String` allocations on its own: derived `Clone` does not override
     `clone_from`, so each `SlotIdentity` is still deep-cloned. A hand-written
     `Clone::clone_from` on `SlotIdentity` delegating to `String::clone_from` /
     `Vec::clone_from` recovers those too.
  2. `Box<str>` instead of `String` inside `SlotIdentity` — same allocation count,
     8 fewer bytes each, and it states that the field is never mutated.
  3. Replace the whole structure with a hash: `fn fingerprint(&ShopSnapshot) -> Option<u64>`
     feeding exactly the same fields into a `DefaultHasher`, and `Option<u64>` for
     both `Controller` fields. Zero allocations, and `Controller` loses four words.
     **State the trade-off before doing this:** it swaps exact equality for a
     64-bit hash, so a collision would mute a genuinely *new* shop — precisely the
     failure `dedup.rs`'s doc comment is written to prevent ("a re-roll redrawing
     the same catalog ids … must read as a new shop"). Negligible probability at
     six slots, but a real change in kind: a deliberate decision, not a drive-by
     refactor.
- **Effort:** small (1–2) / medium (3)

### mem-006 — `format_item` builds its line from six throwaway `format!` Strings, once per slot per frame

- **Severity:** P2
- **Rule:** [`mem-write-over-format`](../../.claude/skills/rust-skills/rules/mem-write-over-format.md) (also [`mem-avoid-format`](../../.claude/skills/rust-skills/rules/mem-avoid-format.md))
- **Site:** `src/render.rs:133-160` (+ `src/ui/journal.rs:122`, `src/render.rs:44`)
- **What:** `format_item` is `let mut line = format!(..)` followed by six
  `line.push_str(&format!(..))` — each inner `format!` allocates a `String`, is
  copied into `line`, and is dropped. That is the literal Bad block of
  `mem-write-over-format` (`result.push_str(&format!(...))`). The substat clause
  adds a `Vec<String>` (one `String` per substat) plus a `join(", ")`. Adjacent
  sites: `ui/journal.rs:122` calls `format!("{}  {}", timestamp(line.at_ms), line.text)`
  per visible row, where `timestamp` itself returns a `String` — two allocations
  where egui forces only one; `render.rs:44` materialises `n.to_string()` purely to
  iterate its digits into a buffer that was correctly `with_capacity`-sized.
- **Why it matters here:** `view_state` (`src/ui/view.rs:71`) calls `format_item`
  for **every slot on every frame**, to fill `SlotRow::detail` — a hover tooltip.
  At the window's 4 Hz repaint (`ui/mod.rs:137`) that is ~6 slots × ~10 throwaway
  allocations, four times a second, rebuilding a string that changes only when a
  new shop arrives.
- **Fix:** two parts, both worthwhile.
  1. `use std::fmt::Write;` and `write!(line, " · {name}").unwrap()` for each
     append (infallible on `String`; `unwrap()` is what the rule shows). Write the
     substats straight into `line` with a `", "` separator instead of collecting
     and joining. In `ui/journal.rs`, hoist one `String` outside the row loop and
     `buf.clear(); write!(&mut buf, ..)` so `timestamp`'s separate allocation
     disappears. In `grouped`, `write!` the digits directly instead of via
     `to_string()`.
  2. Cache `SlotRow::detail` instead of rebuilding it per frame. The file next door
     already does exactly this for the journal — `ShopApp::journal_cache` is
     re-cloned only when `EventLog::generation()` changes, with the comment "the
     journal grows at human pace, repaints at display rate" (`ui/mod.rs:92`,
     `142-146`). The shop snapshot changes at human pace too and gets no such
     treatment; the same argument transfers unchanged.
- **Effort:** small (`write!`) / medium (caching the rows)

### mem-007 — The FFI layout the module calls "the single most dangerous constant in this file" is size-checked only by a runtime test

- **Severity:** P2
- **Rule:** [`mem-assert-type-size`](../../.claude/skills/rust-skills/rules/mem-assert-type-size.md)
- **Site:** `src/capture/pcap.rs:1181-1187` (guarding `PcapPktHdr` at `pcap.rs:142` and `PcapStat` at `pcap.rs:163`); also `src/app/mod.rs:49` (`CaptureEvent`)
- **What:** `assert_eq!(size_of::<PcapPktHdr>(), 16)` and
  `assert_eq!(size_of::<PcapStat>(), 24)` live inside `#[cfg(test)] mod tests`. The
  rule asks for a compile-time `const _: () = assert!(..)` for FFI and
  binary-protocol types, with tests as a complement rather than the guard.
- **Why it matters here:** not a style point. The module documents (`pcap.rs:136-147`,
  `390-406`) that a `timeval` declared with 64-bit members shifts `caplen` onto the
  low half of `tv_usec` and yields *plausible but wrong lengths that slice without
  faulting*, and that `plausible_caplen` is explicitly "a canary, not a proof"
  catching it only "within the first few packets". A `const` assert turns that
  class of mistake into a build failure instead of something a developer must run
  the Windows, feature-gated test suite to discover. The crate demonstrably knows
  the idiom — `capture/mod.rs:77-83`, `stream.rs:418-424` and
  `ui/editor/timing_meter.rs:45-52` all use it — so the one type where the layout
  is a genuine ABI contract with `wpcap.dll` is the gap.
  Second gap, for the reason the crate itself states: `stream.rs:411-417` warns
  that "a `CaptureEvent` holding a `BudgetedSegment` is stored *by value* in a
  512-slot channel: one extra field … silently inflates tens of KiB of queue" — and
  then asserts the *field* types, not the enum that is actually queued. A variant
  added to `CaptureEvent` carrying anything above 120 bytes inflates that 64 KiB
  queue with no canary firing.
- **Fix:** move both `assert_eq!`s to `const _: () = assert!(size_of::<PcapPktHdr>() == 16, "…");`
  beside the structs (the structs are already `#[repr(C)]`, so the assert is
  meaningful; keep the test too if desired, it costs nothing). Add
  `const _: () = assert!(std::mem::size_of::<CaptureEvent>() == 128);` in
  `app/mod.rs`, `cfg(target_pointer_width = "64")`-gated and worded like the
  existing canaries ("re-measure and update the number deliberately, never work
  around it").
- **Effort:** trivial

### mem-008 — The journal snapshot deep-clones up to 500 strings every time one line is added

- **Severity:** P2
- **Rule:** [`mem-reuse-collections`](../../.claude/skills/rust-skills/rules/mem-reuse-collections.md), [`mem-clone-from`](../../.claude/skills/rust-skills/rules/mem-clone-from.md)
- **Site:** `src/journal.rs:100-107` (`EventLog::entries`) + `src/ui/mod.rs:144`
- **What:** `entries()` clones the entire `VecDeque<LogLine>` into a fresh `Vec`,
  allocating a new `String` per entry. The GUI is careful to call it only when the
  generation moved (`ui/mod.rs:142-146`) — but every journal push makes the next
  frame re-clone the whole ring: up to 500 `String` allocations plus the `Vec`, to
  observe one added line.
- **Why it matters here:** `EventLog::emit` is the single sink for every
  player-facing line, and pushes are bursty by design — `apply` returns several
  lines at once and `render_match` emits one per matched slot. The `JOURNAL_CAP`
  of 500 that bounds memory is exactly what makes each snapshot expensive.
- **Fix:** make the text cheap to clone rather than making the clone cheaper —
  `text: Arc<str>` in `LogLine` turns the snapshot into 500 refcount bumps plus one
  `Vec` allocation, while `push` keeps paying the single real allocation it already
  pays. If the type must stay `String`, add
  `fn clone_entries_into(&self, out: &mut Vec<LogLine>)` doing `out.clear(); out.extend(..)`
  so the `Vec` buffer is reused across frames — note that `Vec::clone_from` alone
  reuses only the outer buffer, because derived `Clone` on `LogLine` does not
  override `clone_from` and each `String` is still reallocated. `Arc<str>` is the
  option that removes the work instead of relocating it.
- **Effort:** small

### mem-009 — The ≤6-slot collections on the decode path all heap-allocate

- **Severity:** P3
- **Rule:** [`mem-arrayvec`](../../.claude/skills/rust-skills/rules/mem-arrayvec.md) (also [`mem-smallvec`](../../.claude/skills/rust-skills/rules/mem-smallvec.md))
- **Site:** `src/actuator/plan.rs:522`, `src/app/session/mod.rs:600-604`, `src/app/session/mod.rs:634-637`
- **What:** the Secret Shop has exactly six slots — a game constant the crate
  already encodes (`plan::row_for_slot` rejects `> 5`, `buy_zone` covers rows
  0..=5). Every collection derived from it nevertheless heap-allocates:
  `buy_job`'s `rows` (built by a `filter(|&row| row <= 5)` immediately above, then
  sorted and deduped), `submit_buys`'s `rows`, and `render_match`'s
  `list: Vec<String>` of slot numbers built only to `join(", ")`.
- **Why it matters here:** honestly, very little — these run once per shop message,
  a couple of seconds apart, not per packet. The reason to record it is that
  `buy_job`'s `rows` is the textbook `ArrayVec<u8, 6>` case: a *hard* bound already
  enforced one line earlier, with `sort_unstable` and `dedup` both available on
  `ArrayVec`. The change documents the invariant at least as much as it saves the
  allocation.
- **Fix:** `ArrayVec<u8, 6>` for the two `rows` (weigh that against adding the
  `arrayvec` dependency), and in `render_match` `write!` the slot numbers into the
  line directly instead of collecting `Vec<String>` and joining (folds into
  mem-006). Do **not** convert `Controller::checklist` / `bought` / `plan_targets`'
  `targets`: they cross API boundaries (`checklist() -> &[u32]`,
  `Action::Buy { targets: Vec<BuyTarget> }` is `PartialEq`-compared in ~30 tests)
  and the churn would exceed the benefit. `mem-smallvec` itself says "Profile to
  verify benefit!" — this finding is genuinely optional.
- **Effort:** small

## Clean areas

- **`mem-assert-type-size` — honoured, and unusually well.** `capture/mod.rs:77-83`
  pins `FlowKey == 64` and `Segment == 96`; `stream.rs:418-424` pins
  `BudgetedChunk == 48` and `BudgetedSegment == 120`; `ui/editor/timing_meter.rs:45-52`
  uses the same idiom as a ruler tripwire over eight timing baselines. All are
  `cfg(target_pointer_width = "64")`-gated so a 32-bit build is not broken, and all
  carry a comment saying a failure means "re-measure and update the number
  deliberately", never work around it. mem-007 is a gap in an otherwise exemplary
  application of this rule.
- **`mem-with-capacity` — honoured where it matters, with the reasoning written down.**
  `stream.rs:496-497` explains that `collect` over a slice iterator is already
  exact-size (TrustedLen) so only the `HashMap` needs a hint;
  `config/persist.rs:287` sizes the tidy buffer at `text.len()`; `render.rs:46`
  computes the exact grouped-digit length; `pcap.rs:575` sizes the thread vector
  from `handles.len()`; even the test packet builders (`capture/ip.rs:90,107`,
  `pcap.rs:1156`) use `builder.size(payload.len())`. mem-004 is the one place the
  rule is missed, and only in the `collect()`-from-an-iterator form.
- **`mem-zero-copy` — honoured on the outbound half.** `BudgetedChunk::into_parts`
  hands the `Vec<u8>` straight into `Message::Binary(bytes.into())`
  (`uplink/websocket.rs:186-187`), and `bytes::Bytes::from(Vec<u8>)` takes ownership
  rather than copying — reassembled bytes reach the socket with no further copy.
  `LinkStrip::ip_bytes` and `ethernet_payload_offset` (`pcap.rs:354-388`) return
  borrowed subslices instead of owned buffers, and `HalfStream::pending` moves
  `BudgetedChunk`s by value rather than copying their bytes. mem-001 is specifically
  about the *inbound* half.
- **`mem-avoid-format` — the GUI already reasons about this explicitly.**
  `ui/journal.rs:53-55` and `ui/theme.rs:216-225` both build the accessible name
  *inside* the `widget_info` closure so the `format!` is not paid per frame, each
  with a comment saying exactly that; `ui/editor/mod.rs:104-111` builds a section's
  summary only while the section is folded, "so an expanded Setup tab doesn't
  re-allocate discarded strings every frame". `kind_label`, `describe`, `refusal`,
  `merchant_label`, `status_summary`, `mode_hint` and `TimingPreset::label` all
  return `&'static str` rather than `String`, which is what the rule asks for.
- **Per-frame `String`s in `ui/shop.rs` and `ui/statusbar.rs` are *not* findings.**
  egui's `RichText::new(impl Into<String>)` and `ui.monospace`/`ui.weak` require an
  owned `String`, so `row.slot.to_string()`, `grouped_or_dash(..)` and `against(..)`
  are at the API's floor — one allocation each, unavoidable. `ui/shop.rs:88-94`
  documents having already minimised them ("each caller pays one copy at most").
  Likewise `theme::section`'s `to_uppercase()` costs nothing extra: `RichText` would
  allocate a `String` from a literal anyway.
- **`mem-take-replace` — used correctly and idiomatically.**
  `std::mem::replace(anchor, AnchorState::Steady)` for the state-machine transition
  in `app::flush_anchor` (`app/mod.rs:780`), `std::mem::take(&mut outage_reported)`
  in `uplink/websocket.rs:113`, and `Option::take` for one-shot capabilities
  (`CaptureWorker::thread`, `SurfaceJobGuard::surface`, `TokioWorker::handle`,
  `PcapSource::threads.drain(..)`). No `.clone()`-to-move-out-of-`&mut` anywhere.
- **`mem-smaller-integers` — consistently right.** `u8` for slots/grades/rows/attempt
  counters, `u16` for ports, `u32` for prices and catalog ids, `f32` (not `f64`)
  throughout the coordinate space (`plan::Zone`, `DesignPoint`), `c_int`/`c_uint`
  matched to the FFI. `HaltSource` is a `#[repr(u8)]` bitmask packing two causes
  into one `AtomicU8` (`watch.rs:23-38`) instead of two bools. Field ordering in
  `Segment`, `HalfStream` and `PcapPktHdr` already leads with the wide fields.
- **`mem-drop-order` — the two load-bearing cases are handled explicitly.** `Wpcap`
  keeps `_lib: libloading::Library` in the same struct as the function pointers
  whose validity depends on it, "never separated from them" (`pcap.rs:180-198`);
  `Handle::drop` closes the `pcap_t` on the owning thread and documents why no
  receive can be in flight (`pcap.rs:432-440`); `PayloadLease::drop` releases budget
  and `stream.rs:182-191` explains at length why it must saturate rather than assert
  because it runs during unwinding; `stream.rs:758-761` drops a displaced pending
  chunk explicitly, after decrementing the counter.
- **`mem-reuse-collections` — the pattern is already present and correct** for
  `ShopApp::journal_cache`, a snapshot re-taken only on a generation change, with
  the reasoning in a comment (`ui/mod.rs:92`). mem-006 and mem-008 extend that same
  idea to the shop rows and to the clone itself.

## Not applicable

- **`mem-arena-allocator`** — no parse-tree or request-scoped allocation graph. The
  one bulk-allocating path, `InitialBurst::into_ordered` (`stream.rs:492-533`), runs
  once per resync and hands its `BudgetedSegment`s onward by value; an arena cannot
  own values that escape its scope. Adding `bumpalo` here would be pure cost.
- **`mem-thinvec`** — the rule's case is "many instances, often empty". No such type
  exists here: `Controller` and `EditorState` are singletons, `ShopItem::substats` is
  usually non-empty, and the one often-empty container in a hot type
  (`HalfStream::pending`) is a `BTreeMap`, not a `Vec`. `ThinVec` would also put
  `len`/`cap` behind a pointer indirection on the reassembly path, which the rule
  explicitly warns against ("Avoid: hot loops, performance-critical iteration").
- **`mem-compact-string`** — the rule's case is millions of short strings. The
  largest string population in this crate is the 500-entry journal ring, and
  `CompactString` is the same 24 bytes as `String`, avoiding the heap only for
  entries ≤ 23 bytes — which journal lines
  (`">> actuator: … — stopping the loop"`) are not. mem-008's `Arc<str>` addresses
  the real cost there instead.
- **`mem-boxed-slice`** — the rule's stated case is "fixed-size, heap-allocated,
  **many instances**", where 8 bytes × N is the point. Every build-once-then-frozen
  `Vec` here has a handful of live instances: `ViewState::rows` (one per frame),
  `Job::steps` (one per job), `PcapSource::threads` (one per session),
  `Controller::acted_fingerprint` / `bought_fingerprint` (one each). Converting them
  would save tens of bytes in total and cost a conversion at every construction
  site, so this is **deliberately not filed** — do not "fix" it. The one place
  `Box<str>`/`Arc<str>` genuinely pays is `LogLine::text` × 500, which is mem-008,
  and `SlotIdentity`'s strings, which is mem-005 step 2. `Filter`'s and
  `EditorState`'s `Vec`s must stay `Vec`: the Setup editor pushes to and removes
  from them live (`ui/editor/mod.rs:692-760`).
- **`mem-clone-from`** — covered inside mem-005 and mem-008; there is no third site
  where a value is repeatedly cloned into an existing binding.
  `EditorState::mark_applied` (`ui/editor/mod.rs:79-81`) has the right shape
  (`applied_filter = filter.clone()`) but runs once per Apply click — a human
  action — so filing it would be padding.
