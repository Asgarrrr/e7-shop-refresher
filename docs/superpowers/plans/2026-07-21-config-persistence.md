# Config Persistence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Clicking Apply in the GUI Setup tab writes the edited `[filter]`, `[limits]`, and `[actuator.timings]` sections back to `config.toml`, preserving every other section and the comments above each header.

**Architecture:** A new pure module `src/config/persist.rs` re-serializes a changed section with `toml_edit::ser::to_document`, then splices its table into a `toml_edit::DocumentMut` parsed from the current file — whole-section replacement, decor of an existing header preserved. `ShopApp` maps the Apply `Vec<Command>` into `persist::Section`s and calls `persist::save` best-effort; a write failure is journaled, never fatal.

**Tech Stack:** Rust 2024, `toml_edit` 0.25 (promoted from transitive to direct dep — same version `toml` already pulls), `serde`. `src/config.rs` keeps its current single-file form; the submodule lives at `src/config/persist.rs` (Rust 2024 allows `config.rs` beside a `config/` directory, so no file move).

---

### Task 1: Serialize derives on the persisted types

The three payload types (`Filter`, `Limits`, `Timings`) and their nested types currently derive only `Deserialize`. Persistence re-serializes them, so add `Serialize`. Pin the derives with a round-trip test.

**Files:**
- Modify: `src/domain/filter.rs:5`, `:19`, `:44` (imports + `Filter`, `SubstatReq`)
- Modify: `src/domain/shop.rs:5`, `src/domain/shop.rs:131` (`ItemKind` derive)
- Modify: `src/domain/control/mod.rs:50` (`Limits`)
- Modify: `src/actuator/plan.rs:184`, `:212` (`DelayRange`, `Timings`)
- Test: `src/domain/filter.rs` (round-trip in the existing `#[cfg(test)]` module)

- [ ] **Step 1: Write the failing test**

In `src/domain/filter.rs`'s `#[cfg(test)] mod tests` (add one if absent), append:

```rust
#[test]
fn filter_round_trips_through_toml() {
    let filter = Filter {
        names: vec!["ticketrare_name".to_owned()],
        min_substats: Some(3),
        max_price: Some(300_000),
        required_substats: vec![SubstatReq {
            name: "speed".to_owned(),
            min: Some(8.0),
        }],
        ..Filter::default()
    };
    let text = toml::to_string(&filter).expect("serialize");
    let back: Filter = toml::from_str(&text).expect("deserialize");
    assert_eq!(filter, back);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --no-default-features filter_round_trips_through_toml`
Expected: FAIL — `Filter: Serialize` is not satisfied (`toml::to_string` won't compile).

- [ ] **Step 3: Add the derives**

`src/domain/filter.rs:5` — widen the serde import:
```rust
use serde::{Deserialize, Serialize};
```
`src/domain/filter.rs:19` (`Filter`) and `:44` (`SubstatReq`) — add `Serialize` to each derive list, e.g.:
```rust
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
```
```rust
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
```

`src/domain/shop.rs:5` — widen the import to `use serde::{Deserialize, Deserializer, Serialize};`, then add `Serialize` to `ItemKind`'s derive at `:131`. Keep the existing `#[serde(rename_all = "snake_case")]` and `#[serde(other)]` attributes: `#[serde(other)]` coexists with a `Serialize` derive (verified — it compiles), and `Unknown` is filtered out before persistence so it never serializes.

`src/domain/control/mod.rs:50` (`Limits`) — add `Serialize` (import it at the top of the file if not already in scope).

`src/actuator/plan.rs:184` (`DelayRange`) and `:212` (`Timings`) — add `Serialize` (import it at the top of the file).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --no-default-features filter_round_trips_through_toml`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/domain/filter.rs src/domain/shop.rs src/domain/control/mod.rs src/actuator/plan.rs
git commit -m "domain: derive Serialize on persisted config types"
```

---

### Task 2: The persist module — pure `write_sections`

The core of persistence: rewrite the three managed sections in a document string, preserving everything else. Pure and disk-free so it is fully testable.

**Files:**
- Modify: `Cargo.toml` (add `toml_edit`)
- Modify: `src/error.rs` (add `ConfigWrite` variant)
- Modify: `src/config.rs` (declare `pub mod persist;`)
- Create: `src/config/persist.rs`
- Test: `src/config/persist.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Add the dependency and error variant**

`Cargo.toml`, under `[dependencies]` (next to `toml = "1"`):
```toml
toml_edit = "0.25"
```

`src/error.rs`, add a variant after `Config` (line 10):
```rust
    #[error("config write: {0}")]
    ConfigWrite(String),
```

`src/config.rs`, at the top of the file after the module doc comment (before `use`), add:
```rust
pub mod persist;
```

- [ ] **Step 2: Write the failing tests**

Create `src/config/persist.rs` with only the test module first (so the test names exist and fail to compile against the not-yet-written functions):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::control::Limits;
    use crate::domain::filter::Filter;

    fn hunt_filter() -> Filter {
        Filter {
            names: vec!["ticketrare_name".to_owned()],
            ..Filter::default()
        }
    }

    #[test]
    fn untouched_sections_and_header_comments_survive() {
        let text = "\
# top of file
game_port = 3333

[reconnect]
initial_ms = 1000

# what we hunt
[filter]
names = [\"old_name\"]

[capture]
buffer_size = 65535
";
        let out = write_sections(text, &[Section::Filter(hunt_filter())]).expect("write");
        assert!(out.contains("game_port = 3333"));
        assert!(out.contains("[reconnect]"));
        assert!(out.contains("initial_ms = 1000"));
        assert!(out.contains("[capture]"));
        assert!(out.contains("buffer_size = 65535"));
        assert!(out.contains("# what we hunt"), "above-header comment kept");
        assert!(out.contains("ticketrare_name"));
        assert!(!out.contains("old_name"), "old section value replaced");
    }

    #[test]
    fn round_trips_back_through_config() {
        let filter = hunt_filter();
        let limits = Limits {
            max_refreshes: Some(10),
            max_spend: Some(30),
            ..Limits::default()
        };
        let mut timings = Timings::default();
        timings.refreshed = crate::actuator::plan::DelayRange {
            min_ms: 200,
            max_ms: 800,
        };
        let out = write_sections(
            "",
            &[
                Section::Filter(filter.clone()),
                Section::Limits(limits.clone()),
                Section::Timings(timings),
            ],
        )
        .expect("write");
        let config: crate::config::Config = toml::from_str(&out).expect("reload");
        assert_eq!(config.filter, filter);
        assert_eq!(config.limits, limits);
        assert_eq!(config.actuator.timings, timings);
    }

    #[test]
    fn none_limit_omits_its_key() {
        let limits = Limits {
            max_refreshes: Some(10),
            max_spend: None,
            ..Limits::default()
        };
        let out = write_sections("", &[Section::Limits(limits)]).expect("write");
        assert!(out.contains("max_refreshes = 10"));
        assert!(!out.contains("max_spend"), "None limit is absent, not written");
    }

    #[test]
    fn timings_keep_existing_actuator_mode() {
        let text = "\
[actuator]
dry_run = true
backend = \"input\"
";
        let mut timings = Timings::default();
        timings.between_buys = crate::actuator::plan::DelayRange {
            min_ms: 100,
            max_ms: 500,
        };
        let out = write_sections(text, &[Section::Timings(timings)]).expect("write");
        assert!(out.contains("dry_run = true"), "mode key kept");
        assert!(out.contains("backend = \"input\""), "backend kept");
        assert!(out.contains("[actuator.timings]"));
        // Ranges render inline, matching config.example.toml's style.
        assert!(out.contains("{ min_ms = 100, max_ms = 500 }"));
    }

    #[test]
    fn missing_file_body_yields_just_the_sections() {
        // The disk wrapper starts from "" when the file is absent; the pure
        // core must then emit only the managed sections.
        let out = write_sections("", &[Section::Filter(hunt_filter())]).expect("write");
        assert!(out.contains("[filter]"));
        assert!(out.contains("ticketrare_name"));
        assert!(!out.contains("[reconnect]"));
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --no-default-features --features gui persist`
Expected: FAIL to compile — `write_sections`, `Section`, `Timings` not in scope in `persist.rs`.

- [ ] **Step 4: Write the module implementation**

Prepend the implementation above the test module in `src/config/persist.rs`:

```rust
//! Format-preserving persistence of the GUI-editable config sections back to
//! config.toml. Only `[filter]`, `[limits]`, and `[actuator.timings]` are
//! rewritten; every other section (network, capture, actuator mode) is left
//! exactly as the player wrote it. Whole-section replacement: a section's
//! inner commented-out example lines are dropped on first save, but the
//! comments above each header survive (the replaced table's decor is kept).

use std::path::Path;

use serde::Serialize;
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::actuator::plan::Timings;
use crate::domain::control::Limits;
use crate::domain::filter::Filter;
use crate::error::{Error, Result};

/// One GUI-editable section to persist. Built per Apply from the commands that
/// actually changed, so a limits-only edit never rewrites `[filter]`.
pub enum Section {
    Filter(Filter),
    Limits(Limits),
    Timings(Timings),
}

/// Rewrite `path` so the managed sections reflect `edits`, preserving every
/// other section. A missing file is created with just these sections.
pub fn save(path: impl AsRef<Path>, edits: &[Section]) -> Result<()> {
    let path = path.as_ref();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err.into()),
    };
    let updated = write_sections(&text, edits)?;
    std::fs::write(path, updated)?;
    Ok(())
}

/// Pure core: apply `edits` to the document text, return the new text.
fn write_sections(text: &str, edits: &[Section]) -> Result<String> {
    let mut doc: DocumentMut = text.parse().map_err(write_err)?;
    let root = doc.as_table_mut();
    for edit in edits {
        match edit {
            Section::Filter(filter) => set_table(root, "filter", section_table(filter)?),
            Section::Limits(limits) => set_table(root, "limits", section_table(limits)?),
            Section::Timings(timings) => {
                let mut table = section_table(timings)?;
                inline_ranges(&mut table);
                set_nested_table(root, "actuator", "timings", table);
            }
        }
    }
    Ok(doc.to_string())
}

/// Re-serialize a section value to a standalone table.
fn section_table<T: Serialize>(value: &T) -> Result<Table> {
    let doc = toml_edit::ser::to_document(value).map_err(write_err)?;
    Ok(doc.as_table().clone())
}

/// Replace `parent[key]` with `new`, keeping the old header's leading
/// comments/blank lines when the section already existed.
fn set_table(parent: &mut Table, key: &str, mut new: Table) {
    if let Some(old) = parent.get(key).and_then(Item::as_table) {
        *new.decor_mut() = old.decor().clone();
    }
    parent.insert(key, Item::Table(new));
}

/// Replace `parent[outer][inner]`, creating `outer` as an implicit table if it
/// is absent (so a fresh file grows only `[actuator.timings]`, no bare
/// `[actuator]` header, and an existing `[actuator]` keeps its other keys).
fn set_nested_table(parent: &mut Table, outer: &str, inner: &str, new: Table) {
    if parent.get(outer).and_then(Item::as_table).is_none() {
        let mut created = Table::new();
        created.set_implicit(true);
        parent.insert(outer, Item::Table(created));
    }
    if let Some(outer_table) = parent.get_mut(outer).and_then(Item::as_table_mut) {
        set_table(outer_table, inner, new);
    }
}

/// Render each child table (a `DelayRange`) as an inline `{ min_ms = .. }`,
/// matching the hand-written style in config.example.toml.
fn inline_ranges(table: &mut Table) {
    for (_, item) in table.iter_mut() {
        if let Item::Table(child) = item {
            let inline = child.clone().into_inline_table();
            *item = Item::Value(Value::InlineTable(inline));
        }
    }
}

fn write_err<E: std::fmt::Display>(err: E) -> Error {
    Error::ConfigWrite(err.to_string())
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --no-default-features --features gui persist`
Expected: PASS (all five). If `timings_keep_existing_actuator_mode` fails on the exact inline spacing, adjust the asserted substring to the emitted form — `toml_edit`'s canonical inline spacing is `{ min_ms = 100, max_ms = 500 }`.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/error.rs src/config.rs src/config/persist.rs
git commit -m "config: format-preserving persist module for GUI-editable sections"
```

---

### Task 3: Wire persistence into ShopApp's Apply path

`ShopApp` already collects the Apply `Vec<Command>` and dispatches it. Thread the config path in, map the setup commands to `persist::Section`s, and save best-effort before dispatch.

**Files:**
- Modify: `src/main.rs:122` (pass `CONFIG_PATH` to `ShopApp::new`)
- Modify: `src/ui/mod.rs` (`ShopApp` field, `new` param, persist on Apply)
- Test: `src/ui/mod.rs` (`#[cfg(test)] mod tests`) — a helper-level test on the command→section mapping

- [ ] **Step 1: Write the failing test**

The Apply-dispatch code path pushes through egui and the filesystem, so pin the piece with real logic — the command→section mapping — as a free function. In `src/ui/mod.rs`'s `#[cfg(test)] mod tests`, add:

```rust
#[test]
fn only_setup_commands_become_persisted_sections() {
    use crate::app::Command;
    let commands = vec![
        Command::Start,
        Command::SetLimits(crate::domain::control::Limits::default()),
    ];
    let sections = persisted_sections(&commands);
    // Start is not persisted; the limits edit is.
    assert_eq!(sections.len(), 1);
    assert!(matches!(
        sections[0],
        crate::config::persist::Section::Limits(_)
    ));
}
```

(`Command::Start` is a real unit variant, confirmed in `src/app/mod.rs`; it is filtered out, only the `SetLimits` edit survives.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --no-default-features --features gui only_setup_commands_become_persisted_sections`
Expected: FAIL — `persisted_sections` is not defined.

- [ ] **Step 3: Add the mapping helper and field**

In `src/ui/mod.rs`, add a free function (near the other module-level helpers, e.g. below `render_tab_content`):

```rust
/// The persistable sections for a batch of Apply commands — only the three
/// `Set*` producers; Start/Stop and friends are skipped.
fn persisted_sections(commands: &[Command]) -> Vec<config::persist::Section> {
    commands
        .iter()
        .filter_map(|command| match command {
            Command::SetFilter(filter) => {
                Some(config::persist::Section::Filter(filter.clone()))
            }
            Command::SetLimits(limits) => {
                Some(config::persist::Section::Limits(limits.clone()))
            }
            Command::SetTimings(timings) => {
                Some(config::persist::Section::Timings(*timings))
            }
            _ => None,
        })
        .collect()
}
```

Add the `config` import at the top of `src/ui/mod.rs` if not present:
```rust
use crate::config;
```

Add a `config_path` field to `ShopApp` (near `handles`, `src/ui/mod.rs:83`):
```rust
    config_path: &'static str,
```

Extend `ShopApp::new` (`src/ui/mod.rs:97`) with the path parameter and store it:
```rust
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        handles: SessionHandles,
        error: SessionErrorSlot,
        timings: Timings,
        config_path: &'static str,
    ) -> Self {
```
and in the returned struct literal (`src/ui/mod.rs:113`), add:
```rust
            config_path,
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --no-default-features --features gui only_setup_commands_become_persisted_sections`
Expected: PASS.

- [ ] **Step 5: Persist on Apply and pass the path from main**

In `src/ui/mod.rs`, at the dispatch site (`src/ui/mod.rs:207-213`), persist the setup edits before merging with the status-bar commands:

```rust
        // Persist the Setup edits before dispatch. Best-effort: the live apply
        // below is unaffected by a write failure — a read-only or unwritable
        // config.toml only costs the on-disk copy, journaled and moved past.
        let sections = persisted_sections(&applied);
        if !sections.is_empty()
            && let Err(err) = config::persist::save(self.config_path, &sections)
        {
            self.handles
                .journal
                .push(&[format!("config.toml not saved: {err}")]);
        }
        for command in clicked.into_iter().chain(applied) {
            let _ = self.handles.commands.try_send(command);
        }
```

In `src/main.rs:122`, pass the constant:
```rust
        Box::new(move |cc| {
            Ok(Box::new(ui::ShopApp::new(
                cc,
                handles,
                error,
                seed_timings,
                CONFIG_PATH,
            )))
        }),
```

- [ ] **Step 6: Verify build and tests**

Run: `cargo test --no-default-features --features gui`
Expected: PASS (whole gui-lane suite, including Task 2's persist tests and the mapping test).

- [ ] **Step 7: Commit**

```bash
git add src/ui/mod.rs src/main.rs
git commit -m "gui: persist Setup edits to config.toml on Apply"
```

---

### Task 4: Drop the "session only" footer and refresh stale docs

The Setup footer now lies. Remove it, and update the module/CLAUDE docs that still say config.toml is never rewritten.

**Files:**
- Modify: `src/ui/editor.rs:664-665` (footer), `:21` (module doc), `:750` (comment if it repeats the claim)
- Modify: `CLAUDE.md` (editor row, tranche 4b note, new persist don't-recreate row)

- [ ] **Step 1: Remove the footer**

Delete these two lines at `src/ui/editor.rs:664-665`:
```rust
    ui.add_space(theme::SP_XS);
    ui.weak("edits apply to this session only — config.toml is unchanged");
```
so the function ends with `commands` immediately after the Apply row's closing block. No replacement text — the save is silent; failures land in the journal (Task 3).

- [ ] **Step 2: Fix the stale editor doc comment**

At `src/ui/editor.rs:21`, update the wording that claims session-only. Change the phrase "session-only — config.toml is never rewritten." to reflect that Apply now also writes the changed sections back to config.toml. Keep the surrounding comment intact. Check `:750` for a repeat of the claim and correct it the same way if present.

- [ ] **Step 3: Run the gui suite (no behavior regressed)**

Run: `cargo test --no-default-features --features gui`
Expected: PASS. (If a snapshot/kittest asserts the footer text, update it to the footer's absence.)

- [ ] **Step 4: Update CLAUDE.md**

- In the `Filter/limits/timings drafts for the window` don't-recreate row, replace "session-only; config.toml untouched" with a note that Apply persists the changed sections to config.toml via `config::persist`.
- Add a new don't-recreate row:
  `| Persist GUI edits to config.toml | `config::persist::save` (format-preserving whole-section replace of `[filter]`/`[limits]`/`[actuator.timings]` via `toml_edit`; best-effort, journaled on failure) | `src/config/persist.rs` |`
- In the tranche 4b line, change "config.toml persistence remains a follow-up (`toml_edit`)" to note it landed.

- [ ] **Step 5: Commit**

```bash
git add src/ui/editor.rs CLAUDE.md
git commit -m "gui: drop session-only footer, note config persistence in docs"
```

---

### Task 5: Full verification

**Files:** none (verification only)

- [ ] **Step 1: Formatting**

Run: `cargo fmt --check`
Expected: clean (exit 0).

- [ ] **Step 2: Clippy, all lanes**

Run each; expected clean with `-D warnings`:
```bash
cargo clippy --all-targets -- -D warnings
cargo clippy --no-default-features -- -D warnings
cargo clippy --no-default-features --features gui -- -D warnings
cargo clippy --no-default-features --features actuator -- -D warnings
```
Note: `toml_edit` is used only inside `src/config/persist.rs`, which is compiled in every lane (config is not feature-gated), so no lane can trip an unused-dependency or missing-symbol warning.

- [ ] **Step 3: Tests**

Run: `cargo test --no-default-features --features gui`
Expected: PASS. (The default-feature lanes need the native Windows capture backend and are validated on the game machine per the project's split; the persist logic is platform-independent and fully covered by the gui lane.)

- [ ] **Step 4: Manual smoke (mac dev path)**

Run: `cargo run --no-default-features --features gui`, open Setup, change a limit, click Apply, then inspect `config.toml`.
Expected: the `[limits]` section reflects the new value; `[filter]`, network sections, and their comments are unchanged.

- [ ] **Step 5: Final diff review + commit if anything remains**

Run: `git diff --stat rewrite/network-capture`
Re-read each hunk; delete any debug leftover. Everything should already be committed by Tasks 1-4.
