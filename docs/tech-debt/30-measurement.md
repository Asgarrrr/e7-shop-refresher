# 30 — Measurement: coverage and mutation

**Category priority:** HIGH (it is the gap the audit's own method section names)
**Tools:** `cargo-llvm-cov` 0.9.0 · `cargo-mutants` 27.1.0 · rustc 1.92.0 (the MSRV)
**Measured at:** `592d304`, on a pinned `git worktree` so three concurrent agents
editing `src/` could not move the tree under a run.

## Status — all eight holes closed, two of this report's claims wrong

Closed in `e8d5a61` (§3.1) and `e3f722b` (the rest). Every test was proved red by
applying the exact defect it targets and green again on restore. 599 → 607 tests.

| Hole | Test | Proved red by |
|---|---|---|
| §3.1 `Anchor::Center` | `to_screen_places_an_off_centre_center_anchor` | sign flip, reversed subtraction, `abs` |
| §3.2 delay salt | `every_builder_maps_distinct_seeds_to_distinct_delay_streams` | all 6 `^`→`&`/`\|` mutants across the 3 builders |
| §3.3 `max_spend` | `a_budget_that_divides_by_the_cost_is_fully_spent` | `>` → `>=` |
| C-1 outage protocol | `a_reconnect_reports_the_outage_and_its_end` | deleting `LinkUp`; muting `LinkDown` |
| C-2 outbound lease | `a_completed_send_returns_its_budget` | `drop(lease)` → `mem::forget` |
| C-3 recoverable acquire | `executor_aborts_without_halt_when_acquire_fails_recoverably` | `abort` → `fail`; merging the two arms |
| C-4 dry-run scroll | `executor_dry_run_journals_a_scroll_without_touching_the_surface` | silencing the `Scroll` arm |
| C-5 timeout arm | `an_elapsed_duration_halts_at_the_refresh_gate` | `if false && has_duration_elapsed(..)` |

**Two diagnoses in this report were wrong, and the implementers proved it rather
than working around it.**

- **C-2's defect is not reachable the way it is described.** Deleting the literal
  `drop(lease)` is a behavioural no-op — `lease` is a local binding in the
  `while let` body and drops at scope end regardless; the test stays green with
  the line removed. The real guarantee is that the iteration must not *retain*
  the lease (batching, parking a chunk for retry, moving it into the message),
  which is what `mem::forget` simulates honestly. The line-coverage claim was
  sound; the failure story was not.
- **C-5's arm is not defensive.** This report calls it a duplicate of a check
  `on_tick` catches within one tick. It is on two live routes: `on_tick`'s first
  arm (`Status::Watching` with no expectation armed) falls through to it, which
  is the mainline timeout for a run without recovery, and so does
  `refresh_or_halt`. The suite missed both by accident of fixture — one existing
  test is `Paused`, the other arms an expectation.

**A methodology warning worth more than any single finding.** The first pass on
§3.2 reported the three `^`→`&` mutants as *surviving*. They were not: `&` is the
whole-match backreference in a `sed` replacement, so the mutation never compiled,
and "no `test result: FAILED` line" was read as "the mutant lived". Escaping it
gave six reds. Anyone re-running these must discriminate *did not compile* from
*compiled and passed* — otherwise the harness reproduces the exact defect this
report exists to find: a result that held for a reason unrelated to its claim.

Also settled while closing §3.2: pinning the salt's bijection at
`DELAY_SEED_SALT` — which this report's brief suggested — **kills none of the
five mutants.** The mutated expression is at three call sites in `jobs.rs`;
`jitter.rs` has no operator to mutate, so a theorem about XOR asserted beside the
constant stays true whatever the builders do with it. The property has to be
observed through the builder's output.

## Why this report exists

`README.md`'s "Method and its limits" ends on a sentence that is really a filed
finding:

> No reviewer measured test coverage, because no CI lane does — which is
> precisely how `test-001` survived.

And `_HANDOFF.md` records the defect that makes the second half of this report
necessary: the guard test shipped for `const-003` — the one pinning "six
clickable rows, the top group is `0..=3`", the invariant whose failure clicks the
wrong item's Buy button and spends a player's real gold — **could not fail**. All
six of its assertions were derived from the two constants they were meant to pin,
so `LAST_TOP_ROW = 2`, `LAST_TOP_ROW = 4` and `MAX_ROW = 7` each passed the whole
suite. A human found that by reading it. A mutation run finds that entire class
by construction, because a test that cannot fail cannot kill a mutant either.

So this report answers two questions the 26 category reports could not: *what do
the tests reach*, and *what would they notice changing*.

## What was measured, and what was not

Stated first, because the rest of the document is only worth what this paragraph
allows.

**Measured.**

- Line and region coverage of the **shipped default feature set**
  (`pcap-backend + gui + actuator`) on Windows, from the same 593-test run
  `README.md` quotes. `cargo llvm-cov` needed no change to the crate: no
  `Cargo.toml` entry, no `cfg`, no attribute. It is an external binary, which is
  why it is acceptable here at all — every dependency this crate does *not* have
  (`proptest`, `tempfile`, `arrayvec`/`smallvec`) was declined in writing, and
  nothing in this report adds one.
- Mutation testing of `src/actuator/plan/{geometry,jobs}.rs` and all of
  `src/domain/control/`, likewise with no crate change: `cargo mutants` copies
  the tree to a scratch directory and never edits the working copy.

**Not measured, and the claims that therefore are not made.**

- **Branch coverage is zero everywhere in the table below, and that is a tool
  fact, not a code fact.** `cargo llvm-cov` reports `Branches 0/0 -` for every
  file because branch coverage needs `-Z coverage-options=branch`, which is
  nightly-only; this crate is pinned to the 1.92.0 MSRV. Nothing here should be
  read as "every branch is taken".
- **The mutation runs used `--no-default-features`, and that turned out to
  matter.** It was chosen for build time and it was the wrong trade: because
  `egui_kittest` is an *ungated* dev-dependency, every `cargo test` lane builds
  the whole egui tree anyway, so the saving was near zero — and several
  survivors below are artefacts of the reduced lane rather than holes. Rather
  than argue that from the source, **every survivor was replayed by hand against
  the shipped default lane** (`cargo test --locked --lib`, patch applied, patch
  reverted); each verdict below says which way that went. The `justfile` recipe
  and `.github/workflows/quality.yml` both use the **default** features for
  exactly this reason.
- **Nothing outside the five mutated files was mutation-tested.**
  `domain/shop.rs`, `domain/filter.rs`, `stream/budget.rs` and
  `stream/reassembly.rs` were on the list and were not reached; their coverage
  numbers are here but no mutant was ever built for them. Their line coverage
  being high says only that the tests *execute* them.
- **The workflow file was not run.** `.github/workflows/quality.yml` is written
  against verified local behaviour (both tools install and run on Windows against
  this crate; the `--shard k/4` and `--file` flags are the ones `cargo mutants
  --help` documents at 27.1.0), but no GitHub run has ever executed it. Its
  action SHAs *were* verified — see "Instrumentation" below, where the check also
  turned up a live defect in `ci.yml`.
- **Two survivor mutants were judged by argument rather than replayed**
  (`jobs.rs:87`/`125` `^` → `|`, and `jobs.rs:101` `^` → `|`). Their `&`
  siblings at the same sites were replayed and survive, and the argument in §3.2
  covers both operators, but the `|` variants themselves were not re-run on the
  default lane.
- **The tree moved while this ran.** Three other agents were editing `src/`
  concurrently; every line number, every snippet and every percentage here is
  from `592d304` and will drift. In particular `domain/control` has since grown
  `Gold`/`Crystals` newtypes, so the `Option<u32>` in two survivor lines below is
  already spelled differently at HEAD.

## 1. Coverage

### Headline

| | | |
|---|---|---|
| Lines | 10 773 / 12 692 | **84.88%** |
| Regions | 17 240 / 20 182 | **85.42%** |
| Functions | 1 284 / 1 438 | **89.29%** |
| Branches | — | not measurable on stable |

That is the number `cargo llvm-cov --summary-only` prints, and it is flattering
for a reason worth stating before the ranking: **the inline `#[cfg(test)] mod
tests { … }` block at the bottom of most files is counted**, and test code
executes itself, so it lands near 100% and pulls its file up with it. (The
crate's three standalone `*/tests.rs` files — `domain/control/tests.rs`,
`app/session/tests.rs`, `config/tests.rs` — are excluded by the tool's own
default filename filter, which is why the two halves of the same convention are
counted differently.)

Recomputing over production lines only — every line before the file's own
`#[cfg(test)] mod …` opener dropped — gives the honest figure:

**Production-only line coverage: 4 545 / 6 243 = 72.80%.**

Two further tool facts to read the per-file numbers with:

- A file that is only `#[derive(Deserialize)]` shapes reports *no* production
  lines at all, because derive-generated code is attributed to the macro rather
  than to the call site. `uplink/protocol.rs` is the whole file: it is absent
  from the ranking below, and that means "nothing to measure", not "untested" —
  its wire contract is in fact pinned by seven tests.
- Every `tracing::debug!`/`warn!` body shows as uncovered under `cargo test`,
  because no subscriber is installed and the macro short-circuits before it
  formats its fields. All six of `domain/shop.rs`'s uncovered production lines
  are this, and the surrounding degradation *is* asserted.

### Ranked: the weakest production modules

Production lines only, weakest first. Everything at or above 95% is folded into
the tail note.

| Cover | Lines | File | Reading |
|---|---|---|---|
| 0.00% | 0/125 | `src/main.rs` | Known and filed as `test-006`. Dispatch plus the two `run_mode` arms, neither testable without a display. `proj-003` already moved the testable startup policy out to `lib.rs`. |
| 2.08% | 3/144 | `src/capture/pcap/mod.rs` | Needs Npcap and a live NIC. Not a hole a test can close. |
| 2.88% | 3/104 | `src/migrate.rs` | **The lowest-covered module that *destroys* something**: it deletes files under `%LOCALAPPDATA%` and rewrites a protected DACL. It genuinely touches the filesystem and Win32, so it is not a pure-logic hole — but it is the one place where an untested path removes a player's files, and `config/mod.rs`'s own `TempDir` guard is the fixture that would let it be tested (see `_HANDOFF.md`'s `test-006` note on promoting that guard rather than adding `tempfile`). |
| 5.21% | 11/211 | `src/capture/pcap/sys.rs` | The `wpcap.dll` ABI transcription; `04-unsafe.md` treats it as review surface, not test surface. |
| 8.51% | 12/141 | `src/actuator/shield.rs` | Windows foreground/UIPI manipulation — needs a real window. |
| 30.48% | 32/105 | `src/actuator/win/post_message.rs` | Needs a real HWND. `send_input.rs` (51%) does better only because it has a fake driver. |
| 40.54% | 15/37 | `src/app/console.rs` | Reads stdin. |
| 41.83% | 105/251 | `src/ui/mod.rs` | The eframe shell: `App::update`, window setup. |
| 41.96% | 60/143 | `src/lib.rs` | The startup policy — log directory creation, `install_logging`, `fatal`. Touches the real filesystem. |
| 44.27% | 85/192 | `src/app/mod.rs` | Wiring: `Session::run`, backend selection, `supervise`. |
| 46.99% | 39/83 | `src/crash.rs` | The panic hook itself cannot be invoked from a passing test. |
| 48.08% | 50/104 | `src/actuator/win/mod.rs` | Win32. |
| 51.27% | 81/158 | `src/actuator/win/send_input.rs` | Win32, with a fake driver for the pure half. |
| 51.28% | 20/39 | `src/actuator/win/dpi.rs` | Win32. |
| 65.06% | 108/166 | `src/ui/theme.rs` | Painting. |
| 68.87% | 73/106 | `src/app/ingest.rs` | |
| 76.92% | 100/130 | `src/ui/editor/hunt.rs` | |
| 80.91% | 89/110 | `src/render.rs` | |
| 81.29% | 126/155 | `src/app/reassembly.rs` | |
| 83.08% | 54/65 | `src/app/pressure.rs` | |
| **83.78%** | **155/185** | **`src/uplink/websocket.rs`** | **The weakest of the modules that move money or halt the loop. See §2.** |
| 88.74% | 197/222 | `src/stream/reassembly.rs` | Remainder is defensive/underflow reporting. |
| 89.41% | 211/236 | `src/stream/budget.rs` | Same: `checked_add` overflow arms, the two `report_*_underflow` functions, `Debug`/`PartialEq` impls. |
| 90.16% | 55/61 | `src/domain/shop.rs` | All six uncovered lines are `tracing::debug!` bodies. Effectively 100%. |
| 90.43% | 85/94 | `src/ui/editor/stop.rs` | |
| 90.53% | 153/169 | `src/config/persist.rs` | |
| 91.30% | 105/115 | `src/ui/shop.rs` | |
| 92.12% | 421/457 | `src/app/session/mod.rs` | |
| 92.52% | 99/107 | `src/ui/editor/timing_meter.rs` | |
| **93.08%** | **148/159** | **`src/actuator/mod.rs`** | **Two of the eleven uncovered lines are real holes. See §2.** |
| 94.21% | 114/121 | `src/app/workers.rs` | |

At or above 95%, in ascending order: `capture/ip.rs` 96.08 · `ui/statusbar.rs`
96.77 · `ui/editor/mod.rs` 98.14 · `domain/filter.rs` 98.81 ·
`domain/control/watchdog.rs` 98.82 · `domain/control/mod.rs` 99.68 · and at
**100%**: all four `actuator/plan/*` files, `domain/control/dedup.rs`,
`capture/mod.rs`, `capture/pcap/link.rs`, `config/mod.rs`,
`config/server_url.rs`, `error.rs`, `journal.rs`, `stream/mod.rs`,
`ui/editor/timing.rs`, `ui/journal.rs`, `ui/view.rs`, `watch.rs`.

**The headline for the modules this report was pointed at is that they are the
best-covered code in the crate, not the worst.** Every file under
`actuator/plan/` is at 100% of production lines; `domain/control/` is at 99.68%,
98.82% and 100%. That is a real result and it is the reason the mutation half
below is the part with findings in it: line coverage has nothing left to say
about the money path, and "executed" was never the same claim as "asserted".

### `test-002` is closed

The audit's open question — "`pump`'s inbound arm and `forward()` are unreachable
from every test" — is **no longer true**, and the coverage run is the check. A
`ScriptedLink` now sits beside `StalledLink` in `uplink/websocket.rs` and yields
scripted inbound frames; `forward` and every `Message::*` arm execute. Six
inbound tests exist where there were none. The remaining uncovered lines in that
file are a *different* set, listed in §2.

## 2. Production code with zero coverage that is not obviously untestable

The Win32 backends, `capture/pcap/`, `main.rs` and the eframe shell are excluded
here — their uncovered lines need a window, a NIC or a display, and saying so
once is more useful than listing 800 line numbers. What follows is what is left:
code that is reachable from a plain `cargo test` and is not reached.

Ranked by consequence.

### C-1 — the outage protocol is never exercised: no test ever emits `LinkDown` or `LinkUp`

- **Site:** `src/uplink/websocket.rs:169` (`inbound.send(UplinkEvent::LinkUp)`)
  and `:185-188` (the `Outcome::Disconnected` arm: the `warn!`, `outage_reported
  = true`, and `inbound.send(UplinkEvent::LinkDown(reason))`).
- **Why it matters:** these two events are the *only* producers of
  `Event::LinkDown` / `Event::LinkUp`, and those are what suspend and re-grant
  the recovery watchdog (`Controller::on_link_up`, and the `if !self.link_up`
  guard at the top of `Controller::watchdog`). The controller side of that
  protocol is thoroughly tested — `link_down_suspends_the_watchdog` and
  `link_up_regrants_a_full_deadline` are both there — but **nothing connects the
  two halves**. If `run_with_connector` stopped emitting `LinkUp` after a
  reconnect, every controller test would still pass, and a session that survived
  one outage would then sit with its watchdog permanently suspended: no
  escalation, no honest halt, a hunt that quietly stops recovering.
- **The test that should exist:** `a_reconnect_reports_the_outage_and_its_end`
  — drive `run_with_connector` with a connector that returns
  a `ScriptedLink` yielding `Message::Close(None)` on the first dial and a
  stalling link on the second, and assert the `inbound` channel carries
  `UplinkEvent::LinkDown(_)` followed by `UplinkEvent::LinkUp`. The seam already
  exists and both fakes already exist; this is a test, not a refactor.

### C-2 — no test ever completes a successful send, so the outbound lease is never observed being released

- **Site:** `src/uplink/websocket.rs:327` (the `Ok(Ok(()))` arm of the writer's
  `timeout`) and `:337` (`drop(lease)`).
- **Why it matters:** the one test that pushes a `BudgetedChunk` through `pump`
  (`an_inbound_message_lands_while_the_send_half_is_stalled`) uses
  `ScriptedLink::stalling_sends()`, so the send never completes. The success path
  — frame the chunk as `Message::Binary`, then release its budget lease — is
  never executed. The lease release is a *memory-bound* guarantee: `stream/budget.rs`
  exists so that a payload's bytes are returned to the pool exactly once, and the
  outbound stage's return happens here and nowhere else. A refactor that dropped
  the `drop(lease)` would leak the whole outbound quota over a session and no test
  would notice; the existing test asserts `current_total == 0` only after a
  *stalled* send, where the lease is released by the error path instead.
- **The test that should exist:** `a_completed_send_returns_its_budget` — a
  `ScriptedLink`
  that accepts sends, one `admit_outbound_for_test` chunk pushed through `pump`,
  asserting (a) the sink recorded exactly one `Message::Binary` with those bytes
  and (b) `budget.snapshot().current_total == 0` while the connection is still
  open, not after it died. `ScriptedLink` already records nothing on its `Sink`
  half — it needs a `Vec<Message>` behind a `Mutex`, which is four lines.

### C-3 — `Surface::acquire` returning `Recoverable` is handled by an arm no test reaches

- **Site:** `src/actuator/mod.rs:409,412,413` — the
  `Err(SurfaceError::Recoverable(reason))` arm of `match blocking(|| surface.acquire())`.
- **Why it matters:** it looks covered and is not.
  `executor_aborts_without_halt_on_a_minimized_acquire` is *named* for this case
  but takes a different route: it returns `Ok(ClientRect { width: 0, height: 0 })`
  and the recoverable classification then comes from `ScreenError::DegenerateRect`
  further down. The arm that handles an `acquire` which *fails* recoverably — what
  the Win32 backends return when the window vanishes between two jobs, or when a
  UIPI check refuses at acquire time — is executed by nothing. Its job is to abort
  the job **without disabling the gate**; the neighbouring `Fatal` arm halts the
  watch. Those two arms are three lines apart and one of them is unasserted, so a
  mis-edit turns a transient window blip into a hunt that stops and blames the
  actuator.
- **The test that should exist:**
  `executor_aborts_without_halt_when_acquire_fails_recoverably` — a `FakeSurface`
  constructed with `Err(SurfaceError::Recoverable(..))`, asserting the gate stays
  enabled, no input reached the surface, and the journal carries the abort line.
  It is the existing `executor_stops_the_loop_when_acquire_fails` with `Fatal`
  swapped for `Recoverable` and the gate assertion inverted.

### C-4 — the dry-run scroll journal line is never produced

- **Site:** `src/actuator/mod.rs:463-467` — the `Input::Scroll` arm of the
  `dry_run` branch.
- **Why it matters:** small, but it is the one place where "dry run must not
  touch the game" is enforced for scrolls, and dry run is what a cautious player
  turns on first. `executor_dry_run_journals_without_input` submits a
  `refresh_job`, which is two clicks and no scroll, so it proves the click half
  only. A `buy_job` in dry-run mode would prove both.
- **The test that should exist:** extend
  `executor_dry_run_journals_without_input` (or add
  `executor_dry_run_journals_a_scroll_without_touching_the_surface`) with a
  `buy_job` for a bottom-group row, asserting the journal carries a `dry-run:
  scroll` line and that `events` is still empty.

### C-5 — `stop_reason`'s timeout arm is dead in every test

- **Site:** `src/domain/control/mod.rs:752` — `return Some(StopReason::Timeout);`
  inside `stop_reason`.
- **Why it matters:** this is the *only* uncovered production line in a 765-line
  file that is otherwise at 99.68%, and it is not a defensive branch — it is one
  of the six stop reasons. Every timeout in the suite arrives through
  `on_tick`'s own `has_duration_elapsed` arm, which halts before `stop_reason` is
  ever consulted; so the copy of the timeout check that guards the *refresh
  emission point* has never fired. The two paths are not equivalent:
  `refresh_or_halt` is what a snapshot or an auto-resume goes through, and a
  duration limit that expires between two snapshots is meant to stop the loop
  there rather than one tick later.
- **Judgement:** low consequence (the tick path catches it within one tick) but
  it is genuinely unasserted, and it is cheap to close.
- **The test that should exist:** `an_elapsed_duration_halts_at_the_refresh_gate`
  — start with `max_duration_ms`, deliver a no-match
  snapshot at a `now_ms` past the deadline, and assert the single action is
  `Halt(Timeout)` and **not** `Refresh`. Today the suite only has
  `timeout_fires_via_tick_while_paused`.

### Dismissed as unobservable or untestable-by-design

- `src/domain/control/watchdog.rs:170` — `recovery_buy_targets`' `return
  Vec::new()` when `last_snapshot` is `None`. A purchase expectation can only be
  armed from `evaluate_snapshot`, which stores the snapshot, so the state is
  unreachable; the early return is defence, correctly written.
- `src/domain/filter.rs:77` — `grade_floor`'s "the key was present and null"
  arm. TOML has no null, and an absent key goes through `#[serde(default)]`
  without reaching this function at all. Unreachable from the only format the
  crate parses.
- `src/actuator/mod.rs:272` — `Surface::release`'s default body. Every
  implementor overrides it; the default exists so a future backend need not.
- `src/uplink/websocket.rs:100-118` — `pub async fn run`. It is an eight-line
  delegation to `run_with_connector` whose only unique content is the real
  `connect_async` closure. That closure is the seam the whole module is designed
  around; testing it means dialling a real socket.
- `src/stream/budget.rs` (13 of its 25 uncovered lines) — the `checked_add`
  overflow returns, `report_release_underflow`, `report_pending_underflow`. These
  are the paths the module's own header documents as "saturate and report rather
  than assert, because this runs from a `Drop` during unwind". A test that
  reached them would have to construct an accounting bug.
- `src/domain/shop.rs` (all six) — `tracing::debug!` bodies, see §1.

## 3. Mutation testing

### Scope and result

Two runs, both `--timeout 120` (about sixty times the ~2 s the suite takes once
built) on the `--no-default-features` lane, both against the pinned snapshot.

| Files | Mutants | Caught | **Survived** | Unviable | Timed out |
|---|---|---|---|---|---|
| `actuator/plan/{geometry,jobs}.rs` | 93 | 73 | **8** | 9 | 3 |
| `domain/control/{mod,watchdog,dedup}.rs` | 123 | 85 | **6** | 32 | 0 |

Both runs completed — 25 and 24 minutes of wall clock respectively, on a machine
shared with three other agents compiling the same crate, which is why a
per-mutant rebuild that takes ~3 s in isolation took 30–60 s for much of the run.

Two categories in that table are not results and should not be read as one:

- **Unviable (9 and 32)** — mutants that do not compile, which is the type system
  doing its job twice over. Most are `cargo mutants` trying to replace a return
  value with `Default::default()` for a type that has none (`Slot::row`,
  `Row::slot`, `buy_zone`, the three job builders, `Controller::handle`,
  `Expectation::{snapshot,purchase,armed,regrant,escalate}`, …). The interesting
  subset is **seven `&&` → `||` mutants in `Controller::{plan_targets,
  on_purchase,stop_reason}` that are unviable because the `&&` in question is a
  `let`-chain**, where `||` is not grammatical. Every one of those seven is a
  guard on the buy decision or on a stop limit, so `let`-chain syntax is quietly
  doing mutation-proofing that a plain boolean `&&` would not.
- **Timed out (3)** — all three are `ClientRect::is_degenerate`
  (`geometry.rs:246`): the function forced to `true`, and each of its two `<=`
  flipped. Making every rect degenerate makes an executor test spin instead of
  fail. A timeout is *not* a survivor — the suite noticed — but it is not a clean
  kill either, and it is exactly what `--timeout` exists to bound. It is also the
  reason `quality.yml` must never be a required check: this verdict is a function
  of how loaded the runner was.

### Every survivor, judged

Seven of the fourteen were replayed by hand against the **shipped default
feature lane** — `cargo test --locked --lib` in a throwaway worktree, patch
applied then reverted — because a survivor from a reduced lane proves nothing on
its own. Where the "Default lane" column says **survives** or **killed**, that is
a run, not an argument. Where it says *not replayed*, the mutant shares a site
and a cause with one that was, and the column says which.

| Mutant | Default lane | Verdict |
|---|---|---|
| `geometry.rs:338:40` `+` → `-` | **survives** | **real hole** — §3.1 |
| `geometry.rs:338:51` `-` → `/` | **survives** | **real hole** — §3.1 (same cause) |
| `jobs.rs:87:38` `^` → `&` (`confirm_retry_job`) | **survives** | **real hole** — §3.2 |
| `jobs.rs:87:38` `^` → `\|` (`confirm_retry_job`) | not replayed | same cause as above |
| `jobs.rs:125:38` `^` → `&` (`buy_job`) | **survives** | **real hole** — §3.2 |
| `jobs.rs:125:38` `^` → `\|` (`buy_job`) | not replayed | same cause as above |
| `jobs.rs:101:38` `^` → `\|` (`refresh_job`) | not replayed | §3.2 — partial: `^` → `&` at the same site *is* caught |
| `control/mod.rs:742:45` `>` → `>=` (`stop_reason`) | **survives** | **real hole** — §3.3 |
| `geometry.rs:329:22` `<` → `<=` | not replayed | **false positive** — §3.4 |
| `control/mod.rs:336` `is_recovery_enabled` → `false` | **killed** (`app::tests::setup_enables_recovery_only_when_live`, `src/app/mod.rs:548`) | **lane artefact** — §3.5 |
| `control/mod.rs:369` `refresh_meta` → `None` | **killed** (`ui::view::tests::view_state_copies_refresh_meta_when_present`, `…::view_state_balance_survives_meta_less_snapshot`) | **lane artefact** — §3.5 |
| `control/mod.rs:381` `gold_balance` → `None` | **killed** (`ui::view::tests::view_state_surfaces_gold_balance_from_a_purchase`) | **lane artefact** — §3.5 |
| `control/mod.rs:381` `gold_balance` → `Some(0)` / `Some(1)` | not replayed | same accessor, same gate as above |

**One `domain/control/` survivor is real and it is on the money path** (§3.3);
the other five are `pub` accessors whose only consumer is behind
`#[cfg(feature = "gui")]` or `#[cfg(all(windows, feature = "actuator"))]`. Apart
from that single boundary, **85 of the 91 viable mutants aimed at the buy
decision, the dedup fingerprint, the stop-reason ladder and the watchdog rungs
died**, and the six that lived are the five accessors plus that boundary —
`watchdog.rs` and `dedup.rs` produced no survivor at all. For the
module the whole audit orbits, that is the most reassuring result in this
document, and it is the first time it has been checked by anything other than
reading.

### 3.1 Real hole — every `Anchor::Center` test point is exactly the centre

Two surviving mutants, one cause, and it is the highest-value finding here. Both
were **confirmed surviving on the shipped default lane.**

```
src/actuator/plan/geometry.rs:338:40: replace + with - in to_screen
src/actuator/plan/geometry.rs:338:51: replace - with / in to_screen
```

Line 338 is the `Anchor::Center` arm of the design→screen transform:

```rust
Anchor::Center => view_w / 2.0 + (point.x - DESIGN_W / 2.0),
```

Every test that exercises `Anchor::Center` — `to_screen_is_identity_at_design_resolution`,
`to_screen_scales_and_offsets_at_16_9`,
`to_screen_anchors_follow_the_view_edges_when_wide`, and the 1 152-case lattice
in `to_screen_maps_every_shape_inside_the_client_area` — passes a design x of
**exactly 640.0**, which is `DESIGN_W / 2.0`. At that point the parenthesised
offset is `0.0`, so the whole term is inert and neither its sign nor its operator
is observable:

- `+` → `-` gives `view_w / 2.0 - 0.0`: identical at every resolution.
- `-` → `/` gives `point.x / DESIGN_W / 2.0` = `640 / 1280 / 2` = `0.25`, which
  after `to_screen`'s closing `.round()` lands on the same pixel at 1280×720
  (640.25 → 640), at 1920×1080 (1060.375 → 1060) and at 1440×720 (720.25 → 720).

This is the `const-003` shape exactly: assertions that hold for a reason
unrelated to the thing they claim to pin. And it is not academic — **both
`Anchor::Center` zones in the crate are off-centre**: `CONFIRM_REFRESH.cx = 747.5`
and `CONFIRM_BUY.cx = 750.0`. Under the sign flip, `CONFIRM_BUY` maps to design
x ≈ 530 instead of 750, i.e. ~220 design px to the *left* of the button it is
aiming at — still inside the confirm modal, on or beside whatever the modal puts
there. The failure mode is a buy confirmation that never lands: the watchdog
re-clicks blindly, escalates, re-issues, and finally halts `Unresponsive`,
blaming the game for a coordinate bug. That is the same hazard class `const-003`
was filed against, on the other axis.

**The test that should exist:** `to_screen_places_an_off_centre_center_anchor`.
Assert
`to_screen` for `CONFIRM_BUY.cx` (750.0, i.e. 110 design px right of centre) at
1280×720, at 1920×1080, and past the aspect cap at 3440×1440, checking each
result is *right* of the mapped centre by the scaled 110 px. Why the current
suite cannot fail: it never asks about a Center point that is not the centre, so
the offset term it is testing is always zero. Adding `point(750.0, 508.0,
Anchor::Center)` to the lattice's `points` array is the cheaper half of the fix —
but only if the sweep gains an assertion comparing it against the mapped centre,
because "inside the client area" does not distinguish 750 from 530.

### 3.2 Real hole — the delay salt is unobservable in two of the three job builders

```
src/actuator/plan/jobs.rs:87:38:  replace ^ with & in confirm_retry_job   (confirmed on the default lane)
src/actuator/plan/jobs.rs:87:38:  replace ^ with | in confirm_retry_job
src/actuator/plan/jobs.rs:125:38: replace ^ with & in buy_job             (confirmed on the default lane)
src/actuator/plan/jobs.rs:125:38: replace ^ with | in buy_job
src/actuator/plan/jobs.rs:101:38: replace ^ with | in refresh_job
```

All three builders open their second stream the same way:

```rust
let mut delay = Jitter::new(seed ^ DELAY_SEED_SALT);
```

`DELAY_SEED_SALT` (`jitter.rs`) exists, in its own doc comment's words, so that
"click coordinates depend on the timing config" is impossible: XOR with a fixed
constant is a bijection, so distinct seeds stay distinct and the delay stream can
never coincide with the position stream. `&` and `|` are *not* bijections, and
`&` in particular collapses hard here: `DELAY_SEED_SALT = 0xD31A_7000_D31A_7000`
has no bit set below bit 12, so **every seed below 4096 maps to the same delay
stream**. That is not hypothetical — it is why `^` → `&` at line 101 is the one
mutant of the five that *is* caught: `draws_are_deterministic_per_seed_and_vary_across_seeds`
scans seeds 100..110 against 42, all of them below 4096, and under `&` they all
draw the same wait, so the test's `differs` assertion fails.

The interesting part is why the other four survive, and it is a "test that cannot
fail" of exactly the family this report was commissioned to find:

- `buy_job` and `confirm_retry_job` have **no seed-varying test at all**. Nothing
  in `jobs.rs` asks either of them to produce two different waits from two
  different seeds.
- `confirm_retry_job_folds_in_the_recovery_range` *looks* like it exercises the
  delay stream and does not. It passes `range(250, 250)`, and `DelayRange::draw`
  returns `self.min_ms` early whenever `span == 0` — **without touching the
  jitter**. So the delay `Jitter` in `confirm_retry_job` is never advanced by any
  test in the crate, and its seed is therefore unobservable by construction.
  Same for the `extra_buy_ranges_land_on_scroll_confirm_and_between_buys` point
  ranges in `buy_job`.
- `^` → `|` survives even in `refresh_job`, because no test states the property
  the salt is *for*. "Different seeds give different waits" holds under `|` for
  the seeds the suite happens to use; "the map from seed to delay-seed is
  injective, and never the identity" does not, and nothing asserts it.

**Consequence:** lower than §3.1 — a colliding delay stream does not move a click
out of its zone, because positions still come from the unsalted `Jitter` and
`point_in` stays within the central 75%. What it costs is the variability the
jitter exists for: waits correlating with positions, or many distinct `now_ms`
seeds collapsing onto one wait sequence, making the loop's timing signature more
regular rather than less. That is the anti-detection property, so it is worth an
assertion even though nothing misclicks.

**The tests that should exist:**

1. `every_job_builder_varies_its_waits_with_the_seed` — for all three builders,
   with a **wide** range (`span > 0`, e.g. `range(0, 1_000)`), assert that some
   seed in a small scan produces a different `wait_ms` from seed 42. This is
   `draws_are_deterministic_per_seed_and_vary_across_seeds` generalised from one
   builder to three, and it kills the `&` mutants at lines 87 and 125.
2. `the_delay_stream_is_salted_apart_from_the_position_stream` — for each
   builder, with a wide range, assert the click positions equal those of the
   same builder at the same seed with `Timings::default()`. `extra_ranges_add_a_bounded_draw_on_top_of_the_baselines`
   already does this for `refresh_job`; extending it to `buy_job` and
   `confirm_retry_job` costs three lines each.
3. Whichever of those is written, it must use a range with `max_ms > min_ms`.
   A point range cannot fail: `DelayRange::draw` never reaches the jitter.

### 3.3 Real hole — the `max_spend` ceiling has no exact-boundary test

```
src/domain/control/mod.rs:742:45: replace > with >= in Controller::stop_reason
```

**Confirmed surviving on the shipped default lane.** Line 742 is the second half
of the hard-ceiling test, the one the comment above it calls "also stop when the
*next* refresh would cross it":

```rust
if let Some(max) = self.limits.max_spend
    && (self.progress.spent >= max
        || self.progress.spent.checked_add(self.refresh_cost())
               .is_none_or(|next| next > max))
```

`>` and `>=` differ on exactly one input: `spent + cost == max`, i.e. a budget
that is an exact multiple of the refresh cost. `>` spends it to the last crystal
(reaching the ceiling is not crossing it); `>=` stops one refresh early and
leaves the player's whole last refresh unbought.

There are four tests of `max_spend` in the suite and not one of them can reach
that comparison at its boundary. Three — `max_spend_is_hard_ceiling_no_overshoot`,
`max_spend_enforced_without_meta_via_constant_cost` and
`wire_cost_overrides_the_constant` — **all use `max_spend: Some(7)`**, against a
cost of 3 or 5. Neither divides 7, so `spent + cost` jumps from 6 to 9 (or from 5
to 10) and steps straight over the boundary. The fourth,
`stop_reason_priority_order`, uses `max_spend: Some(0)`, which satisfies the
*first* clause (`spent >= max`, `0 >= 0`) and short-circuits before the mutated
one is evaluated at all. Every assertion in all four holds identically under
`>=`. These are not weak tests; they simply all picked a budget the cost does not
divide, and nothing in the file picks one it does.

What makes this worth ranking rather than noting is that **the crate already has
the mirror of this test for the neighbouring gate**:
`balance_equal_to_cost_still_affords_one_refresh` exists precisely to pin
`OutOfFunds`' boundary, and its comment says so — *"the gate is `<`, not `<=`, so
the last affordable refresh still goes out."* The same sentence is true of
`max_spend` and nothing states it.

**The test that should exist:** `a_budget_that_divides_by_the_cost_is_fully_spent`
— `max_spend: Some(6)` with a cost of 3: assert two refreshes go out, that
`progress().spent == 6`, and that the third halts `MaxSpend`. Under `>=` the
second refresh becomes a halt and the test fails on its first assertion. Written
next to `balance_equal_to_cost_still_affords_one_refresh`, with the same comment
shape, so the pair reads as the two boundaries of one policy.

### 3.4 Dismissed — the aspect-comparison epsilon

```
src/actuator/plan/geometry.rs:329:22: replace < with <= in to_screen
```

`if aspect + 1e-3 < DESIGN_W / DESIGN_H` — the guard that refuses a window
narrower than 16:9. `<` and `<=` differ only when `aspect + 1e-3` is *exactly*
`16.0/9.0` in `f32`, i.e. on a measure-zero family of `(width, height)` pairs. A
test pinning that would be pinning an artefact of the chosen epsilon rather than
a behaviour, and it would need rewriting every time the epsilon moved. **Not a
finding.** The behaviour either side of the boundary — refusal below, mapping
above — is already asserted by `to_screen_refuses_a_narrow_window` and by the
lattice's `min_width` row, which walks the exactly-16:9 case at eight heights.

### 3.5 Dismissed — five survivors that are artefacts of the reduced feature lane

```
src/domain/control/mod.rs:336:9: replace Controller::is_recovery_enabled -> bool with false
src/domain/control/mod.rs:369:9: replace Controller::refresh_meta -> Option<RefreshMeta> with None
src/domain/control/mod.rs:381:9: replace Controller::gold_balance -> Option<u32> with None
src/domain/control/mod.rs:381:9: replace Controller::gold_balance -> Option<u32> with Some(0)
src/domain/control/mod.rs:381:9: replace Controller::gold_balance -> Option<u32> with Some(1)
```

All five are `pub` accessors on `Controller` whose only non-test caller sits
behind a feature the mutation lane turned off. Replayed on the default lane, all
three distinct mutants died:

- `refresh_meta()` and `gold_balance()` are read only by `ui::view::view_state`
  (`src/ui/view.rs:206-207`), `#[cfg(feature = "gui")]`. Killed by
  `view_state_copies_refresh_meta_when_present`,
  `view_state_balance_survives_meta_less_snapshot` and
  `view_state_surfaces_gold_balance_from_a_purchase`.
- `is_recovery_enabled()` is read only by
  `app::tests::setup_enables_recovery_only_when_live`, which has **two** `#[cfg]`
  bodies: the `not(all(windows, feature = "actuator"))` one asserts `false` and
  therefore passes under the mutant; the `all(windows, feature = "actuator")` one
  asserts `true` and kills it (measured: `assertion failed` at
  `src/app/mod.rs:548`).

Worth naming as a class, because the next run will meet it again: **a `pub`
accessor whose sole consumer is `#[cfg]`-gated is invisible to any lane that
turns the gate off**, and mutation testing reports that identically to a real
hole. The only defence is to replay each survivor on the shipped lane, which is
what this table is — and the reason both pieces of instrumentation left behind
use the default features.

## 4. Instrumentation left behind

- **`justfile`** gains `coverage` and `mutants`. Neither is in `verify`, and the
  recipe comments argue why at length: they need tools that are not part of the
  toolchain (so a clean checkout would fail with a missing-subcommand error
  rather than a code error), they are slow in a way no other lane is, and neither
  produces a number that should ever become a threshold — a coverage percentage
  turned into a gate is optimised by writing tests that execute lines without
  asserting anything, which is precisely the defect this crate already shipped
  once.
- **`.github/workflows/quality.yml`** is new: a weekly (`41 5 * * 1`, offset from
  `ci.yml`'s `17 4 * * 1`) and `workflow_dispatch` workflow with two jobs — a
  coverage job that publishes the summary into the run page and uploads the lcov,
  and a mutation job sharded four ways over the five money files. It is
  **deliberately not** wired to `push` or `pull_request` and must not become a
  required check: a mutation pass is minutes-to-hours of compiling, and a
  *timeout* verdict is a property of runner load rather than of the code. A gate
  that reddens PRs on timing noise is switched off within a month, and a switched
  off gate is worse than no gate because everyone still believes in it. The
  header says so.
- `permissions: contents: read` at workflow level; neither job writes anything
  back. Every third-party action is pinned by full commit SHA with a `# vN`
  trailer, following `ci.yml`'s `actions/cache` convention.

### A defect found while pinning those SHAs — needs `ci.yml`, which this pass does not own

`ci.yml`'s two cache steps are pinned as:

```yaml
uses: actions/cache@11d5960a326750d5838078e36cf38b85af677262 # v4
```

**That SHA does not exist in `actions/cache`.** It is `actions/checkout`'s `v4`
commit (`GET /repos/actions/cache/commits/11d5960a…` → `422 No commit found for
SHA`; the same SHA in `actions/checkout` resolves to "backport fixes to
releases-v4 (#2524)"). GitHub cannot resolve the action, so both steps in the
`dependency-policy` job fail before they run — which means the cargo-deny binary
cache and the advisory-database cache have never worked, and quite possibly the
job has been red or has been reinstalling `cargo-deny` from source on every run.
The correct `actions/cache` v4 SHA, verified against the API, is
`0057852bfaa89a56745cba8c7296529d2fc39830` (v4.3.0); `quality.yml` uses that one.

## 5. What a reader should do with this

Eight holes, in consequence order. Every one of them is a test, not a refactor,
and none of them needs a new dependency:

1. **§3.1** — the off-centre `Anchor::Center` assertion
   (`actuator/plan/geometry.rs`). The only finding here that can put a click in
   the wrong place, and the one confirmed by two independent mutants surviving
   the shipped lane.
2. **C-1** — the `LinkDown`/`LinkUp` outage protocol (`uplink/websocket.rs`).
   The controller half is well tested and the producer half is not tested at all,
   so the seam between them is unasserted end to end, and its failure mode is a
   watchdog that stays suspended for the rest of the session.
3. **§3.3** — the `max_spend` exact boundary (`domain/control/mod.rs`). Every
   test of that ceiling picked a budget the refresh cost does not divide, so the
   comparison they exist to pin is never reached. One test, and its mirror for
   the neighbouring gate already exists.
4. **C-3** — `acquire` failing recoverably (`actuator/mod.rs`). A test named for
   this case takes a different route through the code.
5. **C-2** — the completed send and its lease release (`uplink/websocket.rs`).
6. **§3.2** — the delay salt in `buy_job` and `confirm_retry_job`
   (`actuator/plan/jobs.rs`), including the point-range tests that cannot fail
   because `DelayRange::draw` never reaches the jitter when `span == 0`.
7. **C-5** — `stop_reason`'s timeout arm (`domain/control/mod.rs`).
8. **C-4** — the dry-run scroll line (`actuator/mod.rs`).

And two measurements this pass could not make, which the next one should:

- **Mutation-test `domain/shop.rs`, `domain/filter.rs`, `stream/budget.rs` and
  `stream/reassembly.rs`.** They were in scope and were not reached.
- **Run `quality.yml` once, manually, via `workflow_dispatch`,** before anyone
  relies on its schedule. Its tools and flags are verified locally; the workflow
  itself has never executed.
