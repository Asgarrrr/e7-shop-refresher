# 24 — Project Structure (`proj-`)

**Category priority:** LOW
**Rules audited:** 14 · **Files read:** 46 (42 Rust files, `Cargo.toml`, `.github/workflows/ci.yml`, `rust-toolchain.toml`, `justfile`) · **Findings:** 13 (P0 0 / P1 3 / P2 7 / P3 3)

## Verdict

Structurally this crate is in better shape than its file sizes suggest: the module tree is feature-shaped, `pub(super)` is used deliberately and widely (`ui/theme.rs` alone has 21), `src/render.rs` is a model private-module-with-`pub(crate)`-items, and the MSRV is declared *and* pinned *and* in the CI matrix. The worst offender is **`src/app/mod.rs` at 2266 lines** — but only 1118 of those are production code, and those 1118 lines are five mutually independent concerns joined by nothing but mpsc channels, which makes them unusually cheap to split. The single highest-value fix is that split, and its first step is free: move the 1148-line inline `mod tests` into `src/app/tests.rs`, exactly as `src/app/session/` and `src/domain/control/` already do. `src/capture/pcap.rs` is the only file whose *production* half genuinely exceeds a thousand lines.

Two findings are not about file size at all and matter more than any of them. **`rust-toolchain.toml` silently neutralises the `stable` arm of the CI matrix** (rustup's toolchain file outranks `rustup default`, which is all `dtolnay/rust-toolchain` sets), so nothing in this project has ever been compiled on current stable — the matrix tests the MSRV twice. And **`src/main.rs` holds ~200 lines of startup policy no test can reach** in a crate that otherwise carries ~5600 lines of tests; `config.rs`'s own test re-implements `seed_config_if_missing` by hand because the real function lives in the binary.

## Findings

### proj-001 — `src/app/mod.rs` joins five independent concerns in one 1118-line production module

- **Severity:** P1
- **Rule:** [`proj-mod-by-feature`](../../.claude/skills/rust-skills/rules/proj-mod-by-feature.md), [`proj-flat-small`](../../.claude/skills/rust-skills/rules/proj-flat-small.md)
- **Site:** `src/app/mod.rs` (2266 lines total; production code is lines 1–1118, tests 1119–2266 — see `proj-004` for the tests)
- **What:** The module is the crate's orchestrator, and it currently *is* five things. The concerns do not interleave; each occupies a contiguous run of lines:

  | Lines | Concern | Items |
  |---|---|---|
  | 49–131 | capture↔reassembly backpressure protocol | `CaptureEvent`, `PressureResync`, `RESYNC_ACK/PENDING/ENQUEUED` |
  | 133–143, 638–859 | the reassembly pump | `AnchorState`, `INITIAL_ANCHOR_WINDOW`, `reassemble_loop_with_pressure`, `flush_anchor`, `forward_segment`, `ForwardStatus`, `forward_chunks` |
  | 861–1023, 1077–1117 | the capture pump | `CAPTURE_PROGRESS_EVERY`, `capture_loop_budgeted`, `build_source` (both `cfg` arms) |
  | 445–614 | worker supervision and teardown | `CaptureWorker`, `TokioWorker`, `SessionWorkers`, `report_join`, `WORKER_SHUTDOWN_GRACE`, `spawn_capture_with_budget` |
  | 1025–1075 | console input | `stdin_loop`, `input_loop`, `parse_command` |
  | 149–443, 616–636 | the wiring itself | `Command`, `SessionHandles`, `ShutdownSignal`, `Session`, `setup`, `run`, `Session::run`, `supervise`, `actuator_mode`, `redacted_server_url` |

- **Why it matters here:** This is the file where a mistake costs a player an orphaned live capture session in the Npcap driver, or a crash reported as a clean "session ended". Two sibling reviewers already found duplication inside it that a reader cannot see because the file is too long to hold in view at once: the `macro-` reviewer found `reassemble_loop_with_pressure` hand-expanding the same `ForwardStatus` dispatch four times (lines 709/719/729/757), and the `trait-` reviewer found two near-identical actuator-backend arms at 368–400. Both live entirely inside one of the runs above, and both become obvious once that run is a 230-line file instead of a middle third.

  The seam that makes this safe is the concurrency topology the `conc-` reviewer confirmed: **every one of those five concerns talks to the others only through `tokio::sync::mpsc` channels and `Arc<AtomicU8>`/`Arc<PipelineBudget>` handles.** There is no shared `&mut` state, and the one lock nesting in this crate (`controller` → `timings`) lives entirely inside `src/app/session/`, which this file does not touch. Concretely: `capture_loop_budgeted` and `reassemble_loop_with_pressure` run on different threads and share exactly two types (`CaptureEvent`, `PressureResync`); `SessionWorkers` owns handles and never inspects payloads; `input_loop` touches only `mpsc::Sender<Command>` and a `watch::Receiver<bool>`.

- **Fix:** Six files, no behaviour change. `src/app/session/` stays as it is.

  ```
  src/app/
  ├── mod.rs        ~300  Command, SessionHandles, ShutdownSignal, Session, setup,
  │                       run, Session::run, supervise, actuator_mode,
  │                       redacted_server_url, build_source
  ├── pressure.rs   ~95   CaptureEvent, PressureResync, RESYNC_* — the vocabulary
  │                       the two pumps share; `pub(super)` items only
  ├── ingest.rs     ~170  CAPTURE_PROGRESS_EVERY, capture_loop_budgeted (+ the
  │                       cfg(test) capture_loop shim)
  ├── reassembly.rs ~230  AnchorState, INITIAL_ANCHOR_WINDOW, ForwardStatus,
  │                       reassemble_loop_with_pressure, flush_anchor,
  │                       forward_segment, forward_chunks
  ├── workers.rs    ~150  WORKER_SHUTDOWN_GRACE, CaptureWorker, TokioWorker,
  │                       SessionWorkers, report_join, spawn_capture_with_budget
  ├── console.rs    ~55   stdin_loop, input_loop, parse_command
  └── session/            (unchanged)
  ```

  Three notes for whoever does it:

  1. `pressure.rs` exists because `CaptureEvent` and `PressureResync` are the *only* two items both pumps need. Making it its own file rather than leaving them in `mod.rs` is what keeps `ingest.rs` and `reassembly.rs` from each importing half of the root.
  2. `build_source` stays in `mod.rs` rather than moving to `ingest.rs`: it is called from `Session::run`, and its two `cfg` arms are part of the wiring story (which backend this build has), not of the receive loop.
  3. The `#[cfg(test)]`-only shims — `spawn_capture` (598), `reassemble_loop` (640), `capture_loop` (1007) and the `CaptureEvent::Segment` variant (54) — exist purely to give tests shorter signatures. After the split they become cross-module `pub(super)`, which is fine, but they are also the thing to delete if the shared test fixtures move to a `#[cfg(test)] pub(super) mod fixtures` (see `proj-004`).
- **Effort:** medium

### proj-002 — `rust-toolchain.toml` overrides the CI matrix: the `stable` arm is a second MSRV arm, and nothing is ever built on current stable

- **Severity:** P1
- **Rule:** [`proj-msrv-declare`](../../.claude/skills/rust-skills/rules/proj-msrv-declare.md)
- **Site:** `.github/workflows/ci.yml:16` (`matrix.toolchain: ["1.92.0", stable]`) against `rust-toolchain.toml:2` (`channel = "1.92.0"`)
- **What:** `dtolnay/rust-toolchain` activates its toolchain with **`rustup default` and nothing else** — it sets neither `RUSTUP_TOOLCHAIN` nor a directory override (verified against the action's `action.yml`). rustup's override precedence is `+toolchain` > `RUSTUP_TOOLCHAIN` > directory override > **`rust-toolchain.toml`** > `rustup default`. The repo-root `rust-toolchain.toml` therefore wins over what the action set, and every `cargo` invocation in the `verify` job runs on 1.92.0 regardless of which matrix arm it is in.
- **Why it matters here:** The matrix's whole purpose is "the floor we promise, plus the compiler people actually have". Right now the floor is checked twice at double the CI cost and the ceiling is never checked at all. The failure this hides is precisely the one a pinned MSRV cannot: a new stable rustc turning something in this crate into a `-D warnings` clippy failure, a dependency bump that needs a newer compiler, or an edition-2024 behaviour that shifted. None of it surfaces until a release build on a developer's machine — and `rust-toolchain.toml` pins that too.

  The `if: matrix.toolchain == '...'` guards still fire as written (they test the matrix *value*, not the live toolchain), so `cargo fmt --check`, `cargo build --release` and the `requireAdministrator` manifest assertion all still run. They simply all run on 1.92.0. No coverage is lost there; what is lost is stable entirely.
- **Fix:** Set the environment variable that outranks the toolchain file, at job level:

  ```yaml
  jobs:
    verify:
      runs-on: windows-latest
      env:
        RUSTUP_TOOLCHAIN: ${{ matrix.toolchain }}
  ```

  Keep `rust-toolchain.toml` as it is — pinning local developer builds to the MSRV is a deliberate and good choice, and this fix does not weaken it. (`rustup override set` would also work but leaves per-runner state; `cargo +${{ matrix.toolchain }}` would work but has to be threaded through nine commands.) The `dependency-policy` job pins `@1.92.0` and is unaffected — the file agrees with it by coincidence, so consider adding the same `env` there to make the agreement explicit rather than accidental.
- **Effort:** trivial

### proj-003 — ~200 lines of startup policy live in `src/main.rs`, where no test can reach them

- **Severity:** P1
- **Rule:** [`proj-lib-main-split`](../../.claude/skills/rust-skills/rules/proj-lib-main-split.md)
- **Site:** `src/main.rs:22–100` (`config_path`, `seed_config_if_missing`, `log_dir`, `install_logging`), `:169–198` (retired-key reporting), `:224–234` (`fatal`), `:236–340` (both `run_mode` arms). `src/lib.rs` is 41 lines, of which 15 are `pub mod` declarations.
- **What:** `lib.rs` is thin in the sense of "short", not in the sense of "holds the logic". Everything that decides *where the app's files live*, *whether logging works at all*, *what a player is told about retired config keys*, and *what exit code a closing window produces* is in the binary crate, which integration tests and `cargo test` unit tests cannot import.

  This is not theoretical. `src/config.rs:957` (`bundled_example_config_parses_validates_and_is_restrictive`) has to re-implement the seeding step by hand —

  ```rust
  let text = include_str!("../config.example.toml");
  std::fs::write(&path, text).expect("seed the example");
  ```

  — because `seed_config_if_missing` is unreachable from the library. The test's own comment says it exists so that "the shipped exe [does not hand] 100% of new players an 'Invalid configuration' window", yet it cannot exercise the function that actually writes the file on a new player's machine.

  The untested decisions, specifically:
  - `config_path()` — the `%APPDATA%`-then-`./config.toml` fallback. A wrong branch means the app reads a config nobody edits.
  - `seed_config_if_missing()` — the create-parent-then-write, and its "do nothing if it exists" guard.
  - `log_dir()` / `install_logging()` — including the `Err(_) => stdout` fallback, which is the exact failure mode `src/migrate.rs` exists to prevent and which is invisible in the windowed build.
  - lines 169–198 — a three-arm decision table (`Ok(Some)` past tense / `Err` present tense / `Ok(None)` "already gone") whose whole point is that a player reads the right one. `config.rs` tests `strip_retired_keys` thoroughly; nothing tests this dispatch.
  - `run_mode` (GUI) — spawn, `supervise`, `run_native`, `shutdown.request()`, the `TEARDOWN_GRACE` timeout, and the exit-code rule at `:333` whose comment says "scripts and smoke checks read the exit code". A documented external contract with zero coverage.
- **Why it matters here:** Everything else in this crate is tested to an unusual depth — ~5600 lines of tests, `#[should_panic]` split by `debug_assertions`, paused-clock teardown-ordering tests. The startup path is the one place where that discipline stops, and it is the path every single run takes first.
- **Fix:** Move the logic into the library and leave `main` as dispatch. A new `src/startup.rs` (private `mod`, `pub(crate)` items, plus one `pub fn main() -> ExitCode`) is the smallest shape:

  ```rust
  // src/main.rs — the whole file
  #![cfg_attr(all(windows, feature = "gui"), windows_subsystem = "windows")]
  fn main() -> std::process::ExitCode {
      arkyve_refresh_shop::startup::run()
  }
  ```

  `config_path`, `log_dir` and the retired-key reporting are pure or near-pure and become directly testable; `install_logging` and `run_mode` stay hard to test but at least become *reachable*, and `seed_config_if_missing` lets `config.rs:957` call the real thing instead of a copy. Note the one constraint: `#![cfg_attr(..., windows_subsystem = "windows")]` is a crate-level attribute on the *binary* and must stay in `main.rs`.
- **Effort:** medium

### proj-004 — Test placement follows two conventions; the file-splitting one is applied to only 2 of 7 candidates

- **Severity:** P2
- **Rule:** [`proj-mod-rs-dir`](../../.claude/skills/rust-skills/rules/proj-mod-rs-dir.md), [`proj-flat-small`](../../.claude/skills/rust-skills/rules/proj-flat-small.md)
- **Site:** measured production/test split per file:

  | File | Total | Production | Inline tests |
  |---|---:|---:|---:|
  | `src/app/mod.rs` | 2266 | 1118 | **1148** |
  | `src/config.rs` | 1274 | 434 | **840** |
  | `src/actuator/mod.rs` | 1220 | 404 | **816** |
  | `src/actuator/win.rs` | 1510 | 742 | **768** |
  | `src/stream.rs` | 1418 | 837 | **581** |
  | `src/config/persist.rs` | 876 | 400 | **476** |
  | `src/actuator/plan.rs` | 1024 | 560 | **464** |
  | `src/app/session/mod.rs` | 673 | 671 | 2 (`mod tests;`) |
  | `src/domain/control/mod.rs` | 718 | 716 | 2 (`mod tests;`) |

- **What:** The crate already owns the fix and uses it twice: `src/app/session/mod.rs:673` and `src/domain/control/mod.rs:10–11` declare `#[cfg(test)] mod tests;` and keep the tests in a sibling `tests.rs` (1423 and 1781 lines respectively). Seven other files keep tests inline, and in five of them the tests are the *majority* of the file. Four of the eight files the brief flagged as oversized are oversized only because of this — `config.rs` (434 production lines) and `actuator/mod.rs` (404) need no restructuring whatsoever once the tests move.
- **Why it matters here:** This is the reason the file-size picture of this crate is misleading, and it is what makes "`src/app/mod.rs` is 2266 lines" read as a far worse problem than it is. It also has a real cost: a reader opening `config.rs` to change a validation rule scrolls past 840 lines of tests to find 434 lines of code, and `git log --stat` cannot distinguish "the loader changed" from "a test was added".
- **Fix:** For each file, replace the inline `mod tests { ... }` with `#[cfg(test)] mod tests;` and move the body to a sibling. `src/config.rs` needs the adjacent-file question settled first (see `proj-009`); the others are mechanical:
  - `src/actuator/mod.rs` → `src/actuator/tests.rs`
  - `src/actuator/win.rs` → `src/actuator/win/tests.rs` (needs `win.rs` → `win/mod.rs`, or do it as part of `proj-007`)
  - `src/actuator/plan.rs`, `src/stream.rs`, `src/config/persist.rs` → same shape
  - `src/app/mod.rs` → `src/app/tests.rs`, then redistribute to `src/app/<submodule>/tests` as `proj-001` lands

  The one real friction point is `src/app/mod.rs`: its test module owns fixtures used by more than one of the five concerns (`initial_anchor_segment`, `segment_with_capacity`, `recv_exact`, `NoopStop`, `LiveGuard`, `blocking_capture`, `fake_actuator`, `EnableOnFirstSegment`, `LosingSource`, `BlockingSource`/`BlockingStop`, `ImmediateErrorSource`). Those want a `#[cfg(test)] pub(super) mod fixtures;` under `src/app/` rather than being duplicated or left behind.
- **Effort:** small (mechanical), medium for `app/mod.rs` because of the shared fixtures

### proj-005 — `src/capture/pcap.rs` is the only file whose *production* half exceeds 1000 lines, and it holds four separable layers

- **Severity:** P2
- **Rule:** [`proj-mod-by-feature`](../../.claude/skills/rust-skills/rules/proj-mod-by-feature.md), [`proj-flat-small`](../../.claude/skills/rust-skills/rules/proj-flat-small.md)
- **Site:** `src/capture/pcap.rs` (1213 lines; 1035 production, only 178 tests)
- **What:** Four layers, already separated by the file's own `// --- ... ---` banner comments, which is the clearest possible signal that the author sees the same seams:
  - **FFI** (47–311, plus the constants at 58–117): `PcapIf`, `PcapPktHdr`, `BpfProgram`, `PcapStat`, `PcapT`, `Wpcap` and its 13 resolved symbols, `cstr`, `DLL_CANDIDATES`, `INSTALL_HINT`, `SNAPLEN`, `READ_TIMEOUT_MS`, `NEXT_EX_*`, `DLT_*`. ~250 lines.
  - **Link-layer stripping** (313–406): `LinkStrip`, `TPID_8021Q/8021AD`, `ETHERTYPE_OFFSET`, `MAX_VLAN_TAGS`, `ethernet_payload_offset`, `plausible_caplen`. ~95 lines — and **8 of the 10 real tests in the file test only this**, so the tests move with it almost intact.
  - **Device enumeration and opening** (408–448, 678–898): `Handle`, `Refusal`, `enumerate`, `open_device`, `no_usable_device_error`, `npcap_admin_only`, `wide`, `short_device_name`. ~260 lines.
  - **The per-adapter receive thread** (900–1034): `capture_loop`, `poll_drops`, `STATS_EVERY_PACKETS`. ~135 lines.
  - Remainder for the root: `PcapSource`, `PcapStop`, `Funnel`, `FUNNEL_LOG_EVERY`. ~200 lines.
- **Why it matters here:** The link-stripping layer is pure, has a documented **untested** VLAN path (`ethernet_payload_offset`'s `⚠ Untested` note), and is where a future bug will land — a machine that runs tagged VLANs sees "a silent, total parse failure". It is 95 lines of pure function currently buried at the midpoint of a 1035-line FFI file. Pulling it out makes it the obvious place to add the missing coverage.
- **Fix:** `src/capture/pcap.rs` → `src/capture/pcap/{mod,ffi,link,devices,worker}.rs`, splitting on the existing banners.

  **The one real cost, stated plainly:** `Handle` (416–440) carries `unsafe impl Send for Handle` whose safety argument is *"Nothing else in this module retains the raw pointer."* `Handle` is created in `devices.rs` (`open_device`) and consumed in `worker.rs` (`capture_loop`), so a split has to widen `handle`/`strip`/`device`/`wpcap` to `pub(super)` — which widens the set of code that argument has to be checked against from one file to three. Two ways to keep it honest: put `Handle` (and only `Handle`) in the `pcap/mod.rs` root with its fields private and add the two accessors `worker.rs` needs, so the `unsafe impl` and every field access stay in one file; or leave `capture_loop` in the same file as `Handle` and split off only `ffi` and `link`. The second is smaller and captures most of the value — `ffi` + `link` alone removes 345 lines and takes 8 of the 10 tests with it.
- **Effort:** medium

### proj-006 — `src/stream.rs` holds byte accounting and TCP reassembly in one file, coupled only by two private fields

- **Severity:** P2
- **Rule:** [`proj-mod-by-feature`](../../.claude/skills/rust-skills/rules/proj-mod-by-feature.md)
- **Site:** `src/stream.rs` (1418 lines; 837 production, 581 tests)
- **What:** Two concerns with almost no overlap:
  - **Byte budget** (30–424): `Stage`, `BudgetLimits`, `Usage`, `BudgetInner`, `PipelineBudget`, `PipelineStats`, `PayloadLease`, `BudgetedChunk`, `BudgetedSegment`, `stage_bytes`, `stage_bytes_mut`, the four `*_STAGE_BYTES` constants, and the two `size_of` canaries. ~395 lines. Nothing in it knows what TCP is.
  - **Reassembly** (426–836): `MAX_STREAMS`, `INITIAL_ANCHOR_MAX_*`, `InitialBurst`, `Reassembler`, `HalfStream`, `HalfOutcome`, `ReassemblyOutcome`, `segment_data_seq`, `seq_diff`. ~410 lines. It uses the budget only as an opaque carrier.
- **Why it matters here:** The module doc comment is 19 lines about TCP sequence wrap; the first 400 lines of code are about `Mutex<Usage>` and lease accounting, and a reader looking for the wrap logic reads all of it first. More concretely, the two halves fail differently and have different rules — `PayloadLease::drop` deliberately saturates rather than asserting because it runs during unwind (`release`'s comment, 182–191), while `try_retag` keeps a hard assert "in every profile" because it never runs from a `Drop`. That distinction is a property of the *budget* layer and is much easier to hold when the budget layer is its own file.
- **Fix:** `src/stream.rs` → `src/stream/{mod,budget,reassembly}.rs`, with `mod.rs` keeping the module docs and `pub(crate) use` re-exports so no call site outside the module changes.

  **The seam that needs work first:** the reassembly half currently reaches into `BudgetedChunk`'s private fields three times, so a split needs two small accessors added to `budget.rs` before it will compile:
  - `src/stream.rs:739` — `payload.bytes.drain(..already)` inside `HalfStream::absorb`. Needs a `BudgetedChunk::drain_front(&mut self, n: usize)`, which is also a better home for the "already delivered" comment than the caller.
  - `src/stream.rs:559` — `segment.payload.lease.budget.clone()` inside `Reassembler::push_budgeted`. Needs `BudgetedSegment::budget(&self) -> PipelineBudget` (or `&PipelineBudget`).
  - `src/stream.rs:819`/`902` — `chunk.bytes` in the two `#[cfg(test)]` flatteners. `as_slice()` plus `to_vec()`, or a `#[cfg(test)] into_bytes()`.

  Prefer accessors over `pub(super)` fields here: the budget's whole invariant is that a `Vec<u8>` and its lease are never separated except through `into_parts`, and `pub(super) bytes` would make that unenforceable across the two files.
- **Effort:** medium

### proj-007 — `src/actuator/win.rs` holds two independent input backends plus their shared Win32 layer

- **Severity:** P2
- **Rule:** [`proj-mod-by-feature`](../../.claude/skills/rust-skills/rules/proj-mod-by-feature.md)
- **Site:** `src/actuator/win.rs` (1510 lines; 742 production, 768 tests)
- **What:** The file's own doc comment names the split in its first sentence — "`MessageSurface` (`PostMessageW`, background, shielded — the default) and `WinSurface` (`SendInput`, real cursor, foreground — the fallback)". They share no state and are selected between at `src/app/mod.rs:374–399`.
  - **`SendInput` backend:** `InputEvent` (65–71), `InputDriver` (73–91), `SystemInputDriver` (93–140), `WinSurface` (142–277), `system_metric`, `move_cursor`, `send_mouse`, `send_input`, `sendinput_result` (445–525), plus `FOCUS_SETTLE_MS`. ~330 lines. All 21 of the `FakeInputDriver` tests belong to it.
  - **`PostMessageW` backend:** `post_refusal` (527–556), `MessageSurface` (558–591), `Target::{to_client, engage, verify}` (600–634), `impl Surface for MessageSurface` (636–703), `pack_point`, `post` (705–741), plus `SHIELD_DRAIN_MS`. ~215 lines.
  - **Shared Win32:** `wide`, `ensure_dpi_awareness`, `find_game_window`, `client_rect`, `probe_window_reachable`, `preflight_refusal`, `Target`'s definition, `GAME_WINDOW_TITLE`, `WHEEL_DELTA`, `MOVE_SETTLE_MS`. ~195 lines, including the two longest and most load-bearing doc comments in the crate (`probe_window_reachable`, `preflight_refusal`).
- **Why it matters here:** `probe_window_reachable` and `preflight_refusal` carry the measured UIPI evidence this whole product's elevation model rests on, and they are currently sandwiched between the two backends that consume them. Hoisting them into the module root makes it structurally obvious that they are the shared contract, not a detail of either backend.
- **Fix:** `src/actuator/win.rs` → `src/actuator/win/{mod,send_input,post_message}.rs`.

  One Rust detail worth knowing before starting: `Target` is used by both backends but *asymmetrically* — `WinSurface` reads only its `hwnd`/`rect` fields, while `to_client`/`engage`/`verify` are `MessageSurface`-specific. Put the `struct Target` definition in `win/mod.rs` and leave its `impl Target` block in `win/post_message.rs`; inherent impls may live in any module of the same crate, so this needs no visibility widening beyond `pub(super)` on the two fields. `wide` is already `pub(super)` (used by `shield.rs`) and stays so.
- **Effort:** medium

### proj-008 — `src/ui/editor/mod.rs` holds three independent editor sections in one 787-line production module

- **Severity:** P2
- **Rule:** [`proj-mod-by-feature`](../../.claude/skills/rust-skills/rules/proj-mod-by-feature.md)
- **Site:** `src/ui/editor/mod.rs` (1140 lines; 787 production, 353 tests)
- **What:** The module doc names three groups ("Hunt … Stop … Click timing") and the code implements them as three disjoint sets of functions:
  - **Hunt:** `hunt_summary` (146–183), `hunt_body` (226–283), `quick_add_names` (288–301), `string_list` (692–720), `substat_reqs` (723–760), `optional_value` (769–786). ~185 lines.
  - **Stop:** `stop_summary` (187–206), `stop_body` (310–317), `limit_row` (325–349), `duration_row` (354–376), `limit_ledger_row` (383–410), `compact_drag` (416–429). ~135 lines.
  - **Click timing:** `timing_summary` (211–216), `timing_body` (436–459), `preset_row` (468–501), `mode_hint` (506–519), `pass_estimate` (525–541), `fine_tune_body` (546–589), `ROUTINE` (597–602). ~145 lines. `timing_meter.rs` already exists as this group's widget file.
  - **Shell:** `EditorState`, `edit_sections`, `section`, `commit_row`, `dirty_summary`, `count_label`, `remove_button`. ~230 lines.
- **Why it matters here:** `timing_meter.rs` proves the intended shape — the timing group's *painting* was already lifted out, and its comment says so ("lifted out of the editor shell"). The three sections were simply never followed. `src/ui/` is otherwise the most disciplined part of the crate (`statusbar`, `shop`, `journal`, `theme`, `view` each in their own file, all `pub(super)`), which makes `editor/mod.rs` the one place the convention lapses.
- **Fix:** `src/ui/editor/{mod,hunt,stop,timing}.rs`, with `timing_meter.rs` staying beside `timing.rs`.

  **The seam to fix first:** `hunt_body` and `stop_body` take `&mut EditorState` rather than `&mut Filter`/`&mut Limits`, because they also need the scratch fields (`name_input`, `set_input`, `substat_input`) and the open flags. Splitting on the current shape forces all twelve `EditorState` fields to `pub(super)`. The better move is to group the drafts with their scratch state first:

  ```rust
  pub(super) struct HuntDraft { filter: Filter, applied: Filter, name_input: String,
                                set_input: String, substat_input: String, open: bool }
  pub(super) struct StopDraft { limits: Limits, applied: Limits, open: bool }
  pub(super) struct TimingDraft { timings: Timings, applied: Timings, open: bool,
                                  fine_tune_open: bool }
  pub struct EditorState { hunt: HuntDraft, stop: StopDraft, timing: TimingDraft }
  ```

  Each section file then owns exactly one draft type, `commit_row`'s three `dirty_*` checks become three `is_dirty()` calls, and `mark_applied`'s match arms delegate. Note `EditorState` and `EditorState::new` are two of the five `unreachable_pub` hits in `proj-010` and should become `pub(super)` in the same pass.
- **Effort:** medium

### proj-009 — The crate mixes both multi-file-module conventions and enforces neither; `Cargo.toml` has no `[lints]` table at all

- **Severity:** P2
- **Rule:** [`proj-mod-rs-dir`](../../.claude/skills/rust-skills/rules/proj-mod-rs-dir.md)
- **Site:** `src/config.rs` + `src/config/persist.rs` (adjacent-file style) against nine `mod.rs` directories: `src/app/`, `src/app/session/`, `src/actuator/`, `src/capture/`, `src/domain/`, `src/domain/control/`, `src/ui/`, `src/ui/editor/`, `src/uplink/`. `Cargo.toml` has no `[lints]` section; there is no `clippy.toml`.
- **What:** Nine to one. The rule's own "Consistency Rule" section says to pick one and pin it with a lint (`clippy::self_named_module_files` for a `mod.rs` crate like this one, `clippy::mod_module_files` for the opposite convention — the names read backwards, and reaching for the wrong one reverses what it pins); neither is set, so nothing stops the next module from picking whichever.
- **Why it matters here, and the fair reading:** The 2024 edition and clippy's default lean the *other* way — `foo.rs` + `foo/` is the shape `self_named_module_files` endorses, and `src/config.rs` is arguably the one file already doing it right. But "which convention is better in the abstract" is not the question the rule asks; consistency is, and the cost of the two directions is nine files versus one. **Pick `mod.rs`.** Both lints are allow-by-default, so neither is "the default" in any enforced sense, and moving nine files (each of which changes every `use super::` path inside it) to satisfy a stylistic preference is not a good trade against moving one.
- **Fix:** Two edits.
  1. `git mv src/config.rs src/config/mod.rs`. Nothing else changes: `src/lib.rs:18` already says `pub mod config;`, `persist.rs`'s `super::CaptureConfig` / `super::ForwardConfig` doc links still resolve, and `include_str!("../config.example.toml")` at `src/config.rs:966` becomes `include_str!("../../config.example.toml")` — that is the only path in the file that moves. Do this together with `proj-004`'s `src/config/tests.rs` extraction; the two touch the same file.
  2. Add the lint table `Cargo.toml` does not have yet, which is also where `proj-010`'s check belongs:

     ```toml
     [lints.rust]
     unreachable_pub = "warn"

     [lints.clippy]
     self_named_module_files = "warn"
     ```

  A `[lints]` table is worth adding on its own account: `cargo clippy --all-targets -- -D warnings` in CI covers the default lint set, but nothing in the repo records which *non-default* lints this crate has decided to honour, so a decision like "we chose `mod.rs`" has nowhere to live except a code review.
- **Effort:** trivial

### proj-010 — Over-broad `pub`: four `pub mod`s no external consumer needs, and five `unreachable_pub` items the compiler will name for free

- **Severity:** P2
- **Rule:** [`proj-pub-crate-internal`](../../.claude/skills/rust-skills/rules/proj-pub-crate-internal.md)
- **Site:** `src/lib.rs:15,20,25,28` (`pub mod actuator`, `pub mod error`, `pub mod stream`, `pub mod uplink`), `pub mod capture` at `:17`; `src/ui/view.rs:9,40,53`; `src/ui/editor/mod.rs:29,52`; `src/actuator/plan.rs` (41 bare `pub` items).
- **What:** Two distinct layers, one of them machine-detectable.

  **(a) Machine-detectable.** `RUSTFLAGS="-W unreachable_pub" cargo check --all-targets` reports exactly five warnings, all `pub` items inside private modules that no path can reach:

  ```
  src/ui/editor/mod.rs:29   pub struct EditorState
  src/ui/editor/mod.rs:52   pub fn new
  src/ui/view.rs:9          pub struct ViewState
  src/ui/view.rs:40         pub struct SlotRow
  src/ui/view.rs:53         pub fn view_state
  ```

  All five should be `pub(super)`, matching every other item in `src/ui/`. Note this is the *one* thing in this whole audit a tool can find, and it takes an allow-by-default rustc lint to find it — `cargo clippy --all-targets` is silent on this crate.

  **(b) Not machine-detectable, and larger.** This crate has exactly two consumers outside the library: `src/main.rs` and `examples/ui_preview.rs` (an example is a separate crate, so it genuinely pins part of the public surface). Between them they import:

  - `main.rs`: `Config`, `APP_NAME`, `APP_DIR`, `app::{setup, run, supervise, Session, SessionHandles, ShutdownSignal}`, `crash::install`, `migrate::{clean_windivert_leftovers, Leftovers::report}`, `config::persist::strip_retired_keys`, `ui::{show_fatal, ShopApp, SessionErrorSlot}`, and `config.actuator.timings`.
  - `ui_preview.rs`: `app::{Command, SessionHandles}`, `domain::control::{Controller, Event, Limits, Status}`, `domain::filter::Filter`, `domain::shop::*`, `journal::EventLog`, `ui::{SessionErrorSlot, ShopApp}`, `watch::WatchGate`.

  Nothing outside the library touches `capture`, `stream`, `uplink`, or — with one exception — `actuator`. Verified item by item where it matters: `MAX_ASPECT`, `buy_zone`, `Jitter`, `DesignPoint`, `Anchor`, `TimedStep` and `REFRESH` in `src/actuator/plan.rs` appear **nowhere** in the crate outside `plan.rs` itself (the earlier apparent `REFRESH` hit in `ui/editor/mod.rs` is the UI string `"OPEN & REFRESH"`).
- **Why it matters here:** The brief is right that `pub` on an internal item is not a public API in a `publish = false` binary, and this is not a finding about semver. It is a finding about the compiler's ability to help. Right now `unreachable_pub` finds five items out of roughly 190 `pub` declarations, because `lib.rs` re-exports almost everything and the lint has nothing to bite on. Narrowing five `mod` declarations turns that same lint into a working, permanent, zero-maintenance check over the whole `actuator`/`capture`/`stream`/`uplink` subtree — and those four modules hold the FFI, the unsafe `Send` impl, the byte-budget invariants and the input backends, i.e. exactly the code where "who can call this" is worth knowing.

  The crate already demonstrates it knows how: `mod render;` is private with 13 `pub(crate)` items and two of them feature-gated; `src/capture/mod.rs` keeps `mod ip` private and re-exports only `parse_segment`; `src/uplink/mod.rs` keeps `mod websocket` private and re-exports only `run`.
- **Fix:** Four `mod` lines, then let the compiler enumerate the rest.

  1. `src/lib.rs:17,25,28` — `pub mod capture;` → `mod capture;`, same for `stream` and `uplink`.
  2. `src/lib.rs:20` — `pub mod error;` → `mod error;`. The root `pub use error::{Error, Result};` at `:32` already gives the only path anything uses.
  3. `src/lib.rs:15` — `actuator` cannot simply become private: `plan::Timings` (and through its `pub` fields, `plan::DelayRange`) is genuinely part of the public surface, reached both by `Config.actuator.timings` (read at `src/main.rs:270`) and by `ui::ShopApp::new`'s fourth parameter (`examples/ui_preview.rs:185` passes `Default::default()`). Two options: keep `pub mod actuator` and reduce the ~37 other `pub` items in `plan.rs` plus the seven in `actuator/mod.rs` (`Mode`, `SnapshotEpoch`, `ActuatorHandle`, `SubmitError`, `SurfaceError`, `Surface`, `run_executor`) and `win.rs`'s two (`WinSurface`, `MessageSurface`) to `pub(crate)` by hand; or make it `pub(crate) mod actuator;` and add `pub use actuator::plan::{DelayRange, Timings};` to `lib.rs`, which lets `unreachable_pub` find the other 46 for you. Prefer the second.
  4. Regardless of which: `MAX_ASPECT`, `buy_zone`, `Jitter` (and its three `pub` methods) and `REFRESH` in `plan.rs` are used nowhere outside that file and should be plain private.
  5. Add `unreachable_pub = "warn"` under `[lints.rust]` (see `proj-009`) so this does not drift back.

  Do **not** narrow `app`, `config`, `crash`, `domain`, `journal`, `migrate`, `ui` or `watch` — the example needs all of them, and shrinking the library to fit only `main.rs` would break `cargo run --example ui_preview`, which is the only way the GUI is inspectable on a machine with no Npcap.
- **Effort:** small

### proj-011 — Four of the eight feature combinations are never built in CI (all four compile today)

- **Severity:** P3
- **Rule:** [`proj-feature-additive`](../../.claude/skills/rust-skills/rules/proj-feature-additive.md)
- **Site:** `.github/workflows/ci.yml:28–39,59–64`; `justfile:11–12,15–16,27–30`; `Cargo.toml:10–27`
- **What:** CI and the `justfile` between them cover four shapes: `--no-default-features`, `+gui,actuator`, `+pcap-backend`, and the default set (`pcap-backend,gui,actuator`, which is also `--all-features` since WinDivert was removed). Untested: `gui` alone, `actuator` alone, `pcap-backend,gui`, `pcap-backend,actuator`.

  I built all four. Every one compiles clean (`cargo check --locked --no-default-features --features <combo> --all-targets`), so this is a coverage gap, not a break. The gap is real rather than nominal because the three features gate overlapping code in overlapping files: `src/main.rs:6` and `:237`/`:256` on `gui`, `src/journal.rs:63` on `not(gui)`, `src/app/mod.rs:368`/`:402` on `all(windows, actuator)`, `src/app/mod.rs:1101`/`:1112` on `all(windows, pcap-backend)`. A `gui`-only build is a genuinely distinct `cfg` shape from `gui,actuator`.
- **Why it matters here — and the honest counter-argument:** Nobody ships `--features gui` alone, and the `Cargo.toml` and `justfile` comments are explicit and correct that the meaningful arms are "no backend" and "the backend", plus "the combination actually shipped". That reasoning holds. What it does not cover is a *future* edit adding a `cfg(feature = "gui")` block inside an `all(windows, feature = "actuator")` region: the four tested lanes would still pass while `--features gui` broke, and nobody would find out until someone typed it.

  On strict additivity itself: the features **are** additive in the sense the rule enforces — each adds either an optional dependency (`pcap-backend` → `dep:libloading`, `gui` → `dep:eframe`) or `cfg`-gated code, none is mutually exclusive, and no `compile_error!` guard is needed or missing. The `#[cfg(not(feature = ...))]` arms are all fallback stubs (`build_source` returning an error, `actuator_mode` returning `Mode::Off`, the console `run_mode`), which is the exact pattern the rule's own Good example uses. The one wrinkle worth naming out loud: enabling `gui` *subtracts* the console (`windows_subsystem = "windows"` at `src/main.rs:6`, and `journal.rs:63`'s `println!` mirror disappearing), which is the non-additive shape in the abstract. It cannot hurt anything here — `publish = false`, no dependents, and the feature is a build-lane selector for a single binary rather than something Cargo could unify from a third crate. Recording it so nobody "fixes" it into an inverted `console` feature.
- **Fix:** Either add two lanes to the `justfile`'s `clippy` recipe and the CI job —

  ```
  cargo clippy --locked --no-default-features --features gui --all-targets -- -D warnings
  cargo clippy --locked --no-default-features --features actuator --all-targets -- -D warnings
  ```

  — or, better, replace the hand-maintained list with one `cargo hack --feature-powerset --depth 2 check --all-targets` job that cannot drift as features change. With three features the powerset is eight builds; at that size it is cheaper than the four hand-written lanes it replaces.
- **Effort:** trivial

### proj-012 — `src/actuator/plan.rs` mixes design-space geometry, timing configuration and job construction

- **Severity:** P3
- **Rule:** [`proj-mod-by-feature`](../../.claude/skills/rust-skills/rules/proj-mod-by-feature.md)
- **Site:** `src/actuator/plan.rs` (1024 lines; 560 production, 464 tests)
- **What:** Three groups, each self-contained:
  - **Geometry** (9–155): `DESIGN_W/H`, `MAX_ASPECT`, `Anchor`, `Zone`, `REFRESH`, `CONFIRM_REFRESH`, `CONFIRM_BUY`, `SCROLL_ZONE`, `row_for_slot`, `buy_zone`, `DesignPoint`, `ClientRect`, `to_screen`. ~145 lines, pure.
  - **Timings** (16–30, 157–381): the eight `WAIT_*_MS` baselines, `Trigger`, `DelayRange`, `Timings`, `TimingPreset`. ~230 lines. This is the group `config.rs`, `ui/editor/` and `ui/editor/timing_meter.rs` all import.
  - **Job construction** (383–559): `DELAY_SEED_SALT`, `Input`, `TimedStep`, `Job`, `Jitter`, `click`, `scroll`, `confirm_retry_job`, `refresh_job`, `buy_job`. ~175 lines.
- **Why it matters here:** Lower value than the other split findings, and stated as such: 560 production lines is not a crisis and the three groups are cleanly ordered already. The one concrete argument for doing it is that `Timings`/`DelayRange` are the only part of this module that is genuinely public (see `proj-010`), so isolating them in `plan/timings.rs` makes the public/internal boundary a file boundary instead of a per-item annotation. `ui/editor/timing_meter.rs` also `const _: () = assert!(...)`s against seven of the eight `WAIT_*_MS` constants, so those constants have a real cross-module contract worth naming a file after.
- **Fix:** `src/actuator/plan.rs` → `src/actuator/plan/{mod,geometry,timings,jobs}.rs` with `mod.rs` re-exporting. Do it only when `proj-004` or `proj-010` already has the file open; it does not earn its own change on its own.
- **Effort:** small

### proj-013 — `build.rs` uses the legacy `cargo:` directive prefix rather than `cargo::`

- **Severity:** P3
- **Rule:** [`proj-build-rs-minimal`](../../.claude/skills/rust-skills/rules/proj-build-rs-minimal.md)
- **Site:** `build.rs:41,68,69–71`
- **What:** `println!("cargo:rerun-if-changed=build.rs")` and the two `cargo:rustc-link-arg-bins=...` lines use the single-colon form. The double-colon form (`cargo::rerun-if-changed=`, `cargo::rustc-link-arg-bins=`) has been the documented spelling since Cargo 1.77 and is what the rule's Good example shows; the crate's `rust-version = "1.92"` is far past that.
- **Why it matters here:** Barely — both forms work and will keep working, and Cargo emits no warning for the old one. Filing it only because it is the single deviation in an otherwise exemplary build script and it is a two-character edit per line. Batch it with any other `build.rs` change.
- **Fix:** `cargo:` → `cargo::` on all four directives.
- **Effort:** trivial

## Clean areas

**Build script (`proj-build-rs-minimal`)** — `build.rs` is 72 lines of which ~30 are code and the rest is the measured UIPI/STOVE rationale. It emits `rerun-if-changed=build.rs` (line 41), which is complete: the only other input is `CARGO_CFG_TARGET_ENV`, and Cargo keys the build-script fingerprint on the target itself, so no `rerun-if-env-changed` is needed. Fully deterministic — no timestamps, no UUIDs, no network, no `rustc --version` parsing, no codegen. Idempotent: it only prints. It writes nothing at all, so the "avoid writing outside `OUT_DIR`" guidance is trivially satisfied. The `println!` at 68 is also *deliberately* unconditional (its comment explains that a feature-gated manifest would make dev and shipped builds differ in exactly the property the design turns on) — do not "fix" that into a `cfg`.

**MSRV declaration (`proj-msrv-declare`)** — `rust-version = "1.92"` in `Cargo.toml:5`, `channel = "1.92.0"` in `rust-toolchain.toml` so local builds cannot drift above it, `1.92.0` in the CI matrix across five clippy lanes and four test lanes, and the `dependency-policy` job pinned to `dtolnay/rust-toolchain@1.92.0`. Edition 2024 brings resolver 3, so MSRV-aware dependency resolution is active without an explicit `[workspace] resolver`. The declaration and its enforcement are both right; `proj-002` is about the *other* matrix arm, not this one.

**Binary layout (`proj-bin-dir`)** — one binary from `src/main.rs`, no `src/bin/`, no `[[bin]]` sections, no `default-run`. Exactly the shape the rule prescribes for a single-binary crate. The one `[[example]]` entry (`Cargo.toml:102–104`) correctly carries `required-features = ["gui"]` so `cargo build --examples` on the console lane does not fail.

**`pub(super)` for module-tree-local helpers (`proj-pub-super-parent`)** — used deliberately and in the right places: `src/ui/theme.rs` (21 items — the whole palette and every widget helper), `src/domain/control/watchdog.rs` (5), `src/ui/editor/timing_meter.rs` (3), `src/domain/control/dedup.rs` (2), `src/actuator/shield.rs` (2), and one each in `ui/{shop,statusbar,journal}.rs`. Every one is genuinely parent-tree-local, and none of them could be `pub(super)` if the author had reached for `pub(crate)` reflexively. `SlotIdentity` in `dedup.rs` is the sharpest example — an identity type only `control/` may construct.

**Selective re-export over deep paths (`proj-pub-use-reexport`)** — `src/capture/mod.rs:3,16` keeps `mod ip` private and re-exports only `parse_segment`; `:9–19` does the same for `mod pcap` → `PcapSource`; `src/uplink/mod.rs:4,8` for `mod websocket` → `run`; `src/lib.rs:24` keeps `mod render` private entirely, with all 13 of its items `pub(crate)` and two of them additionally `#[cfg(feature = "gui")]`-gated because only the window renders them. `src/lib.rs:31–32` flattens `Config`, `Error` and `Result` to the root while leaving `config::persist` reachable by path, which is exactly what `main.rs` needs.

**Feature additivity (`proj-feature-additive`)** — each feature adds rather than replaces: `pcap-backend = ["dep:libloading"]`, `gui = ["dep:eframe"]`, `actuator = []` gating code only. `dep:` syntax is used correctly so no optional dependency leaks into the feature namespace. Nothing is mutually exclusive, so no `compile_error!` guard is missing. `windows-sys` is deliberately *not* optional (`Cargo.toml:70–73` records why: three features each turning it on with `dep:windows-sys` is how a lane could end up without the `migrate` DACL fix), which is a correct read of feature unification. All eight combinations compile — verified.

**Small-project structure (`proj-flat-small`)** — 40 source files puts this squarely in the rule's "20+ files: feature folders with submodules" band, and that is what it is. Two things that look like over-structure on a directory listing are not: `src/domain/mod.rs` is 7 lines of `pub mod` because it fronts `control/` (4 files, 2694 lines), `filter.rs` (514) and `shop.rs` (228) — the rule's own "Hybrid Approach" for a feature that grew a sub-feature. `src/uplink/mod.rs` is 21 lines but holds real content (`UplinkEvent`), not just re-exports. No directory in the crate holds one file, and no `mod.rs` is a pure re-export shim.

**Feature organisation (`proj-mod-by-feature`)** — the top level is `capture` / `stream` / `domain` / `uplink` / `actuator` / `app` / `ui` / `config`, i.e. named after the pipeline stages the crate doc diagram draws. There is no `models/`, `handlers/` or `services/` anywhere. `src/render.rs` is the only module named after a *kind* of code rather than a feature, and it earns it: its whole purpose is that the console and the window cannot disagree on wording, which is a cross-cutting concern by construction.

**Dedicated `tests.rs` submodules where they exist** — `src/app/session/tests.rs` (1423) and `src/domain/control/tests.rs` (1781). The technique is right; `proj-004` is only about the five files that did not adopt it.

**`impl` blocks split across submodules** — `src/domain/control/watchdog.rs:75` puts `impl Controller { watchdog, recovery_buy_targets, on_link_up }` in its own file while `Controller` is defined in `mod.rs`. That is precisely the technique `proj-006` and `proj-007` need, already in use in this crate, which makes those two splits lower-risk than they would otherwise be.

## Not applicable

- **`proj-prelude-module`** — deliberately absent, and should stay absent. A prelude serves library consumers writing many imports; this crate has exactly two consumers (`src/main.rs`, `examples/ui_preview.rs`), both in-repo, both with fully explicit import lists totalling 9 lines. The rule's own "Be conservative" and "Removing items is a breaking change" guidance argues against adding one to a `publish = false` binary. Filing nothing.
- **`proj-workspace-large`** — no `[workspace]` table; single package, ~22k lines. A split is not justified and I would argue against one. The rule's own table says "Single binary/library → no workspace needed", and the dependency graph here is a straight line: `capture` → `stream` → `uplink` → `domain` → `app` → `ui`, with `config` and `journal` as leaves. There are no independently versionable pieces, no second binary, no plugin boundary, and no compile-time problem to solve — a clean `cargo check --all-targets` on this crate takes ~6 s. The one thing a workspace would buy is enforcing the layering that `proj-010` proposes to enforce with four `mod` keywords instead, at a fraction of the cost. Leaning conservative: single crate is correct here.
- **`proj-workspace-deps`** — no workspace, so no member crates and no version drift to inherit away. Vacuously satisfied.
