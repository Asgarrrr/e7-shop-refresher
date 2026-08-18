//! Format-preserving persistence of the GUI-editable config sections back to
//! config.toml. Only `[filter]`, `[limits]`, and `[actuator.timings]` are
//! rewritten; every other section (network, capture, actuator mode) is left
//! exactly as the player wrote it. Whole-section replacement: a section's
//! inner commented-out example lines are dropped on first save, but the
//! comments above each header survive (the replaced table's decor is kept).
//!
//! The one exception to "every other section is left alone" is
//! [`strip_retired_keys`], which deletes the retired `[capture]` / `[forward]`
//! keys once — see its documentation for why that has to happen here rather
//! than by asking the player to hand-edit a file the app owns.

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
    replace_file(path, &updated)
}

/// Put `contents` at `path`, atomically, creating the parent directory first.
///
/// Shared by [`save`] and [`strip_retired_keys`] so there is exactly one way
/// this crate writes `config.toml`: a second write path is how one of the two
/// ends up non-atomic, and the file it would truncate on failure is the one the
/// player hand-wrote.
///
/// # Errors
///
/// [`Error::ConfigWrite`] — creating the parent directory, writing the sibling
/// temp file, or renaming it over the target failed. The target is left
/// untouched in every one of those cases.
fn replace_file(path: &Path, contents: &str) -> Result<()> {
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
    if let Err(source) = std::fs::write(&tmp, contents).and_then(|()| std::fs::rename(&tmp, path)) {
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
/// The mirror image of [`CaptureConfig::retired_keys`] and
/// [`ForwardConfig::retired_keys`]: those two name what a loaded config *sets*,
/// this names what can be taken out of the file. The two lists have to stay in
/// step — a key named there but not here would warn forever, and a key named
/// here but not there would be deleted from the player's file with nothing in
/// the log to say so.
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
/// Both keys of each section were retired but kept parsed-and-ignored, because
/// `Config` and its sub-structs are `deny_unknown_fields`: deleting the fields
/// outright turns the next launch of every existing installation into
/// "Invalid configuration". The compatibility shim came with a startup warning
/// — and that warning could never stop, because [`save`] is format-preserving
/// and only ever rewrites `[filter]`, `[limits]` and `[actuator.timings]`, so
/// the keys survived every Apply. A one-time nudge became permanent noise whose
/// only cure was hand-editing a file the README says the app owns. Removing the
/// keys ourselves is what makes the nudge one-time.
///
/// Both `CaptureConfig` and `ForwardConfig` consist *only* of retired keys, so
/// stripping them empties the table; the emptied table's header goes too,
/// because a bare `[capture]` left behind is a worse artefact than the key was.
/// What does *not* go with it is any comment the player wrote around it — see
/// [`tidy`], which is where that distinction is made and why it costs a text
/// pass. A table this pass did not touch is never removed, empty or not:
/// `config.example.toml` ships both headers with every key commented out, and a
/// fresh install must come back `None` here.
///
/// # Errors
///
/// - [`Error::ConfigRead`] — the file could not be read. A *missing* file is
///   not an error: there is nothing to strip, so it yields `None`.
/// - [`Error::ConfigReparse`] — the file is not valid TOML. Unreachable from
///   the startup path, which only calls this after `Config::load` parsed the
///   same bytes, and kept typed rather than silently skipped.
/// - [`Error::ConfigWrite`] — the rewrite failed (read-only file, antivirus
///   lock). The file is left exactly as it was, retired keys included, which is
///   why the caller must keep warning about them in that case.
///
/// Callers treat every one of these as non-fatal: the keys are inert, so
/// failing to delete them costs a log line, not a startup.
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
/// out in `capture.buffer_size` form — ready to name in the log, so a player
/// reading it later sees which lines left their file.
#[derive(Debug)]
struct Stripped {
    text: String,
    removed: String,
}

/// Pure core: drop the retired keys from the document text. `None` when the
/// text sets none of them, which is the whole point — no rewrite, no log line,
/// and no mtime change on the file of every install from here on.
fn strip_sections(text: &str) -> Result<Option<Stripped>> {
    let mut doc: DocumentMut = text.parse()?;
    let root = doc.as_table_mut();
    let mut removed = Vec::new();
    // Headers whose table this pass emptied, and the retired key names of every
    // table it edited. Both are finished off by `tidy`, on the rendered text —
    // see there for why the tree cannot do it.
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
/// dotted name. Returns true when the table is now empty *and* this pass is
/// what emptied it — the caller takes its header line out in [`tidy`].
///
/// Every spelling a player's file can use is handled: the header table
/// (`[capture]`) the example ships, the inline one (`capture = { .. }`), and
/// the dotted `capture.filter = ".."` — `toml_edit` models the last as a table
/// too, which is why it is told apart by `is_dotted` rather than by its type.
/// The two that have no header line of their own are removed here and now; the
/// header table is left in the tree for `tidy`, which can drop the header
/// without taking the comments above it along.
///
/// The emptiness check is deliberately gated on having removed something: an
/// untouched empty table is somebody's commented-out section — the shape
/// `config.example.toml` seeds — not our leftover.
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

/// Finish the removal on the rendered text: drop each header in `headers`, drop
/// every line that is a commented-out assignment of one of `keys`, and absorb
/// the blank line a dropped header no longer needs.
///
/// A text pass, because neither of those two lines is reachable from the tree.
/// `toml_edit` attaches a comment to whatever *follows* it, so a section's
/// leading comments are not its own — they are the header's decor, and the
/// commented-out example lines at its end belong to the *next* header (or, for
/// a last section, to the document's trailer). Removing `[capture]` through
/// `Table::remove` therefore does both halves of the wrong thing at once, and
/// the live `%APPDATA%` file demonstrates both:
///
/// - it deletes the `# server_url = "ws://127.0.0.1:3001/refresh-shop"` line
///   sitting above `[forward]`, which is the player's own commented-out
///   alternative for a live key — silently editing settings, the one thing this
///   whole pass must not do;
/// - it keeps `# filter = "tcp and tcp.SrcPort == 3333"` from the end of
///   `[capture]` and re-homes it under `[reconnect]`, where it reads as a
///   commented `reconnect.filter`.
///
/// Dropping the header line and nothing else leaves every comment exactly where
/// its author put it, which is why the emptied table is rendered before being
/// removed rather than removed before being rendered.
///
/// The commented-key match is deliberately narrow — `#`, key, `=` — so prose
/// survives even when it names a retired key, and it runs only for the keys of
/// a table this pass edited. A prose comment that was *inside* a removed
/// section outlives it; that is the accepted cost of never touching a comment
/// we are not sure about.
fn tidy(text: &str, headers: &[&str], keys: &[&str]) -> String {
    let mut out = String::with_capacity(text.len());
    // A dropped header leaves the blank line that separated it from the section
    // before *and* the one before the section after; swallow one of them, but
    // only when the file had both, so the pass never invents or removes spacing
    // anywhere else.
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

    /// The user's live `%APPDATA%` file, in miniature: `config.example.toml`
    /// once shipped both blocks uncommented, so this is what is on disk for
    /// every player who launched an early build. Two warnings every startup,
    /// forever, until something removes the keys.
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
        // The log line names every key that left the file, in a fixed order.
        assert_eq!(
            stripped.removed,
            "capture.buffer_size, capture.filter, forward.server_to_client, forward.client_to_server"
        );
        // And what is left still loads: stripping must not produce a file the
        // next launch refuses.
        let config: crate::config::Config = toml::from_str(&stripped.text).expect("reload");
        assert_eq!(config.game_port, 3333);
        assert_eq!(config.capture.retired_keys(), None);
        assert_eq!(config.forward.retired_keys(), None);
    }

    #[test]
    fn stripping_keeps_untouched_sections_and_their_header_comments() {
        // The same guarantee `untouched_sections_and_header_comments_survive`
        // pins for `save`, extended to this second write path: it is the reason
        // both go through `toml_edit` instead of a regex.
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
        // `CaptureConfig` and `ForwardConfig` are made of retired keys only, so
        // stripping always empties the table. A leftover `[capture]` header
        // would be a worse artefact than the key it used to hold.
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
        // The normal case for every install from here on, and the reason the
        // pure core reports `None` rather than "the same text": no rewrite at
        // all means no log line and no mtime change on the player's file.
        let text = "\
# top of file
game_port = 3333

[filter]
names = [\"ticketrare_name\"]
";
        assert!(strip_sections(text).expect("valid TOML").is_none());
        // Including the shape `config.example.toml` seeds: both headers there,
        // every key inside them commented out. An empty table we did not touch
        // is the player's commented section, not our leftover.
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
        // Defensive: no such case exists today — both structs are made only of
        // retired keys, and `deny_unknown_fields` refuses anything else — so
        // this pins the rule as "remove the table when it is *empty*" rather
        // than "remove the retired table", which is what a later release adding
        // a live `[capture]` key would otherwise silently delete.
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
        // `capture = { buffer_size = .. }` and `capture.filter = ".."` are both
        // legal TOML for the same document, and both are what a hand-editing
        // player may leave behind. Missing them would keep the warning going
        // with no way out but the header form.
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
        // never wrote would send them looking for a line that is not there.
        let stripped = strip_sections("[capture]\nfilter = \"tcp\"\n")
            .expect("valid TOML")
            .expect("rewrites");
        assert_eq!(stripped.removed, "capture.filter");
    }

    /// **The regression that made this a text pass.** `Table::remove` on the
    /// emptied `[forward]` takes its decor with it — and a table's decor is not
    /// its own contents, it is every comment line above the header. In the
    /// user's live `%APPDATA%` file that is their commented-out loopback
    /// `server_url`: a setting of theirs, deleted by a pass whose whole promise
    /// is that it changes no setting.
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
        // Exact text, because the guarantee is the *layout*: the player's
        // commented alternative kept, in place, and the blank line the dropped
        // header used to need not left behind as a second one.
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
        // The other half: a commented example line at the end of `[capture]` is
        // `toml_edit`-attached to the *next* header, so removing the section
        // re-homes it — `# filter = ..` would resurface under `[reconnect]`,
        // reading as a commented `reconnect.filter`.
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
        // bytes first), and still typed rather than swallowed: a rewrite that
        // guessed at broken TOML would be a rewrite of the player's file.
        let err = strip_sections("game_port = = 3333\n").expect_err("malformed TOML");
        assert!(
            matches!(err, Error::ConfigReparse(_)),
            "expected a typed re-parse error, got {err:?}"
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
