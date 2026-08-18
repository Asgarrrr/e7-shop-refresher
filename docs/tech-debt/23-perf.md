# 23 — Performance Patterns (`perf-`)

**Category priority:** MEDIUM
**Rules audited:** 13 · **Files read:** 41 (every `.rs` in `src/`, `build.rs`, `examples/ui_preview.rs`) + `Cargo.toml` · **Findings:** 4 (P0 0 / P1 0 / P2 3 / P3 1)

## Verdict

This crate is already written by someone who thinks about iterators and allocation, and it
shows: not one `for i in 0..len`, not one intermediate `collect` in a decision path, correct
`entry` use in the two maps that matter, `Vec::with_capacity` where the size is known, and
size canaries on the per-packet types. `cargo clippy --all-targets` is silent (verified: exit
0, zero diagnostics), and everything below is something `clippy::perf` structurally cannot
see.

The honest headline is not a defect list, it is a measurement gap. The three "hot paths" this
category is pointed at are hot in *frequency*, not in *volume*: the kernel BPF filter admits
one TCP port's server-to-client traffic from one game client, so the per-packet path runs on
a handful of large TLS records every couple of seconds — and with `SNAPLEN = 262_144` plus
RSC/LRO coalescing, `src/capture/pcap.rs` measured a single 48 870-byte "packet". Low packet
rate, large packets. That inverts the usual conclusion: per-packet **fixed** costs (a SipHash
of a `FlowKey`, a `BTreeMap` probe, a small `Vec`) are irrelevant here, and the only per-packet
cost with real weight is the two full **per-byte** copies (`ip.to_vec()` in
`src/capture/pcap.rs:963` then `tcp.payload().to_vec()` in `src/capture/ip.rs:66`) — which is
`mem-zero-copy`'s finding, not mine. I am deferring it there rather than dressing it up as an
iterator problem.

Worst offender file: `src/ui/view.rs` — not the packet path. `view_state` rebuilds every shop
row from scratch **while holding the controller mutex**, including a `format_item` call per row
(up to seven `format!`s each) whose only consumer is a hover tooltip at most one row ever
reads. That is the single highest-value fix, and its value is lock-contention with the session
loop and actuator, not CPU. The second is `HalfStream::buffer_future`, which probes the same
`BTreeMap` key three times where `entry` probes once.

## Findings

### perf-001 — Nothing in this crate can be measured, so every fix below is unranked

- **Severity:** P2
- **Rule:** [`perf-profile-first`](../../.claude/skills/rust-skills/rules/perf-profile-first.md)
- **Site:** absence of `benches/`, absence of any `[[bench]]` or `[profile.profiling]` in `Cargo.toml`; the paths in question are `src/capture/pcap.rs:910` (`capture_loop`), `src/stream.rs:556` (`push_budgeted`), `src/ui/view.rs:53` (`view_state`)
- **What:** There is no benchmark, no flamegraph, and no profiling profile anywhere in the
  repository. The crate documents its hot paths in prose (`src/stream.rs` module docs, the
  size canaries at `src/capture/mod.rs:77` and `src/stream.rs:418`) but has never measured
  one. Consequently I cannot tell you whether perf-002, perf-003 or perf-004 are worth a
  single minute, and I am not going to pretend otherwise.
- **Why it matters here:** This is the rule that governs the other three. The size canaries
  are evidence that someone already reasoned "one extra field is paid per packet" — a
  reasonable inference that a measurement would either confirm or retire. Meanwhile the
  reasoning above (low packet rate, 48 KB packets) suggests the per-packet path may be
  entirely uninteresting and the per-*frame* path is where the time goes. Acting on the
  packet path first would be exactly the mistake this rule names.
  One thing is already in place and should not be undone: `Cargo.toml:117` sets
  `strip = "debuginfo"` rather than `strip = true` specifically to keep the symbol table for
  `crash.rs`. That means `cargo flamegraph` on a release build already yields *named* frames
  — a first measurement costs nothing but the run.
- **Fix:** Take one measurement before touching anything else. Concretely: run the shipped
  release binary against a live Secret Shop session under `cargo flamegraph`, and read the
  width of `Reassembler::push_budgeted`, `parse_segment`, and `ShopApp::ui`. If the reassembly
  path is a hairline and `ShopApp::ui` is not, close perf-003 and perf-004 as "measured, not
  worth it" and do perf-002 only.
  A criterion bench would settle the one question a flamegraph cannot: how the reassembler
  behaves on the *reordered* burst that perf-003 is about, which a healthy LAN never produces.
  `InitialBurst` is capped at 128 segments / 256 KiB (`src/stream.rs:435`), so the input is
  trivially synthesizable. The harness itself belongs to the `test-` reviewer
  (`test-criterion-bench`); I am only naming the question it would answer.
- **Effort:** small (flamegraph) / medium (bench harness, and not mine to file)

### perf-002 — Every row's tooltip text is formatted on every frame, under the controller lock, and at most one row reads it

- **Severity:** P2
- **Rule:** [`perf-iter-lazy`](../../.claude/skills/rust-skills/rules/perf-iter-lazy.md)
- **Site:** `src/ui/view.rs:71` (`detail: format_item(item, index)`), consumed at `src/ui/shop.rs:79` (`response.on_hover_text(&row.detail)`); the lock is taken at `src/ui/mod.rs:138-141`
- **What:** `view_state` materialises a `SlotRow` per shop slot, and each one eagerly builds
  `detail` via `render::format_item` — which does up to seven `format!` allocations plus a
  `Vec<String>` + `join` for substats (`src/render.rs:130-161`). `shop_table` then hands that
  string to `on_hover_text` for the row under the pointer, so on any given frame at most one
  of the six strings is ever looked at; the rest are built, moved, and dropped unread.
  The whole projection — including all of that formatting — happens inside
  `lock_ignoring_poison(&self.handles.controller)`.
- **Why it matters here:** The cost is not the allocations, it is the lock. That same
  `Arc<Mutex<Controller>>` is taken by `session_loop`'s `dispatch`, `on_message`,
  `handle_purchase` and `heartbeat` (`src/app/session/mod.rs:200, 237, 326, 357, 406`), on the
  path that turns a captured shop into a click job. Frame frequency is *not* the 4 Hz the
  `request_repaint_after(250ms)` at `src/ui/mod.rs:137` suggests: egui also repaints on input,
  and hover state changes are input — so while the player's pointer is moving over the shop
  table (exactly when the tooltip matters) this runs at display rate, lengthening every lock
  hold the session loop is competing for. Whether that is measurable is perf-001's question,
  but it is the only per-frame item in this crate where the cost lands on something other
  than the frame itself.
  Note also that egui already offers the lazy form and this codebase *knows* it: both
  `src/ui/journal.rs:55` and `src/ui/theme.rs:219` deliberately build their `format!` strings
  *inside* the `widget_info` closure with a comment explaining that egui only calls it when
  something is reading. The shop tooltip is the one place that pattern was not applied.
- **Fix:** Make the detail lazy, the same way the accessible names already are. Drop `detail`
  from `SlotRow`, have `ViewState` carry the projected item fields (or one clone of the
  ~6-slot `ShopSnapshot`, which is cheaper than 42 `format!`s), and in `shop_table` use
  `response.on_hover_ui(|ui| { ui.label(format_item(item, index)); })` — egui invokes that
  closure only for the hovered widget. Alternatively, cache `rows` in `ShopApp` behind a
  snapshot generation counter, mirroring the existing `journal_cache` / `journal_generation`
  pair at `src/ui/mod.rs:93-94, 142-146`; that pattern is already proven in this file and
  would take the whole projection out of the per-frame path, not just the tooltip.
  The allocation-avoidance half of this overlaps `mem-avoid-format` / `mem-write-over-format`
  and the `anti-format-hot-path` reviewer owns the `format!` calls themselves; my finding is
  the access pattern — a value computed eagerly for N rows and read for at most one.
- **Effort:** small (`on_hover_ui`) / medium (generation-cached rows)

### perf-003 — `buffer_future` probes the same `BTreeMap` key three times where `entry` probes once

- **Severity:** P2
- **Rule:** [`perf-entry-api`](../../.claude/skills/rust-skills/rules/perf-entry-api.md)
- **Site:** `src/stream.rs:748-774`
- **What:** The out-of-order buffering path does `self.pending.get(&offset)`, then
  `self.pending.remove(&offset)`, then `self.pending.insert(offset, payload)` — three
  independent `O(log n)` traversals of the same map for the same key:

  ```rust
  if self.pending.get(&offset).is_some_and(|old| old.as_slice().len() >= payload.as_slice().len()) {
      return true;
  }
  if let Some(old) = self.pending.remove(&offset) { self.pending_bytes -= old.capacity(); drop(old); }
  // ... quota checks ...
  self.pending.insert(offset, payload);
  ```

  This is the exact "get + insert" shape the rule's Bad section names, on a `BTreeMap` rather
  than a `HashMap` (the entry API is the same).
- **Why it matters here:** `pending` is bounded by `MAX_PENDING_BYTES = 8 MiB`
  (`src/stream.rs:31`), so at ~1500-byte segments it can hold thousands of entries and the
  traversals are not free. The frequency is per *reordered* segment, not per segment — rarer
  than the packet rate, which is why this is P2 and not P1. The second reason to do it is
  clarity: the current order removes the old chunk and decrements `pending_bytes` *before*
  the quota check that may reject the new one, leaving a hole in the map. That is harmless
  today only because returning `false` propagates to `HalfOutcome::Pressure`, which makes
  `Reassembler::push_budgeted` call `self.clear()` and wipe every stream anyway
  (`src/stream.rs:579-582`). The `entry` form removes the hazard rather than relying on that
  chain holding.
- **Fix:** One `entry` match, deciding after the quota check:

  ```rust
  use std::collections::btree_map::Entry;
  let capacity = payload.capacity();
  match self.pending.entry(offset) {
      Entry::Occupied(mut slot) => {
          if slot.get().as_slice().len() >= payload.as_slice().len() {
              return true; // keep the larger segment already held
          }
          let freed = slot.get().capacity();
          if (self.pending_bytes - freed).checked_add(capacity)
              .is_none_or(|bytes| bytes > MAX_PENDING_BYTES)
              || !payload.try_retag_pending()
          {
              return false; // the old chunk is still in the map
          }
          self.pending_bytes = self.pending_bytes - freed + capacity;
          slot.insert(payload); // returns and drops the old chunk
      }
      Entry::Vacant(slot) => {
          if self.pending_bytes.checked_add(capacity)
              .is_none_or(|bytes| bytes > MAX_PENDING_BYTES)
              || !payload.try_retag_pending()
          {
              return false;
          }
          self.pending_bytes += capacity;
          slot.insert(payload);
      }
  }
  true
  ```

  The borrow checker will not let `self.pending_bytes` be touched while a `slot` borrow is
  live, so hoist the arithmetic into locals first. The existing tests
  (`gap_fill_moves_chunks_without_exceeding_budget`,
  `pending_bytes_are_global_across_sixty_four_streams`,
  `reordering_flushes_multiple_buffered_segments`) cover both arms and the quota rejection, so
  this is verifiable without new tests.
  Optional second step on the same map, and honestly marginal: `HalfStream::drain`
  (`src/stream.rs:778-782`) does `first_key_value()` then `pop_first()`, two leftmost walks
  per drained chunk. `pop_first()` unconditionally and re-inserting on the `offset > next_off`
  break is one walk per chunk plus one re-insert per drain call. Only worth folding in while
  the file is already open.
- **Effort:** small

### perf-004 — `InitialBurst::into_ordered` collects two whole `HashMap`s where one would do

- **Severity:** P3
- **Rule:** [`perf-collect-once`](../../.claude/skills/rust-skills/rules/perf-collect-once.md)
- **Site:** `src/stream.rs:496-522`
- **What:** The burst-ordering pass builds `flows: HashMap<FlowKey, Vec<BudgetedSegment>>`,
  then immediately re-collects it into a *second*, differently-typed
  `HashMap<FlowKey, VecDeque<BudgetedSegment>>` purely so the final replay loop can call
  `VecDeque::pop_front`. That is a second table allocation and a re-hash of every key for a
  container change. (The `Vec` → `VecDeque` conversion itself is `O(1)` and reuses the buffer;
  the map is the waste.)
- **Why it matters here:** Barely, and I want that on the record. This runs **once per
  resync** — after a player pause, a `pcap_stats` drop, or a byte-pressure event — over at
  most `INITIAL_ANCHOR_MAX_SEGMENTS = 128` segments across (nominally) one flow, and the map
  is even created with `with_capacity(1)` because the author already knew that. It is a P3
  because it is a *legibility* improvement on a function that is otherwise the subtlest code
  in the file, not because it costs anything a profiler would see. Do it if you are editing
  `into_ordered` for another reason; do not schedule it.
- **Fix:** Keep the one `HashMap<FlowKey, Vec<_>>`, sort each value in place through
  `values_mut()`, `reverse()` it, and let the replay loop use `Vec::pop` instead of
  `VecDeque::pop_front`. That drops the second map, the second hash of every key, and the
  `VecDeque` from this path entirely:

  ```rust
  for segments in flows.values_mut() {
      let origin = segments.iter().map(segment_data_seq)
          .reduce(|earliest, candidate| if seq_diff(candidate, earliest) < 0 { candidate } else { earliest })
          .expect("a burst flow is never empty");
      segments.sort_by_key(|segment| seq_diff(segment_data_seq(segment), origin));
      segments.reverse(); // so `pop` yields sequence order
  }
  slots.into_iter()
      .map(|key| flows.get_mut(&key).and_then(Vec::pop).expect("every burst slot has one segment"))
      .collect()
  ```

  `initial_anchor_burst_orders_all_six_permutations`,
  `initial_anchor_burst_order_is_wrap_safe_and_overlap_stays_centralized` and
  `initial_anchor_burst_preserves_inter_flow_slots` pin the observable behaviour.
- **Effort:** trivial

## Clean areas

**Iterators and indexing (`perf-iter-over-index`, `perf-iter-lazy`, `perf-collect-once`)**

- Not a single `for i in 0..len` in the crate. Traversals are `iter()`, `iter().enumerate()`,
  `iter_mut()`, or slice patterns throughout.
- `capture::ip::parse_segment` and `pcap::ethernet_payload_offset` use a manual cursor with
  `frame.get(at..at + 2)?` — that is the variable-stride, non-sequential case
  `perf-iter-over-index` explicitly permits, and it is bounds-checked via `get` rather than
  indexing.
- `Filter::matches` (`src/domain/filter.rs:55`) short-circuits on every criterion and ends in
  `.all(...)`; nothing is collected. `Haul::others` (`src/domain/control/mod.rs:209`) is a
  lazy `filter().map().fold()`. `Controller::plan_targets` walks the slots once and builds
  exactly one `Vec`. `recovery_buy_targets` is a single `filter_map().collect()`.
- `ShopSnapshot::slot_by_id` and `session::purchase_line` use `find`, not
  `collect().first()`.
- `InitialBurst::into_ordered:496` collects `slots` from a slice iterator with a comment
  explaining that it is already `TrustedLen` and therefore a single exact-size allocation.
  That is the right reasoning; perf-004 is only about the *second* map beside it.
- `render::grouped` (`src/render.rs:43`) pre-sizes its `String` with the exact final capacity
  including separators. Textbook.

**Entry API (`perf-entry-api`)**

- `Haul::record` (`src/domain/control/mod.rs:220`) uses `entry(name).or_insert(0)` — correct.
- `InitialBurst::into_ordered:499` uses `entry(flow).or_default().push(segment)` — correct.
- `Reassembler::push_budgeted:567` looks like a `contains_key` + `entry` double lookup and is
  not: `self.streams.len() >= MAX_STREAMS` short-circuits first, and in steady state (one
  armed game connection) `len()` is 1, so the `contains_key` never runs. It also cannot be
  folded into the `entry` below, because eviction must happen *before* `entry` inserts the new
  key. **Do not "fix" this** — collapsing it would either evict on every packet or evict the
  key just inserted.

**IO buffering (`perf-io-buffering`) — the brief predicted a finding here; there isn't one**

- `src/journal.rs` has no file writer at all. It is an in-memory `VecDeque` ring capped at 500
  entries plus a mirror to `tracing`; the disk side is `tracing_appender`.
- The log writer *is* buffered, in the way that matters: `main.rs:77` wraps the rolling
  appender in `tracing_appender::non_blocking`, which moves every write onto a dedicated
  worker thread behind a channel. The per-line `File::write` the rolling appender performs
  never touches a request path. Adding a `BufWriter` under it would only delay lines the
  crash-diagnosis story depends on.
- `config::persist::replace_file` (`src/config/persist.rs:93`) is a single
  `fs::write(&tmp, contents)` of a fully-built `String` followed by `fs::rename` — one write
  syscall, and atomic. `Config::load` and `strip_retired_keys` each do one `read_to_string`.
  `persist::tidy` builds its output with `String::with_capacity(text.len())` and
  `split_inclusive('\n')` + `push_str`, i.e. zero intermediate allocations. Nothing to buffer.
- `crash.rs::append` writes one complete entry per panic with a single `write_all`. Buffering
  a panic hook would be a defect, not a fix.
- The only per-line syscalls in the crate are the `println!` calls in `render::render_shop`
  and `EventLog::emit`, and both are behind `#[cfg(not(feature = "gui"))]` — the console-only
  lane, which is not the shipped configuration. ~7 line-buffered writes per refresh cycle
  there. Not worth a finding.

**Chain (`perf-chain-avoid`)**

- Four `chain` sites, none in a hot loop, and the `wide()` helpers deserve a note because they
  *look* like they might be: `capture/pcap.rs:891`, `actuator/win.rs:46` and
  `migrate.rs:170,243` all build NUL-terminated UTF-16 with
  `encode_utf16().chain(once(0)).collect()`, and every call site is startup, one registry
  read, or one `find_game_window` per actuator job — a job that then sleeps 100+ ms of Win32
  settle time. `ui/mod.rs:219` chains two command vectors of at most a handful of elements on
  a click. All four are the rule's own "When Chain Is Fine".

**Buffer reuse (`perf-drain-reuse`)**

- `VecDeque` and `Vec` are cleared rather than reallocated where it matters:
  `EventLog::push` pops from the front of a capacity-500 ring that stops growing after warm-up
  (so `extend` would gain literally nothing over the `push_back` loop — see Not filed);
  `Controller` reuses `checklist`/`bought` via `clear()` and `swap_remove` rather than
  rebuilding; `PcapSource::drop` uses `self.threads.drain(..)`.

**Hasher choice (`perf-ahash`) — audited and deliberately not applied**

- The only `HashMap` on the per-packet path is `Reassembler::streams: HashMap<FlowKey, _>`,
  hashed once per segment. Its keys are **network-derived** — the source and destination
  `SocketAddr`s of captured packets — and `src/stream.rs:426-431` documents the exact threat
  ("a flood of forged source ports would otherwise mint keys without bound"), which is why the
  map is capped at 64 with LRU eviction. `perf-ahash` is explicit that a non-DoS-resistant
  hasher must never key on untrusted external input, so `FxHashMap` is ruled out on security
  grounds, not taste. `ahash` would be defensible (randomized per-process seed) but buys an
  unmeasurable amount of one hash per packet at a packet rate bounded by one game client's
  shop traffic, against a new dependency in a crate whose dep list is deliberately tight and
  individually justified in `Cargo.toml`. **Recommendation: keep SipHash.** Revisit only if
  perf-001's flamegraph puts `SipHasher` on screen, which it will not.
- `Haul::named` is a `BTreeMap<String, u32>` by design (ordered output for the haul readout),
  not a hasher decision.

## Not applicable

- `perf-collect-into` — nightly-only (`#![feature(iter_collect_into)]`); this crate is stable
  (`rust-version = "1.92"`). Its stable equivalent is `extend`, which is covered under
  `perf-extend-batch` and `perf-drain-reuse` above.
- `perf-black-box-bench` — no benchmarks exist in the crate (no `benches/`, no `[[bench]]`),
  so there is no `black_box` to audit. If perf-001's bench is ever written, this rule applies
  to it and the `test-` reviewer owns that file.
- `perf-release-profile` — **deferred to the `opt-` reviewer**, who owns the `Cargo.toml`
  profiles per the audit scope. For their benefit: `[profile.release]` sets `lto = "thin"`,
  `codegen-units = 1`, and `strip = "debuginfo"` with a documented reason for not using
  `strip = true` (`crash.rs` needs the symbol table for `Backtrace::force_capture`);
  `[profile.dev.package."*"]` sets `opt-level = 2` with a documented reason (egui at
  opt-level 0 is visibly sluggish). There is no `[profile.bench]` and no
  `[profile.profiling]`; the latter is perf-001's concern and I have named it there rather
  than duplicating a profile finding.

## Explicitly considered and not filed

Recorded so a later pass does not re-derive them and file them as wins.

- **`perf-extend-batch` on `EventLog::push` (`src/journal.rs:81-89`).** A textbook
  `push_back`-in-a-loop over a known-length slice, and `extend` is a one-line change. Not
  filed: the `VecDeque` is capped at `JOURNAL_CAP = 500` and only ever pops from the front, so
  after the first 500 lines its capacity is stable and `push_back` never reallocates. `extend`
  would reserve nothing that is not already reserved.
- **`HalfStream::push` allocating an output `Vec` per packet (`src/stream.rs:696`).** Looks
  like a `perf-drain-reuse` candidate (give `Reassembler` a scratch buffer, `clear()` it per
  packet, hand out a `drain(..)`). Not filed for two reasons. First, `Vec::new()` does not
  allocate: on the common duplicate / buffered-future outcome the chunk list stays empty and
  costs nothing, so the allocation only happens on a packet that is *already* about to be
  moved through a retag and a WebSocket send. Second, the buffer cannot be lent across the
  await: `forward_chunks` takes `&mut Reassembler` so it can call `clear()` on pressure
  (`src/app/mod.rs:827-859`), which conflicts with holding a `&mut Vec` borrowed out of the
  same reassembler. The fix would be a borrow-structure change for one small malloc. The
  allocation side of this belongs to `mem-` in any case.
- **`serde_json::Value` round-trip in `object_or_none` / `lenient_elements`
  (`src/domain/shop.rs:43-66`).** Each tolerated field is materialised as a `Value` (which
  allocates a `Map`/`String` per member) before being re-deserialized into the target type,
  which is an intermediate materialisation in the `perf-collect-once` spirit. Not filed: the
  frequency is once per shop snapshot (~0.5/s), over ~6 items, and the leniency it buys is
  documented, deliberate and load-bearing — a partial `refresh` object must not fail the whole
  message. The allocation-free alternative is a hand-written visitor per field, which trades
  real legibility for an unmeasurable gain on a cold path.
- **`hunt_summary`'s `name.clone()` then overwrite (`src/ui/editor/mod.rs:150-158`).** The
  clone is wasted whenever the name matches a headliner; a
  `HAUL_HEADLINERS.iter().find(...).map_or_else(|| name.clone(), ...)` avoids it. Not filed
  under `perf-`: this is `own-borrow-over-clone` / `anti-clone-excessive` territory, and it
  runs only while the Hunt section is *folded* (the code comments say so explicitly), over a
  handful of filter names.
- **The whole actuator path (`src/actuator/win.rs`, `src/actuator/shield.rs`,
  `src/actuator/mod.rs`).** Deliberately not audited for micro-optimisation. Every `Surface`
  call sleeps 30-170 ms of Win32 settle time (`MOVE_SETTLE_MS`, `FOCUS_SETTLE_MS`,
  `SHIELD_DRAIN_MS`, `press_ms`), and `run_executor` awaits `step.wait_ms` — 150-1180 ms per
  step — before each one. The path is latency-dominated by deliberate waits by three to four
  orders of magnitude. Nothing an iterator change does there is observable.
- **`src/watch.rs`.** `WatchGate` is the safety cutoff, polled in a `biased` select branch per
  session-loop iteration. Its `SeqCst` orderings and its double `store(false)` around the halt
  latch are correctness devices, not performance defects. Correctness outranks speed here and
  the atomic orderings are the `conc-` reviewer's call in any case.
