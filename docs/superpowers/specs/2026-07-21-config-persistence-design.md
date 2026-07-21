# Persist GUI Setup edits to config.toml

## Goal

When the player clicks **Apply** in the Setup tab, the edited settings are written
to `config.toml` in addition to being applied live. Silent, best-effort. The
"edits apply to this session only — config.toml is unchanged" footer goes away.

Today the Apply loop (`ui::editor::edit_setup`) emits `Vec<Command>`
(`SetFilter`/`SetLimits`/`SetTimings`) applied to the live controller only; the
file is never rewritten. This closes the `toml_edit` follow-up noted in the
controller rollout (tranche 4b).

## Scope

Only the three GUI-editable sections are persisted:

- `[filter]`
- `[limits]`
- `[actuator.timings]`

Everything else (`game_port`, `server_url`, `[forward]`, `[reconnect]`,
`[capture]`, `actuator.dry_run`, `actuator.backend`) is left untouched by
`toml_edit`.

## Decisions (locked in brainstorming)

- **Trigger:** auto-save on Apply, no extra button or prompt.
- **Writer:** `toml_edit` (0.25.12, already transitive — promoting it to a
  direct dependency costs nothing), **full-section replacement** per changed
  section.
- **Location:** new module `src/config/persist.rs`. `src/config.rs` becomes
  `src/config/mod.rs` (mechanical move, no logic change).
- **Granularity:** full-section replace. Uniform for add/change/remove of keys.
  Accepted loss: commented-out example lines *inside* the three managed sections
  disappear on first save (e.g. `# min_substats = 3`). Comments *above* each
  `[section]` header survive.

## Architecture

### Serialization

Derive `Serialize` on the three payload types and their nested types so the
sections can be re-serialized from the live values:

- `Filter` + `RequiredSubstat` (`src/domain/filter.rs`)
- `Limits` (`src/domain/control/mod.rs`)
- `Timings` + `DelayRange` (`src/actuator/plan.rs`)
- `ItemKind` (`src/domain/shop.rs`) — already `serde(rename_all)` for
  deserialize; the same rename applies on serialize.

`Option::None` omits its key (toml serializer skips `None`), so a limit toggled
off simply disappears from the file — matching its "no limit" semantics. The
serialized form must round-trip through the existing `Deserialize` (verified by
a test).

### Persistence module — `src/config/persist.rs`

Pure core, testable without disk:

```rust
/// Which managed section an edit targets.
enum Section { Filter, Limits, Timings }

/// Replace the given sections' whole tables in `doc_text` with the
/// re-serialized values, preserving everything else. Sections not listed are
/// untouched.
fn write_sections(doc_text: &str, edits: &[SectionEdit]) -> Result<String>
```

`SectionEdit` carries the target section + its serialized `toml_edit::Item`
(built by serializing the value to a toml string and parsing it, or via
`toml_edit::ser`). `write_sections` parses `doc_text` into a
`toml_edit::DocumentMut`, assigns each section table, and renders back to a
`String`.

Thin disk wrapper:

```rust
/// Read config.toml (missing -> empty document, created on write), apply the
/// edits, write back. Returns the IO/format error for the caller to journal.
pub fn save(path, edits: &[SectionEdit]) -> Result<()>
```

`[actuator.timings]` is a nested table under `[actuator]`: `write_sections`
addresses it as `doc["actuator"]["timings"]`, creating `[actuator]` if absent
(an implicit table, no stray `dry_run`/`backend` keys invented).

### Trigger wiring — `ShopApp`

`ShopApp::new` gains the config path (`&'static str CONFIG_PATH`, threaded from
`main.rs`). Where the Apply `Vec<Command>` is already dispatched to the command
channel, build the matching `Vec<SectionEdit>` from the *same* command values
(a command's presence decides its section — a limits-only edit never rewrites
`[filter]`, keeping the current per-section semantics) and call
`persist::save`.

On error, push one journal line via `handles.journal`
(`config.toml not saved: <err>`) and continue. The live apply already happened;
persistence is best-effort and never interrupts the session.

### Editor footer

`ui::editor` drops the "edits apply to this session only — config.toml is
unchanged" weak line. No replacement text (silent save); the journal carries any
failure.

## Data flow

```
Apply click
  -> editor::edit_setup -> Vec<Command>            (unchanged: live apply)
  -> ShopApp: commands.try_send(...)               (unchanged)
  -> ShopApp: persist::save(CONFIG_PATH, edits)    (new)
       ok  -> file updated, sections replaced, rest preserved
       err -> journal "config.toml not saved: <err>"
```

## Error handling

- File read-only / disk full / parent missing: `save` returns `Err`, journaled,
  session continues.
- `config.toml` absent: treated as an empty document; `save` creates it with
  just the managed sections (nothing is lost — network keys fall back to their
  defaults on next load).
- Malformed existing `config.toml`: cannot happen at this point — `Config::load`
  already parsed it at startup, so the file is valid TOML. If a `toml_edit`
  parse still fails, it is journaled like any other save error.

## Testing

Pinned with the change:

1. `write_sections` preserves untouched content: input with comments +
   `[reconnect]`/`[capture]` + `[filter]`; write a new `[filter]` -> network
   sections and above-header comments survive, `[filter]` reflects new values.
2. Round-trip: serialize a non-default `Filter`/`Limits`/`Timings` through
   `write_sections`, reload via `Config::load` (or `toml::from_str`) -> equal to
   the source values.
3. `None` limit omits its key: a `Limits` with `max_spend = None` produces a
   `[limits]` table without a `max_spend` key.
4. `[actuator.timings]` written under a pre-existing `[actuator]` keeps
   `dry_run`/`backend` intact.
5. Missing-file path: `save` on a nonexistent path creates it with the managed
   sections and no others.

## Out of scope

- Persisting network/capture/actuator-mode settings (not GUI-editable).
- A separate Save button or session/permanent distinction (rejected: auto-save).
- Preserving commented example lines inside the three managed sections
  (accepted loss of full-section replacement).
- Config file migration/versioning.

## Definition of done

`cargo fmt --check` clean, all clippy lanes clean with `-D warnings`,
`cargo test` green, the five tests above written with the change, and the Setup
footer line removed.
