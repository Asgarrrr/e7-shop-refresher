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
    // The config lives in a per-user app-data subdir that may not exist yet on
    // first run (nothing created it before this first Apply); make it so the
    // sibling-temp write below has a directory to land in.
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    // Atomic replace: write a sibling temp then rename, so a mid-write failure
    // never truncates the hand-authored config. On any failure, remove the temp
    // so a read-only or locked target doesn't accrete a stale `config.toml.tmp`.
    let tmp = path.with_extension("toml.tmp");
    if let Err(err) = std::fs::write(&tmp, updated).and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err.into());
    }
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

/// Replace `parent[outer][inner]`, ensuring `outer` is a header table first.
/// Absent → a fresh implicit table (so a new file grows only
/// `[actuator.timings]`, no bare `[actuator]` header). Authored inline
/// (`actuator = { .. }`) → promoted to a header table in place so its other
/// keys (dry_run/backend) survive the splice. Already a header table → left as
/// is, its keys preserved.
fn set_nested_table(parent: &mut Table, outer: &str, inner: &str, new: Table) {
    if let Some(inline) = parent.get(outer).and_then(Item::as_inline_table).cloned() {
        parent.insert(outer, Item::Table(inline.into_table()));
    } else if parent.get(outer).and_then(Item::as_table).is_none() {
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
        let mut timings = Timings::default();
        timings.between_buys = crate::actuator::plan::DelayRange {
            min_ms: 100,
            max_ms: 500,
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
        let mut timings = Timings::default();
        timings.buy_modal = crate::actuator::plan::DelayRange {
            min_ms: 50,
            max_ms: 250,
        };
        let out = write_sections(text, &[Section::Timings(timings)]).expect("write");
        // Promoted to a header table; mode keys survive; timings spliced in.
        assert!(out.contains("dry_run = true"), "dry_run kept");
        assert!(out.contains("backend = \"input\""), "backend kept");
        assert!(out.contains("[actuator.timings]"));
        // The whole thing still loads and the mode is intact.
        let config: crate::config::Config = toml::from_str(&out).expect("reload");
        assert!(config.actuator.dry_run);
        assert_eq!(
            config.actuator.backend,
            crate::config::ActuatorBackend::Input
        );
    }

    #[test]
    fn missing_file_body_yields_just_the_sections() {
        let out = write_sections("", &[Section::Filter(hunt_filter())]).expect("write");
        assert!(out.contains("[filter]"));
        assert!(out.contains("ticketrare_name"));
        assert!(!out.contains("[reconnect]"));
    }
}
