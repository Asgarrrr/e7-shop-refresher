# Tech-debt audit — `arkyve-refresh-shop`

Full review of every `.rs` file in the crate (40 files, 22 757 lines, plus `Cargo.toml`,
`build.rs`, `.github/`, `justfile` and the tracked `docs/`) against the 265 rules of the
`rust-skills` skill, one Opus reviewer per rule category, 26 categories.

Date: 2026-08-18 · Branch: `rewrite/network-capture` · Reviewed at `97e8807`.

| | |
|---|---|
| **Findings** | **185** — P0 **1** · P1 **27** · P2 **82** · P3 **75** |
| Reports | 26, one per category (`01-own.md` … `26-anti.md`), 10 000 lines |
| Shared brief | [`_BRIEF.md`](_BRIEF.md) — severity scale, output contract, project calibration |

## Implementation status

**The audit has been largely implemented.** The findings below are the record of what was
*found*; this section is what has since been *fixed*. Read
[`_HANDOFF.md`](_HANDOFF.md) for the live ledger of what remains.

| | |
|---|---|
| Commits | 17, on `rewrite/network-capture`, from `def899b` to `320c20d` |
| Tests | **516 → 589 passing**, 0 failed (+73, none weakened or removed) |
| Test lanes | 589 / 465 / 532 / 478 across the four feature combinations, all 0 failed |
| Gates | clippy **0 diagnostics on all six CI lanes** · `cargo fmt --check` clean · `cargo doc --document-private-items` **0 warnings** · `cargo deny check bans` ok |
| P0 | **fixed and now unrepresentable** — `uplink::run` takes `ServerUrl`, whose `Display` redacts |
| P1 | **26 of 27 fixed.** `api-005` declined on a design argument — see below |
| Ledger | [`_HANDOFF.md`](_HANDOFF.md): **59 resolved, 13 open** — the file splits, two design decisions needing their own review, and one newly filed follow-up |

New enforcement that did not exist before, so this cannot silently regress: a `[lints]`
table in `Cargo.toml` (`correctness`/`suspicious` at **deny**), `undocumented_unsafe_blocks`,
`unreachable_pub`, `multiple_unsafe_ops_per_block`, `redundant_clone`,
`#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::panic))]` at both crate roots, a
rustdoc gate in CI and `just verify`, and `overflow-checks = true` in release. Every entry
was measured at zero sites before being enabled.

**`proj-002` deserves separate mention**: `rust-toolchain.toml` was overriding the CI matrix,
so the `stable` arm was a second MSRV arm and **nothing had ever been built on current
stable**. That is fixed, which means the other 13 commits are the first work in this repo
verified on both toolchains.

### Where the audit was wrong

Recorded because the audit's own numbers were the reason several findings looked cheap, and
because a reviewer should know which of its claims were tested:

- **`api-001`** had **111** dropped-result sites, not the ~7 the report estimated.
  **`own-003`** touched 11 sites, not 6. **`api-007`** 6, not 3.
- **`err-008`**'s proposed fix does not work: `{err:#}` does not walk a source chain for a
  `thiserror` enum. A real `Error::report()` was written instead.
- **`conc-001`**'s offered "minimal fix" (`SeqCst` on four stores) is **insufficient** — built
  and measured, the new multi-threaded test still failed 3/3 against it. Only the single-atomic
  version makes the bad state unrepresentable.
- **`mem-003`** predicted 40 bytes for the boxed `Error`; it is **48** on Windows, because
  `PathBuf` is 32 there (`OsString` wraps a `Wtf8Buf` with an `is_known_utf8` flag).
- **`num-002`**'s premise was inverted: release sets `overflow-checks`, so the bare `-=` would
  have **panicked**, not wrapped silently.
- The brief given to the reviewers had three wrong premises of its own, corrected in-report:
  `crash.rs`/`render.rs` contain no `unsafe`, the predicted `perf-io-buffering` findings do not
  exist, and the UI runs at **4 Hz** at rest, not 60.

### Declined, with reasons

- **`api-005`** (P2) — the newtype would *add* a state rather than remove one. The GUI must be
  able to hold an unrestricted draft (that is why it boots where the console fails fast), so
  `Controller` would carry both a draft and an armed filter and every reader would pick a side.
  The rule is not duplicated; only the reaction differs by build, deliberately.
- **`lint-008`** — resolved by measurement: `redundant_pub_crate`'s only suggestion is `pub`,
  and applying it makes all 13 items fire under `unreachable_pub` instead. For an item in a
  private module the two lints have no common fixed point.
- **`type-003`'s domain half** — `src/domain/` and `src/actuator/` have zero dependency on each
  other today; a shared `Slot` forces a new architectural edge in one direction or the other.
  That is a design decision, not a mechanical fix.
- **All file splits** (`proj-001`/`004`/`005`/`006`/`009`) — excluded from this run by decision,
  so that behavioural fixes stayed reviewable. `app/mod.rs`'s named six-way plan remains valid
  in [`24-proj.md`](24-proj.md) for a pass of its own.

## Read this first

**This crate is in good shape.** That is not politeness, it is the audit's most
load-bearing measurement, and it changes how you should read everything below:

- `cargo clippy --all-targets --all-features`, the default lane, and
  `--no-default-features --all-targets` each emit **exactly 0 diagnostics**.
  `cargo fmt --check` passes. Verified independently by four reviewers.
- **Zero `#[allow(...)]`** in the entire crate. One suppression, an exemplary
  `#[expect(..., reason = "…")]`.
- **Zero `.unwrap()`** outside tests (all 253 are `#[cfg(test)]`). 13 `expect()` in the
  shipped binary, every one on a genuine invariant — not on user input, a file, a
  socket, or an FFI return.
- **No unsoundness.** All ~35 `unsafe` blocks carry `// SAFETY:` comments that were
  *verified true*, not merely present: the 13 `wpcap.dll` signatures match libpcap's
  ABI including pointer mutability, `PcapPktHdr` is correctly 16 bytes for Windows'
  two-`long` `timeval`, the `pcap_next_ex` buffer is copied before invalidation.
- No `std` lock guard crosses an `.await`; all six `select!` sites are cancellation-safe
  with the two accumulating futures pinned outside the macro.
- 518 tests across 4 CI feature lanes. All 8 feature combinations compile.

**Consequence: almost nothing in this audit is machine-detectable.** Three findings in
185 could have been caught by a tool (`lint-001` rustdoc, `proj-013` `unreachable_pub`,
`pat-`'s five clippy-visible wildcards). Everything else required reading the code. That
is the justification for the audit — and the reason the P2/P3 tail is worth keeping
rather than discarding.

## The one P0

**`obs-001` — the un-redacted `server_url` is written to the log we ask players to send us.**
`src/uplink/websocket.rs:112,129`. Userinfo and query included. Three things make this
worse than a stray log line:

1. `app::redacted_server_url` **already exists** and is used correctly at
   `app/mod.rs:412` — this is two sites missing the helper, not a design gap.
2. `README.md:235` promises the opposite.
3. It is the support channel, so the leak travels to us by construction.

Cheap, isolated fix. Do it first.

Worth stating plainly, because it was the pre-audit worry: **no packet payload, hex dump,
or token is ever logged** — only lengths. That part is clean.

## The 27 P1s, ranked by real consequence

Category priority is not severity. This ranking is by what a user or an operator
actually loses, deduplicated where two reviewers found the same defect.

### Silently does the wrong thing

| # | Finding | Where | Consequence |
|---|---|---|---|
| 1 | `anti-001` | `actuator/win.rs:52-63` | `SetProcessDpiAwarenessContext`'s return dropped by a bare semicolon. The comment conflates *"already set"* with *"set to what we want"*. In the shipped GUI build winit sets it first so the call **always** fails — the entire click-coordinate chain rests on winit's undocumented choice. A DPI-unaware or system-aware process makes **every click land off-target, silently**, since `SendInput`/`PostMessageW` both report success. |
| 2 | `conc-001` | `src/watch.rs` | `WatchGate`'s halt/re-arm handshake spans two atomics but only one is `SeqCst`, so **a safety halt can be silently re-armed**: the executor keeps clicking and capture keeps forwarding after the player pressed Stop. Self-heals within one `biased` select iteration (hence P1, not P0). All five `watch.rs` tests are single-threaded, so none can catch it. This is the crate's *only* safety cutoff. |
| 3 | `async-001` | `uplink/websocket.rs` | `connect_async` awaited with **no timeout**. A half-open handshake wedges the uplink forever, with no `LinkDown` journal line and no watchdog escalation (the controller starts `link_up: true` and never arms an expectation). The relay **looks armed and forwards nothing**. |
| 4 | `serde-001` + `test-001` | `uplink/protocol.rs:10-23` | The load-bearing `"shop"` wire discriminator is pinned by **no fixture** — only `"purchase"` has one; all ~150 tests build `ShopSnapshot` as a Rust literal. `#[serde(other)]` makes a tag mismatch *silent*. A server-side shape drift makes the app **permanently, silently blind with green CI**. Two reviewers, two routes, one hole. Fix is two serde tests. |
| 5 | `type-003` | `actuator/plan.rs` | 1-based display slot and 0-based click row are both bare `u8`, hand-converted in both directions. A swap clicks the wrong item's Buy button, **spends real gold**, then wedges the pause. |
| 6 | `type-001` (= `api-003` P2) | `plan::*_job` ×3 | `epoch: u64` and `seed: u64` are **adjacent parameters** in all three builders. A transposition compiles and makes the executor drop *every* click as "the shop changed" — blaming the wrong subsystem in the journal. Two reviewers, independently. |
| 7 | `type-002` | `actuator/win.rs` | Window handles travel as bare `isize` beside an `isize` LPARAM in `post(hwnd, msg, wparam, lparam)`; `pack_point` returns exactly the handle's type. |

Items 5–7 are one fix: three newtypes (`Epoch`, `Slot`/`Row`, `Hwnd`) close all three at
zero runtime cost. `#[repr(transparent)] Hwnd(isize)` also makes the existing `as HWND`
casts' implicit layout assumption explicit.

### Loses the diagnostic — in an app whose only channel is a log file

The shipped build is windowed with no console: stdout is an inert sink, logs go to
`%LOCALAPPDATA%`, and `crash.rs` is the product's only post-mortem channel. Every entry
here converts a diagnosable failure into an unreproducible bug report.

| # | Finding | Where | Consequence |
|---|---|---|---|
| 8 | `obs-002` = `anti-004` | `main.rs:83` | An unwritable log directory **silently degrades to an inert sink and discards the reason**. The windowed build then has no console *and* no file. `crash.rs` already solves this correctly with a two-candidate path list; `migrate::Leftovers` already shows the deferred-report pattern. Two reviewers, same line. |
| 9 | `obs-003` | journal targets | The product's two worst events — actuator halt, session abort — are `info!(target:"journal")` with the target stripped. **The `RUST_LOG=warn` the README suggests deletes them.** |
| 10 | `obs-005` | `crash.rs` | A panic leaves **no marker** in the rotated log; `crash.log` and `logs\` are disjoint with no cross-reference. |
| 11 | `obs-004` | `main.rs:336` | A window that fails to open reports only to an inert stderr, leaving a log that stops after "starting". |
| 12 | `obs-006` | GUI error paths | Two paths use `journal.push` instead of `emit`, so `config.toml not saved: …` **dies with the process**. |
| 13 | `anti-002` | `domain/shop.rs:49,64` | Tolerant `.ok()` discards the serde error. A mistyped `limit` makes a sold-out slot read buyable → actuator clicks Buy → no echo → watchdog halts `Unresponsive` **blaming the game**. The tolerance is right; the missing log line is the defect. `capture/ip.rs:21` does the same thing *and counts it*. |
| 14 | `anti-003` | `main.rs:44,46` | Config-seed failure is silent although logging is installed 15 lines earlier; the console build then dies naming a `config.toml` that was never written. |
| 15 | `err-002` | `app/session/mod.rs` ×5, `actuator/mod.rs` ×2 | Mutex-poisoning panics **against the crate's own documented policy** — seven other lock sites deliberately tolerate poison, each with written rationale. |

### Stale or unenforced — the documentation and CI lanes

| # | Finding | Where | Consequence |
|---|---|---|---|
| 16 | `proj-002` | `rust-toolchain.toml` | **`channel = "1.92.0"` overrides the CI matrix.** `dtolnay/rust-toolchain` only calls `rustup default`, which rustup ranks *below* the toolchain file (verified against `action.yml` and the rustup docs). The `stable` arm is a second MSRV arm — **nothing has ever been built on current stable.** One-line fix: job-level `env: RUSTUP_TOOLCHAIN: ${{ matrix.toolchain }}`. |
| 17 | `doc-001` | `README.md:22`, `docs/capture-backend-choice.md:12` | Both quote a BPF filter the code does not use: `tcp and port 3333` vs the actual `tcp and src port {game_port}`. The missing `src` is **exactly the direction distinction `f6b3ce6` made load-bearing**, and it is the string that substantiates the README's privacy claim. Both in-code copies are correct; only the docs drifted. |
| 18 | `lint-001` | rustdoc lane | `cargo doc --no-deps` emits 7 warnings including 2 genuinely dead links, and **nothing in CI or the justfile runs `cargo doc` at all**. |
| 19 | `doc-002` | `stream.rs:549` | `push_budgeted` documents a `Vec<u8>` return replaced by `ReassemblyOutcome`, and **never mentions `Pressure`** — the outcome that has already cleared every flow's state and requires the caller to re-anchor. |
| 20 | `doc-003` | `stream.rs:465` | `InitialBurst::push` panics on a caller precondition documented only by its own assertion message. The crate has zero `# Panics` sections. |

Note the pattern in 17 and the P0: **two independent reviewers found a README privacy
claim the code does not honour.** Those claims are verified by nothing.

### Structure and cost

| # | Finding | Where | Consequence |
|---|---|---|---|
| 21 | `mem-001` = `own-001` | `pcap.rs:963` → `ip.rs:66` | Every captured packet is copied **twice**: frame into a channel `Vec` (mandatory), then payload into a second `Vec` (avoidable — it copies a subslice of the first *before* it is dropped). Measured: a 48 870-byte coalesced frame, `SNAPLEN = 262_144`. The duplicate is also **the buffer `PipelineBudget` cannot see**. Caveat for the fix: `admit_capture` charges `capacity()`, which an in-place trim changes. |
| 22 | `api-001` | `Controller::handle` | Returns the decision the product acts on; dropping it is silent, and it is **not** `#[must_use]`. `clippy::must_use_candidate` structurally cannot flag it (`&mut self`) — 41 candidates reported, `handle` not among them. |
| 23 | `proj-001` | `app/mod.rs` | 2266 lines, of which **1118 are production code**, joining five independent concerns held together only by mpsc channels — which makes the split cheap. Named 6-file plan in the report. Both the `macro-` 4× dispatch duplication and the `trait-` backend arms fall inside single runs. |
| 24 | `proj-003` | `main.rs` | ~200 lines of startup policy stranded where tests cannot reach it. Evidence it already hurt: `config.rs:957` hand-reimplements `seed_config_if_missing` because it is unreachable. |
| 25 | `test-002` | `uplink/websocket.rs` | `pump`'s inbound arm and `forward()` are **unreachable from every test** — the only fake link returns `Poll::Pending` forever, so the `Text`/`Binary`/`Close`/`Err` arms never execute. |
| 26 | `unsafe-001` | `capture/pcap.rs:300` | `cstr` is a **safe** fn whose SAFETY comment states a raw-pointer obligation its signature cannot bind (7 call sites; two don't need raw pointers at all). Split into `unsafe fn cstr` + a genuinely safe `errbuf_text`. |
| 27 | `type-...` / `obs-...` | — | Remaining P1s are folded into the groups above. |

## Cross-cutting themes

Four reviewers or more converged on each of these independently. They are the real
output of the audit — more useful than any individual finding.

### 1. An invariant that is real, known, and documented in prose — but enforced weakly

The crate's dominant debt pattern. It *knows* its invariants; it writes them in
comments instead of encoding them where the compiler or a build step checks them.

- `const-001` = `mem-007` — `PcapPktHdr`'s layout, which the module's own comment calls
  **"the single most dangerous constant in this file"**, is guarded only by a runtime
  `#[test]` on a module gated to Windows + `pcap-backend`. A release build on that lane
  ships a wrong layout **without ever evaluating the guard**. Two-line fix; the crate
  already does `const _: () = assert!(…)` three times elsewhere.
- `unsafe-001` — pointer precondition in a comment the signature cannot bind.
- `trait-001` — `Send` requirement in prose at `actuator/win.rs:565` instead of a
  supertrait the compiler checks at the `impl`.
- `num-002` — bare `-=` on `pending_bytes`, the per-stream twin of the counter whose
  `release()` spends **nine comment lines and two profile-split tests** arguing that
  bare subtraction is unacceptable.
- `const-003` — the "six clickable rows, top group is 0..=3" rule as bare `<= 5` / `> 3`
  literals at three sites, with no test that fails if only one is edited.

**Recommendation:** prefer `const _: () = assert!(…)`, a newtype, or a supertrait over a
comment, every time. The crate already invented this practice — apply it consistently.

### 2. An invariant proven at the boundary, then thrown away

Parse-don't-validate, applied *inward*. **This class has already shipped a bug once**
(`kinds = ["unknown"]`, fixed by deleting a checkbox), which is why it leads the
recommendations rather than sitting in the P2 tail.

- `api-002` — `Config::validate` proves "wss:// or loopback only", then returns
  `server_url` as a bare `String` that **two hand-rolled authority parsers re-inspect**.
- `num-001` — the Apply dirty-check is a float `!=` in disguise (derived `PartialEq` over
  `Option<f64>`); `validate()` never rejects a non-finite `min` and **TOML 1.0 accepts
  `nan`**. Reproduced against the crate's real deps: `f == f.clone()` is `false`, so Apply
  stays lit forever and the filter can never match while `is_unrestricted()` reports it
  restricted. `clippy::float_cmp` cannot see this.
- `type-010` — config invariants enforced only by a private `validate()` that just the
  disk path runs.
- `serde-004` — `min_grade`/`min_substats` accept out-of-domain values that silently match
  nothing — the exact failure `kinds` *is* checked for.
- `conv-001` / `api-004` / `api-005` — the same invariant re-derived at 3, 3 and 5 sites.
- `coll-`'s escalation: **`ShopSnapshot::slots` is wire-deserialized with no length cap**,
  unlike every other wire-fed collection here (`MAX_STREAMS`, `MAX_PENDING_BYTES`,
  `JOURNAL_CAP`, `INITIAL_ANCHOR_MAX_*`). The fix is a cap, *not* a collection swap.

### 3. A swallowed error turns a diagnosable failure into the wrong accusation

See P1s 8–15. The through-line: when the app loses an error, it does not fail — it
**blames another subsystem**. `anti-002` is the cleanest example (a mistyped `limit`
ends with the watchdog halting `Unresponsive` and blaming the game).

Credit where due: of 26 `let _ =` sites judged individually, **23 are deliberately
correct**. This is a small number of specific holes, not a habit.

### 4. Per-frame deep copies in the GUI — one fix, four symptoms

`own-002` (`ui/view.rs:53-89` deep-copies the snapshot per frame), `name-001`
(`EventLog::entries()` deep-clones 500 journal lines behind a free-getter name),
`mem-006` (`format_item` = 6× `push_str(&format!)` per slot per frame), `mem-008`
(journal snapshot clones up to 500 strings per added line).

Two corrections from the `perf-` reviewer that shrink this: the app runs at **4 Hz at
rest** (`request_repaint_after(250ms)`), not 60 Hz, rising to display rate only while the
pointer is over the window. And the real cost is **lock contention with the session
loop**, not CPU — the tooltip formatting happens under the controller lock and at most
one row reads it. The correct generation-gated cache already exists in the same file.

## Do NOT do these

Reviewers were asked to record what is already right, so a later pass does not "fix" it.
This list is the highest-value part of the audit for whoever implements.

**Build and CI**
- **No Miri job.** Every unsafe op is a foreign call Miri cannot execute; the only FFI
  test is `#[ignore]`d.
- **No `panic = "abort"`.** Breaks five unwind-dependent sites: the supervised-worker →
  "task panicked" → banner design, capture-thread join detection, `PipelineBudget::release`'s
  drop-during-unwind contract, and two `#[should_panic]` tests.
- **No `strip = true`.** Reduces every `crash.log` backtrace to bare addresses.
  `strip = "debuginfo"` is the deliberate compromise.
- **No `clippy::cargo`.** Its only output here is 29 duplicate transitive crates from the
  eframe tree; `deny.toml` + `cargo-deny check bans` covers that better.
- **No whole-group `pedantic` (161) or `nursery` (121).** Per-lint counts and rationales
  are in `25-lint.md`.
- **`unwrap_used`/`panic`/`todo`/`unimplemented`/`dbg_macro` cannot go in a `[lints]`
  table** — it applies to the test harness too, where 257 unwraps and 19 panics live.
  They need `#![cfg_attr(not(test), warn(...))]`. All measure **0 sites** in `--lib --bins`,
  so they are free ratchets *if* applied that way.
- **No workspace split** at 22k lines. Argued against in `24-proj.md`.
- **No `ahash`/FxHash** on the per-packet map: it is keyed by network-derived
  `SocketAddr`s and the module documents the forged-source-port threat.

**Code**
- `watchdog.rs:103` **is** clippy-flagged but is a correct identity pass-through — filed as
  `pat-005` specifically so the ugly suggestion is not applied there.
- The `S: Surface` bound on `SurfaceJobGuard` looks like the anti-pattern but is required
  by its `Drop` impl.
- The explicit `'a` on `LinkStrip::ip_bytes` — elision would tie the output to the wrong
  borrow.
- The by-value `tx`/`Handle` params clippy flags as needless: those moves are what close
  the pipeline in producer order and keep the `pcap_t` on its owning thread.
- The six `u64` monotonic packet counters, and the documented casts (`seq_diff as i32`,
  `expected_seq`'s modular truncation, the `ERROR_* as i32` Win32 boundary).
- The `ItemKind::Unknown` checkbox must **stay** removed.
- `docs/initial-stream-anchor.md`'s amendment header is accurate and complete.
- `mem-boxed-slice`, `const-generics`, `conv-asmut-mutable`, `closure-impl-fn-return`,
  `coll-*`'s three suspects, `perf-io-buffering` — each audited and filed
  **not applicable with reasons**. Do not revisit.

## Suggested order

1. **`obs-001`** (P0) — redact the URL. Isolated, minutes.
2. **`proj-002`** — one `env:` line, and it un-blinds the whole CI matrix. Do it before
   anything else, so the rest is verified on real stable.
3. **`serde-001` + `test-001`** — two serde fixtures. Closes the "silently blind with green
   CI" hole and is the highest ratio of risk removed to effort in the audit.
4. **`conc-001`** — the safety cutoff. Collapse both fields into one `AtomicU8` (which also
   makes everything `Relaxed`); minimal fix is `SeqCst` on the four `enabled` stores.
5. **`anti-001`** — assert or log the DPI result, and stop depending on winit's undocumented
   choice.
6. **`async-001`** — wrap `connect_async` in a timeout.
7. **Diagnostic batch** — P1s 8–15 together; they are one theme and share reviewers'
   suggested patterns (`crash.rs`'s two-candidate path list, `migrate::Leftovers`' deferred
   report, `capture/ip.rs:21`'s count-and-continue).
8. **Three newtypes** (`Epoch`, `Slot`/`Row`, `Hwnd`) — closes `type-001/002/003` + `api-003`.
9. **Doc corrections** — `doc-001/002/003`, plus gate `cargo doc` in CI (`lint-001`) so they
   cannot drift again.
10. **`proj-001`** decomposition — do it last; it moves the most code and every earlier fix
    is easier to review before the split.

Items 1–6 are each small and independently shippable. Item 10 is the only large one.

## Index

| Report | Category | Priority | P0 | P1 | Verdict in one line |
|---|---|---|---|---|---|
| [01-own](01-own.md) | Ownership & Borrowing | CRITICAL | 0 | 0 | Clean; `&Vec`/`&String` params nil, lock types all judged right, one double-copy. |
| [02-err](02-err.md) | Error Handling | CRITICAL | 0 | 1 | Zero `unwrap()` shipped; only gap is mutex-poison panics against own policy. |
| [03-mem](03-mem.md) | Memory Optimization | CRITICAL | 0 | 1 | Better than CRITICAL implies; the double packet copy is the one real cost. |
| [04-unsafe](04-unsafe.md) | Unsafe Code | CRITICAL | 0 | 1 | No unsoundness; ABI signatures verified, not assumed. |
| [05-api](05-api.md) | API Design | HIGH | 0 | 1 | `#[must_use]` used deliberately in 11 places; `handle` is the miss. |
| [06-async](06-async.md) | Async/Await | HIGH | 0 | 1 | Lock-across-await and cancel-safety provably clean; uplink lacks a connect timeout. |
| [07-conc](07-conc.md) | Concurrency | HIGH | 0 | 1 | Deliberate and near-correct; the safety gate's atomics are the exception. |
| [08-opt](08-opt.md) | Compiler Optimization | HIGH | 0 | 0 | Profile already right; nothing is CPU-bound. 3×P3. |
| [09-num](09-num.md) | Numeric Safety | HIGH | 0 | 0 | Unusually good; every hostile-value site already explicit. |
| [10-type](10-type.md) | Type Safety | MEDIUM | 0 | 3 | Exemplary enums, **zero newtypes over primitives** — three swappable-argument holes. |
| [11-trait](11-trait.md) | Trait & Generics | MEDIUM | 0 | 0 | 4 traits, all dyn-compatible, none speculative. |
| [12-conv](12-conv.md) | Conversions | MEDIUM | 0 | 0 | No `From`/`TryFrom`/`FromStr` at all; defensible for a binary. |
| [13-const](13-const.md) | Const & Compile-Time | MEDIUM | 0 | 0 | Crate already invented this category's best practice. |
| [14-serde](14-serde.md) | Serde | MEDIUM | 0 | 1 | Config surface exemplary; the 84-line wire surface is the weak one. |
| [15-pat](15-pat.md) | Pattern Matching | MEDIUM | 0 | 0 | 74 `matches!`, zero lint hits; one catch-all clippy cannot see. |
| [16-macro](16-macro.md) | Macros | MEDIUM | 0 | 0 | One `macro_rules!` in 22k lines, and it is justified. |
| [17-closure](17-closure.md) | Closures | MEDIUM | 0 | 0 | Near-clean; all 6 `Fn` bounds already weakest-that-works. |
| [18-coll](18-coll.md) | Collections | MEDIUM | 0 | 0 | Clean and provably so; every scan is ≤ 8 elements. |
| [19-name](19-name.md) | Naming | MEDIUM | 0 | 0 | Close to clean; two names that lie about cost. |
| [20-test](20-test.md) | Testing | MEDIUM | 0 | 2 | Best-tested crate in the repo; the inbound wire path is the hole. |
| [21-doc](21-doc.md) | Documentation | MEDIUM | 0 | 3 | Well above average, so the value is specific wrongness — a stale BPF filter. |
| [22-obs](22-obs.md) | Observability | MEDIUM | **1** | 5 | Setup better than average; what reaches the file is wrong. |
| [23-perf](23-perf.md) | Performance | MEDIUM | 0 | 0 | Iterator/API half already honoured; the finding is a measurement gap. |
| [24-proj](24-proj.md) | Project Structure | LOW | 0 | 3 | Healthier than file sizes suggest; the CI toolchain override is the real bug. |
| [25-lint](25-lint.md) | Clippy & Linting | LOW | 0 | 1 | Clean with hard numbers; the enforced set is written down nowhere. |
| [26-anti](26-anti.md) | Anti-patterns | REFERENCE | 0 | 3 | No anti-pattern problem — a diagnostic-silence problem in three places. |

## Method and its limits

Each of the 26 reviewers read its rule files in full, then every source file, and had
read-only access to the source (no reviewer edited any `.rs` file or `Cargo.toml`).
Several ran `cargo clippy` with targeted lints, inspected generated asm, measured type
sizes in a throwaway crate, or reproduced a defect against the crate's locked deps —
those findings say so.

**Known limits.** Reviewers worked in parallel and could not see each other's reports, so
convergence was reconciled here rather than by them; where two reviewers described one
defect, this index merges them and the individual reports still carry both entries.
Findings inherited a project calibration written before any file was read, and three of
its premises were wrong — the brief claimed `crash.rs`/`render.rs` contained `unsafe`
(they contain none), predicted `perf-io-buffering` findings that do not exist, and
assumed a 60 Hz UI that actually runs at 4 Hz. Reviewers corrected all three in their
reports. No reviewer measured test coverage, because no CI lane does — which is precisely
how `test-001` survived.
