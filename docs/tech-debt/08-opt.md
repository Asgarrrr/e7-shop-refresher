# 08 — Compiler Optimization (`opt-`)

**Category priority:** HIGH
**Rules audited:** 12 · **Files read:** 41 `.rs` + `Cargo.toml` + `rust-toolchain.toml` · **Findings:** 3 (P0 0 / P1 0 / P2 0 / P3 3)

## Verdict

This category is in good shape and — more importantly — the crate is not the kind of
program this category is for. The release profile already carries the two settings that
matter (`lto = "thin"`, `codegen-units = 1`), there is not one `#[inline(always)]` and
not one integer-indexed slice access in production code, and the per-packet path is
throttled by a kernel BPF filter to one TCP port's server-to-client half — tens to a few
hundred packets a second, with the process otherwise blocked in `pcap_next_ex` or
`thread::sleep`. Nothing here is CPU-bound, and no profiling exists to claim otherwise,
so every finding below is a P3: worth doing when the file is already open, never on its
own. The nearest thing to an offender is `src/app/mod.rs`'s `capture_loop_budgeted`,
which inlines three multi-field `warn!` blocks (each taking the budget mutex to build a
`PipelineStats`) into the per-packet loop; the single highest-value fix is opt-001 —
extract those and their five siblings behind `#[cold] #[inline(never)]` helpers.

Two things in this file are **warnings, not findings**: `panic = "abort"` and
`strip = true` — both of which the skill's own "Recommended Cargo.toml Settings" block
suggests — would each break a shipped, load-bearing behaviour of this product. See
*Not applicable*; do not let a later pass apply that block wholesale.

## Findings

### opt-001 — the per-packet functions inline their rare diagnostic and pressure paths

- **Severity:** P3
- **Rule:** [`opt-cold-unlikely`](../../.claude/skills/rust-skills/rules/opt-cold-unlikely.md), [`opt-inline-never-cold`](../../.claude/skills/rust-skills/rules/opt-inline-never-cold.md)
- **Site:** `src/app/mod.rs:932-940`, `src/app/mod.rs:947-967`, `src/app/mod.rs:982-997`
  (+ `src/stream.rs:195-205`, `src/stream.rs:579-601`, `src/stream.rs:727-735`,
  `src/capture/pcap.rs:469-480`, `src/capture/pcap.rs:941-946`,
  `src/capture/pcap.rs:1027-1033`)
- **What:** Every function on the packet path builds its rare-branch diagnostics inline.
  The three worst sit in the same loop body:

  ```rust
  // src/app/mod.rs, inside capture_loop_budgeted's per-packet loop
  if pressure_resync.request(&budget) {
      let stats = budget.snapshot();          // takes the budget mutex
      warn!(
          current_total = stats.current_total,
          capture_bytes = stats.current_capture,
          /* …7 fields… */
          "capture pipeline byte pressure; dropping until resync acknowledgement"
      );
  }
  ```

  The same shape recurs in `Reassembler::push_budgeted`'s `HalfOutcome::Pressure` arm
  (`clear()` + two counter updates + `snapshot()` + a 7-field `warn!`), in
  `PipelineBudget::release`'s underflow report — which runs on *every* lease drop, i.e.
  once per payload — in `HalfStream::absorb`'s invariant `error!`, and in
  `pcap.rs`'s `Funnel::report` (called once per delivered *and* once per unparsed frame,
  with the `debug!` body behind a modulus test) and its implausible-`caplen`
  `format!`.
- **Why it matters here:** These are the only functions in the crate that run per
  captured packet: `PcapSource::next_segment` (`src/capture/pcap.rs:631`),
  `capture_loop_budgeted` (`src/app/mod.rs:866`), `Reassembler::push_budgeted`
  (`src/stream.rs:556`) and `PayloadLease::drop` → `release` (`src/stream.rs:192`).
  With `codegen-units = 1` LLVM sees all of it and will happily inline the tracing
  callsite machinery and the `PipelineStats` construction into those bodies, which is
  register pressure and instruction-cache footprint spent on branches that fire once a
  session, or never. Be honest about the scale: the kernel filter is
  `tcp and src port {game_port}` (`src/capture/pcap.rs:551`) and the measured feasibility
  probe saw *82* matched packets end to end (`src/capture/pcap.rs:1-13`), so the
  throughput win is somewhere between "unmeasurable" and "small". The real return is
  code layout and readability of the hot bodies, and that is why this is P3 and not P2.
- **Fix:** Extract each rare block into a free function annotated `#[cold]`
  `#[inline(never)]`, keeping only the branch test in the hot body — the
  "Pattern: Extract Cold Code" shape from `opt-inline-never-cold`:

  ```rust
  #[cold]
  #[inline(never)]
  fn warn_pipeline_pressure(budget: &PipelineBudget, what: &'static str) {
      let stats = budget.snapshot();
      warn!(current_total = stats.current_total, /* … */ "{what}");
  }
  ```

  Where the branch cannot be moved out of the function (the `Pressure` match arm,
  `release`'s underflow test), `std::hint::cold_path()` inside the rare branch is the
  cheaper edit. **Verified on this crate's pinned toolchain:** `std::hint::cold_path()`
  compiles on 1.92.0; `std::hint::likely`/`unlikely` are still unstable there
  (`E0658`, tracking issue 151619), so do not reach for them.
- **Effort:** small

### opt-002 — `lto = "thin"` is the only release-profile key with no recorded reason

- **Severity:** P3
- **Rule:** [`opt-lto-release`](../../.claude/skills/rust-skills/rules/opt-lto-release.md)
- **Site:** `Cargo.toml:112-118`
- **What:** The profile is three keys, and two of them carry a paragraph of rationale
  (`strip = "debuginfo"` explains the `crash.rs` interaction, the `[profile.dev.package."*"]`
  block explains the kittest lane). `lto = "thin"` carries none, and nothing in `docs/`,
  `plans/` or the justfile mentions LTO at all (grepped). The rule's own table puts
  `"fat"` at "release binaries" and `"thin"` at "CI builds".
- **Why it matters here:** This is a single shipped `.exe` a player downloads
  (11.6 MB as currently built), produced once per release, not a CI artefact — so the
  compile-time argument for thin is the weakest it can be, while the artefact-size
  argument for fat is real. But the crate has no benchmark, no profiling profile and no
  hot loop (see the Verdict), so "fat is faster" is exactly the unmeasured claim
  `anti-premature-optimize` warns against. The debt is therefore the *undocumented
  decision*, not the value: a later reader cannot tell whether `"thin"` was chosen or
  defaulted into, which is precisely what the rest of this file is careful about.
- **Fix:** Either add the one-line rationale next to it (e.g. "thin, not fat: the
  release build is a developer's machine, and the measured difference was N%/N MB"), or
  switch to `lto = "fat"` after measuring. The measurement is cheap and does not disturb
  the working tree:

  ```sh
  CARGO_TARGET_DIR=/tmp/fat cargo build --release --config 'profile.release.lto="fat"'
  ls -l target/release/arkyve-refresh-shop.exe /tmp/fat/release/arkyve-refresh-shop.exe
  ```

  Do not change `strip` or `codegen-units` while doing it.
- **Effort:** trivial (document) / small (measure and switch)

### opt-003 — nothing in this category can be measured: there is no profiling profile

- **Severity:** P3
- **Rule:** [`opt-pgo-profile`](../../.claude/skills/rust-skills/rules/opt-pgo-profile.md) (prerequisite), [`opt-lto-release`](../../.claude/skills/rust-skills/rules/opt-lto-release.md)
- **Site:** `Cargo.toml:112-118` (no `[profile.bench]`, no `[profile.profiling]`)
- **What:** `[profile.release]` sets `strip = "debuginfo"`, and there is no profile that
  inherits from it with debug info restored. There is no benchmark harness either
  (no `criterion`, no `benches/`), and the only end-to-end exercise of the capture path
  is `#[ignore]`d because it needs Npcap and a real adapter
  (`src/capture/pcap.rs:1196-1203`).
- **Why it matters here:** Every remaining rule in this category — PGO, `target-cpu`,
  fat LTO, and the value of opt-001 — is a question you answer with a profiler, and this
  crate has no way to attach one to a build that resembles what ships. That is why this
  audit produces three P3s instead of a ranked list: the evidence to rank them does not
  exist. It also means a future perf claim about this app will be a guess unless this
  gap is closed first.
- **Fix:** Add a profiling profile that changes nothing about `release`:

  ```toml
  [profile.profiling]
  inherits = "release"
  debug = 1
  strip = "none"
  ```

  Note the ordering for whoever implements it: this is *additive*. Editing
  `[profile.release]`'s `strip` to get symbols would break `crash.rs` for every player
  (see *Not applicable*). Overlaps `perf-release-profile` (category 23) — dedupe there.
- **Effort:** trivial

## Clean areas

- **`opt-codegen-units`** — `codegen-units = 1` is set (`Cargo.toml:118`). Nothing to do.
- **`opt-lto-release`** — LTO *is* enabled; only its grade is undocumented (opt-002).
  Cross-crate inlining across the `lib` → `bin`/`example` boundary is therefore already
  available.
- **`opt-inline-always-rare`** — zero `#[inline(always)]` in the crate (grepped), which
  is the correct answer: nothing here has been profiled, and the tempting candidates are
  cold. `Jitter::next`/`unit`/`point_in`/`press_ms` (`src/actuator/plan.rs:437-465`) look
  exactly like the rule's hash-function example but run a few dozen times per *click
  job* — a handful per minute. Do not annotate them.
- **`opt-inline-small`** — correctly absent. This is one binary crate: with
  `codegen-units = 1` LLVM already has every body in one unit, so `#[inline]` on
  intra-crate helpers such as `WatchGate::is_enabled` (`src/watch.rs:63`, called once per
  captured packet), `Deref for BudgetedChunk`/`BudgetedSegment` (`src/stream.rs:378`,
  `403`) or `ShopItem::catalog_id` adds nothing. The only genuine cross-crate calls are
  `main.rs` → `app::setup`/`Session::run`/`Config::load`/`crash::install`/`migrate::*`,
  all of which run once at startup — and thin LTO covers them anyway.
- **`opt-bounds-check`** — genuinely clean, verified rather than assumed. There is **no**
  integer-indexed slice access anywhere in production code: every sequence walk is
  `iter().enumerate()` (`src/render.rs:123`, `src/domain/control/mod.rs:525`,
  `src/app/session/mod.rs:661`, `src/ui/editor/mod.rs:695`) and the two `segments[index]`
  hits are test fixtures (`src/stream.rs:1065`, `src/app/mod.rs:1602`). The byte-slicing
  code the brief points at does the right thing already: `parse_segment`
  (`src/capture/ip.rs:20`) hands the whole frame to `etherparse`'s checked slice API and
  never indexes; `LinkStrip::ip_bytes` (`src/capture/pcap.rs:354`) is a single
  `frame.get(len..)`; `HalfStream::absorb`/`drain` (`src/stream.rs:707`, `777`) use
  `first_key_value`/`pop_first`/`drain(..)` with no indexing. The one place a per-byte
  check *could* have survived — `ethernet_payload_offset`'s `field[0]`/`field[1]` after a
  `frame.get(at..at + 2)?` (`src/capture/pcap.rs:375-388`) — was compiled standalone at
  `-O`: it emits a fully unrolled, branch-only body, one `movzwl` per tag, and **zero**
  panic references. The bounds checks are already elided; adding `get_unchecked` or a
  slice pattern would buy nothing and cost safety.
- **`opt-likely-hint`** — the stable half of this rule is already applied throughout: the
  unlikely case is an early return everywhere (`parse_segment`'s four
  `?`/`let … else` bails, `absorb`'s `offset > next_off` fast exit, `Funnel::report`'s
  guard, `drop_reason`'s `None`-means-go), and match arms are ordered
  common-first (`Controller::handle`, `on_tick`, `ServerMessage` dispatch). Recorded so
  nobody "fixes" it: on the pinned 1.92.0 toolchain `std::hint::cold_path()` is stable
  and `likely`/`unlikely` are not (both compiled to check).
- **`opt-cache-friendly`** — this crate is already more careful than the rule asks. The
  per-packet types carry compile-time size canaries with the queue arithmetic spelled out
  (`src/capture/mod.rs:72-83`: `FlowKey` 64 B, `Segment` 96 B; `src/stream.rs:411-424`:
  `BudgetedChunk` 48 B, `BudgetedSegment` 120 B stored by value in a 512-slot channel),
  which is exactly the "keep the hot record small" discipline. There is no AoS/SoA
  question here — no bulk numeric arrays exist — and the collection choices
  (`BTreeMap` keyed by offset for gap buffering, `VecDeque` for the burst replay,
  `Vec` everywhere else) are each justified in place.
- **The GUI is not a hot path, by design** — `request_repaint_after(250ms)`
  (`src/ui/mod.rs:137`), i.e. 4 Hz, and the two accessible-name closures deliberately
  defer their `format!` until egui actually asks (`src/ui/theme.rs:216-225`,
  `src/ui/journal.rs:52-61`), with the reason written down. That is opt-cold-unlikely
  reasoning correctly applied in the one other place this crate could feel it.
- **Tooling** — `cargo clippy` is silent on the default feature set, so there is no
  outstanding perf-shaped lint to cite. `[profile.dev.package."*"] opt-level = 2`
  (`Cargo.toml:109-110`) is set and documented.

## Not applicable

- **`opt-target-cpu`** — not applicable, and correctly so: the product is one `.exe` a
  player downloads onto an unknown consumer CPU. `-C target-cpu=native` would bake the
  *build* machine's ISA into it and crash older machines with
  `EXCEPTION_ILLEGAL_INSTRUCTION` before `main`. There is no `.cargo/config.toml` in the
  repo — that absence is the right state, not an omission. Even a conservative
  `x86-64-v2`/`v3` floor buys nothing measurable: there is no vectorizable loop (see
  `opt-simd-portable`) and the process is I/O-blocked.
- **`opt-simd-portable`** — nothing to vectorize. The only bulk byte work on the packet
  path is two copies (`ip.to_vec()` at `src/capture/pcap.rs:963`,
  `tcp.payload().to_vec()` at `src/capture/ip.rs:66`) and one memmove
  (`payload.bytes.drain(..already)` at `src/stream.rs:739`) — all already optimal as
  emitted; everything else is branchy header parsing inside `etherparse`. Portable SIMD
  is nightly-only and the toolchain is pinned to stable 1.92.0
  (`rust-toolchain.toml`). *(Those three copies are a real per-packet cost, but they are
  `mem-zero-copy` / `perf-` territory, not this category's — flagging for synthesis, not
  filing here.)*
- **`opt-pgo-profile`** — deferred, not refused. PGO needs a representative workload, and
  the only representative workload for this binary is a live Epic Seven Secret Shop
  session on a machine with Npcap: the one end-to-end capture test is `#[ignore]`d for
  exactly that reason (`src/capture/pcap.rs:1189-1203`), so an instrumented build cannot
  be exercised in CI and an unrepresentative profile would make things worse. The
  prerequisite gap is filed as opt-003; revisit only if a profiler ever shows CPU time
  in this app at all.
- **`panic = "abort"` — deliberately absent; do not add it.** Unwinding is load-bearing
  in five places: `app::Session::run` catches a session-loop panic and re-raises it after
  teardown (`src/app/mod.rs:417-437`), `SessionWorkers::spawn` turns a worker panic into
  the `"{name} task panicked"` fatal that the player sees in the banner
  (`src/app/mod.rs:496`), `spawn_capture_with_budget` does the same for the capture thread
  (`src/app/mod.rs:587`), `PcapSource::drop` reports a panicked capture thread from
  `join()` (`src/capture/pcap.rs:623`), and `SurfaceJobGuard`'s release-on-unwind is
  covered by a `catch_unwind` test (`src/actuator/mod.rs:548`). `PipelineBudget::release`
  is written the way it is *specifically* because it may run while a worker unwinds
  (`src/stream.rs:182-205`). With `abort`, every one of those becomes an immediate process
  death: no banner, no failed exit code, and an orphaned capture session. Two
  `#[should_panic]` tests (`src/stream.rs:968`, `976`) also require unwind.
- **`strip = true` — must not be applied**, even though the skill's recommended profile
  block lists it. `crash.rs`'s `Backtrace::force_capture()` (`src/crash.rs:24`) is the
  product's only post-mortem channel in the windowed build, where stdout/stderr are inert
  sinks; dropping the symbol table reduces every crash report to bare addresses. The
  current `strip = "debuginfo"` is the correct compromise and is already documented at
  `Cargo.toml:114-117`. Anything that needs symbols *and* line numbers should go through
  the new profile in opt-003, never through `[profile.release]`.
