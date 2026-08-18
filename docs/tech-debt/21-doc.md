# 21 — Documentation (`doc-`)

**Category priority:** MEDIUM
**Rules audited:** 12 · **Files read:** 48 (41 `.rs` + `Cargo.toml`, `README.md`, `config.example.toml`, `justfile`, `docs/capture-backend-choice.md`, `docs/initial-stream-anchor.md`, `docs/superpowers/{plan,spec}`) · **Findings:** 12 (P0 0 / P1 3 / P2 4 / P3 5)

## Verdict

This crate's documentation is far above average and the rules aimed at library
surface are mostly noise here — so the honest result is a short list of *specific
wrongness*, not volume. `doc-safety-section` is **clean**: the crate has exactly
one `unsafe fn` (`Wpcap::error_text`, `src/capture/pcap.rs:291`) and it already
carries a correct `# Safety`. The real damage is elsewhere: **`README.md:22` and
`docs/capture-backend-choice.md:12` both quote a kernel filter string
(`tcp and port 3333`) that the code has never used** — it is
`tcp and src port {game_port}` (`src/capture/pcap.rs:551`) — and that one word is
exactly the direction distinction commit `f6b3ce6` made load-bearing three
commits ago. The worst offender file for *internal* doc rot is `src/stream.rs`:
`Reassembler::push_budgeted`'s doc still describes a `Vec<u8>` return that was
replaced by `ReassemblyOutcome`, and never mentions `Pressure` — the outcome that
silently clears every flow's reassembly state. Highest-value single fix:
**doc-001**, three characters of text in two files, because it is the one place
the docs contradict the product's own privacy/direction claim.

Secondary, and cheap: `cargo doc --no-deps` emits **8 warnings** (2 genuinely
broken links) and nothing in `justfile`, `.github/workflows/ci.yml` or
`Cargo.toml` ever runs rustdoc — which is precisely why they accumulated.

> Method note, for honesty: every production path was read line by line. The
> `#[cfg(test)] mod tests` tails of `app/mod.rs`, `config.rs`, `config/persist.rs`,
> `actuator/{mod,plan,win}.rs`, `domain/filter.rs`, `uplink/websocket.rs` and the
> two dedicated `tests.rs` files were read at their heads and then swept with
> targeted greps for doc sections, stale identifiers and removed-architecture
> vocabulary rather than read in full. No doc-category finding lives in a test
> body; two stale *fixture strings* found that way are in doc-012.

## Findings

### doc-001 — README and the capture ADR quote a BPF filter the code does not use

- **Severity:** P1
- **Rule:** [`doc-crate-readme`](../../.claude/skills/rust-skills/rules/doc-crate-readme.md) (drift between README and the code's own docs), with [`doc-link-types`](../../.claude/skills/rust-skills/rules/doc-link-types.md) as the reason the drift went unseen
- **Site:** `README.md:22`, `docs/capture-backend-choice.md:12` (against `src/capture/pcap.rs:551`)
- **What:** The code builds `format!("tcp and src port {game_port}")`. Two tracked
  documents say `port`, not `src port`:
  - `README.md:22` — “The kernel-side filter is fixed (`tcp and port 3333`, built
    from `game_port`), so no other traffic on the machine is even copied.”
  - `docs/capture-backend-choice.md:12` — “one handle and one kernel-side BPF
    filter (`tcp and port 3333`) each.”

  The two *source* copies of the same fact are correct — `src/capture/ip.rs:16`
  (“The kernel filter (`tcp and src port {game_port}`, see `pcap`)”) and
  `src/config.rs:155` — so the divergence is README/ADR vs. code, not code vs.
  code.
- **Why it matters here:** `port` matches both directions of the connection;
  `src port` matches only server→client. The last three commits
  (`ef00803`, `f6b3ce6`, `1a5327e`) exist to make “there is one direction”
  structurally true, and `src/capture/ip.rs`'s doc leans on the kernel filter as
  the *first* of two places that rule is enforced (“a backend with a laxer filter
  cannot smuggle the wrong half of a connection into reassembly”). The README
  sentence is also the one a player reads to satisfy themselves nothing else is
  captured; a reader who copies that string into a Wireshark capture to verify the
  claim gets a *broader* capture than the tool actually takes and concludes the
  docs are lying in the other direction. This is the “confidently stale” case: a
  precise-looking quoted literal that is wrong.
- **Fix:** Replace `tcp and port 3333` with `tcp and src port 3333` in both files.
  In `README.md:22` also say why the `src` is there (“only what the game server
  sends is copied at all — the client→server half never leaves the driver”), since
  that is the sentence's actual point. Do **not** apply this rule's literal
  prescription (`#![doc = include_str!("../README.md")]`): this README is a
  player-facing install/UAC/troubleshooting guide, not API docs, and folding it
  into the rustdoc front page would be worse than the drift. The structural point
  stands though — the pipeline is described three times (`README.md:11-15`,
  `src/lib.rs:9-13`, the ADR) with nothing binding them, and this is the second
  time one copy drifted (`README.md:93-95` records the first).
- **Effort:** trivial

### doc-002 — `Reassembler::push_budgeted`'s doc describes a return type it no longer has, and omits `Pressure`

- **Severity:** P1
- **Rule:** [`doc-all-public`](../../.claude/skills/rust-skills/rules/doc-all-public.md), [`doc-errors-section`](../../.claude/skills/rust-skills/rules/doc-errors-section.md)
- **Site:** `src/stream.rs:549-556`
- **What:** The doc reads:

  ```rust
  /// Integrates a segment and returns the newly contiguous (ordered) bytes.
  ///
  /// Returns an empty vector when the segment is a duplicate, partially fills
  /// a gap, or still waits on a missing segment. …
  pub(crate) fn push_budgeted(&mut self, segment: BudgetedSegment) -> ReassemblyOutcome {
  ```

  There is no vector and no “bytes” in the signature. `git log -S` puts the doc
  text at `025e9fd` and `ReassemblyOutcome` at `b460140` — the doc describes the
  pre-`b460140` `-> Vec<u8>` shape. Confusingly, the *test-only* shim
  `Reassembler::push` (`src/stream.rs:606`, `-> Vec<u8>`) is the function this doc
  now correctly describes, so a reader who checks does not find the mismatch
  obvious. Worse, the second failure mode is entirely absent: on
  `ReassemblyOutcome::Pressure` this method has already called `self.clear()`
  (line 583) — dropping *every* tracked flow's pending bytes and anchors,
  crate-wide — recorded a drop, and bumped the resync counter. Nothing in the doc
  says so.
- **Why it matters here:** `Pressure` is the caller's whole contract. All three
  call sites (`src/app/mod.rs:785-788`, `:808-811`) must translate it into
  `ForwardStatus::Pressure` and re-arm `AnchorState::AwaitingFirst`, and
  `src/app/mod.rs:791-796` comments that missing this “abandons the rest of the
  burst”. A future caller reading only the doc concludes that “nothing was
  delivered” is always benign (duplicate / gap fill) and will not re-anchor —
  which is the exact bug that freezes a half-stream forever, the failure the
  `absorb` comment at `src/stream.rs:722-725` was written to prevent elsewhere.
- **Fix:** Rewrite as:

  ```rust
  /// Integrates a segment, returning the bytes that became contiguous.
  ///
  /// [`ReassemblyOutcome::Chunks`] may be empty: the segment was a duplicate,
  /// partially filled a gap, or still waits on a missing predecessor. FIN is not
  /// modelled — a stream is never torn down, so a segment reordered ahead of a
  /// gap (a FIN-flagged one included) keeps its buffered payload until the gap
  /// fills.
  ///
  /// [`ReassemblyOutcome::Pressure`] is not “nothing yet”: the pending-byte
  /// quota was exhausted, **every** tracked flow's state has already been
  /// cleared, and this segment was dropped and counted. The caller must re-anchor
  /// (see `AnchorState::AwaitingFirst`) rather than wait for a gap fill that can
  /// never arrive.
  ```

  While there, give `enum ReassemblyOutcome` (`src/stream.rs:810`) and its two
  variants the one-line docs they lack.
- **Effort:** trivial

### doc-003 — `InitialBurst::push` panics on a precondition documented only by its own assertion

- **Severity:** P1
- **Rule:** [`doc-panics-section`](../../.claude/skills/rust-skills/rules/doc-panics-section.md)
- **Site:** `src/stream.rs:465-472` (+ `src/stream.rs:98-100`, `PipelineBudget::with_limits`)
- **What:** `push` opens with
  `assert!(!self.would_exceed(&segment), "initial anchor burst limits must be checked before insertion")`
  and carries no doc comment at all. Its sibling `is_at_limit`
  (`src/stream.rs:485-490`) gets six lines of rationale; `would_exceed` gets none;
  the one method that *panics* gets none either. `PipelineBudget::with_limits` has
  three unconditional `assert!`s on the stage-vs-global relation with no `# Panics`
  (private and only ever called with the file's own constants, so lower stakes).
- **Why it matters here:** This is the one panic in the crate that a caller can
  realistically cause, and the crate has **zero** `# Panics` sections anywhere
  (verified by grep). The contract is not obvious from the name: `push` reads like
  `Vec::push`. The single production caller
  (`src/app/mod.rs:727,736,753,763`) does guard correctly, but `reassemble_loop_with_pressure`
  is 120 lines with four `burst.push`/`would_exceed` pairings across three
  `AnchorState` arms — adding a fifth arm and forgetting the guard panics the
  reassembly task, which `SessionWorkers::spawn`'s `catch_unwind`
  (`src/app/mod.rs:496`) turns into `"reassembly task panicked"` and a dead
  pipeline. `src/stream.rs:182-191` shows this crate already reasons carefully
  about which asserts survive into shipped builds; this one just was not
  documented as part of that reasoning.
- **Fix:** On `InitialBurst::push`:

  ```rust
  /// Admits `segment` into the burst.
  ///
  /// # Panics
  ///
  /// Panics if the segment would exceed either burst cap. This is a caller
  /// contract, not a runtime condition: check [`Self::would_exceed`] first and
  /// flush instead. The assert is deliberate — silently accepting the segment
  /// would let one post-resync burst grow past the 256 KiB / 128-segment bound
  /// the anchor decision is predicated on.
  ```

  Add a matching two-line `# Panics` to `PipelineBudget::with_limits`.
- **Effort:** trivial

### doc-004 — `cargo doc` emits 8 warnings, 2 of them genuinely broken links, and nothing ever runs rustdoc

- **Severity:** P2
- **Rule:** [`doc-intra-links`](../../.claude/skills/rust-skills/rules/doc-intra-links.md), [`doc-link-types`](../../.claude/skills/rust-skills/rules/doc-link-types.md)
- **Site:** `src/migrate.rs:81`, `src/actuator/mod.rs:58` (broken); `src/actuator/mod.rs:248,251`, `src/actuator/win.rs:561`, `src/config/persist.rs:138`, `src/capture/pcap.rs:616` (private-item, see doc-008); `justfile`, `.github/workflows/ci.yml`, `Cargo.toml` (no enforcement)
- **What:** `cargo doc --no-deps 2>&1` — the only machine evidence available for
  this category, since `cargo clippy --all-targets` is silent on this crate —
  reports 7 warnings, 8 with `--document-private-items`. Two resolve to nothing at
  all:
  - `src/migrate.rs:81` — ``Call it before [`crate::main`]'s logging setup`` →
    *“unresolved link to `crate::main`: no item named `main` in module
    `arkyve_refresh_shop`”*. `main` lives in the **binary** target
    (`src/main.rs:102`); the library's rustdoc can never see it.
  - `src/actuator/mod.rs:58` — ``Shared with [`setup`]'s live-edit path`` →
    *“unresolved link to `setup`”*. The intended target is
    `crate::app::setup` (`src/app/mod.rs:218`), which is not in scope in
    `crate::actuator`.

  Neither `justfile` (`verify` = fmt-check + clippy + test; `backends` = clippy +
  test), nor `.github/workflows/ci.yml`, nor `Cargo.toml` contains `cargo doc`,
  `rustdoc` or `RUSTDOCFLAGS`. `Cargo.toml` has no `[lints.rustdoc]` table.
- **Why it matters here:** These are cheap, machine-checkable, and they rot in one
  direction only. `migrate.rs`'s link is on the *ordering contract* that keeps the
  first post-upgrade launch's log file writable (`src/main.rs:103-109`) — the
  single most order-sensitive line in `main` — and the link that points at it is
  dead. And the reason both survived is structural: nothing in this project has
  ever run rustdoc, so the count can only go up.
- **Fix:** Two text edits, then close the hole:
  - `src/migrate.rs:81` → ``Call it before `main`'s logging setup (`src/main.rs`)``
    — plain backticks, since no intra-doc link into a binary target exists.
  - `src/actuator/mod.rs:58` → ``[`crate::app::setup`]``.
  - Add to `Cargo.toml`:
    ```toml
    [lints.rustdoc]
    broken_intra_doc_links = "deny"
    private_intra_doc_links = "warn"
    ```
  - Add a `doc` recipe to `justfile` and fold it into `verify`:
    `cargo doc --locked --no-deps --document-private-items` under
    `RUSTDOCFLAGS="-D warnings"`. `--document-private-items` is the right setting
    for a `publish = false` binary: it is what surfaces the `setup` break above,
    and it makes the private-item warnings in doc-008 disappear on their own
    rather than needing text edits.
- **Effort:** small

### doc-005 — mutex-poison panics on public API, undocumented, against a crate-wide policy documented everywhere else

- **Severity:** P2
- **Rule:** [`doc-panics-section`](../../.claude/skills/rust-skills/rules/doc-panics-section.md)
- **Site:** `src/actuator/mod.rs:94-99` and `:102-107` (`ActuatorHandle::timings`, `set_timings`); collapsed with `src/app/session/mod.rs:200,237,326,357,406` and `src/actuator/mod.rs:213,221`
- **What:** `ActuatorHandle::timings()` and `set_timings()` are `pub fn` on a
  `pub` `Clone` struct and both end in `.expect("actuator timings mutex poisoned")`.
  Neither has a `# Panics` section. This is the *only* lock in the crate that
  panics on poison, and every other one documents the opposite choice with its
  reasoning:
  - `src/journal.rs:73-80` — “Poison-tolerant … panicking here after one poisoning
    would cascade across tasks and freeze the very history the GUI is meant to
    still show.”
  - `src/actuator/shield.rs:29-34` — “Poisoning carries no meaning here … a panic
    elsewhere must not turn every later click into a fatal.”
  - `src/ui/mod.rs:37-44` (`lock_ignoring_poison`), `src/main.rs:285-290`,
    `src/stream.rs:147,168,193,248` (`unwrap_or_else(|err| err.into_inner())`).

  `src/app/session/mod.rs` has five more `.expect("controller mutex poisoned")`
  sites; `session_loop`'s doc (`:27-33`) documents the lock *discipline* (“only ever
  held across synchronous calls, never an `.await`”) but not that a poisoned lock
  ends the loop by panic.
- **Why it matters here:** In the shipped GUI build the window is the only
  interface. `set_timings` is on the Setup tab's Apply path
  (`src/app/session/mod.rs:280`) and `timings()` is called while building every
  queued job (`:494,577,621`). `src/actuator/win.rs:569-574` states this crate's own
  standard explicitly — “a panic here would kill the actuator task and take the
  whole session down, where `Fatal` stops the loop with a real reason” — and this is
  the one spot that neither takes the poison-tolerant route nor tells a reader it
  did not. A maintainer extrapolating from the five documented sites will assume
  wrongly.
- **Fix:** Documentation-only for this category — add to both methods:

  ```rust
  /// # Panics
  ///
  /// Panics if the timings mutex was poisoned, i.e. if a thread panicked while
  /// holding it. Unlike the journal, shield and view locks, this one is *not*
  /// poison-tolerant: <state the reason, or change the behaviour>.
  ```

  If no reason can be stated, that is a finding for the `err-`/`conc-` reviewers
  (switch to `unwrap_or_else(PoisonError::into_inner)` like `shield::lock`), not
  for this one. Add one sentence to `session_loop`'s doc noting that a poisoned
  controller lock ends the loop by panic, which `Session::run`
  (`src/app/mod.rs:434-437`) re-raises and `app::supervise` turns into a banner.
- **Effort:** trivial (docs) / small (if the behaviour is unified)

### doc-006 — a load-bearing comment cites "Plan 008" as authoritative; the document is not in the repository

- **Severity:** P2
- **Rule:** [`doc-intra-links`](../../.claude/skills/rust-skills/rules/doc-intra-links.md) (an unresolvable reference, in prose rather than in link syntax)
- **Site:** `src/app/mod.rs:701-703`, `docs/initial-stream-anchor.md:84-87`
- **What:** `src/app/mod.rs:701` reads *“Plan 008 remains authoritative: never hold
  a SYN behind the anchor deadline.”* `docs/initial-stream-anchor.md:84` reads
  *“…passed immediately to `Reassembler`, whose plan-008 incarnation reset remains
  authoritative.”* No plan 008 exists in the tree: `plans/` is in `.gitignore`
  (line: `/plans`), it is untracked (`git ls-files plans/` is empty), and
  `plans/README.md` records that plans 001–014 were deleted on 2026-08-17.
- **Why it matters here:** I verified the invariant itself is still true — the SYN
  fast-path at `src/app/mod.rs:704-715` does flush the burst first, and
  `Reassembler::syn_starts_new_incarnation` (`src/stream.rs:621-637`) is the
  incarnation reset — so this is not a *false* comment, which is why it is P2 and
  not P1. But it is an appeal to an authority no reader of this repository can
  obtain, on an invariant subtle enough that three separate comments defer to it
  instead of stating it. The next person who wants to change the SYN ordering has
  nothing to consult and no way to know whether the reasoning still holds. Note
  that `docs/initial-stream-anchor.md` is otherwise the *model* of how to handle
  this (see Clean areas) — the amendment header is precise and correct.
- **Fix:** Replace the citation with the reason, in `src/app/mod.rs:701`:
  *“A SYN is never held behind the anchor deadline: it re-anchors the sequence
  space, so buffering it would make the burst's own ordering meaningless. Commit
  any older burst first, then let `Reassembler` classify/reset the incarnation
  immediately.”* In `docs/initial-stream-anchor.md:84`, either drop `plan-008` or
  name the code: `Reassembler::syn_starts_new_incarnation`. Same for
  `docs/initial-stream-anchor.md:86-87`'s reference to the global-window rationale.
- **Effort:** trivial

### doc-007 — 6 of 10 fallible public functions have no `# Errors`; all 4 that do are in `config`

- **Severity:** P2
- **Rule:** [`doc-errors-section`](../../.claude/skills/rust-skills/rules/doc-errors-section.md)
- **Site:** `src/actuator/plan.rs:130` (`to_screen`), `src/actuator/mod.rs:83` (`submit`), `src/actuator/mod.rs:153` (`Surface::acquire`), `src/app/mod.rs:288` (`app::run`), `src/app/mod.rs:304` (`Session::run`), `src/capture/pcap.rs:537` (`PcapSource::open`)
- **What:** The crate has exactly four `# Errors` sections, all in configuration
  code (`src/config.rs:303-325`, `src/config/persist.rs:34-50`, `:74-78`,
  `:143-155`) — and they are genuinely excellent, variant by variant, with the
  reachability of each spelled out. Nothing else fallible has one. The two worth
  fixing are:
  - **`plan::to_screen`** — `pub fn … -> Result<(i32, i32), String>`. A stringly
    error with two distinct causes (degenerate rect, aspect narrower than 16:9)
    that the caller must tell apart in effect if not in code: at
    `src/app/mod.rs:318-326` the `Err` is routed to `fail()`, which halts the whole
    watch. The doc describes the aspect rule in prose but never says the function
    returns `Err` for it, nor mentions the degenerate-rect arm at all.
  - **`PcapSource::open`** — three structurally different `Err`s
    (`Wpcap::load` failed → “install Npcap”; zero usable devices →
    `no_usable_device_error`, itself three-way; thread spawn failed) and only the
    third sentence of the prose (“Only *zero* usable devices is fatal”) hints at
    any of it. `src/app/mod.rs:1077-1100` documents the *caller's* side of this
    beautifully, which makes the gap on the callee more visible, not less.

  `submit` is a near-miss rather than a real gap: `SubmitError`'s own doc
  (`src/actuator/mod.rs:110-126`) explains both variants and why they must not
  collapse; only the cross-reference from `submit` is missing.
  `app::run` / `Session::run` return `Result<()>` where the error is the session's
  own fatal, already described in `session_loop`'s doc — a one-line `# Errors`
  pointing there is enough.
- **Why it matters here:** Not the library-API argument the rule makes — nobody
  outside this crate calls these. The argument is that this crate's *own* pattern
  is to name every failure mode and its reachability, and the two capture/actuator
  boundaries are exactly where a maintainer needs it, because both `Err`s stop the
  refresh loop and both are diagnosed from a log file on a machine nobody can
  inspect.
- **Fix:** Add `# Errors` to `to_screen` and `PcapSource::open` in the style
  already established in `src/config.rs:303-325` (bulleted, one bullet per cause,
  each saying when it is reachable). For `submit`, one line: *“# Errors — see
  [`SubmitError`]; both variants mean a lost click and want opposite advice.”*
  For the two `run`s, one line each pointing at the session-loop fatal.
- **Effort:** small

### doc-008 — private-item, redundant and mis-targeted intra-doc links

- **Severity:** P3
- **Rule:** [`doc-intra-links`](../../.claude/skills/rust-skills/rules/doc-intra-links.md), [`doc-link-types`](../../.claude/skills/rust-skills/rules/doc-link-types.md)
- **Site:** `src/actuator/win.rs:561` (two defects on one line), `src/actuator/mod.rs:248`, `:251`, `src/config/persist.rs:138`, `src/capture/pcap.rs:616`
- **What:** Four public docs link to private items (`super::shield`, `fail`,
  `drop_reason`, `tidy`, `PcapStop`) — harmless in intent, and they all resolve
  under `--document-private-items`, which is why doc-004's fix makes them vanish
  without text edits. Two on `src/actuator/win.rs:561` are real mistakes:

  ```rust
  /// … re-asserts the [`shield`](super::shield) over the game until [`release`](Surface).
  ```
  - the explicit target is redundant (rustdoc says so: *“because label contains
    path that resolves to same destination”*);
  - ``[`release`](Surface)`` renders the word “release” as a link to the
    **`Surface` trait**, not to `Surface::release`. It resolves, so no warning
    fires, and it is silently wrong. Compare `src/actuator/mod.rs:156-157`, which
    gets the same two links right: ``[`acquire`](Surface::acquire)`` /
    ``[`release`](Surface::release)``.
- **Why it matters here:** One is a wrong link that no tool will ever flag; a
  reader clicking “release” lands on the trait page and has to hunt.
- **Fix:** `src/actuator/win.rs:561` →
  ``… re-asserts the [`shield`] over the game until [`release`](Surface::release).``
  Leave the four private-item links alone and let doc-004's
  `--document-private-items` lane silence them.
- **Effort:** trivial

### doc-009 — the two dedicated test submodules have no `//!` header

- **Severity:** P3
- **Rule:** [`doc-module-inner`](../../.claude/skills/rust-skills/rules/doc-module-inner.md)
- **Site:** `src/domain/control/tests.rs:1`, `src/app/session/tests.rs:1`
- **What:** Every other one of the 41 `.rs` files in the crate — including
  `build.rs`, `examples/ui_preview.rs` and `src/domain/mod.rs` (7 lines) — opens
  with a `//!` header. These two, the crate's two largest files at 1781 and 1423
  lines, open with `use`.
- **Why it matters here:** Not the rustdoc argument (they are `#[cfg(test)]`). The
  argument is that these two files are 3204 lines of behavioural specification for
  the controller state machine and the session loop, and a reader arriving at
  either has no statement of what the file covers or how it is organised — while
  every neighbouring file gives them one. The individual tests are exemplarily
  documented, which makes the missing top-level orientation the only thing absent.
- **Fix:** A three-to-five line `//!` on each, naming what the file covers and the
  ordering convention (e.g. for `domain/control/tests.rs`: arming/refusal, snapshot
  evaluation and dedup, pause/purchase, limits, the watchdog ladder).
- **Effort:** trivial

### doc-010 — no `# Examples` and no doctests anywhere; four pure functions would earn one nearly free

- **Severity:** P3
- **Rule:** [`doc-examples-section`](../../.claude/skills/rust-skills/rules/doc-examples-section.md), [`doc-question-mark`](../../.claude/skills/rust-skills/rules/doc-question-mark.md), [`doc-hidden-setup`](../../.claude/skills/rust-skills/rules/doc-hidden-setup.md)
- **Site:** crate-wide; candidates are `src/actuator/plan.rs:130` (`to_screen`), `src/actuator/plan.rs:90` (`row_for_slot`), `src/actuator/plan.rs:97` (`buy_zone`), `src/domain/shop.rs:122` (`ShopItem::effective_slot`)
- **What:** Zero `# Examples` sections and zero ` ``` ` doc code blocks in the
  crate, so `cargo test --doc` is a no-op. `doc-question-mark` and
  `doc-hidden-setup` are consequently vacuous — there is no `.unwrap()` in a doc
  example because there are no doc examples. Reported, not padded: for a
  `publish = false` binary this is the right default almost everywhere.
- **Why it matters here:** Four exceptions, all pure, all fiddly, all already
  covered by a unit test whose asserts would transplant verbatim into a doctest:
  the design→screen transform (a 16:9-and-wider projection with three anchor
  modes), the 1-based-slot ↔ 0-based-row mapping, the scroll-position-dependent
  buy geometry, and the `slot == 0` sentinel fallback. Each is a place where the
  prose describes a coordinate convention that one two-line example would pin
  unambiguously — `row_for_slot`'s doc says “`None` for anything a degraded shop
  put outside the six rows”, and `assert_eq!(row_for_slot(1), Some(0));
  assert_eq!(row_for_slot(0), None);` says it better in less space.
- **Fix:** Add a `# Examples` block to those four, lifting asserts from the
  existing `#[cfg(test)]` modules. Use `?` and `#`-hidden setup per the two
  companion rules so the pattern is right from the first one. Do **not** roll this
  out further — a doctest on a `pub` item nobody outside the crate calls is a
  second copy of a unit test with a slower runner.
- **Effort:** small

### doc-011 — undocumented `pub` types, capped at one finding

- **Severity:** P3
- **Rule:** [`doc-all-public`](../../.claude/skills/rust-skills/rules/doc-all-public.md)
- **Site:** `src/watch.rs:47` (`WatchGate`), `src/journal.rs:24` (`EventLog`), `src/error.rs:7` (`Result`), `:10` (`Error`), `:58` (`Error::Capture`), `:66` (`Error::Io`), `src/actuator/win.rs:142` (`WinSurface`), `src/domain/shop.rs:8` (`ShopSnapshot`), `:143` (`SubStat`), `src/config.rs:114` (`ReconnectConfig`), `src/domain/control/mod.rs:83` (`Status`), `:65` (`StopReason`), `:94` (`Event`)
- **What:** ~35 `pub` items (of 159) carry no `///`. Per the brief this is
  deliberately **one** finding, not thirty-five, and most of it is not worth
  touching: the constants in `src/actuator/plan.rs:20-30` are self-naming and the
  module header explains the whole family; `ClientRect`/`Zone`/`DesignPoint` fields
  are documented collectively by `src/actuator/plan.rs:1-6`.
- **Why it matters here:** A narrow subset stands out because the *type* has no
  statement of purpose at all while its methods are richly documented — a reader
  gets the parts before the whole. `WatchGate` is the clearest: its four methods
  have careful docs about halt latching and re-arm races, but the struct itself
  says nothing, and the concept is explained only in `src/watch.rs:1-6`. Same for
  `EventLog` (the crate's single sink for player-facing lines), `WinSurface` (the
  non-default backend), and the `Error` enum, where 6 of 8 variants are documented
  and `Capture`/`Io` are not. Note the contrast: `WinSurface`'s counterpart
  `MessageSurface` (`src/actuator/win.rs:558-561`) *is* documented — so the gap
  reads as an oversight, not a policy.
- **Fix:** One `///` line on `WatchGate`, `EventLog`, `WinSurface`, `Result`, the
  `Error` enum and its two bare variants, `ShopSnapshot`, `SubStat`,
  `ReconnectConfig`, `Status`, `StopReason`, `Event`. Stop there. Do **not** enable
  `#![warn(missing_docs)]` — on a `publish = false` binary it would demand ~35
  restatements of what the module headers already say better, which is the
  outcome this rule's `Enforcement` section would produce and that this crate's
  conventions do not want.
- **Effort:** small

### doc-012 — stale identifiers in comments and test fixtures name files that do not exist

- **Severity:** P3
- **Rule:** [`doc-all-public`](../../.claude/skills/rust-skills/rules/doc-all-public.md) (accuracy of prose)
- **Site:** `src/crash.rs:118`, `:124`, `examples/ui_preview.rs:3`
- **What:** Two references to a `native` capture backend that is not in this tree:
  - `src/crash.rs:118,124` — the `crash_entry` test uses
    `"src/capture/native.rs:60"` as its location fixture and asserts on it. Purely
    cosmetic (any string works), but it names a file that does not exist, in the
    one module whose job is to make a crash location legible.
  - `examples/ui_preview.rs:3` — “On a machine without the native capture backend
    (mac dev)”. The backend is Npcap, and it is *not* “native” in any sense this
    tree uses; the accurate statement is “without `pcap-backend` (mac dev)”, which
    is also what its own run instruction on line 10 encodes
    (`--no-default-features --features gui`).
- **Why it matters here:** Small, but this is the residue of the same removal
  sweep as doc-001 and doc-006, and “native backend” is the vocabulary of an
  architecture the ADR spent 220 lines burying. Left alone it is what a future
  reader greps for.
- **Fix:** `src/crash.rs` → use `"src/capture/pcap.rs:60"` (or a neutral
  `"src/lib.rs:1"`) in both the fixture and the assert.
  `examples/ui_preview.rs:3` → “On a machine without the capture backend
  (`pcap-backend` is Windows-only; mac dev)”.
- **Effort:** trivial

## Clean areas

- **`doc-safety-section` — fully clean, and this is the headline clean result.**
  The crate has exactly one `unsafe fn`: `Wpcap::error_text`
  (`src/capture/pcap.rs:291`). It has a `# Safety` section, that section states a
  real caller obligation (“`handle` must be a live `pcap_t` opened by this
  library”), and the body's own `// SAFETY:` correctly delegates to it. Both call
  sites (`src/capture/pcap.rs:800,804,976`) satisfy it demonstrably. The one
  `unsafe impl` (`Send for Handle`, `:430`) carries a five-line justification.
  Nothing to do.
- The `unsafe-` reviewer's P1 on `cstr` (`src/capture/pcap.rs:300`) — a *safe* fn
  whose `// SAFETY:` states an obligation its signature cannot bind — is
  referenced here, not re-filed. Worth noting for whoever fixes it that the
  natural repair (make it `unsafe fn`, move the obligation into a `# Safety`
  section) lands squarely in this category and would make this crate's
  `# Safety` count 2 of 2 rather than 1 of 1.
- **`docs/initial-stream-anchor.md` is the model for how stale docs should be
  handled** and should not be "fixed". Its amendment header (`:3-16`) names the
  commit, names the three claims that stopped being true, states what replaced
  each, and explicitly says what the change does *not* affect (the
  ADD_BOUNDED_WINDOW decision) — while deliberately freezing the body as an
  evidence record. I checked all three claims against the code and the amendment
  is accurate and complete. Only its `plan-008` citation dangles (doc-006).
- **The `# Errors` sections that exist are exemplary.** `Config::load`
  (`src/config.rs:303-325`), `persist::save` (`src/config/persist.rs:34-50`),
  `replace_file` (`:74-78`) and `strip_retired_keys` (`:143-155`) each enumerate
  causes per error variant, say which are unreachable and why, and state the
  post-failure guarantee (“The original file is left untouched whenever this
  returns `Err`”). `strip_retired_keys` even documents caller policy (“Callers
  treat every one of these as non-fatal”). doc-007 asks for this style elsewhere,
  not for changes here.
- **`doc-module-inner` is otherwise complete**: 39 of 41 `.rs` files open with
  `//!`, including `build.rs` (39 lines of UAC/UIPI rationale) and
  `src/domain/mod.rs`. The headers on `src/capture/pcap.rs` (three `#`-sections),
  `src/migrate.rs`, `src/stream.rs` and `build.rs` are the crate's best
  documentation, full stop. Only the two `tests.rs` files lack one (doc-009).
- **The removed architecture was scrubbed properly nearly everywhere.**
  `README.md:111-118` keeps a correct WinDivert *upgrade* note; `src/migrate.rs`,
  `Cargo.toml:70-73` and `src/main.rs:104` name WinDivert only in the
  cleanup/compatibility context that still exists; `src/capture/pcap.rs:5-13` and
  `build.rs:34-38` describe the removal as history and say why. Every
  `client_to_server` occurrence in `src/config.rs` and `src/config/persist.rs` is
  the intentional retired-key shim, documented at length at `src/config.rs:71-100`
  and `:151-180`. `README.md:79-95` even records that an earlier version of that
  section said the opposite. doc-001 is the single leak.
- The privilege model is stated consistently and correctly in all five places it
  appears: `README.md:79-95`, `build.rs:1-38`, `src/capture/pcap.rs:15-17`,
  `src/app/mod.rs:1085-1087`, `src/migrate.rs:17-20`. Each says *elevated for the
  actuator, not for capture*, and each points at `build.rs` as the single
  authority.
- `config.example.toml` matches the code on every value I checked: the eight
  timing baselines (`:106-107` vs `src/actuator/plan.rs:20-30`), the 60 000 ms
  ceiling (`:105` vs `src/config.rs:39`), `backend = "message"` as default
  (`:91` vs `src/config.rs:147`), the retired-key wording, and the loopback-only
  `ws://` rule. Note `config.toml` in the repo root *is* stale on several of these
  (`backend` default, WinDivert filter comment, `client_to_server`), but it is
  gitignored (`.gitignore`: `/config.toml`) and untracked — a local dev file, not
  a repository defect, and out of scope for this audit. It will also be
  self-repaired at the next launch by `strip_retired_keys`.
- `README.md`'s Troubleshooting log-line list (`:240-267`) matches the actual
  emitted lines: `"arkyve-refresh-shop starting"` (`src/main.rs:138`),
  `"wpcap.dll loaded"` (`pcap.rs:543`), `"adapter opened and filtered"`
  (`pcap.rs:816`), `"first server-to-client segment admitted"` (`pcap.rs:665`),
  `"capture progress"` (`app/mod.rs:978`), `"capture funnel"` (`pcap.rs:477`),
  `"the capture driver dropped packets"` (`pcap.rs:1032`), `"session heartbeat"`
  (`session/mod.rs:210`), `"session aborted"` (`session/mod.rs:139`). All nine.
- `doc-hidden-setup` and `doc-question-mark`: no violations possible — see
  doc-010. There is no `.unwrap()` in a doc example anywhere because there is no
  doc example anywhere.
- `src/capture/pcap.rs` documents the `PcapPktHdr` layout danger the `const-`
  reviewer flagged, in three places a reader will actually hit: on the struct
  (`:135-140`), on `plausible_caplen` (`:390-403`, including the honest
  “canary, not a proof … about a quarter of the time”), and in the runtime error
  text (`:941-945`) that names the layout as the cause. The size assertion
  (`:1181-1187`) carries the “single most dangerous constant in this file”
  comment. A caller of this module cannot miss it. No finding.

## Not applicable

- `doc-cargo-metadata` — `publish = false`. `name`, `version`, `edition`,
  `rust-version`, `description` and `license` are all present and accurate; the
  remaining fields the rule asks for (`repository`, `documentation`, `readme`,
  `keywords`, `categories`, `authors`, `homepage`, `include`/`exclude`, `[badges]`)
  exist only to serve crates.io search and the docs.rs build, neither of which
  will ever see this crate. Adding them would be pure ceremony. `Cargo.toml` does
  have a real documentation gap of a different kind — no `[lints.rustdoc]` table —
  which is filed under doc-004.
- `doc-crate-readme` — the rule's prescription
  (`#![doc = include_str!("../README.md")]` + `readme = "README.md"`) does not
  apply: this README is a player-facing install, UAC-consent and troubleshooting
  guide, and rustdoc would try to compile nothing from it but would put 270 lines
  of Npcap installer instructions on the library front page. The rule's *stated
  failure mode* — README drifting from the code it describes — did occur, and is
  filed as doc-001 with the narrow fix instead.
- `doc-all-public` / `doc-examples-section` at full strength — a `publish = false`
  binary has no external API contract; `pub` here is a module-visibility choice.
  Both are audited at reduced severity (doc-010, doc-011) rather than dismissed,
  and both findings explicitly cap their own scope.
