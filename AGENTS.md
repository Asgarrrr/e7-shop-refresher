# AGENTS.md

Windows-only Epic Seven shop automation tool. The primary binary is the egui
GUI (`e7-shop-refresher`); `e7-shop-refresher-cli` is the headless sibling for
scripted or CI use. Code map and invariants: `ARCHITECTURE.md`. User manual:
`README.md`.

---

## Commands

| Task | Command |
|------|---------|
| Format | `cargo fmt --all` |
| Lint (CI gate) | `cargo clippy --all-targets -- -D warnings` |
| Test (CI gate) | `cargo test --all-targets` |
| Build GUI + CLI | `cargo build --all-targets` |
| Run CLI safely | `cargo run --bin e7-shop-refresher-cli -- --dry-run` |

CI (`.github/workflows/ci.yml`) runs fmt-check, clippy `-D warnings`, build,
and test on windows-latest — all four must pass before merge.

---

## Recipe: add a config field

Adding one `ShopConfig` field touches these files in order:

1. **`src/config/sections.rs`** — add the field to the relevant struct and set
   its default in the struct's `Default` impl (or via a `#[serde(default)]`
   helper function for non-`Default` types).

2. **`src/config/validate.rs`** — add a constraint in the struct's validate
   function only if the field has values that are outright invalid (e.g. ratios
   outside `[0, 1]`). Cross-field warnings (e.g. stop-count set but the buy
   flag is off) go here too; use `tracing::warn!` rather than returning `Err`
   so the GUI doesn't brick on stale configs.

3. **`src/gui/persist.rs`** — THREE sites, all required:
   - The `AutoSavedFields` struct — add a field of the same type.
   - `AutoSavedFields::from_config` — copy the value from `cfg`.
   - `write_all_back` — emit a `set_scalar` (or `set_rect_in`) call for the
     field inside the correct `// [section]` block. Missing this third site
     means GUI edits are silently discarded on the next launch.

4. **The owning GUI panel under `src/gui/panels/`** — add an editor widget
   (typically a `DragValue` or `Checkbox`) in the relevant panel file. For
   `ShopConfig` fields this is `src/gui/panels/run_tab.rs`; for timing fields
   it is `src/gui/panels/timing.rs`.

5. **The runtime consumer** — for shop-loop stop conditions this is
   `src/shop.rs`; for window/capture behaviour check `src/capture.rs` and
   `src/detector.rs`.

To see every touch-point for an existing field, grep the codebase for it:

```
grep -r stop_when_gold_spent src/
```

---

## Recipe: add a stop condition

1. Add a `ShopConfig` field in `src/config/sections.rs` (convention: `0` means
   disabled).

2. Add the check to `stop_condition_for` in `src/shop.rs`. The function uses a
   fixed priority order: **duration → mystic → covenant → gold**. Pick a
   position in that chain deliberately — the first condition that fires wins
   and the reason string is what gets logged.

3. Add unit tests next to the existing `stop_condition_*` tests in `src/shop.rs`
   using the `shop_with` helper. Cover: fires at threshold, does not fire below
   threshold, respects priority when multiple conditions are set.

4. Add a display entry in the stop-condition summary formatter in
   `src/gui/panels/run_tab.rs` (search for `stop_when_covenants` to find the
   block).

5. Follow recipe 3 above (persist + validate).

---

## Test conventions

Tests live in `#[cfg(test)] mod tests` at the bottom of each source file.
There is no `tests/` directory.

For shop-loop behaviour use the doubles already in `src/shop.rs`'s test module:
- `FakeCapture` / `FakeInput` — trait doubles for capture and input.
- `gray_frame` / `paint_zone` — helpers to build synthetic grayscale frames.
- `runner_for_loop_tests` — constructs a fully-wired `Runner` from a frame
  sequence; returns the runner and the event log from `FakeInput`.

---

## House rules

- Match sibling-code comment density — most helpers carry no doc comment.
- Keep UI strings and tooltips concise.
- Never add AI attribution to commits, comments, or docs.
- Commit messages are short imperative sentences without type prefixes
  (no `feat:`, `fix:`, etc.).
