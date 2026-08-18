# 13 — Const & Compile-Time (`const-`)

**Category priority:** MEDIUM
**Rules audited:** 4 · **Files read:** 41 `.rs` (+ `Cargo.toml`) · **Findings:** 5 (P0 0 / P1 0 / P2 3 / P3 2)

## Verdict

This category is in good shape and, unusually, the crate already *invented* the
best practice this category asks for: three `const _: () = assert!(…)` canaries
(`src/capture/mod.rs:78`, `src/stream.rs:419`, and eight of them in
`src/ui/editor/timing_meter.rs:45`) pin type sizes and the timing-ruler
invariant at compile time. Every `const`/`static` in the crate is on the right
side of `const-vs-static` — including the two `static`s
(`shield::WINDOW: Mutex<…>`, `win::DPI: Once`), which genuinely need a single
address. There is no `static mut` anywhere.

The gaps are all of the same shape: **invariants the crate already knows how to
assert at compile time are asserted somewhere weaker instead.** The worst
offender is `src/capture/pcap.rs`, whose own comment calls the `PcapPktHdr`
layout "the single most dangerous constant in this file" and then guards it with
a `#[test]` rather than a `const` assert — in a module that only compiles on
Windows with `pcap-backend`, i.e. exactly the lane most likely to be built
without being tested. The single highest-value fix is const-001: two lines that
turn that silent-corruption risk into a build failure. Second is const-002:
`PipelineBudget::with_limits` re-checks a relation between four `pub(crate)`
constants at run time on every session start.

`src/actuator/plan.rs` is the one file with a real magic-number problem
(const-003): the "six clickable rows / top group is rows 0..=3" rule is
re-encoded as bare `<= 5` and `> 3` literals at three sites, in the file that
decides which slot gets clicked.

## Findings

### const-001 — the `pcap_pkthdr` layout canary is a test, not a compile-time assert

- **Severity:** P2
- **Rule:** [`const-block`](../../.claude/skills/rust-skills/rules/const-block.md)
- **Site:** `src/capture/pcap.rs:1180-1187` (the test) guarding
  `src/capture/pcap.rs:141-170` (`PcapPktHdr`, `PcapStat`)
- **What:** the FFI layout invariant is checked only inside
  `#[cfg(test)] mod tests`:

  ```rust
  // The single most dangerous constant in this file: a 24-byte header
  // would put `caplen` where `tv_usec` is and report nonsense lengths
  // instead of crashing.
  assert_eq!(size_of::<PcapPktHdr>(), 16);
  assert_eq!(size_of::<PcapStat>(), 24);
  ```

  The crate asserts the *same class* of invariant at compile time twice
  elsewhere — `src/capture/mod.rs:77-83` and `src/stream.rs:418-424` both use
  `#[cfg(target_pointer_width = "64")] const _: () = { assert!(size_of::<…>() == N); }`.
- **Why it matters here:** `mod pcap` is gated
  `#[cfg(all(windows, feature = "pcap-backend"))]`, so this test only exists in
  one build lane. `cargo build --release` on that lane compiles the FFI structs
  and ships them without ever evaluating the assertion. The failure mode is the
  one the module documents at length: `caplen` reads the low half of `tv_usec`,
  `plausible_caplen` catches only ~3 in 4 packets, and the session dies with a
  message about a layout nobody changed on purpose. A `const` assert fires on
  the build that would ship the defect.
- **Fix:** add beside the struct definitions (the runtime canary
  `plausible_caplen` and the test both stay — they cover different edits):

  ```rust
  // A `timeval` of two 32-bit longs, not a 64-bit `time_t`: 16 bytes, or
  // `caplen` lands on the low half of `tv_usec`. See `plausible_caplen`.
  const _: () = {
      assert!(size_of::<PcapPktHdr>() == 16);
      assert!(size_of::<PcapStat>() == 24);
  };
  ```
- **Effort:** trivial

### const-002 — the pipeline byte-budget relation is a runtime `assert!` over four constants

- **Severity:** P2
- **Rule:** [`const-block`](../../.claude/skills/rust-skills/rules/const-block.md)
- **Site:** `src/stream.rs:97-100`, over the constants at `src/stream.rs:31-36`
- **What:** `PipelineBudget::with_limits` asserts at run time what, for the
  production path, is a pure relation between four `const`s:

  ```rust
  assert!(limits.capture <= limits.global);
  assert!(limits.reassembly <= limits.global);
  assert!(limits.outbound <= limits.global);
  ```

  `PipelineBudget::new()` always feeds it `CAPTURE_STAGE_BYTES`,
  `REASSEMBLY_STAGE_BYTES`, `OUTBOUND_STAGE_BYTES`, `PIPELINE_GLOBAL_BYTES`.
  Two further relations between these constants are never asserted at all:
  `MAX_PENDING_BYTES` (8 MiB, per-stream, `src/stream.rs:31`) against
  `REASSEMBLY_STAGE_BYTES` (16 MiB, global), and `INITIAL_ANCHOR_MAX_BYTES`
  (256 KiB, `src/stream.rs:435`) against `CAPTURE_STAGE_BYTES` — a burst is held
  in the capture stage while it buffers, so a burst cap above that quota could
  never fill.
- **Why it matters here:** these four numbers are the crate's only defence
  against unbounded memory on a capture path that runs for hours, and they are
  the kind of value a later tuning pass edits by hand. Getting them wrong today
  is caught on the first `PipelineBudget::new()` of a session — i.e. a panic in
  `Session::run` on the player's machine, which in the windowed build surfaces
  as a crash banner. The relation is knowable at compile time.
- **Fix:** keep the runtime asserts (`with_test_limits` passes arbitrary values
  and must stay checked) and add a module-level block next to the constants:

  ```rust
  const _: () = {
      assert!(CAPTURE_STAGE_BYTES <= PIPELINE_GLOBAL_BYTES);
      assert!(REASSEMBLY_STAGE_BYTES <= PIPELINE_GLOBAL_BYTES);
      assert!(OUTBOUND_STAGE_BYTES <= PIPELINE_GLOBAL_BYTES);
      // The per-stream pending cap must fit the global reassembly quota, or
      // it is dead code: the stage limit trips first, every time.
      assert!(MAX_PENDING_BYTES <= REASSEMBLY_STAGE_BYTES);
      // A burst is held in the capture stage while it buffers.
      assert!(INITIAL_ANCHOR_MAX_BYTES <= CAPTURE_STAGE_BYTES);
  };
  ```
- **Effort:** trivial

### const-003 — the six-clickable-rows rule is three bare literals in the click planner

- **Severity:** P2
- **Rule:** [`const-block`](../../.claude/skills/rust-skills/rules/const-block.md)
  (named value + compile-time assert; the same shape as `const-vs-static`'s
  "small configuration values, bitmasks, and magic numbers" guidance)
- **Site:** `src/actuator/plan.rs:91`, `:522`, `:540`; geometry at `:97-109`
- **What:** the shop's row invariants appear only as literals, in the one file
  that decides where a click lands:

  ```rust
  slot.checked_sub(1).filter(|&row| row <= 5)          // :91  row_for_slot
  rows.iter().copied().filter(|&row| row <= 5)         // :522 buy_job
  if row > 3 && !at_bottom                             // :540 buy_job
  ```

  and `buy_zone` builds its rectangle from five unnamed floats
  (`166.5 + 145.0 * row`, `cy -= 217.0`, `cx: 1154.0`, `w: 190.0`, `h: 61.0`)
  while its three sibling zones (`REFRESH`, `CONFIRM_REFRESH`, `CONFIRM_BUY`,
  `SCROLL_ZONE`) are all named `const Zone`s. Related unnamed values in the same
  file: the click band `0.75` / press hold `40 + … % 51` (`:455-464`, whose
  bounds the tests restate as `0.375` and `40..=90`) and the aspect epsilon
  `1e-3` (`:139`).
- **Why it matters here:** `<= 5` and `> 3` are two halves of one fact — "six
  rows, the first four reachable at scroll-top". Nothing ties them together, and
  the third site (`> 3`) has no test that would fail if only one of the two were
  edited: a change to the shop's row count silently produces a plan that scrolls
  to the bottom for a row that is still at the top, i.e. a click on the wrong
  item with real gold behind it. This is the only file in the crate where an
  unnamed number can cost the player money.
- **Fix:** name the two, assert their relation once, and use them at all three
  sites:

  ```rust
  /// Highest 0-based clickable row (six display slots).
  const MAX_ROW: u8 = 5;
  /// Highest row reachable at scroll-top; above it the list must be scrolled.
  const LAST_TOP_ROW: u8 = 3;
  const _: () = assert!(LAST_TOP_ROW < MAX_ROW);

  const BUY_ROW_PITCH: f32 = 145.0;   // design px between two Buy buttons
  const BUY_ROW_TOP_CY: f32 = 166.5;  // row 0 centre at scroll-top
  const SCROLL_BOTTOM_SHIFT: f32 = 217.0;
  ```

  Then `filter(|&row| row <= MAX_ROW)` (both sites) and
  `if row > LAST_TOP_ROW && !at_bottom`.
- **Effort:** small

### const-004 — tuning and protocol literals inline where every sibling value is a named const

- **Severity:** P3
- **Rule:** [`const-vs-static`](../../.claude/skills/rust-skills/rules/const-vs-static.md)
  ("small configuration values, bitmasks, and magic numbers" belong in a `const`)
- **Site:** collapsed; one entry per value:
  - `src/app/mod.rs:321-326` — the pipeline channel capacities are bare
    literals: `mpsc::channel::<CaptureEvent>(512)`, `(256)`, `(256)`, `(4)`
    (plus `Command`(16) at `:223`, `Job`(8) at `:238`). `src/stream.rs:411-417`
    reasons about "a 512-slot channel" in a comment across the module boundary,
    with nothing linking the two: the size canary there exists *because* of that
    512, and a change to it silently invalidates the canary's rationale.
  - `src/app/session/mod.rs:169` — `Duration::from_millis(150)`, the
    fatal-report race window, and `:50` `Duration::from_secs(1)`, the tick
    period. `HEARTBEAT_EVERY_TICKS = 30` (`:22`) is documented as "30 ticks
    (30 s)" — an arithmetic relation to that unnamed 1 s that the compiler
    cannot see. Every comparable value in the crate *is* named
    (`WORKER_SHUTDOWN_GRACE`, `TEARDOWN_GRACE`, `SEND_TIMEOUT`,
    `INITIAL_ANCHOR_WINDOW`, `READ_TIMEOUT_MS`).
  - `src/actuator/win.rs:464-471` — `65_535` appears four times (twice as the
    normalisation scale, twice as a clamp bound). It is the Win32
    `MOUSEEVENTF_ABSOLUTE` coordinate range, i.e. a protocol constant, and
    `WHEEL_DELTA` right above it is correctly named.
  - `src/capture/pcap.rs:379-385` — `frame.get(at..at + 2)?` and `at += 4` are
    the EtherType field width and the VLAN tag length; `ETHERTYPE_OFFSET` and
    `MAX_VLAN_TAGS` beside them are named. `LinkStrip::Fixed(4)` / `Fixed(0)`
    at `:345-347` are the `DLT_NULL` / `DLT_RAW` header sizes.
  - `src/ui/mod.rs:153` — `egui::Margin::symmetric(16, 10)`: the `16` *is*
    `theme::EDGE`, whose own doc comment says "Matches the chrome's 16px side
    margin" — the number is written twice, in two files, and
    `render_setup_tab` (`:353`) already uses `theme::EDGE` for the same axis.
  - `src/ui/theme.rs:122-123` — `item_spacing = vec2(8.0, 8.0)` and
    `button_padding = vec2(12.0, 8.0)` are inline in the very file whose spacing
    scale claims "Every gap the layout inserts comes from here"; `SP_SM` is 8.0
    and there is no name for 12.0 or 16.0. Corner radii (`same(2)`/`(3)`/`(4)`/
    `(5)`/`(6)`/`(8)` across `theme.rs`, `journal.rs:82`,
    `timing_meter.rs:24,129,172`, `editor/mod.rs:425,480`) form an unnamed
    second scale beside the named colour palette.
  - `src/domain/control/tests.rs` — `tick(10_001)`, `tick(20_001)`,
    `tick(30_001)`, `tick(35_000)` (≈20 sites) and
    `src/app/session/tests.rs:1187,1219,1291,1397` hard-code
    `watchdog::EXPECT_SNAPSHOT_MS + 1` and its multiples. The constant is
    private to `control::watchdog`, so the sibling test module cannot name it.
- **What:** each is a value whose meaning lives only in a comment or in the
  reader's head, in files where the crate's own convention is to name it.
- **Why it matters here:** this is a long-running background app tuned by
  measurement — the comments in `pcap.rs`, `stream.rs` and `plan.rs` are records
  of experiments. A number that is not named cannot be found by the next person
  running one, and cross-module ones (512, `EDGE`/16, the watchdog window) are
  already duplicated with nothing keeping the copies in step.
- **Fix:** name each at the top of its module (`CAPTURE_QUEUE_SLOTS`,
  `OUTBOUND_QUEUE_SLOTS`, `TICK_PERIOD`, `FATAL_REPORT_GRACE`,
  `ABSOLUTE_COORD_MAX`, `ETHERTYPE_LEN`, `VLAN_TAG_LEN`, `SP_MD`/`SP_LG`,
  `RADIUS_*`). For the two cross-module cases: use `theme::EDGE` at
  `src/ui/mod.rs:153`, and raise `EXPECT_SNAPSHOT_MS`/`EXPECT_PURCHASE_MS` to
  `pub(super)` so the tests can write `EXPECT_SNAPSHOT_MS + 1`.
- **Effort:** small (mechanical, one file at a time)

### const-005 — pure helpers not `const fn`, and const-derivable values recomputed at run time

- **Severity:** P3
- **Rule:** [`const-fn`](../../.claude/skills/rust-skills/rules/const-fn.md)
- **Site:** 41 candidates; `cargo clippy --all-targets -- -W clippy::missing_const_for_fn`
  lists them all (the lint is nursery and off by default; the crate declares no
  `[lints]` table and `cargo clippy` is otherwise **clean**). The ones with an
  actual payoff:
  - `src/actuator/plan.rs:169` `Trigger::pre_wait_ms` and
    `src/domain/control/watchdog.rs:22` `Proof::window_ms` — both are `match`
    over a `Copy` enum returning a `const`. As `const fn` they can appear in the
    compile-time asserts that today have to name the eight `WAIT_*` constants
    individually (`src/ui/editor/timing_meter.rs:45-52`), and in the tests that
    currently hard-code `10_001` (see const-004).
  - `src/ui/editor/mod.rs:532` — `let base_total: u64 = ROUTINE.iter().sum();`
    sums four constants on every frame the Setup tab paints, and the test at
    `:972` repeats the sum. Replace with
    `const ROUTINE_TOTAL_MS: u64 = ROUTINE[0] + ROUTINE[1] + ROUTINE[2] + ROUTINE[3];`
    (or an inline `const { … }` block), which both fixes the per-frame work and
    gives the test something to assert against.
  - `src/render.rs:15-27` — `HAUL_HEADLINERS: [(&str, &str); 2]` has its length
    hand-copied into two signatures: `haul_tally() -> ([(&'static str, u32); 2], u32)`
    and `ui::view::ViewState::haul: [(&'static str, u32); 2]`
    (`src/ui/view.rs:34`). Write the length as `HAUL_HEADLINERS.len()` — a
    `const fn` call, legal in array-length position — and adding a third
    headliner stops being a three-file edit.
  - `src/capture/pcap.rs:343,404` (`LinkStrip::for_datalink`,
    `plausible_caplen`) and `src/stream.rs:834` (`seq_diff`, called once per
    captured segment) are pure and free to mark.
- **What:** pure, allocation-free functions that `const fn` would widen at zero
  cost; plus one value derived from constants at run time.
- **Why it matters here:** the payoff is not speed — it is that a value which is
  a compile-time constant can be *asserted* at compile time. The crate already
  wants that (const-001, const-002, and the eight existing ruler asserts); the
  missing `const fn`s are what stop it from expressing the timing invariants
  against the accessor everyone else calls instead of against eight raw
  constants.
- **Fix:** mark the six named functions `const fn`, replace the `ROUTINE` sum
  with a `const`, and use `HAUL_HEADLINERS.len()` as the array length. **Do not
  blanket-apply the lint to all 41 sites** — this is a binary crate, so `const`
  on a getter nothing calls in a const context is churn, and enabling a nursery
  lint crate-wide is a separate decision (`lint-clippy-nursery-selected`).
- **Effort:** small

## Clean areas

- **`const-vs-static` — fully honoured.** Every one of the ~70 `const` items is
  a small scalar, string literal, or ≤48-byte array (correct: inlined, no
  address needed); the only two `static`s are the ones that must be
  (`src/actuator/shield.rs:27` `Mutex<Option<isize>>`, needing a single address
  and interior mutability; `src/actuator/win.rs:53` `Once`). No `static mut`
  anywhere in the crate, and the mutable-global cases use exactly what the rule
  prescribes: `AtomicU64`/`AtomicU8`/`AtomicBool` inside `Arc`
  (`SnapshotEpoch`, `PressureResync`, `watch::WatchGate`, `EventLog`).
  `src/config/persist.rs:114` `RETIRED_KEYS: &[(&str, &[&str])]` is a `const`
  reference and so is static-promoted — one copy, correctly.
- **`const-block` — the crate uses it, and uses it well.** `src/capture/mod.rs:77`
  and `src/stream.rs:418` guard per-packet type sizes with
  `#[cfg(target_pointer_width = "64")] const _: () = { assert!(size_of…) }`, each
  with a comment saying a failure means "re-measure and update the number
  deliberately". `src/ui/editor/timing_meter.rs:45-52` is the best example in the
  tree: eight `const _: () = assert!(plan::WAIT_* <= RULER_MS_U64)` tripwires
  whose comment explains the exact panic (`f32::clamp` inside an egui
  interaction) they prevent. Do not remove or "simplify" any of these.
- **`const-fn` where it counts.** `src/config.rs:20` `RECONNECT_FLOOR: Duration`
  and the `Duration::from_*` initialisers of `WORKER_SHUTDOWN_GRACE`,
  `INITIAL_ANCHOR_WINDOW`, `SEND_TIMEOUT`, `TEARDOWN_GRACE` already rely on
  `const fn` in const position; `src/watch.rs:26-27` uses `1 << 0` / `1 << 1`
  discriminants evaluated at compile time; `src/capture/pcap.rs:60`
  `PCAP_ERRBUF_SIZE` is used as an array length in two places rather than
  duplicated as a literal.
- **Named tuning constants are the norm.** `plan.rs`'s eight `WAIT_*_MS`
  baselines, `pcap.rs`'s `SNAPLEN`/`READ_TIMEOUT_MS`/`DLT_*`/`TPID_*`,
  `stream.rs`'s four stage budgets, `journal.rs`'s `JOURNAL_CAP`,
  `config.rs`'s `MAX_TIMING_MS`, `watchdog.rs`'s two `EXPECT_*_MS` — all named,
  all documented with the measurement behind them. const-003 and const-004 are
  the exceptions to an otherwise consistent practice, not the rule.

## Not applicable

- **`const-generics`** — no type or function in the crate carries a length that
  wants to be a type parameter. The fixed-size arrays that exist
  (`[&str; 2]`, `[&str; 3]`, `[u64; 4]`, `[TimingPreset; 3]`,
  `[(&'static str, DelayRange); 8]`, `[egui::Rect; 4]`, `[0u32; 16]`) are each
  used at exactly one arity and are already compile-time sized; the only
  runtime-length buffers (`Vec<u8>` payloads, `Vec<u16>` wide strings, the pcap
  error buffer) are either genuinely dynamic or already `const`-sized
  (`[0 as c_char; PCAP_ERRBUF_SIZE]`). Parameterising any of them would be
  `anti-over-abstraction`, not an improvement. The one length that *is*
  duplicated by hand — `HAUL_HEADLINERS`' `2` — is fixed by a `const fn` call
  in array-length position, not by a const generic (const-005).
