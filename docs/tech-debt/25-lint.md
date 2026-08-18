# 25 — Clippy & Linting (`lint-`)

**Category priority:** LOW
**Rules audited:** 13 · **Files read:** 25 in full + 9 in part (targeted at every site cited below) + `Cargo.toml`, `justfile`, `.github/workflows/ci.yml`, `deny.toml`, `rust-toolchain.toml` · **Findings:** 10 (P0 0 / P1 1 / P2 3 / P3 6)

## Verdict

The sibling's report is correct and I can put numbers on it: `cargo clippy --all-targets
--all-features` emits **exactly zero diagnostics**, and so do the default lane and the
`--no-default-features` lane; `cargo fmt --check` produces zero bytes of output. The
crate also contains **zero `#[allow(...)]`** and exactly one lint suppression — an
exemplary `#[expect(..., reason = "…")]`. So there is nothing to clean up under the
default lint set, and no suppression debt to audit. What is genuinely missing is the
*written-down* policy: there is no `[lints]` table, no `clippy.toml`, no crate-level
`#![warn]`, and the enforced set exists only as four `cargo clippy … -- -D warnings`
invocations in CI and the `justfile`. The one lane nothing gates at all is **rustdoc**,
and it is not clean: `cargo doc --no-deps` emits **7 warnings** (8 with private items),
including two genuinely unresolved intra-doc links. In a crate whose stated design value
is dense, cross-referenced explanatory comments, that is the single highest-value fix —
`lint-001`. Worst offender file for the ungated-doc problem is `src/actuator/mod.rs`
(3 of the 8); for the ungated-lint problem it is `Cargo.toml`, by omission.

### Measured baseline (all runs on rustc/clippy 1.92.0, this machine)

| Command | Result |
|---|---|
| `cargo clippy --all-targets --all-features` | **0 diagnostics**, exit 0 |
| `cargo clippy --all-targets` (default features) | **0 diagnostics**, exit 0 |
| `cargo clippy --no-default-features --all-targets` | **0 diagnostics**, exit 0 — the portable lane *does* build |
| `cargo fmt --check` | passes, 0 bytes of output |
| `cargo doc --no-deps` | **7 warnings** |
| `cargo doc --no-deps --document-private-items` | **8 warnings** |

The `Cargo.toml` comment about the portable lane is accurate: with `pcap-backend` off,
`app::build_source` (`src/app/mod.rs:1112`) returns
`Error::Capture("no capture backend compiled — enable \`pcap-backend\` (the default) on Windows")`.

### Measured candidate-lint counts

Two columns, because they lead to opposite conclusions. "lib+bin" is
`cargo clippy --lib --bins` (shipped code only); "all targets" adds the test harness and
the example. Every count is unique `(lint, file:line)` sites.

| Lint | lib+bin | all targets |
|---|---:|---:|
| `clippy::correctness` / `suspicious` / `style` / `complexity` / `perf` (whole groups) | **0** | **0** |
| `clippy::undocumented_unsafe_blocks` | **0** | **0** |
| `clippy::todo` · `unimplemented` · `dbg_macro` · `mem_forget` · `exit` · `float_cmp_const` | **0** | **0** |
| `unexpected_cfgs` · `trivial_casts` | **0** | **0** |
| `clippy::unwrap_used` | **0** | 257 |
| `clippy::expect_used` | 13 | 157 |
| `clippy::panic` | **0** | 19 |
| `clippy::indexing_slicing` | 3 | 112 |
| `clippy::arithmetic_side_effects` | 36 | 39 |
| `clippy::as_conversions` | 54 | 73 |
| `missing_docs` | 141 | 141 |
| `clippy::missing_assert_message` | 18 | 18 |
| `unused_qualifications` | 6 | 26 |
| `unreachable_pub` | 5 | 5 |
| `clippy::print_stdout` / `print_stderr` | 4 / 3 | 5 / 4 |
| `clippy::multiple_unsafe_ops_per_block` | 5 | 5 |
| `clippy::string_slice` | 1 | 1 |
| **`clippy::pedantic` (whole group)** | **161** | **198** |
| **`clippy::nursery` (whole group)** | **121** | **128** |

Pedantic breakdown (lib+bin): `must_use_candidate` 41 · `doc_markdown` 18 ·
`borrow_as_ptr` 15 · `cast_precision_loss` 12 · `cast_possible_wrap` 10 ·
`missing_errors_doc` 9 · `cast_possible_truncation` 8 · `needless_pass_by_value` 8 ·
`format_push_string` 6 · `cast_sign_loss` 5 · `map_unwrap_or` 5 ·
`redundant_closure_for_method_calls` 5 · `ignored_unit_patterns` 4 · `too_many_lines` 4 ·
then 10 lints at 1–2 sites.

Nursery breakdown (lib+bin): `missing_const_for_fn` 41 · `use_self` 24 ·
`too_long_first_doc_paragraph` 18 · `redundant_pub_crate` 14 · `option_if_let_else` 12 ·
`suboptimal_flops` 6 · `significant_drop_tightening` 3 · `needless_collect` 1 ·
`needless_pass_by_ref_mut` 1 · `redundant_clone` 1.

## Findings

### lint-001 — Rustdoc is the one ungated lane, and it is not clean: 8 warnings including 2 dead intra-doc links

- **Severity:** P1
- **Rule:** [`lint-workspace-lints`](../../.claude/skills/rust-skills/rules/lint-workspace-lints.md) (its `[lints.rustdoc]` table and its `RUSTDOCFLAGS="-D warnings" cargo doc` CI step); cross-refs [`lint-missing-docs`](../../.claude/skills/rust-skills/rules/lint-missing-docs.md) ("Combining with doc Attributes")
- **Site:** all 8, from `cargo doc --no-deps --document-private-items`:
  - **Genuinely unresolved** (renders as plain text, no target exists):
    - `src/actuator/mod.rs:58` — ``/// Shared with [`setup`]'s live-edit path`` — there is no
      `setup` in `crate::actuator`; the intended target is `crate::app::setup`.
    - `src/migrate.rs:81` — ``/// Call it before [`crate::main`]'s logging setup`` — `main`
      lives in the *binary* crate (`src/main.rs`), so `crate::main` cannot resolve from the lib.
  - **Resolves only under `--document-private-items`** (i.e. broken in the docs anyone
    would actually generate): `src/actuator/mod.rs:248` (`` [`fail`] ``),
    `src/actuator/mod.rs:251` (`` [`drop_reason`] ``), `src/actuator/win.rs:561`
    (`` [`shield`](super::shield) ``), `src/config/persist.rs:138` (`` [`tidy`] ``),
    `src/capture/pcap.rs:616` (`` [`PcapStop`] ``).
  - **Redundant explicit target:** `src/actuator/win.rs:561` — ``[`shield`](super::shield)``;
    the label already resolves to the same destination.
- **What:** Neither `.github/workflows/ci.yml` nor the `justfile` runs `cargo doc` at all.
  Both gate `cargo fmt --all --check` and `cargo clippy … -D warnings` across four feature
  combinations, and then stop. rustdoc's `broken_intra_doc_links` and
  `private_intra_doc_links` are warn-by-default, so these 8 have simply never been read.
- **Why it matters here:** The audit brief states the comments in this crate are
  "unusually dense and explanatory *on purpose*", and the code bears that out — the
  doc-comments are the design record, and they navigate by `[`Type::method`]` links. Two of
  those links are dead and five silently degrade to plain text in a normal `cargo doc`,
  so the cross-reference network the documentation depends on is partially broken with
  nothing to notice it. This is also the only category of defect in my whole audit where
  the crate is actually *wrong* rather than merely unconfigured. `src/config/persist.rs`
  already demonstrates the correct fix two lines away, at :112–113:
  ```
  /// [`CaptureConfig::retired_keys`]: super::CaptureConfig::retired_keys
  ```
  so the idiom is established in the very file that also gets it wrong at :138.
- **Fix:** Three parts.
  1. Fix the 8 sites: give the five private-item links an explicit reference definition
     in the `super::…` form used at `persist.rs:112`; change `` [`setup`] `` to
     `` [`crate::app::setup`] ``; demote `` [`crate::main`] `` to plain `` `main` ``
     (it is unreachable from the lib by construction); drop the redundant `(super::shield)`.
  2. Add to `Cargo.toml`:
     ```toml
     [lints.rustdoc]
     broken_intra_doc_links = "deny"
     private_intra_doc_links = "warn"
     redundant_explicit_links = "warn"
     ```
  3. Add a CI step so it stays fixed — beside the existing clippy steps in
     `.github/workflows/ci.yml`, and a `doc` recipe in the `justfile`'s `verify`:
     ```yaml
     - name: Check the documentation cross-references
       run: cargo doc --locked --no-deps --document-private-items
       env:
         RUSTDOCFLAGS: -D warnings
     ```
     `--document-private-items` is the right mode for a `publish = false` binary: it is the
     only way the two extra internal links get checked, and it catches the strictly larger set.
- **Effort:** small

### lint-002 — No `[lints]` table anywhere: the enforced lint set is four CLI strings, invisible to every editor

- **Severity:** P2
- **Rule:** [`lint-deny-correctness`](../../.claude/skills/rust-skills/rules/lint-deny-correctness.md), [`lint-warn-suspicious`](../../.claude/skills/rust-skills/rules/lint-warn-suspicious.md), [`lint-warn-style`](../../.claude/skills/rust-skills/rules/lint-warn-style.md), [`lint-warn-complexity`](../../.claude/skills/rust-skills/rules/lint-warn-complexity.md), [`lint-warn-perf`](../../.claude/skills/rust-skills/rules/lint-warn-perf.md), [`lint-workspace-lints`](../../.claude/skills/rust-skills/rules/lint-workspace-lints.md)
- **Site:** `Cargo.toml` (no `[lints]` section — verified by read and by
  `cargo metadata`); no `clippy.toml`/`.clippy.toml`; no `#![deny]`/`#![warn]` in
  `src/lib.rs` (41 lines) or `src/main.rs` — the only crate-level attribute in either is
  `src/main.rs:6`, `#![cfg_attr(all(windows, feature = "gui"), windows_subsystem = "windows")]`.
- **What:** The five default-on groups the rule files ask for are enforced only because
  CI passes `-- -D warnings`. Nothing in the repository *states* the policy, so it is not
  visible to `rust-analyzer`, to a plain `cargo build`, or to a reader of `Cargo.toml`.
  `correctness` and `suspicious` are also merely `warn`-severity locally, where the rule
  files call for `deny`.
- **Why it matters here:** This is a debt-of-omission rather than a defect, and it is
  **free to pay**: all five groups measure **0 sites** in every lane, so the table below
  changes no build outcome today. Its value is that the four CI strings and the developer's
  editor stop being able to disagree — and per the user's memory note, this repo already
  fights a rust-analyzer/`cargo` divergence problem, so editor-visible policy is worth more
  here than in an average crate. It is also the anchor every other finding hangs off:
  `lint-003`, `lint-004`, `lint-006`, `lint-007`, `lint-008` and `lint-009` are all
  additional rows in this one table.
- **Fix:** Add to `Cargo.toml`. Every entry is annotated with its measured site count, so
  the reviewer can see that nothing here is a migration:
  ```toml
  [lints.rust]
  unexpected_cfgs = { level = "warn", check-cfg = [] }  # 0 — see lint-005
  unused_qualifications = "warn"                        # 6 — see lint-006
  unreachable_pub = "warn"                              # 5 — see lint-007
  trivial_casts = "warn"                                # 0

  [lints.clippy]
  # Whole groups: 0 sites each, verified on all four feature lanes.
  correctness = { level = "deny", priority = -1 }        # 0
  suspicious  = { level = "deny", priority = -1 }        # 0
  style       = { level = "warn", priority = -1 }        # 0
  complexity  = { level = "warn", priority = -1 }        # 0
  perf        = { level = "warn", priority = -1 }        # 0
  ```
  `priority = -1` is required so the individual entries added by the findings below can
  override a group. Two caveats for whoever lands this:
  - CI runs a `stable` matrix leg as well as the pinned 1.92.0. An unknown lint name emits
    `warning[E0602]`, which `-D warnings` turns into a hard failure — so a clippy lint that
    is renamed or removed upstream will break the `stable` leg. Prefer group names (stable)
    plus the individually-named lints below, all of which I verified exist on 1.92.0.
  - A `[lints]` table applies to **every** target, the test harness included. That is why
    `unwrap_used` and `panic` are *not* in the block above — see `lint-003`.
- **Effort:** trivial

### lint-003 — `unwrap_used`, `panic`, `todo`, `unimplemented`, `dbg_macro` are all at zero in shipped code: five free ratchets, but they need test scoping

- **Severity:** P2
- **Rule:** [`lint-workspace-lints`](../../.claude/skills/rust-skills/rules/lint-workspace-lints.md) (its "Restriction (selective)" block)
- **Site:** measured across the whole crate. In `--lib --bins`: `unwrap_used` 0,
  `panic` 0, `todo` 0, `unimplemented` 0, `dbg_macro` 0, `mem_forget` 0, `exit` 0.
  In `--all-targets`: `unwrap_used` 257, `panic` 19 — i.e. **every single one is in test
  code**. Largest test concentrations: `src/app/mod.rs` 85, `src/actuator/mod.rs` 57,
  `src/actuator/win.rs` 33, `src/app/session/tests.rs` 30, `src/uplink/websocket.rs` 23.
- **What:** The shipped binary contains no `unwrap`, no `panic!`, no `todo!`, no
  `unimplemented!` and no `dbg!` at all. That is a real, hard-won property of this
  codebase — the error paths go through `Result`, `saturating_*`, `unwrap_or_else`,
  poison-tolerant `PoisonError::into_inner`, and `Option`-returning guards throughout —
  and nothing currently prevents the next commit from breaking it.
- **Why it matters here:** The app is a windowed binary with no console
  (`#![cfg_attr(…, windows_subsystem = "windows")]`), so a panic on a worker task is
  invisible except through `src/crash.rs`. `src/stream.rs:186-191` documents the sharpest
  version of this: a panic raised from `PayloadLease::drop` during an unwind aborts the
  process with *no* banner and *no* `crash.log`. A lint that keeps `unwrap` out of the
  shipped build is directly protecting that. Zero sites today means the ratchet costs
  nothing to install.
- **Fix:** These five cannot go in the `[lints.clippy]` table, because the table also
  applies to the test-harness compilation and CI runs `--all-targets -- -D warnings` —
  276 test-code warnings would fail the build immediately. Put them at crate level,
  gated off for the test build, in **both** `src/lib.rs` and `src/main.rs`:
  ```rust
  // Shipped code has none of these today (measured: 0 sites in --lib --bins).
  // `not(test)` keeps the ratchet off the test harness, where `unwrap` in a
  // fixture is the correct spelling — 257 sites and rising.
  #![cfg_attr(
      not(test),
      warn(
          clippy::unwrap_used,
          clippy::panic,
          clippy::todo,
          clippy::unimplemented,
          clippy::dbg_macro,
      )
  )]
  ```
  Deliberately **not** included, with reasons:
  - `expect_used` — 13 shipped sites, every one intentional and load-bearing
    (`src/main.rs:120` installs the rustls `ring` provider whose absence panics at
    handshake anyway; `src/app/session/mod.rs:200/237/326/357/406`;
    `src/actuator/mod.rs:95/103/210/218`; `src/stream.rs:508/527/782`, where the messages
    *are* the invariant, e.g. `"a burst flow is never empty"`). Set
    `expect_used = "allow"` explicitly if the group form is ever used.
  - `indexing_slicing` — only 3 shipped sites, and all three are provably in bounds:
    `src/capture/pcap.rs:381` (`field[0]`/`field[1]` on a slice just obtained from
    `frame.get(at..at + 2)?`), `src/ui/editor/mod.rs:181` (`parts[..3]` in the
    `else` of `if parts.len() <= 3`), `src/ui/journal.rs:121` (`&journal[rows]` where
    `rows` is the range egui's `show_rows` derived from `journal.len()`). Three forced
    `#[expect]`s buys nothing.
  - `arithmetic_side_effects` (36) and `as_conversions` (54) — see `lint-009`.
- **Effort:** trivial

### lint-004 — `undocumented_unsafe_blocks` reports zero: enable it to keep it that way

- **Severity:** P2
- **Rule:** [`lint-unsafe-doc`](../../.claude/skills/rust-skills/rules/lint-unsafe-doc.md)
- **Site:** whole crate. Measured: **0 omissions** in both `--lib --bins` and
  `--all-targets`.
- **What:** This confirms the `unsafe-` reviewer independently: every `unsafe` block in
  the crate already carries a real `// SAFETY:` comment. I read `src/actuator/shield.rs`
  and `src/migrate.rs` in full and the comments are substantive, not ceremonial — e.g.
  `migrate.rs:172-182` explains the four optional out-parameters, the single-`LocalAlloc`
  ownership of the descriptor, and the exact failure mode; `shield.rs:41-44` explains why
  `GetWindow` on a dead HWND is defined behaviour.
- **Why it matters here:** The lint is currently costless and stays costless only by
  luck. This is an FFI-heavy crate — `windows-sys` plus a runtime-loaded `wpcap.dll` —
  where new `unsafe` arrives with every Win32 call added, and the 100%-documented state is
  exactly the kind of property that erodes silently. Turning it on converts a convention
  into a check.
- **Fix:** Add to the `[lints.clippy]` table from `lint-002`:
  ```toml
  undocumented_unsafe_blocks = "warn"      # 0 sites today
  ```
  Also worth adding, and the reason it is a separate line:
  ```toml
  # 5 sites, all pre-existing and already filed under `unsafe-` as P2. Land this
  # only together with those fixes, or it fails CI's `-D warnings` on day one.
  multiple_unsafe_ops_per_block = "warn"   # 5 sites
  ```
  The 5 are `src/capture/pcap.rs:239` (13 unsafe ops in one block), `:768` (2), `:789` (5),
  and `src/migrate.rs:183` (3), `:258` (2). The `trait-` reviewer raised this lint and the
  individual sites are filed under `unsafe-` — I am formalising the lint here and
  deliberately not re-listing the site rationales.
- **Effort:** trivial for `undocumented_unsafe_blocks`; the `multiple_unsafe_ops_per_block`
  half is gated on the `unsafe-` P2 work.

### lint-005 — `unexpected_cfgs` is already clean, and already on: no cfg typo exists

- **Severity:** P3
- **Rule:** [`lint-cfg-check`](../../.claude/skills/rust-skills/rules/lint-cfg-check.md)
- **Site:** every `cfg` predicate in `src/`, `build.rs` and `examples/`.
- **What:** The brief flagged this as the likely P0/P1 of the category. It is not one, and
  the evidence is two independent checks that agree. First, `unexpected_cfgs` measures
  **0 sites** — and since it is warn-by-default from Rust 1.80, it has been running on
  every CI build all along. Second, the exhaustive diff of every cfg string against the
  declared feature names:

  | cfg predicate | uses | declared in `Cargo.toml`? |
  |---|---:|---|
  | `cfg(test)` | 46 | built-in |
  | `cfg(feature = "gui")` / `not(...)` | 8 / 2 | ✅ `gui` |
  | `cfg(all(windows, feature = "actuator"))` / `not(...)` | 6 / 3 | ✅ `actuator` |
  | `cfg(all(windows, feature = "pcap-backend"))` / `not(...)` / `any(test, ...)` | 3 / 1 / 1 | ✅ `pcap-backend` |
  | `cfg(windows)` | 5 | built-in |
  | `cfg(target_pointer_width = "64")` | 2 | built-in |
  | `cfg(debug_assertions)` / `not(...)` | 1 / 1 | built-in |
  | `cfg_attr(all(windows, feature = "gui"), …)` | 1 | ✅ `gui` |
  | `cfg!(feature = …)` × 3 (`gui`, `pcap-backend`, `actuator`) | 3 | ✅ all three |

  Zero mismatches, zero custom cfgs, so nothing needs a `check-cfg` entry. Two structural
  reasons the codebase is resistant here, both worth recording so a later reader does not
  "harden" what is already safe: every feature gate is paired with an explicit `not(...)`
  arm supplying the same item (`src/app/mod.rs:1101`/`:1112`, `:270`/`:281`,
  `src/main.rs:255`/`:237`), so a typo in either arm produces a duplicate-or-missing-symbol
  *compile error* rather than silent dead code; and `src/main.rs:134-136` prints all three
  feature flags into the startup log, so a lane built wrong announces itself at runtime.
- **Why it matters here:** It doesn't, as a defect — this is a clean result and the report
  says so rather than manufacturing a finding. The only residual value is documentary.
- **Fix:** Include `unexpected_cfgs = { level = "warn", check-cfg = [] }` in the
  `[lints.rust]` table from `lint-002`. It is a no-op today; its purpose is to give the
  next custom cfg (a `coverage_nightly`, a `loom`) an obvious declared home instead of an
  invisible one.
- **Effort:** trivial

### lint-006 — `unused_qualifications`: 6 sites, all `std::mem::size_of*`, now in the Rust 2024 prelude

- **Severity:** P3
- **Rule:** [`lint-workspace-lints`](../../.claude/skills/rust-skills/rules/lint-workspace-lints.md) (its `[workspace.lints.rust]` "Quality" block names this lint)
- **Site:** `src/actuator/win.rs:502`, `src/capture/mod.rs:80`, `src/capture/mod.rs:82`,
  `src/migrate.rs:261`, `src/stream.rs:421`, `src/stream.rs:423`
- **What:** All six are `std::mem::size_of::<T>()` / `std::mem::size_of_val(&x)`. Since
  Rust 1.80 these live in the prelude, so the `std::mem::` prefix is redundant. Example,
  `src/actuator/win.rs:502`:
  ```rust
  let inserted = unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) };
  ```
- **Why it matters here:** Marginal on its own — but four of the six sit in the size
  canaries (`src/capture/mod.rs:78-83`, `src/stream.rs:418-424`) that this crate uses as
  deliberate compile-time tripwires against per-packet struct growth. Those blocks are
  read closely whenever a field is added, so keeping them at minimum noise has some real
  value, and the lint makes it a one-time fix rather than a recurring judgement.
- **Fix:** Drop the `std::mem::` prefix at the six sites and add
  `unused_qualifications = "warn"` to `[lints.rust]` (already listed in `lint-002`).
- **Effort:** trivial

### lint-007 — `unreachable_pub`: 5 `pub` items inside private modules, advertising a reach they do not have

- **Severity:** P3
- **Rule:** [`lint-workspace-lints`](../../.claude/skills/rust-skills/rules/lint-workspace-lints.md)
- **Site:** `src/ui/view.rs:9` (`pub struct ViewState`), `:40` (`pub struct SlotRow`),
  `:53` (`pub fn view_state`); `src/ui/editor/mod.rs:29` (`pub struct EditorState`),
  `:52` (`pub fn EditorState::new`)
- **What:** `src/ui/mod.rs:11-16` declares `mod editor;` … `mod view;` — both private. So
  the `pub` on those five items grants nothing beyond `pub(super)`; the effective
  visibility is the `ui` module. `ui/mod.rs` itself gets this right elsewhere:
  `content_inset` at :372 is correctly `pub(super)`, and `ui/journal.rs:24`/`:110` and
  `ui/editor/timing_meter.rs:12`/`:62` are all `pub(super)`.
- **Why it matters here:** The brief is explicit that `pub` on an internal item is not a
  public API and that I should not file forty findings about it — so this is deliberately
  one collapsed P3, not five. The reason it is worth the one line: `ViewState` and
  `SlotRow` are the crate's egui-free projection boundary (`ui/mod.rs:1-3`: "No egui type
  crosses into the app layer or the domain"), and a reader auditing that boundary reads
  visibility as the signal for it. Five items claiming crate-wide reach they do not have
  makes that signal noisier than the surrounding, correctly-annotated code.
- **Fix:** Change the five to `pub(super)`, matching the sibling modules; add
  `unreachable_pub = "warn"` to `[lints.rust]` (already listed in `lint-002`).
- **Effort:** trivial

### lint-008 — `clippy::redundant_pub_crate`: 14 sites, 13 of them in the private module `src/render.rs`

- **Severity:** P3
- **Rule:** [`lint-clippy-nursery-selected`](../../.claude/skills/rust-skills/rules/lint-clippy-nursery-selected.md)
- **Site:** `src/render.rs` (13 sites: `HAUL_HEADLINERS`, `haul_tally`, `kind_label`,
  `grouped`, `grouped_or_dash`, `status_summary`, `status_label`, `refusal`, `describe`,
  `merchant_label`, `render_shop`, `format_item`, `print_controls`), plus
  `src/capture/pcap.rs:1` (1 site)
- **What:** `src/lib.rs:24` declares `mod render;` — private, unlike every one of its
  siblings. So `pub(crate)` inside it is exactly `pub(super)`, i.e. redundant. The same
  root cause as `lint-007`, one visibility level up.
- **Why it matters here:** Same reasoning and same restraint — one collapsed finding.
  `render.rs` is genuinely reached from both `app` (console output, via
  `crate::render::print_controls` at `src/app/mod.rs:24`) and `ui` (`src/ui/view.rs:5`),
  so the *author's* intent behind `pub(crate)` is legible; it is the `mod render;`
  declaration that is narrower than the usage pattern suggests. Whoever fixes this should
  decide which of the two to change — making `render` `pub(crate) mod` and keeping the
  members, or keeping `mod` and demoting 13 members — rather than mechanically demoting.
- **Fix:** Pick one direction, then add `redundant_pub_crate = "warn"` to
  `[lints.clippy]`. This is the lint that will make the choice stick either way.
- **Effort:** small

### lint-009 — Selective pedantic/nursery: 3 lints worth enabling, and 8 that must not be, with the counts that decide it

- **Severity:** P3
- **Rule:** [`lint-pedantic-selective`](../../.claude/skills/rust-skills/rules/lint-pedantic-selective.md), [`lint-clippy-nursery-selected`](../../.claude/skills/rust-skills/rules/lint-clippy-nursery-selected.md)
- **Site:** as tabulated below
- **What:** Both rule files say the same thing — cherry-pick, never enable the group.
  This crate makes that concrete: `pedantic` as a group is 161 shipped sites and
  `nursery` is 121, and I read the sites. The great majority are the crate deliberately
  doing something the lint has no way to know is intended.

  **Worth enabling (add to `[lints.clippy]`):**

  | Lint | Sites | Why here |
  |---|---:|---|
  | `doc_markdown` | 18 | `migrate.rs` 6, `capture/pcap.rs` 5, `actuator/win.rs` 2, +5. In a crate whose docs *are* the design record (and after `lint-001`), unbackticked identifiers in prose are worth catching. |
  | `format_push_string` | 6 | All six in one function, `src/render.rs:130-160` (`format_item`), all of the form `line.push_str(&format!(" · {name}"))`. `write!(line, …)` removes six throwaway `String`s from the shop-item render path. A genuine, contained improvement. |
  | `redundant_clone` | 1 | `src/main.rs:307`, `config_path.clone()` inside the `eframe::run_native` app-creator closure. One site, so it is nearly free — but verify the closure's `Fn`-ness before removing the clone, since that is why it is there. |

  **Must NOT be enabled — the sites are deliberate, and I checked them:**

  | Lint | Sites | Evidence against |
  |---|---:|---|
  | `must_use_candidate` | 41 | Pure accessors on internal types (`actuator/plan.rs` 13, `domain/control/mod.rs` 13). The crate already applies `#[must_use]` where it *carries meaning* — `src/main.rs:66`, `src/migrate.rs:83`, `src/ui/mod.rs:250`, `src/actuator/win.rs:603` — each with a `= "reason"` string. 41 mechanical additions would dilute exactly that signal. |
  | `cast_possible_truncation` / `_wrap` / `_sign_loss` / `_precision_loss` | 8 / 10 / 5 / 12 | FFI- and screen-geometry-load-bearing, and already documented at the site. `src/stream.rs:800` carries a five-line comment explaining `(next_off as u64) as u32` **is** the intended mod-2³² conversion. `src/actuator/win.rs:471` says "Clamped, not truncated" and explains why a wrapped `as i32` would aim anywhere. `src/actuator/plan.rs:152` is `.round() as i32` on a screen coordinate. `src/domain/control/mod.rs:492` is `targets.len() as u32` on a ≤6-slot shop. 35 forced `#[expect]`s over prose that is already better than the attribute. |
  | `as_conversions` | 54 | `actuator/win.rs` 19, `ui/editor/timing_meter.rs` 8, `actuator/shield.rs` 6, `capture/pcap.rs` 5. `HWND`/`isize` round-trips and `ERROR_ACCESS_DENIED as i32` are the vocabulary of `windows-sys`. Not negotiable in this crate. |
  | `arithmetic_side_effects` | 36 | Sampled every distinct pattern; all are provably bounded, and the crate uses `checked_*`/`saturating_*` wherever they are not. `src/domain/control/mod.rs:538` is `balance - price` reached only under `in_reach`, which implies `price <= balance`. `src/render.rs:46` is `(len - 1) / 3` where `len = u32::to_string().len() >= 1`. `src/domain/control/watchdog.rs:69` is `attempt + 1` on a u8 ladder capped at 3. `src/actuator/plan.rs:211` is `% modulus` guarded by a `checked_add(1)` whose `None` arm exists precisely to avoid `% 0`. |
  | `float_cmp` | 2 | **Both sites are inside rustlib**, `core/src/macros/mod.rs:46` and `:59` — i.e. the expansion of `assert_eq!` on floats in test code, not this crate's own comparisons. Enabling it produces two warnings pointing into `std` that cannot be suppressed at the offending line. Concrete example of a pedantic lint that is unusable here. |
  | `use_self` | 24 | `actuator/plan.rs` 18. A consistent, deliberate house style: match arms spell the type (`Trigger::ShopOpened`, `TimingPreset::Instant`, `Input::Click`) while constructors already use `Self` (`Jitter::new` at `plan.rs:428`, `Controller::new` at `control/mod.rs:281`). The lint would fight the readable half of a convention the crate applies coherently. |
  | `missing_const_for_fn` | 41 | Real (`Trigger::pre_wait_ms`, `DelayRange::is_inert`, `Input::at` could all be `const fn`) and of no value in a binary that never evaluates them at compile time. 41 diffs for zero behaviour change. |
  | `suboptimal_flops` | 6 | `actuator/plan.rs` 5, in `Jitter::point_in`/`unit`. `mul_add` on a code path that runs a handful of times per shop refresh. Pure noise. |
  | `missing_assert_message` | 18 | 13 of the 18 are *compile-time* tripwires with a prose block above them explaining the invariant: `ui/editor/timing_meter.rs:45-52` (8 × `const _: () = assert!(plan::WAIT_… <= RULER_MS_U64)`), `stream.rs:421/423` and `capture/mod.rs:80/82` (size canaries), `stream.rs:98-100` (`with_limits` invariants on constants). The crate already adds messages where a human will read one at runtime — `stream.rs:176`, `:203-204`, `:466-469`. |
  | `too_long_first_doc_paragraph` | 18 | Directly contradicts the brief's instruction not to file against this crate's deliberately dense doc-comments. |
  | `significant_drop_tightening` | 3 | `src/actuator/shield.rs:142`, `src/app/session/mod.rs:357`, `src/stream.rs:168`. Not mine to judge: `shield.rs:142` holds the `WINDOW` mutex across `spawn_window()` on purpose (it is what prevents two shields), and lock-scope questions belong to the `conc-` reviewer. Recording the three sites and the count so `conc-` can rule; not recommending the lint either way. |
- **Why it matters here:** The measured cost of getting this wrong is 282 warnings and a
  broken CI leg. The measured cost of getting it right is three lints and roughly a dozen
  lines of diff. This finding exists so nobody enables `pedantic = "warn"` on the strength
  of the rule file's title.
- **Fix:** Add only the three from the first table to the `lint-002` block, fix their
  sites, and paste the second table's reasoning into a comment above them so the next
  reviewer does not relitigate it.
- **Effort:** small

### lint-010 — `missing_docs`: 141 sites, and this crate should not enable it

- **Severity:** P3
- **Rule:** [`lint-missing-docs`](../../.claude/skills/rust-skills/rules/lint-missing-docs.md)
- **Site:** 141 sites, concentrated in `src/actuator/plan.rs` (40),
  `src/domain/control/mod.rs` (36), `src/domain/shop.rs` (16), `src/error.rs` (8),
  `src/actuator/mod.rs` (6), `src/app/mod.rs` (6), remainder spread thin.
- **What:** The largest single count in the whole audit, and — I am filing it as a
  recommendation *against* action. `publish = false`, and the brief is explicit that
  documentation rules aimed at public library surface are weaker here and that `pub` on
  an internal item is not a public API. The 141 are things like the `pub` fields of
  `Zone`/`ClientRect` in `actuator/plan.rs`, and the `StopReason`/`Status` variants and
  `Limits`/`Progress` fields in `domain/control/mod.rs`.
- **Why it matters here:** Two reasons this is worth a line rather than silence. First,
  the crate is already *mostly* documented where documentation carries information — every
  non-obvious variant in `domain/control/mod.rs` has a doc comment (`SessionEnded`,
  `ActuatorFailed`, `Unresponsive`, `Paused`, `max_spend`, `max_matches` all do), and the
  141 are the residue where the name genuinely is the documentation
  (`StopReason::OutOfFunds`, `Zone::cx`). A lint demanding `/// The cx.` on those would
  make the file worse, not better. Second, filing the number explicitly stops a later
  agent seeing "141" in a raw clippy run and treating it as a backlog.
- **Fix:** Do not add `missing_docs`. If a future release ever publishes this crate,
  revisit. Note that `lint-001`'s `[lints.rustdoc]` table covers the part of the
  documentation story that *is* actionable here — links that are broken, rather than docs
  that are absent.
- **Effort:** none (recommendation to not act)

## Clean areas

- **Baseline clippy is genuinely silent.** 0 diagnostics on `--all-targets --all-features`,
  on the default set, and on `--no-default-features --all-targets`. The five default-on
  groups (`correctness`, `suspicious`, `style`, `complexity`, `perf`) have nothing to say
  about this crate. Confirmed, not assumed.
- **`cargo fmt --check` passes** with zero output, on a crate with no `rustfmt.toml` —
  i.e. it conforms to stock rustfmt. `lint-rustfmt-check` is satisfied: the `justfile`'s
  `fmt-check` recipe and `.github/workflows/ci.yml` both run `cargo fmt --all --check`,
  the latter pinned to the 1.92.0 matrix leg (correct — it avoids stable-rustfmt drift
  breaking CI).
- **CI gating is better than the rule file asks for.** `lint-workspace-lints`' CI section
  wants one `cargo clippy --all-targets -- -D warnings`; this repo runs **four**, one per
  meaningful feature combination (`--no-default-features`, `+gui,actuator`,
  `+pcap-backend`, and the shipped default set), each `--locked`, mirrored exactly between
  `.github/workflows/ci.yml` and the `justfile`'s `verify`/`backends` recipes, on two
  toolchains. The comments in both files explain why there is no `--all-features` lane
  (with WinDivert gone, `--all-features` *is* the default set — which `cargo metadata`
  confirms).
- **Suppression hygiene is essentially perfect.** I grepped every `allow`/`expect`/`deny`/
  `forbid`/`warn` attribute in the crate. There are **zero `#[allow(...)]`** anywhere, and
  exactly one suppression in total: `src/app/session/mod.rs:34`, which uses `expect` (not
  `allow`, so it warns if it goes stale — the preference the brief asked me to check for)
  *and* carries a `reason = "…"` explaining why bundling the eight arguments would hide
  which handles the loop owns. That is the ideal form of this attribute; there is no
  suppression debt to audit in this crate.
- **`undocumented_unsafe_blocks` = 0.** Independently confirms the `unsafe-` reviewer.
- **`unexpected_cfgs` = 0**, and every one of the 8 feature-gate strings matches a declared
  feature (`gui`, `pcap-backend`, `actuator`). No cfg typo exists. See `lint-005`.
- **`trivial_casts` = 0**, `todo`/`unimplemented`/`dbg_macro`/`mem_forget`/`exit`/
  `float_cmp_const` = 0 everywhere.
- **`print_stdout`/`print_stderr` are correctly placed.** All 7 shipped sites are in the
  two places a console-only build legitimately prints: `src/render.rs:122/124/164` and
  `src/journal.rs:64` (behind `#[cfg(not(feature = "gui"))]`), and `src/main.rs:228/231/336`
  (startup-fatal paths, where the comment at `main.rs:220-223` explains stderr is inert in
  the windowed build and the log file is the real channel). The crate routes player-facing
  output through `EventLog::emit`, which mirrors to `tracing` with `target: "journal"` —
  the lint would fire on the two deliberate exceptions and nothing else.
- **The toolchain pins its own linters.** `rust-toolchain.toml` pins `channel = "1.92.0"`
  with `components = ["clippy", "rustfmt"]`, so `cargo clippy`/`cargo fmt` cannot be
  missing or version-skewed locally.

## Not applicable

- **`lint-cargo-metadata`** — N/A on two independent grounds, both verified.
  `publish = false`, so `cargo_common_metadata` is moot (and the manifest carries
  `description` and `license = "MIT"` anyway). More usefully: the only `clippy::cargo`
  output on this crate is `multiple_crate_versions`, and it is **29 duplicated
  dependencies** — `windows-sys` at four versions, `objc2-*` and `hashbrown` and `rustix`
  and `thiserror` at two or three — every one of them transitive through the
  `eframe`/`winit` tree and not resolvable from here. That job is already done better
  elsewhere: `deny.toml` sets `[bans] multiple-versions = "warn"`, `wildcards = "deny"`,
  and CI runs `cargo deny --all-features --locked check bans licenses sources` against a
  version-pinned `cargo-deny 0.20.2`. `clippy::cargo` would add 29 unactionable warnings
  and duplicate a tool that is already wired up. Do not enable it; set
  `cargo = "allow"` if a group form is ever introduced.
- **`lint-workspace-lints`, workspace half** — this is a single-crate repository, not a
  workspace (`cargo metadata` reports one package, no `[workspace]` section). The
  `[workspace.lints]` + `[lints] workspace = true` inheritance pattern has nothing to
  inherit between. The rule's *content* still applies and is filed as `lint-002` and
  `lint-001`, in the plain `[lints]` form.
