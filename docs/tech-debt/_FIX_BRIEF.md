# Fix brief — shared instructions for every implementer

You are one of several agents applying the findings of the audit in this directory.
The audit is complete; 26 reports (`01-own.md` … `26-anti.md`) and a synthesis
(`README.md`) are already written. **You implement. You do not re-audit.**

## Your scope is a FILE SET, not a category

Your prompt names the files you own. **You may edit only those files.** Every other
`.rs` file, `Cargo.toml`, and every file in `docs/tech-debt/` belongs to someone else
working in parallel right now. Editing outside your set will collide with another
agent's work and corrupt the run.

## Finding your work

Every finding carries a `**Site:** src/foo.rs:123` line. To get yours:

```bash
grep -n "src/your_file.rs" docs/tech-debt/*.md
```

Run that for **each** file you own. Then read each matching finding in full in its
report — the `**Fix:**` field tells you what to do, and the surrounding "Why it matters
here" tells you why, which you need in order to not break the intent.

Findings arrive from many categories. A single file can have findings in `03-mem.md`,
`09-num.md`, `22-obs.md` and `26-anti.md` at once. Collect them all before you start
editing, then apply them in one coherent pass per file rather than one edit per finding
— several findings in the same function often collapse into one change.

Work every severity: P0, P1, P2, P3. The user asked for all of them.

## Hard rules

1. **Leave the tree compiling and green.** Baseline is **516 passing / 0 failed / 2
   ignored** on `cargo test --all-features`. Run it before you finish. If your change
   drops a test, you broke something — fix it or revert that finding.
2. **Do not weaken a test to make it pass.** If a fix makes a test fail because the
   test asserted the old buggy behaviour, update the test *and say so in your report*.
   If a test fails and you cannot tell why, revert that finding and record it.
3. **If a fix needs a file you do not own, do not touch that file.** Do the part inside
   your scope if it stands alone; otherwise skip the finding. Either way, append a line
   to `docs/tech-debt/_HANDOFF.md` (create it if absent; append only, never rewrite
   another agent's line) in this format:
   ```
   - [ ] <finding-id> — needs `<file you don't own>` — <one line on what remains>
   ```
   A later sequential pass with full access will finish it. This is the normal outcome
   for anything cross-cutting; it is not a failure.
4. **Public-signature changes are cross-cutting.** Adding `#[must_use]`, changing a
   return type, or renaming a `pub(crate)` item can break or warn at call sites outside
   your files. Before making one, `grep -rn "<item_name>" src/` and check. If callers
   are outside your set, record a handoff instead of half-doing it.
5. **CI runs `-D warnings`.** A new warning is a broken build. Check with
   `cargo clippy --all-targets --all-features 2>&1 | tail -20` before you finish —
   the crate is currently at **zero** clippy diagnostics and must stay there.
6. `cargo fmt` your files when done (only yours — `cargo fmt` on the whole crate is
   fine because the tree is already formatted, so it will not touch others' work).
7. **Do not commit, stage, stash, branch, or run any other `git` write command.** The
   orchestrator commits. `git diff`/`git status`/`git log` are fine.
8. Do not edit any file in `docs/tech-debt/` except appending to `_HANDOFF.md`. The
   reports are the record of what was found; they stay as written.

## Do NOT "fix" these — verified correct, recorded deliberately

The audit spent real effort establishing that these are right. Changing them is a
regression, and several look exactly like the anti-pattern they are not.

- `src/domain/control/watchdog.rs:103` — clippy flags it, but it is a correct identity
  pass-through. Filed as `pat-005` precisely so the suggestion is not applied.
- The `S: Surface` bound on `SurfaceJobGuard` — required by its `Drop` impl.
- The explicit `'a` on `LinkStrip::ip_bytes` — elision would tie the output to the
  wrong borrow.
- By-value `tx` / `Handle` parameters that clippy calls needless — those moves close
  the pipeline in producer order and keep the `pcap_t` on its owning thread.
- The six `u64` monotonic packet counters.
- The deliberate documented casts: `seq_diff`'s `as i32`, `expected_seq`'s modular
  truncation, the `ERROR_* as i32` Win32 boundary.
- `ItemKind::Unknown`'s checkbox must **stay** removed.
- `docs/initial-stream-anchor.md`'s amendment header is accurate and complete.
- The wire surface's deliberate leniency toward unknown fields (inbound
  forward-compatibility) — `serde-001`/`002` fix the *silence*, not the leniency.
- Do not add a Miri job, `panic = "abort"`, `strip = true`, `clippy::cargo`, whole-group
  `pedantic`/`nursery`, `ahash`/FxHash on the per-packet map, or a workspace split.
  Each is argued against with measurements in the reports.
- `mem-boxed-slice`, `const-generics`, `conv-asmut-mutable`, `closure-impl-fn-return`
  and `perf-io-buffering` were audited and filed **not applicable**. Skip them.

If a report's `**Fix:**` field contradicts something in this list, the list wins —
and say so in your report.

## Judgement you are expected to exercise

The reports were written by reviewers who could not see each other's work and who did
not edit code. You are the first person to actually try the change. So:

- **If a proposed fix is wrong, do not apply it.** Say why in your report. A reviewer
  reasoning about a fix is not the same as a fix that compiles and passes.
- If two findings on the same lines propose conflicting changes, pick the better one,
  apply it, and record the conflict.
- If a finding is already fixed (another wave, or it was never real), say so and move on.
- Prefer the smallest change that removes the defect. This crate's comments carry design
  rationale on purpose — **update a comment that your change makes stale, never delete
  rationale**, and do not reformat or "tidy" code you are not fixing. A reviewer must be
  able to see exactly what changed and why.
- Several findings ask you to encode an invariant that currently lives in a comment
  (`const _: () = assert!(…)`, a newtype, a supertrait). Keep the comment *and* add the
  check — the comment explains, the check enforces.

## Your report back

Your final message to the orchestrator must be **≤ 25 lines**:

- One line per finding you applied: `<finding-id> — done: <what you changed>`
- One line per finding you skipped: `<finding-id> — skipped: <why>`
- Handoffs you appended (just the ids).
- The final `cargo test --all-features` result line and whether clippy is still silent.
- Anything the orchestrator must know before committing your work.

Do not paste diffs; the orchestrator reads the tree.
