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
///
/// # Errors
///
/// - [`Error::ConfigRead`] — the existing file could not be read (locked by an
///   antivirus, permission denied, a directory in its place). A *missing* file
///   is not an error: it is treated as an empty document and created.
/// - [`Error::ConfigReparse`] — the existing file is not valid TOML. Rewriting
///   is format-preserving, so it has to parse what the player wrote first; the
///   error carries the offending span.
/// - [`Error::ConfigSerialize`] — a section could not be rendered back to TOML.
///   Unreachable for the three concrete section types today, and kept typed so
///   it stays distinguishable from the parse failure above.
/// - [`Error::ConfigWrite`] — creating the parent directory, writing the
///   sibling temp file, or renaming it over the target failed. The path is in
///   the message: this is the read-only / OneDrive-locked `config.toml` case,
///   where every Setup change is otherwise lost without explanation.
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
    // The config lives in a per-user app-data subdir that may not exist yet on
    // first run (nothing created it before this first Apply); make it so the
    // sibling-temp write below has a directory to land in.
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
    if let Err(source) = std::fs::write(&tmp, updated).and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::ConfigWrite {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
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
        let timings = Timings {
            refreshed: crate::actuator::plan::DelayRange {
                min_ms: 200,
                max_ms: 800,
            },
            ..Timings::default()
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
        let timings = Timings {
            between_buys: crate::actuator::plan::DelayRange {
                min_ms: 100,
                max_ms: 500,
            },
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
            buy_modal: crate::actuator::plan::DelayRange {
                min_ms: 50,
                max_ms: 250,
            },
            ..Timings::default()
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
    fn inert_timing_ranges_are_not_written() {
        // Whole-section replacement means every field of `Timings` lands in the
        // file. Without the skips, the first Apply after touching one knob
        // wrote all eight ranges — seven of them `{ min_ms = 0, max_ms = 0 }`
        // no-ops — into a file this module exists to leave alone.
        let timings = Timings {
            refreshed: crate::actuator::plan::DelayRange {
                min_ms: 200,
                max_ms: 800,
            },
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
    fn all_default_timings_write_no_range_and_still_round_trip() {
        // The degenerate case of the skips: every range inert leaves the
        // header alone. It must still reload as the calibrated default rather
        // than fail to parse.
        let out = write_sections("", &[Section::Timings(Timings::default())]).expect("write");
        assert!(!out.contains("min_ms"), "no no-op range written: {out:?}");
        let config: crate::config::Config = toml::from_str(&out).expect("reload");
        assert_eq!(config.actuator.timings, Timings::default());
    }

    #[test]
    fn a_malformed_existing_file_reports_the_parse_error_not_a_write_error() {
        // The two failure modes used to funnel through one `String` variant:
        // "the file you wrote is broken" and "we failed to serialize" read
        // identically in the banner. They must not.
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

    #[test]
    fn missing_file_body_yields_just_the_sections() {
        let out = write_sections("", &[Section::Filter(hunt_filter())]).expect("write");
        assert!(out.contains("[filter]"));
        assert!(out.contains("ticketrare_name"));
        assert!(!out.contains("[reconnect]"));
    }
}
