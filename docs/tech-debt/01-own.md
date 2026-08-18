# 01 — Ownership & Borrowing (`own-`)

**Category priority:** CRITICAL
**Rules audited:** 12 · **Files read:** 34 (every `.rs` in `src/`, `examples/`, `build.rs`, plus `Cargo.toml`) · **Findings:** 9 (P0 0 / P1 0 / P2 2 / P3 7)

## Verdict

This category is in good shape, and unusually so for a crate with a per-packet
hot path: `own-slice-over-vec` is honoured **everywhere** (not one `&Vec<T>`,
`&String` or `&PathBuf` read-only parameter exists in the crate),
`own-lifetime-elision` is honoured exactly (the one explicit `'a` is the one
elision would get wrong), the move-only `BudgetedChunk`/`PayloadLease` pair in
`src/stream.rs` is a textbook use of ownership to enforce an accounting
invariant, and `Mutex` vs `RwLock` was chosen correctly at every one of the four
sharing points. `cargo clippy` with `redundant_clone`, `ptr_arg`,
`assigning_clones`, `needless_pass_by_value` and `trivially_copy_pass_by_ref`
turned up exactly **one** redundant clone in the whole tree.

The worst offender is the packet path: every captured packet is heap-copied
**twice** — once in `src/capture/pcap.rs:963` (`ip.to_vec()`) and again in
`src/capture/ip.rs:66` (`tcp.payload().to_vec()`), the second copying a subslice
of the first before the first is dropped. That is the single highest-value fix
(`own-001`), in a module that already treats per-packet cost as a first-class
concern via its size canaries. Second is `src/ui/view.rs`, which deep-copies the
stored shop snapshot (one `String` per slot plus a freshly `format!`-built detail
line) on *every* egui frame, when the correct pattern — a generation-gated cache
— already exists twenty lines away in `src/ui/mod.rs` for the journal.

Nothing here is a bug. No `P0`/`P1`.

## Findings

### own-001 — Every captured packet is heap-copied twice on the hot path

- **Severity:** P2
- **Rule:** [`own-borrow-over-clone`](../../.claude/skills/rust-skills/rules/own-borrow-over-clone.md)
- **Site:** `src/capture/pcap.rs:963` and `src/capture/ip.rs:66` (consumer at `src/capture/pcap.rs:637-651`)
- **What:** The capture thread copies the stripped IP packet out of libpcap's
  buffer into an owned `Vec<u8>`:

  ```rust
  if packets.send(ip.to_vec()).is_err() {   // pcap.rs:963 — allocation #1
  ```

  That first copy is unavoidable: the slice is invalidated by the next
  `pcap_next_ex` on the handle, and the `// SAFETY:` comment says so. The
  *second* one is not. `PcapSource::next_segment` receives that owned `Vec`, and
  `parse_segment` immediately allocates again for a subslice of it:

  ```rust
  payload: tcp.payload().to_vec(),          // ip.rs:66 — allocation #2
  ```

  `packet` (allocation #1) is then dropped at the bottom of the loop iteration.
  So each packet costs two allocations, two memcpys of very nearly the same
  bytes, and two frees.
- **Why it matters here:** This is the one loop in the crate that runs per
  captured packet, and the module is explicitly built around that cost —
  `src/capture/mod.rs:72-83` and `src/stream.rs:411-424` carry size canaries
  whose comments say "a field added here is paid for on every packet". Meanwhile
  the payload copy is silently paying for a whole second buffer per packet, and
  the buffers are not small: the `SNAPLEN` doc comment at
  `src/capture/pcap.rs:62-73` records a **48 870-byte** measured RSC/LRO frame on
  the development machine, so allocation #2 can be tens of kilobytes.
- **Fix:** Keep the one unavoidable copy and reuse its allocation instead of
  making a second. Split `parse_segment` into a header decode that reports the
  payload's *range* inside the frame, then trim the already-owned `Vec` in place:

  ```rust
  // capture/ip.rs
  pub struct SegmentMeta { pub flow: FlowKey, pub seq: u32, pub syn: bool, pub payload: Range<usize> }
  pub fn parse_segment_meta(bytes: &[u8], game_port: u16) -> Option<SegmentMeta> { /* … no to_vec() */ }

  // capture/pcap.rs, in next_segment:
  let mut packet = self.packets.recv()…?;                  // already owned
  let Some(meta) = parse_segment_meta(&packet, self.game_port) else { … };
  packet.truncate(meta.payload.end);
  packet.drain(..meta.payload.start);                       // no allocation
  return Ok(Segment { flow: meta.flow, seq: meta.seq, syn: meta.syn, payload: packet });
  ```

  Keep the existing `parse_segment` as a thin wrapper so `src/capture/ip.rs`'s
  eight tests and `src/capture/pcap.rs:1147`'s end-to-end test keep asserting on
  the same surface.

  **One caveat that must be handled deliberately, not silently:**
  `PipelineBudget::admit_capture` charges `segment.payload.capacity()`
  (`src/stream.rs:127`), and after `truncate` + `drain` the capacity is the whole
  frame's, not the payload's. That is arguably *more* honest — it is the memory
  actually retained — but it changes the numbers the budget tests in
  `src/stream.rs:908-1059` pin, and it raises the effective per-packet charge
  against `CAPTURE_STAGE_BYTES`. Either accept it and re-baseline those tests, or
  add a `shrink_to_fit()` after the trim (which reintroduces one allocation, but
  only for the segments that are actually buffered).
- **Effort:** medium

### own-002 — `ViewState` deep-copies the stored snapshot on every egui frame

- **Severity:** P2
- **Rule:** [`own-borrow-over-clone`](../../.claude/skills/rust-skills/rules/own-borrow-over-clone.md) (fix uses [`own-arc-shared`](../../.claude/skills/rust-skills/rules/own-arc-shared.md))
- **Site:** `src/ui/view.rs:53-89` (esp. `:67` `name: item.name.clone()` and `:71` `detail: format_item(item, index)`), called from `src/ui/mod.rs:138-141`
- **What:** `ShopApp::ui` calls `view_state(&ctrl)` unconditionally, once per
  frame. For each slot that rebuilds a `SlotRow` containing a cloned
  `Option<String>` name plus a `detail: String` freshly assembled by
  `render::format_item` (`src/render.rs:130-161`), which is a `format!` for the
  base line and then one `push_str(&format!(…))` per present field — roughly six
  to twelve allocations per row, on a six-slot shop with substats.
- **Why it matters here:** The 4 Hz `request_repaint_after` at
  `src/ui/mod.rs:137` is the *floor*, not the rate. The shop table senses hover
  per row (`src/ui/shop.rs:61` `response.contains_pointer()`) and attaches a
  tooltip (`:79` `response.on_hover_text(&row.detail)`), so any pointer movement
  over the table repaints at display rate. The data being rebuilt changes once
  per shop refresh — every couple of seconds. The file already knows this is the
  wrong shape: `src/ui/mod.rs:91-94` documents `journal_cache` as "re-cloned only
  when the generation changes: the journal grows at human pace, repaints at
  display rate", and `src/ui/journal.rs:53-61` and `src/ui/theme.rs:216-225` both
  go out of their way to build strings *inside* `widget_info` closures precisely
  so a `format!` is not paid every frame. The shop table is the one surface that
  did not get the treatment.
- **Fix:** Two steps, either of which helps on its own:
  1. Store the snapshot behind an `Arc` in the domain —
     `last_snapshot: Option<Arc<ShopSnapshot>>` in `Controller`
     (`src/domain/control/mod.rs:252`), with `last_snapshot()` returning
     `Option<&Arc<ShopSnapshot>>`. `view_state` then clones one refcount out from
     under the lock instead of the contents.
  2. Cache the derived `Vec<SlotRow>` in `ShopApp` beside `journal_cache`, keyed
     on `Arc::ptr_eq` against the previous snapshot (plus the checklist, which
     also feeds `wanted`). Rebuild only when the pointer or the checklist moved —
     the exact shape `journal_generation` already uses.
- **Effort:** medium

### own-003 — `Limits` is 40 bytes of `Copy` fields but only derives `Clone`

- **Severity:** P3
- **Rule:** [`own-copy-small`](../../.claude/skills/rust-skills/rules/own-copy-small.md)
- **Site:** `src/domain/control/mod.rs:50-62` (declaration); `.clone()` sites at `src/app/mod.rs:224`, `src/ui/mod.rs:115`, `src/ui/mod.rs:308`, `src/ui/view.rs:81`, `src/ui/editor/mod.rs:54`, `src/ui/editor/mod.rs:80`
- **What:** `Limits` is `Option<u32> × 3` plus `Option<u64>` — **40 bytes**
  (measured), every field `Copy`, no `Drop`, no heap. It derives `Clone` but not
  `Copy`, so all six call sites above must write `.clone()`, one of them
  (`view.rs:81`) on every frame.
- **Why it matters here:** This is `own-copy-small`'s "Bad" example verbatim
  ("Small type without Copy - requires explicit clone … Every use needs clone"),
  and the crate is internally inconsistent about it: `Progress` (12 bytes) *is*
  `Copy` and is returned by value from `Controller::progress()`, and
  `plan::Timings` (**128** bytes — see `own-009`) is `Copy` too. A reader cannot
  tell from the call sites which of the three costs anything.
- **Fix:** Add `Copy` to the derive on `Limits` and delete the six `.clone()`
  calls. `Copy` is legal here (all fields `Copy`, no `Drop`) and 40 bytes sits
  inside the rule's "≤ 16 bytes / 17-64 bytes: consider" band. `Deserialize`,
  `Serialize`, `PartialEq` and `Eq` are unaffected.
- **Effort:** trivial

### own-004 — Each new shop deep-clones its whole fingerprint twice

- **Severity:** P3
- **Rule:** [`own-borrow-over-clone`](../../.claude/skills/rust-skills/rules/own-borrow-over-clone.md) (fix uses [`own-arc-shared`](../../.claude/skills/rust-skills/rules/own-arc-shared.md))
- **Site:** `src/domain/control/mod.rs:470`, with the first copy at `src/domain/control/dedup.rs:28-35`
- **What:** `fingerprint()` builds a `Vec<SlotIdentity>` in which every slot deep-clones
  its `set: Option<String>` and its whole `substats: Vec<SubStat>` (each `SubStat`
  owning a `String`). `evaluate_snapshot` then stores that vector in
  `acted_fingerprint` and *immediately deep-clones it again* into
  `bought_fingerprint`:

  ```rust
  self.acted_fingerprint = fingerprint;
  if self.acted_fingerprint != self.bought_fingerprint {
      self.bought.clear();
      self.bought_fingerprint = self.acted_fingerprint.clone();  // second deep copy
  }
  ```

  For a six-slot shop with four substats each that is roughly thirty `String`
  allocations for the first copy and thirty more for the second, per new shop.
- **Why it matters here:** Both fields hold the *same* value whenever the second
  assignment runs, and neither is ever mutated in place — they are only compared
  (`:456`, `:468`) and replaced wholesale. Two owned deep copies of an
  immutable-after-construction value is exactly what `Arc` is for. It is also
  the only remaining unbounded-width clone in the domain layer, which is
  otherwise allocation-frugal.
- **Fix:** Make both fields `Option<Arc<Vec<SlotIdentity>>>` (or
  `Option<Arc<[SlotIdentity]>>`). `PartialEq` on `Arc<T>` delegates to `T`, so
  `:456` and `:468` keep their exact semantics; the assignment at `:470` becomes
  a refcount bump. `Arc`, not `Rc`: `Controller` lives inside
  `Arc<Mutex<Controller>>` (`src/app/mod.rs:231`) and must stay `Send`.
- **Effort:** small

### own-005 — `hunt_summary` allocates a label it may then throw away

- **Severity:** P3
- **Rule:** [`own-borrow-over-clone`](../../.claude/skills/rust-skills/rules/own-borrow-over-clone.md) · see also [`own-cow-conditional`](../../.claude/skills/rust-skills/rules/own-cow-conditional.md)
- **Site:** `src/ui/editor/mod.rs:150-158`
- **What:**

  ```rust
  let mut label = name.clone();                 // always allocates
  for (wire, headliner) in crate::render::HAUL_HEADLINERS {
      if name == wire {
          label = headliner.to_owned();         // …and may immediately replace it
          break;
      }
  }
  ```

  For a hunted headline token (the common case — the quick-add buttons at
  `:288-301` only ever insert those two wire ids) the `name.clone()` is allocated
  and dropped without ever being read. `clippy::assigning_clones` flags line 154.
- **Why it matters here:** Small, but it is on the collapsed-section summary
  path, and the surrounding code is otherwise scrupulous about not allocating
  per frame — `edit_sections` at `:111-123` deliberately builds each summary only
  while its section is folded, with a comment saying so. This line undercuts that
  care.
- **Fix:** Decide before allocating; the winner is a `&str` in both branches.

  ```rust
  let label = crate::render::HAUL_HEADLINERS
      .iter()
      .find(|(wire, _)| name == wire)
      .map_or(name.as_str(), |(_, headliner)| *headliner);
  parts.push(label.to_owned());
  ```
- **Effort:** trivial

### own-006 — Raw `Arc<…>.clone()` at the six sites that are not behind a newtype

- **Severity:** P3
- **Rule:** [`own-arc-shared`](../../.claude/skills/rust-skills/rules/own-arc-shared.md) · [`own-rc-single-thread`](../../.claude/skills/rust-skills/rules/own-rc-single-thread.md) ("Key Points")
- **Site:** `src/capture/pcap.rs:579`, `:580`, `:604`, `:758`; `src/main.rs:274` (two on one line)
- **What:** `stop.clone()`, `capture_loss.clone()`, `stop: stop.clone()`,
  `wpcap: wpcap.clone()`, and `(error.clone(), failed.clone())` are all refcount
  bumps on a bare `Arc`, spelled as though they might be deep copies. Confirmed
  by `clippy::clone_on_ref_ptr` (six hits in non-test code; the rest of the lint's
  output is test fixtures).
- **Why it matters here:** The crate's own convention is the opposite, and
  strongly so: every other shared handle is a newtype whose cheap `Clone` is
  documented at the type — `WatchGate`, `EventLog`, `SnapshotEpoch`,
  `ShutdownSignal`, `PressureResync`, `PipelineBudget`, `SessionHandles`
  ("Cheap clones of the shared session state"). A reader who has learned that
  convention hits `wpcap.clone()` inside `open_device` — a per-adapter function
  that also does `device.to_owned()` on the very next line — and has to go read
  the struct to find out which one is expensive.
- **Fix:** Spell them `Arc::clone(&stop)` / `Arc::clone(&wpcap)` etc. Consider
  turning on `clippy::clone_on_ref_ptr` in `Cargo.toml`'s `[lints]` to keep it
  that way (coordinate with the `lint-` reviewer, who owns that table).
- **Effort:** trivial

### own-007 — Redundant `config_path.clone()` in a `FnOnce` closure

- **Severity:** P3
- **Rule:** [`own-borrow-over-clone`](../../.claude/skills/rust-skills/rules/own-borrow-over-clone.md)
- **Site:** `src/main.rs:307`
- **What:** `ui::ShopApp::new(cc, handles, error, seed_timings, config_path.clone())`
  inside the `Box::new(move |cc| …)` passed to `eframe::run_native`. `config_path`
  is captured by the closure and never used again after it, so the clone is dead
  weight. Verified twice over: `clippy::redundant_clone` flags exactly this line
  and nothing else in the crate, and `eframe`'s `AppCreator` is
  `Box<dyn FnOnce(&CreationContext) -> …>`
  (`eframe-0.35.0/src/epi.rs:49-50`), so moving out of the capture compiles.
- **Why it matters here:** Once, at startup, on a `PathBuf` — the cost is nil.
  It is worth fixing because it is the crate's *only* clippy-detectable redundant
  clone, so removing it makes the lint usable as a standing gate rather than a
  one-line permanent warning.
- **Fix:** Drop the `.clone()`.
- **Effort:** trivial

### own-008 — `crash_log_paths_from` takes owned paths it only borrows

- **Severity:** P3
- **Rule:** [`own-slice-over-vec`](../../.claude/skills/rust-skills/rules/own-slice-over-vec.md) ("Path Types Too")
- **Site:** `src/crash.rs:75`
- **What:** `fn crash_log_paths_from(local_appdata: Option<PathBuf>, temp: PathBuf) -> Vec<PathBuf>`.
  Both parameters are consumed by value and then only *borrowed* —
  `local.join(…)` and `temp.join(…)` both take `&self` and allocate a fresh
  `PathBuf`. Flagged by `clippy::needless_pass_by_value`.
- **Why it matters here:** This is the whole reason the function was split out of
  `crash_log_paths` — it is the pure, testable core, and its two tests
  (`:130-145`) have to mint `PathBuf`s to call it. `Option<&Path>` / `&Path`
  would let them pass literals, and would let the real caller at `:70` hand over
  a borrow instead of a move. The rule names this case explicitly.
- **Why it is only P3:** it runs once per panic, and the panic hook is already
  allocating a backtrace string.
- **Fix:** `fn crash_log_paths_from(local_appdata: Option<&Path>, temp: &Path) -> Vec<PathBuf>`,
  and `crash_log_paths_from(local.as_deref(), &std::env::temp_dir())` at `:70`.
- **Effort:** trivial

### own-009 — `Timings` is `Copy` at 128 bytes, and `named_ranges` copies all of it

- **Severity:** P3
- **Rule:** [`own-copy-small`](../../.claude/skills/rust-skills/rules/own-copy-small.md) ("Size Guidelines")
- **Site:** `src/actuator/plan.rs:228-255` (declaration), `:264-274` (`named_ranges`)
- **What:** `Timings` is eight `DelayRange`s of 16 bytes each — **128 bytes**
  (measured) — and derives `Copy`, above the rule's "> 64 bytes: probably don't,
  prefer references" line. `named_ranges` then takes `&self` and immediately
  copies the whole thing to destructure it:

  ```rust
  pub fn named_ranges(&self) -> [(&'static str, DelayRange); 8] {
      let Timings { shop_opened, … } = *self;   // 128-byte copy, then 8 more
  ```

  It is also passed by value into `refresh_job`, `buy_job` and
  `confirm_retry_job` (`:485`, `:498`, `:519`) and returned by value from
  `ActuatorHandle::timings()` (`src/actuator/mod.rs:94`).
- **Why it matters here:** Mostly as a documented deviation rather than a cost —
  job building happens a few times per refresh, not per packet. The value is in
  writing the measurement down: a reader applying the rule's table would
  reasonably conclude `Copy` is wrong here, and act on it.
- **Fix:** **Keep `Copy`.** Removing it would force a `.clone()` at every one of
  those sites for no gain — the type has no heap data, so `Copy` is semantically
  correct and the ergonomics are the point (`own-clone-explicit`'s whole argument
  is that `.clone()` should signal cost; here there is none beyond a memcpy). Two
  cheap improvements instead:
  1. Destructure through the reference in `named_ranges` — `let Timings { shop_opened, … } = self;`
     and copy the eight 16-byte ranges out individually. Same exhaustiveness
     guarantee the comment at `:262` relies on, no 128-byte copy.
  2. Add a size canary beside the type, in the style of the two the crate already
     has (`src/capture/mod.rs:77-83`, `src/stream.rs:418-424`), so a ninth
     action is a deliberate decision:
     `const _: () = assert!(size_of::<Timings>() == 128);`
- **Effort:** trivial

## Clean areas

**`own-slice-over-vec` — fully honoured, crate-wide.**
- Not one read-only `&Vec<T>`, `&String`, `&PathBuf` or `&Box<T>` parameter
  exists in `src/`. Every `&mut Vec<T>` that does exist genuinely mutates length
  (`src/app/session/mod.rs:468-633` accumulate lines; `src/stream.rs:711`/`:777`
  accumulate chunks; `src/ui/editor/mod.rs:288`/`:692`/`:723` push and remove);
  `src/stream.rs:373`'s `&Vec<u8>` is `PartialEq<Vec<u8>>`'s required signature.
  `clippy::ptr_arg` is silent. `Config::load` and `persist::save` take
  `impl AsRef<Path>`, which is the rule's "even better" form.

**`own-lifetime-elision` — exactly right, including where elision must be overridden.**
- `LinkStrip::ip_bytes<'a>(&self, frame: &'a [u8]) -> Option<&'a [u8]>`
  (`src/capture/pcap.rs:354`) **needs** its explicit lifetime: elision rule 3
  would tie the output to `&self`, which is the wrong borrow. Do not "simplify"
  it.
- `impl<S: Surface> Drop for SurfaceJobGuard<'_, S>` (`src/actuator/mod.rs:234`)
  uses the anonymous `'_` the rule prescribes; the named `'a` on the inherent
  impl is used by `new`'s parameter.
- `object_or_none<'de, …>` / `lenient_elements<'de, …>` (`src/domain/shop.rs:43`,
  `:54`) carry the conventional `'de` a `Deserializer` bound requires.

**`own-mutex-interior` vs `own-rwlock-readers` — judged, and `Mutex` is correct at all four points.** Do not "upgrade" any of these:
- `Arc<Mutex<Controller>>` (`src/app/mod.rs:231`). Reads *do* outnumber writes
  (per-frame `view_state` vs ~1 Hz events), but there is exactly **one** reader
  thread (egui) and one writer (the session loop), and both hold the lock for a
  few microseconds. `RwLock` buys concurrency among readers; with a single reader
  there is none to buy, and the rule's own "When RwLock Hurts" table says
  `Mutex` for a briefly-held lock.
- `Mutex<Usage>` inside `PipelineBudget` (`src/stream.rs:65`). Nearly every
  operation is a write (`reserve_new`, `try_retag`, `release`); only `snapshot`
  reads. `Mutex` is the rule's prescribed choice at that ratio.
- `Arc<Mutex<VecDeque<LogLine>>>` in `EventLog` (`src/journal.rs:29`). Several
  writers (session loop, actuator executor, watchdog), one reader, and the reader
  is already gated by an `AtomicU64` generation so it does not even take the lock
  most frames — a better optimisation than `RwLock` would have been.
- `Arc<Mutex<Timings>>` in `ActuatorHandle` (`src/actuator/mod.rs:62`). In
  practice a single task owns the only `ActuatorHandle`, so this looks
  over-synchronised — but `RefCell` is not reachable: the handle is moved into a
  tokio task and must stay `Send`, and `set_timings`/`timings` are `&self`. Keep
  it.
- Poison handling is consistent and reasoned everywhere it deviates
  (`unwrap_or_else(PoisonError::into_inner)` in `journal.rs`, `stream.rs`,
  `ui/mod.rs`, `actuator/shield.rs`, each with a comment saying why a second
  panic would be worse).

**`own-arc-shared` — the sharing model is deliberate and well-typed.**
- Every cross-thread handle is a newtype over `Arc` with a documented cheap
  `Clone`: `WatchGate`, `EventLog`, `SnapshotEpoch`, `ShutdownSignal`,
  `PressureResync`, `PipelineBudget`, `SessionHandles`. `own-006` is only about
  the six sites that are *not* behind such a newtype.
- No `Arc` is cloned inside a per-packet or per-frame loop. The two per-segment
  `PipelineBudget` clones (`src/stream.rs:559`, `:345`) are each forced by the
  borrow checker — the segment is moved on the next line — and cost one atomic
  increment each.
- `Rc` appears nowhere, correctly: every shared value in this crate crosses a
  thread or task boundary, so `own-rc-single-thread` does not apply.

**`own-clone-explicit` — `BudgetedChunk`/`BudgetedSegment` are the highlight of the audit.**
- `src/stream.rs:318-401`: neither type derives `Clone` or `Copy`, and that is
  load-bearing — `PayloadLease` releases bytes in `Drop`, so a duplicate would
  double-release and corrupt the pipeline accounting. Ownership is used to make
  the bug unrepresentable, `into_parts()` (`:358`) is the one sanctioned way to
  break the pair apart, and `record_drop(self)` (`:338`) consumes so the lease
  cannot outlive the decision. The `#[cfg(test)]`-only `Debug`/`PartialEq` impls
  keep the production surface minimal.
- `Filter` (112 bytes, four `Vec`s) correctly derives `Clone` and not `Copy`; its
  clones are all at genuine ownership boundaries (config → controller, controller
  → editor draft, draft → `Command::SetFilter`).

**`own-move-large` — measured and within budget.**
- `BudgetedSegment` is 120 bytes and `Segment` 96, both under the rule's
  128-byte "don't box" line, and both are pinned by `const _: () = assert!(…)`
  size canaries (`src/capture/mod.rs:77-83`, `src/stream.rs:418-424`) whose
  comments state the queue cost explicitly. Nothing needs boxing.
- Passing `tx`/`gate`/`fatal`/`budget` **by value** into
  `capture_loop_budgeted` (`src/app/mod.rs:866-874`) trips
  `clippy::needless_pass_by_value`, and the lint is wrong here: dropping `tx`
  when the loop returns is what closes the pipeline in producer order, which
  `worker_shutdown_clean_pipeline_closes_in_producer_order` asserts. Same for
  `handle: Handle` in `capture_loop` (`src/capture/pcap.rs:911`), where the
  by-value move is what closes the `pcap_t` on the owning thread (`:999`). Do not
  "fix" either.

**`own-copy-small` — applied well where it was applied.**
- `Target` (`src/actuator/win.rs:593-598`, 24 bytes) is `Copy` and its methods
  take `self` by value (`to_client`, `engage`, `verify`), which is exactly the
  ergonomics the rule is after. `FlowKey` (64 bytes, at the boundary),
  `ClientRect`, `Progress`, `RefreshMeta`, `PurchaseLimit`, `BuyTarget`,
  `DelayRange`, `Zone`, `DesignPoint`, `TimedStep`, `Input`, `Stage`,
  `HaltSource`, `Mode`, `Status`, `StopReason` are all `Copy` and all small.
  `own-003` and `own-009` are the only two deviations.

## Not applicable

- [`own-rc-single-thread`](../../.claude/skills/rust-skills/rules/own-rc-single-thread.md) — no single-threaded shared ownership exists: every shared value crosses a thread (capture thread, egui main thread) or task boundary, so `Arc` is mandatory throughout. `Rc`/`Weak` appear nowhere, and no reference cycle is possible in this data model.
- [`own-refcell-interior`](../../.claude/skills/rust-skills/rules/own-refcell-interior.md) — no `RefCell` or `Cell` in production code (only three in `#[cfg(test)]` harnesses, capturing clicked commands out of an egui closure, which is the rule's own intended use). Every interior-mutability point in the crate is cross-thread, so `Mutex`/atomics are required instead.
- [`own-cow-conditional`](../../.claude/skills/rust-skills/rules/own-cow-conditional.md) — no `Cow` in the crate, and no site where introducing one pays for itself. The nearest candidates are all cold: `is_loopback_ws_host` (`src/config.rs:261-298`) builds two throwaway `String`s while normalising a host, and `render::status_label`/`describe`/`refusal` already return `&'static str` from their static arms rather than allocating. `own-005` is the one place a borrowed-vs-owned decision was worth making, and there a plain `&str` is simpler than `Cow`.
