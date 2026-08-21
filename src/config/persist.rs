//! Format-preserving persistence of the GUI-editable parts of config.toml.
//! `[filter]`, `[limits]` and `[actuator.timings]` are rewritten as whole
//! sections; `actuator.dry_run` and `actuator.backend` are rewritten as single
//! keys. Everything else is left as the player wrote it.
//! Whole-section replacement drops a section's inner commented-out example
//! lines on first save, but keeps the comments above each header; a single-key
//! write keeps even those.
//!
//! [`strip_retired_keys`] is the one exception — see its docs.

use std::io::Write as _;
use std::path::Path;

use serde::Serialize;
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::actuator::plan::Timings;
use crate::config::ActuatorBackend;
use crate::domain::control::Limits;
use crate::domain::filter::Filter;
use crate::error::{Error, Result};

/// One GUI-editable piece of config.toml to persist. Built per Apply from what
/// actually changed, so a limits-only edit never rewrites `[filter]`.
///
/// # Why the last two are keys and not sections
///
/// The first three each own a whole TOML table, so a write can replace them
/// wholesale. The last two do not own anything: `dry_run` and `backend` are two
/// keys of `[actuator]`, a table that also holds `timings`, which
/// [`Section::Timings`] owns. Replacing `[actuator]` wholesale would delete the
/// player's tuned click timings on any Apply that touched the backend switch.
///
/// One variant per key rather than one for the pair is the same rule the
/// sentence above states, one level down — an Apply that moved only the
/// rehearsal switch must not rewrite the backend line.
///
/// `game_port` is deliberately absent. It is a `config.toml` key with no widget
/// (see `ui::editor::startup`), so nothing in the window can produce an edit to
/// persist. A variant for it existed briefly in `8d25453` and left with the
/// field.
///
/// `Debug` because [`save`] is best-effort and journals its failure by name.
#[derive(Debug, Clone, PartialEq)]
pub enum Section {
    Filter(Filter),
    Limits(Limits),
    Timings(Timings),
    /// `actuator.dry_run`.
    DryRun(bool),
    /// `actuator.backend`.
    Backend(ActuatorBackend),
}

/// Rewrite `path` so the managed sections reflect `edits`, preserving every
/// other section. A missing file is created with just these sections.
///
/// # Errors
///
/// - [`Error::ConfigRead`] — the existing file could not be read. A *missing*
///   file is not an error: it is an empty document, and gets created.
/// - [`Error::ConfigReparse`] — the existing file is not valid TOML. Rewriting
///   is format-preserving, so it must parse what the player wrote first.
/// - [`Error::ConfigSerialize`] — a section could not be rendered back to TOML.
///   Unreachable for every [`Section`] today — the three table variants
///   serialize infallibly and the two key variants do not serialize at all —
///   kept typed to stay distinguishable from the parse failure above.
/// - [`Error::ConfigWrite`] — the parent directory, the sibling temp file, or
///   the rename failed. The path is in the message: this is the read-only /
///   OneDrive-locked case, where Setup changes are otherwise lost in silence.
///
/// The original file is left untouched whenever this returns `Err`.
pub fn save(path: impl AsRef<Path>, edits: &[Section]) -> Result<()> {
    let path = path.as_ref();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(Error::ConfigRead {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let updated = write_sections(&text, edits)?;
    replace_file(path, &updated)
}

/// Put `contents` at `path`, atomically, creating the parent directory first.
///
/// Shared by [`save`] and [`strip_retired_keys`] so `config.toml` has exactly
/// one write path: a second one is how it ends up non-atomic.
///
/// # Errors
///
/// [`Error::ConfigWrite`] — the parent directory, the temp write or the rename
/// failed. The target is left untouched in every case.
fn replace_file(path: &Path, contents: &str) -> Result<()> {
    // The per-user app-data subdir may not exist yet on first run, and the
    // sibling-temp write below needs somewhere to land.
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|source| Error::ConfigWrite {
            path: path.to_path_buf(),
            source,
        })?;
    }
    // Atomic replace: write a sibling temp then rename, so a mid-write failure
    // never truncates the hand-authored config. On any failure, remove the temp
    // so a read-only or locked target doesn't accrete a stale `config.toml.tmp`.
    let tmp = path.with_extension("toml.tmp");
    // `sync_all` before the rename, or the sentence above is only nearly true:
    // NTFS can commit the rename durably while the data is still in page cache,
    // the next launch then finds a zero-length `config.toml`, `#[serde(default)]`
    // parses it as an all-default `Config`, and `seed_config_if_missing`
    // declines to restore it because the file exists. Silent total loss.
    let durable_write = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    };
    if let Err(source) = durable_write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::ConfigWrite {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
}

/// The retired-but-still-parsed keys, grouped by the table that holds them.
///
/// Must stay in step with [`CaptureConfig::retired_keys`] /
/// [`ForwardConfig::retired_keys`], or a key warns forever or gets silently
/// deleted with nothing logged.
///
/// [`CaptureConfig::retired_keys`]: super::CaptureConfig::retired_keys
/// [`ForwardConfig::retired_keys`]: super::ForwardConfig::retired_keys
const RETIRED_KEYS: &[(&str, &[&str])] = &[
    ("capture", &["buffer_size", "filter"]),
    ("forward", &["server_to_client", "client_to_server"]),
];

/// Delete the retired `[capture]` / `[forward]` keys from `path`, once.
///
/// Returns the keys it removed (`"capture.filter, forward.client_to_server"`),
/// or `None` when the file held none and was therefore not written at all.
///
/// [`save`] never touches `[capture]` or `[forward]` — it writes only the five
/// pieces [`Section`] names — so without this the startup warning never stops
/// firing. Both structs hold *only* retired keys, so stripping always empties
/// — and removes — the header too, but never a table this pass did not touch.
///
/// Commented-out *assignments* of those keys go as well, wherever they sit and
/// including ones the player typed — see [`tidy`]. Prose never does, nor a
/// commented assignment of a key that still exists.
///
/// # Errors
///
/// - [`Error::ConfigRead`] — unreadable. A *missing* file yields `None`.
/// - [`Error::ConfigReparse`] — not valid TOML. Unreachable from the startup
///   path (`Config::load` parsed the same bytes), kept typed rather than
///   silently skipped.
/// - [`Error::ConfigWrite`] — the rewrite failed. The file keeps its retired
///   keys, so the caller must keep warning about them.
///
/// All non-fatal: the keys are inert, so a failed delete costs a log line.
pub fn strip_retired_keys(path: impl AsRef<Path>) -> Result<Option<String>> {
    let path = path.as_ref();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::ConfigRead {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let Some(stripped) = strip_sections(&text)? else {
        return Ok(None);
    };
    replace_file(path, &stripped.text)?;
    Ok(Some(stripped.removed))
}

/// What one strip pass changed: the rewritten document, and the keys it took
/// out in `capture.buffer_size` form — ready to name in the log.
#[derive(Debug)]
struct Stripped {
    text: String,
    removed: String,
}

/// Pure core: drop the retired keys from the document text. `None` when the
/// text sets none of them — no rewrite, no log line, and no mtime change on the
/// file of every install from here on.
fn strip_sections(text: &str) -> Result<Option<Stripped>> {
    let mut doc: DocumentMut = text.parse()?;
    let root = doc.as_table_mut();
    let mut removed = Vec::new();
    // Headers this pass emptied, and the retired keys of every table it edited
    // — both finished off by `tidy` on the rendered text.
    let mut headers = Vec::new();
    let mut orphaned = Vec::new();
    for (table, keys) in RETIRED_KEYS {
        let before = removed.len();
        if strip_table(root, table, keys, &mut removed) {
            headers.push(*table);
        }
        if removed.len() > before {
            orphaned.extend_from_slice(keys);
        }
    }
    if removed.is_empty() {
        return Ok(None);
    }
    Ok(Some(Stripped {
        text: tidy(&doc.to_string(), &headers, &orphaned),
        removed: removed.join(", "),
    }))
}

/// Remove `keys` from `root[table]`, appending each removal to `removed` as a
/// dotted name. Returns true when the table is now empty *and* this pass
/// emptied it — the caller drops its header line in [`tidy`].
///
/// Handles all three spellings: header (`[capture]`), inline
/// (`capture = { .. }`) and dotted (`capture.filter = ".."`, which `toml_edit`
/// also models as a table, told apart by `is_dotted`). The first two are
/// removed here; the header is left for [`tidy`], which drops it without its
/// leading comments.
///
/// Gated on having removed something: an untouched empty table is the player's
/// commented-out section, the shape `config.example.toml` seeds, not a leftover.
fn strip_table(root: &mut Table, table: &str, keys: &[&str], removed: &mut Vec<String>) -> bool {
    let before = removed.len();
    let (empty, has_header) = match root.get_mut(table) {
        Some(Item::Table(section)) => {
            for key in keys {
                if section.remove(key).is_some() {
                    removed.push(format!("{table}.{key}"));
                }
            }
            (section.is_empty(), !section.is_dotted())
        }
        Some(Item::Value(Value::InlineTable(section))) => {
            for key in keys {
                if section.remove(key).is_some() {
                    removed.push(format!("{table}.{key}"));
                }
            }
            (section.is_empty(), false)
        }
        _ => (false, false),
    };
    let emptied = empty && removed.len() > before;
    if emptied && !has_header {
        root.remove(table);
    }
    emptied && has_header
}

/// Finish the removal on the rendered text: drop each header in `headers`,
/// drop every commented-out assignment of a key in `keys`, and absorb the
/// blank line a dropped header no longer needs.
///
/// A text pass, because neither line is reachable from the tree: `toml_edit`
/// attaches a comment to whatever *follows* it, so a section's leading
/// comments are the header's decor, and its trailing commented-out lines
/// belong to the *next* header. `Table::remove` on `[capture]` therefore does
/// both halves of the wrong thing at once — observed on the live `%APPDATA%`
/// file:
///
/// - deletes the player's own commented-out
///   `# server_url = "ws://127.0.0.1:3001/refresh-shop"` above `[forward]`,
///   silently editing a setting;
/// - keeps `# filter = "tcp and tcp.SrcPort == 3333"` from the end of
///   `[capture]` and re-homes it under `[reconnect]`, reading as a commented
///   `reconnect.filter`.
///
/// So the emptied table is rendered before being removed, leaving every
/// comment where its author put it.
///
/// The commented-key sweep is whole-document on purpose: none of
/// [`RETIRED_KEYS`]' four names is a live key of *any* table in the schema, so
/// such a line can only be a commented-out retired key whichever section it
/// sits under — and a commented sibling left behind is the re-homing defect
/// above. The match is narrow (`#`, key, `=`) so prose survives. A comment
/// *inside* a removed section outlives it: the accepted cost of never touching
/// a comment we are not sure about.
fn tidy(text: &str, headers: &[&str], keys: &[&str]) -> String {
    let mut out = String::with_capacity(text.len());
    // A dropped header leaves the blank line before it *and* the one before the
    // next section; swallow one, but only when both existed, so the pass never
    // invents or removes spacing elsewhere.
    let mut header_dropped = false;
    let mut previous_blank = true;
    for line in text.split_inclusive('\n') {
        if is_commented_assignment(line, keys) {
            continue;
        }
        if is_header_of(line, headers) {
            header_dropped = previous_blank;
            continue;
        }
        let blank = line.trim().is_empty();
        if blank && header_dropped {
            header_dropped = false;
            continue;
        }
        header_dropped = false;
        previous_blank = blank;
        out.push_str(line);
    }
    out
}

/// True for `# filter = "tcp"` and `#buffer_size=1`, false for a prose comment
/// that merely mentions the key.
fn is_commented_assignment(line: &str, keys: &[&str]) -> bool {
    let Some(comment) = line.trim_start().strip_prefix('#') else {
        return false;
    };
    let comment = comment.trim_start();
    keys.iter().any(|key| {
        comment
            .strip_prefix(*key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    })
}

/// True when `line` is the `[capture]` header of one of `headers`. Tolerates
/// the inner spacing TOML allows (`[ capture ]`) and a trailing comment, so a
/// hand-written file does not silently keep a bare header.
fn is_header_of(line: &str, headers: &[&str]) -> bool {
    let head = line.split('#').next().unwrap_or_default().trim();
    head.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .is_some_and(|name| headers.contains(&name.trim()))
}

/// Pure core: apply `edits` to the document text, return the new text.
fn write_sections(text: &str, edits: &[Section]) -> Result<String> {
    let mut doc: DocumentMut = text.parse()?;
    let root = doc.as_table_mut();
    for edit in edits {
        match edit {
            Section::Filter(filter) => set_table(root, "filter", section_table(filter)?),
            Section::Limits(limits) => set_table(root, "limits", section_table(limits)?),
            Section::Timings(timings) => {
                // The write boundary the loader's `validate` never sees. No
                // clamp needed: `plan::DelayRange` is bounded by construction,
                // so any `Timings` here is one `Config::load` accepts.
                let mut table = section_table(timings)?;
                inline_ranges(&mut table);
                set_nested_table(root, "actuator", "timings", table);
            }
            Section::DryRun(dry_run) => set_actuator_key(root, "dry_run", (*dry_run).into()),
            Section::Backend(backend) => {
                set_actuator_key(root, "backend", backend_key(*backend).into());
            }
        }
    }
    Ok(doc.to_string())
}

/// [`ActuatorBackend`] spelled the way the loader reads it.
///
/// An exhaustive match rather than a `Serialize` derive, on purpose: a variant
/// added later fails to compile *here* instead of silently writing a key that
/// the next launch refuses. `every_backend_writes_a_value_the_loader_accepts`
/// pins the pair from the other side.
fn backend_key(backend: ActuatorBackend) -> &'static str {
    match backend {
        ActuatorBackend::Input => "input",
        ActuatorBackend::Message => "message",
    }
}

/// Replace one key's value, keeping the key and every comment around it.
///
/// `Table::insert` replaces the whole `Item`, and an `Item`'s decor is the
/// comments and blank lines attached to it — the same class of loss [`tidy`]
/// exists to undo, one key down instead of one table. Assigning through the
/// existing `Value` touches the value alone, so
/// `game_port = 3333  # the port my client uses` keeps its trailing note and
/// whatever the player wrote above it.
///
/// An absent key is a plain insert: there is no decor to save.
fn set_key(table: &mut Table, key: &str, value: Value) {
    if let Some(existing) = table.get_mut(key).and_then(Item::as_value_mut) {
        let decor = existing.decor().clone();
        *existing = value;
        *existing.decor_mut() = decor;
    } else {
        table.insert(key, Item::Value(value));
    }
}

/// Write one key into `[actuator]`, creating or promoting the table first.
///
/// `set_implicit(false)` because an implicit table prints no header, and a
/// header-less `[actuator]` holding `dry_run` would render that key at the
/// document root — where `deny_unknown_fields` refuses it on the next launch.
/// Reached when the file has `[actuator.timings]` and no `[actuator]` of its
/// own, which is exactly what [`set_nested_table`] creates.
fn set_actuator_key(root: &mut Table, key: &str, value: Value) {
    if let Some(actuator) = ensure_table(root, "actuator") {
        actuator.set_implicit(false);
        set_key(actuator, key, value);
    }
}

/// Re-serialize a section value to a standalone table.
fn section_table<T: Serialize>(value: &T) -> Result<Table> {
    let doc = toml_edit::ser::to_document(value)?;
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

/// Replace `parent[outer][inner]`, ensuring `outer` is a header table first.
fn set_nested_table(parent: &mut Table, outer: &str, inner: &str, new: Table) {
    if let Some(outer_table) = ensure_table(parent, outer) {
        set_table(outer_table, inner, new);
    }
}

/// `parent[key]` as a header table that can hold entries, whatever shape it was
/// in — or `None` if it holds something that is not a table at all.
///
/// Absent → a fresh **implicit** table, so a new file grows only
/// `[actuator.timings]` and no bare `[actuator]` header. Inline
/// (`actuator = { .. }`) → promoted in place, so the keys already there survive
/// the splice. Already a header table → left as is, decor and all.
///
/// Shared by [`set_nested_table`] and [`set_actuator_key`] so the promotion
/// rules cannot drift apart: the second one exists precisely to write into a
/// table the first one may have created.
fn ensure_table<'a>(parent: &'a mut Table, key: &str) -> Option<&'a mut Table> {
    if let Some(inline) = parent.get(key).and_then(Item::as_inline_table).cloned() {
        parent.insert(key, Item::Table(inline.into_table()));
    } else if parent.get(key).and_then(Item::as_table).is_none() {
        let mut created = Table::new();
        created.set_implicit(true);
        parent.insert(key, Item::Table(created));
    }
    parent.get_mut(key).and_then(Item::as_table_mut)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::control::Limits;
    use crate::domain::filter::Filter;
    use crate::domain::shop::Crystals;

    fn hunt_filter() -> Filter {
        Filter {
            names: vec!["ticketrare_name".to_owned()],
            ..Filter::default()
        }
    }

    /// A range the type accepts — the only kind that can reach this module.
    fn range(min_ms: u64, max_ms: u64) -> crate::actuator::plan::DelayRange {
        crate::actuator::plan::DelayRange::try_new(min_ms, max_ms)
            .expect("the fixture range must be valid")
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
    fn inner_section_comment_is_dropped_on_replace() {
        let text = "\
# what we hunt
[filter]
# example: min_substats = 3
names = [\"old_name\"]
";
        let out = write_sections(text, &[Section::Filter(hunt_filter())]).expect("write");
        assert!(out.contains("# what we hunt"), "above-header comment kept");
        assert!(
            !out.contains("min_substats = 3"),
            "inner example comment dropped"
        );
        assert!(out.contains("ticketrare_name"));
    }

    #[test]
    fn round_trips_back_through_config() {
        let filter = hunt_filter();
        let limits = Limits {
            max_refreshes: Some(10),
            max_spend: Some(Crystals::new(30)),
            ..Limits::default()
        };
        let timings = Timings {
            refreshed: range(200, 800),
            ..Timings::default()
        };
        let out = write_sections(
            "",
            &[
                Section::Filter(filter.clone()),
                Section::Limits(limits),
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
        assert!(
            !out.contains("max_spend"),
            "None limit is absent, not written"
        );
    }

    #[test]
    fn timings_keep_existing_actuator_mode() {
        let text = "\
[actuator]
dry_run = true
backend = \"input\"
";
        let timings = Timings {
            between_buys: range(100, 500),
            ..Timings::default()
        };
        let out = write_sections(text, &[Section::Timings(timings)]).expect("write");
        assert!(out.contains("dry_run = true"), "mode key kept");
        assert!(out.contains("backend = \"input\""), "backend kept");
        assert!(out.contains("[actuator.timings]"));
        assert!(out.contains("{ min_ms = 100, max_ms = 500 }"));
    }

    #[test]
    fn inline_actuator_keeps_its_mode_keys() {
        let text = "actuator = { dry_run = true, backend = \"input\" }\n";
        let timings = Timings {
            buy_modal: range(50, 250),
            ..Timings::default()
        };
        let out = write_sections(text, &[Section::Timings(timings)]).expect("write");
        assert!(out.contains("dry_run = true"), "dry_run kept");
        assert!(out.contains("backend = \"input\""), "backend kept");
        assert!(out.contains("[actuator.timings]"));
        let config: crate::config::Config = toml::from_str(&out).expect("reload");
        assert!(config.actuator.dry_run);
        assert_eq!(config.actuator.backend, ActuatorBackend::Input);
    }

    #[test]
    fn inert_timing_ranges_are_not_written() {
        // Whole-section replacement puts every field of `Timings` in the file:
        // without the skips, one knob touched wrote all eight ranges, seven of
        // them `{ min_ms = 0, max_ms = 0 }` no-ops.
        let timings = Timings {
            refreshed: range(200, 800),
            ..Timings::default()
        };
        let out = write_sections("", &[Section::Timings(timings)]).expect("write");
        assert!(out.contains("refreshed = { min_ms = 200, max_ms = 800 }"));
        for inert in [
            "shop_opened",
            "purchase_resumed",
            "recovery",
            "confirm_refresh_modal",
            "buy_modal",
            "between_buys",
            "scroll_settle",
        ] {
            assert!(!out.contains(inert), "{inert} is inert and must be omitted");
        }
        // The omission still round-trips: `#[serde(default)]` restores them.
        let config: crate::config::Config = toml::from_str(&out).expect("reload");
        assert_eq!(config.actuator.timings, timings);
    }

    #[test]
    fn inert_filter_keys_are_not_written() {
        // The `Timings` twin: one item name typed would otherwise write
        // `kinds = []`, `sets = []`, `required_substats = []` and
        // `include_sold_out = false` too.
        let out = write_sections("", &[Section::Filter(hunt_filter())]).expect("write");
        assert!(out.contains("names = [\"ticketrare_name\"]"));
        for inert in ["kinds", "sets", "required_substats", "include_sold_out"] {
            assert!(!out.contains(inert), "{inert} is inert and must be omitted");
        }
        // The omission still round-trips: `#[serde(default)]` restores them.
        let config: crate::config::Config = toml::from_str(&out).expect("reload");
        assert_eq!(config.filter, hunt_filter());
    }

    #[test]
    fn a_persisted_substat_requirement_reloads() {
        // `toml_edit` renders this as the inline `required_substats = [{ name =
        // "speed", min = 8.0 }]`, not the `[[filter.required_substats]]`
        // array-of-tables `config.example.toml` documents. Both are the same
        // document to any parser: a house-style divergence, not a defect.
        let filter = Filter {
            required_substats: vec![crate::domain::filter::SubstatReq {
                name: "speed".to_owned(),
                min: Some(8.0),
            }],
            ..Filter::default()
        };
        let out = write_sections("", &[Section::Filter(filter.clone())]).expect("write");
        assert!(out.contains("required_substats"), "{out}");
        let config: crate::config::Config = toml::from_str(&out).expect("reload");
        assert_eq!(config.filter, filter);
    }

    #[test]
    fn the_widest_writable_timings_reload_through_the_loader() {
        // An unclamped `Timings` used to reach disk here and break the *next*
        // launch. No longer constructible, so what is left to prove is that the
        // widest a `Timings` *can* hold still round-trips.
        let widest = Timings {
            refreshed: range(0, crate::actuator::plan::MAX_TIMING_MS),
            shop_opened: range(
                crate::actuator::plan::MAX_TIMING_MS,
                crate::actuator::plan::MAX_TIMING_MS,
            ),
            ..Timings::default()
        };
        let out = write_sections("", &[Section::Timings(widest)]).expect("write");
        let config: crate::config::Config = toml::from_str(&out).expect("reload");
        config
            .validate()
            .expect("what we write must load and validate");
        assert_eq!(config.actuator.timings, widest);
    }

    #[test]
    fn all_default_timings_write_no_range_and_still_round_trip() {
        // Every range inert leaves the header alone; it must still reload as
        // the calibrated default rather than fail to parse.
        let out = write_sections("", &[Section::Timings(Timings::default())]).expect("write");
        assert!(!out.contains("min_ms"), "no no-op range written: {out:?}");
        let config: crate::config::Config = toml::from_str(&out).expect("reload");
        assert_eq!(config.actuator.timings, Timings::default());
    }

    #[test]
    fn a_malformed_existing_file_reports_the_parse_error_not_a_write_error() {
        // "your file is broken" and "we failed to serialize" used to funnel
        // through one `String` variant and read identically in the banner.
        let err = write_sections(
            "game_port = = 3333\n",
            &[Section::Limits(Limits::default())],
        )
        .expect_err("malformed TOML must not be rewritten");
        assert!(
            matches!(err, Error::ConfigReparse(_)),
            "expected a typed re-parse error, got {err:?}"
        );
        // The source survives now, which is what a `String` funnel destroyed.
        assert!(std::error::Error::source(&err).is_some());
    }

    /// The user's live `%APPDATA%` file, in miniature: `config.example.toml`
    /// once shipped both blocks uncommented, so this is what is on disk for
    /// every player who launched an early build.
    #[test]
    fn a_config_with_both_retired_blocks_comes_back_without_them() {
        let text = "\
game_port = 3333

[forward]
server_to_client = true
client_to_server = false

[capture]
buffer_size = 65575
filter = \"tcp and tcp.SrcPort == 3333\"
";
        let stripped = strip_sections(text)
            .expect("valid TOML")
            .expect("both blocks are retired, so this must rewrite");
        for key in [
            "server_to_client",
            "client_to_server",
            "buffer_size",
            "filter",
        ] {
            assert!(
                !stripped.text.contains(key),
                "{key} survived: {}",
                stripped.text
            );
        }
        assert!(stripped.text.contains("game_port = 3333"), "live key kept");
        assert_eq!(
            stripped.removed,
            "capture.buffer_size, capture.filter, forward.server_to_client, forward.client_to_server"
        );
        // Stripping must not produce a file the next launch refuses.
        let config: crate::config::Config = toml::from_str(&stripped.text).expect("reload");
        assert_eq!(config.game_port.get(), 3333);
        assert_eq!(config.capture.retired_keys(), None);
        assert_eq!(config.forward.retired_keys(), None);
    }

    #[test]
    fn stripping_keeps_untouched_sections_and_their_header_comments() {
        // The reason this second write path goes through `toml_edit` and not a
        // regex, same as `untouched_sections_and_header_comments_survive`.
        let text = "\
# top of file
game_port = 3333

[reconnect]
initial_ms = 1000

# what we hunt
[filter]
names = [\"ticketrare_name\"]

[capture]
buffer_size = 65535
";
        let stripped = strip_sections(text).expect("valid TOML").expect("rewrites");
        assert!(stripped.text.contains("# top of file"));
        assert!(stripped.text.contains("[reconnect]"));
        assert!(stripped.text.contains("initial_ms = 1000"));
        assert!(stripped.text.contains("# what we hunt"), "comment kept");
        assert!(stripped.text.contains("[filter]"));
        assert!(stripped.text.contains("ticketrare_name"));
    }

    #[test]
    fn an_emptied_retired_table_leaves_no_bare_header() {
        // Both structs hold only retired keys, so stripping always empties the
        // table.
        let text = "\
[capture]
buffer_size = 65575

[limits]
max_spend = 300
";
        let stripped = strip_sections(text).expect("valid TOML").expect("rewrites");
        assert!(
            !stripped.text.contains("[capture]"),
            "the emptied header must go too: {}",
            stripped.text
        );
        assert!(stripped.text.contains("[limits]"), "next section kept");
        assert!(stripped.text.contains("max_spend = 300"));
    }

    #[test]
    fn a_config_without_retired_keys_is_left_byte_identical() {
        // No rewrite means no log line and no mtime change on the player's file.
        let text = "\
# top of file
game_port = 3333

[filter]
names = [\"ticketrare_name\"]
";
        assert!(strip_sections(text).expect("valid TOML").is_none());
        // Including the shape `config.example.toml` seeds — both headers
        // present, every key commented out — which is the player's commented
        // section, not our leftover.
        let seeded = "\
[forward]
# server_to_client = true

[capture]
# buffer_size = 65575
";
        assert!(strip_sections(seeded).expect("valid TOML").is_none());
    }

    #[test]
    fn a_retired_table_that_still_holds_another_key_keeps_that_key_and_its_header() {
        // Defensive: no such case exists today, but this pins "remove the table
        // when empty" rather than "remove the retired table", so a later
        // release adding a live `[capture]` key is not silently deleted here.
        let text = "[capture]\nbuffer_size = 65575\nsomething_live = 7\n";
        let stripped = strip_sections(text).expect("valid TOML").expect("rewrites");
        assert!(
            !stripped.text.contains("buffer_size"),
            "retired key removed"
        );
        assert!(stripped.text.contains("[capture]"), "header kept");
        assert!(
            stripped.text.contains("something_live = 7"),
            "live key kept"
        );
        assert_eq!(stripped.removed, "capture.buffer_size");
    }

    #[test]
    fn a_retired_key_written_inline_is_stripped_too() {
        // `capture = { buffer_size = .. }` and `capture.filter = ".."` are the
        // same document to `toml`. Missing them would keep the warning going
        // with no way out but rewriting the file in the header form.
        let stripped = strip_sections("capture = { buffer_size = 65575 }\ngame_port = 3333\n")
            .expect("valid TOML")
            .expect("rewrites");
        assert!(!stripped.text.contains("buffer_size"));
        assert!(
            !stripped.text.contains("capture"),
            "emptied inline table goes"
        );
        assert!(stripped.text.contains("game_port = 3333"));

        let stripped = strip_sections("forward.client_to_server = false\n")
            .expect("valid TOML")
            .expect("rewrites");
        assert_eq!(stripped.removed, "forward.client_to_server");
        assert!(stripped.text.trim().is_empty(), "{:?}", stripped.text);
    }

    #[test]
    fn only_the_retired_keys_that_are_present_are_named() {
        // The list goes straight into the startup log; naming a key the player
        // never wrote sends them looking for a line that is not there.
        let stripped = strip_sections("[capture]\nfilter = \"tcp\"\n")
            .expect("valid TOML")
            .expect("rewrites");
        assert_eq!(stripped.removed, "capture.filter");
    }

    /// **The regression that made this a text pass.** `Table::remove` on the
    /// emptied `[forward]` takes its decor with it, and a table's decor is
    /// every comment line above the header. In the live `%APPDATA%` file that
    /// decor is the player's own commented-out loopback `server_url`.
    #[test]
    fn a_comment_above_a_removed_header_belongs_to_the_line_before_it() {
        let text = "\
server_url = \"wss://ingest.arkyve.dev/refresh-shop\"
# server_url = \"ws://127.0.0.1:3001/refresh-shop\"

[forward]
server_to_client = true

[reconnect]
initial_ms = 1000
";
        let stripped = strip_sections(text).expect("valid TOML").expect("rewrites");
        // Exact text, because the guarantee is the *layout*: the commented
        // alternative kept in place, and the blank line the dropped header
        // used to need not left behind as a second one.
        assert_eq!(
            stripped.text,
            "\
server_url = \"wss://ingest.arkyve.dev/refresh-shop\"
# server_url = \"ws://127.0.0.1:3001/refresh-shop\"

[reconnect]
initial_ms = 1000
"
        );
    }

    #[test]
    fn a_commented_out_retired_key_does_not_outlive_its_section() {
        // A commented example line at the end of `[capture]` is
        // `toml_edit`-attached to the *next* header, so removing the section
        // re-homes it — `# filter = ..` resurfacing under `[reconnect]`, where
        // it reads as a commented `reconnect.filter`.
        let text = "\
[reconnect]
initial_ms = 1000

[capture]
buffer_size = 65575
# filter = \"tcp and tcp.SrcPort == 3333\"   # manual override

[filter]
names = [\"ticketrare_name\"]
";
        let stripped = strip_sections(text).expect("valid TOML").expect("rewrites");
        assert_eq!(
            stripped.text,
            "\
[reconnect]
initial_ms = 1000

[filter]
names = [\"ticketrare_name\"]
"
        );
        // Only the commented *assignment* goes: prose that merely names a
        // retired key is the player's, and we are not sure enough to touch it.
        let text = "[capture]\nbuffer_size = 1\n# the buffer_size story\n\n[limits]\n";
        let stripped = strip_sections(text).expect("valid TOML").expect("rewrites");
        assert!(
            stripped.text.contains("# the buffer_size story"),
            "prose kept: {}",
            stripped.text
        );
    }

    #[test]
    fn a_malformed_file_is_reported_not_stripped() {
        // Unreachable from the startup path (`Config::load` parsed the same
        // bytes first), and still typed rather than swallowed: guessing at
        // broken TOML would be a rewrite of the player's file.
        let err = strip_sections("game_port = = 3333\n").expect_err("malformed TOML");
        assert!(
            matches!(err, Error::ConfigReparse(_)),
            "expected a typed re-parse error, got {err:?}"
        );
    }

    /// A switch is one line in the middle of a hand-written file. Replacing the
    /// `Item` instead of the `Value` would take the note beside it with it, and
    /// the untouched keys around it must not move either.
    #[test]
    fn writing_one_actuator_key_keeps_the_comments_wrapped_around_it() {
        let text = "\
game_port = 3333
# the mode I rehearse in before a long run
[actuator]
dry_run = false # flipped 2026-08 after the geometry check
backend = \"message\"
";
        let out = write_sections(text, &[Section::DryRun(true)]).expect("write");
        assert_eq!(
            out,
            "\
game_port = 3333
# the mode I rehearse in before a long run
[actuator]
dry_run = true # flipped 2026-08 after the geometry check
backend = \"message\"
"
        );
    }

    /// The reason these two are keys and not a section: `[actuator]` is
    /// shared with `Section::Timings`, so a backend switch must not be able to
    /// delete a tuned range.
    #[test]
    fn the_actuator_keys_leave_the_timings_beside_them_alone() {
        let text = "\
[actuator]
dry_run = false
backend = \"message\"

[actuator.timings]
refreshed = { min_ms = 200, max_ms = 800 }
";
        let out = write_sections(text, &[Section::Backend(ActuatorBackend::Input)]).expect("write");
        assert!(out.contains("backend = \"input\""), "{out}");
        assert!(out.contains("dry_run = false"), "sibling key kept: {out}");
        assert!(
            out.contains("refreshed = { min_ms = 200, max_ms = 800 }"),
            "the tuned range must survive a backend switch: {out}"
        );
        let config: crate::config::Config = toml::from_str(&out).expect("reload");
        assert_eq!(config.actuator.backend, ActuatorBackend::Input);
        assert!(!config.actuator.dry_run);
    }

    /// `[actuator.timings]` alone leaves `[actuator]` implicit — header-less.
    /// Writing a key into it without making it explicit puts that key at the
    /// document root, where `deny_unknown_fields` refuses it on next launch.
    #[test]
    fn a_key_written_into_an_implicit_actuator_table_gets_its_header() {
        let text = "[actuator.timings]\nrefreshed = { min_ms = 200, max_ms = 800 }\n";
        let out = write_sections(text, &[Section::DryRun(true)]).expect("write");
        let config: crate::config::Config =
            toml::from_str(&out).expect("the write must reload, not land at the root");
        assert!(config.actuator.dry_run, "{out}");
        assert_eq!(
            config.actuator.timings.refreshed,
            range(200, 800),
            "the timings must survive: {out}"
        );
    }

    /// An inline `[actuator]` is the same document to any parser, and the
    /// promotion path is shared with `Section::Timings` — so it is worth
    /// pinning from this side too.
    #[test]
    fn an_inline_actuator_table_survives_a_key_write() {
        let text = "actuator = { dry_run = true, backend = \"input\" }\n";
        let out =
            write_sections(text, &[Section::Backend(ActuatorBackend::Message)]).expect("write");
        let config: crate::config::Config = toml::from_str(&out).expect("reload");
        assert_eq!(config.actuator.backend, ActuatorBackend::Message);
        assert!(config.actuator.dry_run, "the sibling key kept its value");
    }

    /// The rule this plan establishes, from the writer's side: every value the
    /// editor can produce must be one the loader accepts. `backend_key`'s match
    /// is exhaustive, so a new variant cannot skip this test — it stops the
    /// build first.
    #[test]
    fn every_backend_writes_a_value_the_loader_accepts() {
        for backend in [ActuatorBackend::Input, ActuatorBackend::Message] {
            let out = write_sections("", &[Section::Backend(backend)]).expect("write");
            let config: crate::config::Config =
                toml::from_str(&out).unwrap_or_else(|err| panic!("{backend:?} must reload: {err}"));
            assert_eq!(config.actuator.backend, backend);
        }
    }

    /// The whole document, written from nothing and read back: both keys land
    /// in the table that owns them and nowhere else.
    #[test]
    fn the_two_startup_keys_round_trip_together() {
        let out = write_sections(
            "",
            &[
                Section::DryRun(true),
                Section::Backend(ActuatorBackend::Input),
            ],
        )
        .expect("write");
        let config: crate::config::Config = toml::from_str(&out).expect("reload");
        assert!(config.actuator.dry_run);
        assert_eq!(config.actuator.backend, ActuatorBackend::Input);
        config.validate().expect("what we write must also validate");
    }

    #[test]
    fn missing_file_body_yields_just_the_sections() {
        let out = write_sections("", &[Section::Filter(hunt_filter())]).expect("write");
        assert!(out.contains("[filter]"));
        assert!(out.contains("ticketrare_name"));
        assert!(!out.contains("[reconnect]"));
    }
}
