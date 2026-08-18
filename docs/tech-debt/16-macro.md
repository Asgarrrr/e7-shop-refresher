# 16 — Macros (`macro-`)

**Category priority:** MEDIUM
**Rules audited:** 8 · **Files read:** 41 (+ `Cargo.toml`) · **Findings:** 2 (P0 0 / P1 0 / P2 1 / P3 1)

## Verdict

This category is essentially clean, and that is the honest result rather than a
shortcut. The crate defines **exactly one** `macro_rules!` in ~22 000 lines
(`sym!` in `src/capture/pcap.rs:240`), authors **no** proc macro, has no
workspace and no `proc-macro = true` crate, and exports no macro — so five of
the eight rules are structurally not applicable. The one macro that exists is
*justified*: it injects a `continue` into an enclosing loop across 13
`libloading` symbol resolutions, which is the one thing a generic function
genuinely cannot do, and it uses the tightest fragment specifier available
(`:literal`). Everywhere a macro would have been the lazy answer — Win32
wrapper boilerplate, egui widget scaffolding, config field plumbing, test
fixtures — the crate already reached for a function, a closure, a trait or a
`const` array instead, which is `macro-prefer-functions` honoured rather than
violated.

The worst offender file is `src/app/mod.rs`: `reassemble_loop_with_pressure`
hand-expands the same 5-line `ForwardStatus` dispatch four times. It is the only
duplication cluster in the crate large and mechanical enough that someone might
reach for a macro — and the highest-value fix here is to record that they must
not: an ordinary `async fn` taking `&mut AnchorState` collapses all four sites to
one line each, exactly mirroring the `if !flush_anchor(..).await { break; }`
idiom already used three lines away.

## Findings

### macro-001 — Four hand-expanded copies of the `ForwardStatus` dispatch; the fix is a function, not a macro

- **Severity:** P2
- **Rule:** [`macro-prefer-functions`](../../.claude/skills/rust-skills/rules/macro-prefer-functions.md)
- **Site:** `src/app/mod.rs:709-713`, `:719-723`, `:729-733`, `:757-761` (four sites, one finding)
- **What:** All four sites are byte-identical apart from indentation:

  ```rust
  match forward_segment(&mut reassembler, segment, &raw_tx).await {
      ForwardStatus::Open => {}
      ForwardStatus::Pressure => anchor = AnchorState::AwaitingFirst,
      ForwardStatus::Closed => break,
  }
  ```

  Twenty lines of pure boilerplate inside one 120-line `loop`, three levels of
  `match`/`else` deep. This is the *only* cluster in the crate that meets the
  "mechanically repetitive boilerplate" bar in `macro-prefer-functions`'s
  "when to reach for a macro" table, and it is worth writing down precisely
  because it is the one place a `macro_rules!` looks tempting: the `break`
  targets the caller's loop, which is the same control-flow-injection argument
  that legitimises `sym!` in `pcap.rs`.

- **Why it matters here:** A macro would be the **wrong** answer, and filing this
  is partly to stop a later pass from writing one. The `Pressure` arm mutates
  `anchor` and the `Closed` arm exits the loop — both expressible by a helper
  that takes `&mut AnchorState` and returns "keep going". A local macro would
  buy the same line count while making four already-subtle byte-pressure
  transitions invisible to rust-analyzer, un-steppable in a debugger, and
  opaque in the error messages of a function that is the crate's most
  correctness-critical loop (a wrong `anchor` transition here silently stalls
  reassembly forever — see the `flush_anchor` comments). The duplication also
  has a real maintenance cost today: a fifth `ForwardStatus` variant, or a
  change to what `Pressure` must do, has to be edited in four places with no
  compiler help pointing at the ones you missed.
- **Fix:** Extract a plain `async fn` beside `flush_anchor`, and let each site
  become one line in the shape the surrounding code already uses:

  ```rust
  /// Forwards `segment`, re-arming the anchor under pressure.
  /// `false` means the downstream closed and the caller must break.
  async fn forward_or_rearm(
      reassembler: &mut Reassembler,
      segment: BudgetedSegment,
      raw_tx: &mpsc::Sender<BudgetedChunk>,
      anchor: &mut AnchorState,
  ) -> bool {
      match forward_segment(reassembler, segment, raw_tx).await {
          ForwardStatus::Open => true,
          ForwardStatus::Pressure => {
              *anchor = AnchorState::AwaitingFirst;
              true
          }
          ForwardStatus::Closed => false,
      }
  }
  ```

  Each site then reads
  `if !forward_or_rearm(&mut reassembler, segment, &raw_tx, &mut anchor).await { break; }`,
  matching `if !flush_anchor(&mut anchor, &mut reassembler, &raw_tx).await { break; }`
  one branch over. Two of the four sites currently set `anchor = AnchorState::Steady`
  immediately *before* the match (`:708`, `:728`) — preserve that ordering when
  refactoring; the helper only handles the *post*-forward transition.
- **Effort:** small
- **Scope note:** the duplication itself also falls under `anti-`; what belongs
  to this category is the verdict "function, not macro". Synthesis should treat
  this as one item, not two.

### macro-002 — `sym!` silently captures three outer locals and `continue`s a loop three scopes up, with no note at the macro

- **Severity:** P3
- **Rule:** [`macro-rules-hygiene`](../../.claude/skills/rust-skills/rules/macro-rules-hygiene.md)
- **Site:** `src/capture/pcap.rs:240-253`
- **What:** The crate's only `macro_rules!`. Its body references `lib`, `path`
  and `failures` — all defined in the enclosing `for path in DLL_CANDIDATES`
  loop body — and its `Err` arm ends in a bare `continue` that targets that
  loop, i.e. "give up on this DLL candidate and try the next one". This is
  correct and it is *why* the macro exists (a function cannot `continue` its
  caller's loop), but none of it is stated at the definition. The 8-line SAFETY
  comment immediately above (`:237-238`) explains the `libloading` symbol
  lifetimes and the `Err` reporting, and stops short of the control flow: the
  reader has to reconstruct that a missing symbol abandons 12 sibling
  resolutions and restarts the candidate loop.
- **Why it matters here:** `macro_rules!` hygiene protects bindings the macro
  *introduces*; the three it *captures* resolve at the definition site purely
  because the macro is nested inside the loop body, so the whole construct
  breaks the moment anyone hoists it to module scope for tidiness — a plausible
  edit, since a 13-line macro buried inside an `unsafe` block inside a `for`
  body looks misplaced. Hoisting it produces cannot-find-`lib` /
  `continue`-outside-loop errors that read as unrelated, on the crate's
  load-bearing "is Npcap present" path — the one whose failure the player sees
  as `could not load wpcap.dll`.
- **Fix:** One comment line above `macro_rules! sym`, e.g.
  `// Deliberately scope-local and unhygienic by design: it reads `lib`/`path`/`failures` from the loop body above and its `continue` abandons this DLL candidate for the next one. Hoisting it out of this loop will not compile.`
  No code change. (The `$crate` half of the rule is already satisfied — the
  macro references no crate item, so there is no path to qualify.)
- **Effort:** trivial

## Clean areas

**`macro-prefer-functions` — honoured everywhere a macro would have been easy.**
The four sites the audit was pointed at all resolve to non-macro abstractions,
and each is the textbook answer:

- Win32 FFI / `GetLastError` (`src/actuator/win.rs`): factored into ordinary
  functions — `system_metric`, `send_mouse`, `send_input`, `sendinput_result`,
  `post`, `post_refusal`, `preflight_refusal`, `placement_refusal`. The
  read-`last_os_error`-then-classify pattern recurs, but each site has a
  distinct message and verdict, and the classification is already pulled into
  pure testable functions. `client_rect` has the only genuine twin pair
  (`GetClientRect` + `ClientToScreen`, `:311` / `:320`) — two sites is below any
  abstraction bar.
- `libloading` symbol loading (`src/capture/pcap.rs`): the one place a macro
  *is* correct, for the right reason (caller-scope `continue`), and it is local
  and unexported rather than reaching for `#[macro_export]`.
- Config field plumbing (`src/config.rs`, `src/config/persist.rs`): serde
  derives plus a `const RETIRED_KEYS: &[(&str, &[&str])]` table
  (`persist.rs:114`) drive the repetitive parts. `CaptureConfig::retired_keys`
  and `ForwardConfig::retired_keys` are near-twins — two sites, four `if
  .is_some()` lines each; a macro there would cost more than it saves.
- egui widget scaffolding (`src/ui/*`): generic functions and closures, not
  macros — `content_inset<R>(.., impl FnOnce(&mut Ui) -> R)`,
  `limit_ledger_row(.., impl FnOnce(bool, &mut Ui))`, `compact_drag`,
  `optional_value<T: Numeric>`, `styled(.., impl Into<String>)`, `cell`,
  `section`, `timing_group`. `ui/shop.rs:95` even carries a comment explaining
  why `styled` is a generic *free function* and not a closure ("a closure cannot
  be generic over its argument") — the exact reasoning the rule asks for.
- Test fixtures: `domain/control/tests.rs` builds its whole 1 781-line suite on
  helper *functions* (`snap`, `buy`, `buy_with_gold`, `tick`, `target`,
  `controller`, `started`, `recovering`, `with_ids`, `shop`, `hit_shop`,
  `dud_shop`, `meta`) rather than an `assert_transition!`-style DSL. Same in
  `app/session/tests.rs` (`off`, `recording`, `armed`, `armed_recovering`,
  `never_shutdown`, `timings`) and `actuator/win.rs` (`fake_surface`, `calls`,
  `sent_events`, `validation_calls`).

**`macro-rules-hygiene` (`$crate` half).** No macro in the crate emits a path to
a crate item, so there is no `crate::`-instead-of-`$crate::` bug to have. The
one macro is function-local and unexported, where `$crate` would be noise.

**`macro-fragment-specifiers`.** `sym!` captures `$name:literal`, which is the
most precise specifier the language offers for a byte-string literal (there is
no `:byte_str`); it is not `:tt` soup, it takes exactly one fragment, and it has
no repetition, so the follow-set and trailing-comma hazards the rule describes
do not arise.

**Declarative macros from dependencies are used correctly, including the one
field-name trap.** `tracing`'s `info!`/`warn!`/`error!`/`debug!` are used
throughout with fields-then-format-string ordering and `%`/`?` sigils applied
deliberately (`error = %err`, `?strip`, `since_last_shop_s = ?since_last_shop_s`).
The `message`-field collision — `tracing` fills a `message` field from the format
string, so a user field of that name produces two — is not merely avoided but
documented at `src/main.rs:225-226` ("Field named `reason`, not `message`:
`message` is the field tracing itself fills from the format string, and two would
collide in the file"). `target: "journal"` / `target: "migrate"` are used as the
macro keyword form, not as a field. Nothing here needs recording as debt.
`tracing-attributes` is in the graph but `#[instrument]` is never used — no
async-span pitfalls to audit.

**`const` assertions prefer a plain block over a macro.** `src/stream.rs:418-424`
and `src/capture/mod.rs:77-83` each group several `assert!`s in one
`const _: () = { .. };` block — the right shape, and cheaper than any macro.

## Not applicable

- **`macro-export-crate-path`** — nothing is exported. Zero `#[macro_export]`,
  zero `#[macro_use]`, zero `macro_use` attributes anywhere in `src/`,
  `examples/` or `build.rs`. The single macro is function-local, so it has no
  import path to get right, and the crate is `publish = false` with no external
  consumer that could import one.
- **`macro-private-helpers`** — no exported macro, therefore no generated code
  needing a `#[doc(hidden)] pub mod __private` shim. Adding one here would
  invent public surface on a binary crate.
- **`macro-proc-two-crate`** — no proc macro is authored. `Cargo.toml` declares
  no `[lib] proc-macro = true`, no `[workspace]`, and no `syn`/`quote`/
  `proc-macro2` dependency. Every proc macro in the graph belongs to a
  dependency (`serde_derive`, `thiserror-impl`, `tokio-macros`,
  `tracing-attributes`, `bytemuck_derive`, `zerocopy-derive`,
  `windows-implement`, `windows-interface`, `document-features`) and is consumed
  through exactly the facade re-export the rule prescribes (`serde::Deserialize`,
  `thiserror::Error`, `#[tokio::test]`).
- **`macro-proc-syn-quote`** — same reason: nothing in this repository parses or
  emits a `TokenStream`.
- **`macro-proc-error-spans`** — same reason: no proc-macro entry point exists,
  so there is no panic-instead-of-`syn::Error` to find.

## Evaluated and rejected (deliberately *not* findings)

Recorded so a later pass does not "fix" these into macros:

- **`src/ui/editor/timing_meter.rs:45-52`** — eight consecutive
  `const _: () = assert!(plan::WAIT_* <= RULER_MS_U64);` lines. Mechanical and
  eight-deep, so it reads like a macro candidate. It is not: a
  `macro_rules!`-generated repetition of anonymous consts would obscure *which*
  baseline broke the ruler, and the better non-macro answer already exists in
  this crate — a `const BASELINES: [u64; 8]` plus one `const _: () = { .. }`
  block with a `while` loop, as `stream.rs:418` and `capture/mod.rs:77` do. Even
  that is marginal: the eight lines are self-describing, carry a four-line
  comment above them explaining the invariant and its failure mode, and are
  covered by the `slack_is_clamped_even_when_the_baseline_outgrows_the_ruler`
  test. Leave them alone unless someone is already editing that block.
- **`src/actuator/plan.rs:230-285`** — `Timings`' eight fields each carry
  `#[serde(skip_serializing_if = "DelayRange::is_inert")]`, and `named_ranges`
  re-lists all eight names. `macro_rules!` cannot generate per-field attributes
  without generating the whole struct, and `named_ranges`'s exhaustive
  destructure is *load-bearing*: the doc comment at `:262` says a ninth action
  must stop compiling there until it is named, which is exactly the guarantee a
  macro would destroy. Repetition here is the safety mechanism.
- **`src/app/session/tests.rs`** — nine near-identical `session_loop(...)`
  harness blocks (gate + journal + `Controller::new(...)` + three channels + an
  8-argument call), the crate's largest single duplication cluster by line count.
  Not a macro candidate: a fixture *struct* plus a builder function is the
  answer, and the file already uses that pattern (`off()`, `recording()`,
  `armed()`, `armed_recovering()`, `never_shutdown()`, `timings()`). Belongs to
  `test-`, not here.
- **`src/actuator/mod.rs` tests** — 17 copies of
  `rig.job_tx.send(plan::refresh_job(Trigger::Refreshed, Timings::default(), 0, N)).await.unwrap()`.
  Same verdict: a `fn send_refresh(&Rig, seed)` helper, not a macro. `test-`
  territory.
