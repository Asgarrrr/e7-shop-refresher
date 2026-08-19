# Split brief — shared rules for the module-decomposition pass

Read `_FIX_BRIEF.md` first; it still governs (no `git` write commands — the orchestrator
commits; do not edit any file outside your set; do not edit reports in this directory
except appending to `_HANDOFF.md`).

This pass does **one thing**: move code. It was deliberately held back until every
behavioural fix had landed, so that this diff is reviewable as a pure move.

## The baseline you must not move

- `cargo test --locked --all-features` → **593 passed / 0 failed / 2 ignored**, plus
  **4 doctests**. Other lanes: `--no-default-features` 468, `gui,actuator` 580,
  `pcap-backend` 481.
- `cargo clippy --locked --all-targets -- -D warnings` → **0 diagnostics on all six CI
  lanes**. `correctness` and `suspicious` are at **deny**, so a new warning is a hard
  failure.
- `cargo fmt --all --check` → clean. `cargo doc --locked --no-deps
  --document-private-items` → **0 warnings**.

**The test count must come out identical.** You are moving code, not changing it, so a
changed count means you altered behaviour or lost a test. If a count moves, find out why
before you finish; if another agent's concurrent work moved it, say so and give your own
file set's delta.

## The rules that make this a move and not a rewrite

1. **Every path that worked before must still work.** Callers outside your file set say
   `crate::stream::PipelineBudget`, `crate::config::Config`, and so on. Splitting
   `foo.rs` into `foo/` keeps `mod foo;` valid, but the *items* must stay reachable at
   their old paths — re-export from the new `mod.rs` with `pub(crate) use` / `pub use`
   as needed. **Do not edit files outside your set to fix a path**; re-export instead.
   If a path genuinely cannot be preserved, stop and report rather than reaching out.
2. **No logic edits.** Not a rename, not a signature change, not a "while I'm here"
   improvement, not a clippy suggestion. The only permitted non-move changes are the
   mechanical consequences of moving: `use` lines, visibility widened from private to
   `pub(super)`/`pub(crate)` **only where the move requires it**, and `#[cfg]` attributes
   that must travel with their items.
3. **Visibility is where this goes wrong.** Splitting turns file-private items into
   cross-module ones, and the reflex is to make everything `pub(crate)`. Use the
   narrowest that compiles — `pub(super)` inside the new module tree — because
   `unreachable_pub` and `redundant_pub_crate` were both measured and argued over in
   `25-lint.md`, and `unreachable_pub` is enabled at 0. Widening visibility beyond need
   silently undoes that work.
4. **Move the comments with the code.** This crate's comments carry design rationale on
   purpose and the audit repeatedly relied on them. A module header (`//!`) for each new
   file is expected — say what the module owns and why it is a seam — but do not delete
   or rewrite existing rationale, and do not reflow prose you are not moving.
5. **Tests move with the code they test.** Where a `#[cfg(test)] mod tests` block covers
   items now living in two modules, split it along the same seam and keep every
   assertion. If a test reaches a now-private item, prefer moving the test next to the
   item over widening the item's visibility.
6. **Convention: `foo/mod.rs`.** The crate has nine `mod.rs` files and one exception
   (`config.rs`); `clippy::self_named_module_files` measures that (not `mod_module_files`, which bans `mod.rs` and would invert the convention). Follow the majority — new
   directories get a `mod.rs`. Do not switch the crate to the adjacent style in this pass.

## What to do when the split does not want to happen

Several of these files have a documented prerequisite: a group of fields that must be
bundled first, or a private field reached from three places. `24-proj.md` names them per
file.

If the clean seam needs a real refactor, **do not force the move**. A split that drags a
behavioural change with it defeats the point of having held this pass back. Do the part
that is genuinely mechanical, and append an accurate `- [ ]` line to `_HANDOFF.md`
describing the prerequisite and what remains. A partial split that is honest beats a
complete one that hides a rewrite.

Equally: if after reading the file you judge the proposed split is **wrong** — the
seams are not where the report guessed, or the file is cohesive and splitting would
scatter one idea across three files — say so and do not do it. The report's author never
had to make it compile. `24-proj.md` was explicit that a vague "this file is too long"
is not a finding; the same standard applies to its own suggestions.

## Report

≤ 20 lines. The new file list with line counts, what moved into each, every visibility
you had to widen and why, the four gates numerically, and anything you declined with its
reason. State plainly whether the diff is a pure move or whether anything else rode
along.
