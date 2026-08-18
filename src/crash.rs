//! Crash logging: a global panic hook that records every panic to a file.
//!
//! In the windowed build stdout/stderr are inert, and a panic on a worker task
//! or the capture thread is swallowed by the runtime — it would otherwise
//! surface only as a bare "session ended". The hook appends each panic (thread,
//! location, message, backtrace) to `crash.log`, preferring the per-user
//! app-data dir and falling back to the temp dir when that isn't writable.
//!
//! The file is the primary record. A `tracing` event is emitted *beside* it so
//! that `logs\` and `crash.log` cross-reference each other: without it the
//! rotated log — the artefact a player is actually asked to send — simply stops
//! mid-session with no marker, and nobody knows a second file exists.

use std::path::{Path, PathBuf};

/// Cap on `crash.log`. It is append-only across runs, unlike `logs\` which keeps
/// five rotated files, so a crash loop would otherwise grow it without bound.
/// Rotation here is "start over with a marker" rather than a numbered set: the
/// interesting entry is the most recent one, and the hook must stay simple.
const MAX_CRASH_LOG_BYTES: u64 = 1 << 20;

/// Installs the global panic hook. Call once, before anything can panic. Chains
/// the previous hook so the console build still gets its stderr output.
pub fn install() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_owned();
        let location = info
            .location()
            .map(|l| l.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let message = panic_message(info.payload());
        let epoch = epoch_secs();
        let entry = crash_entry(epoch, &thread, &location, &message, &backtrace);
        let candidates = crash_log_paths();
        let written = write_first_writable(&candidates, &entry);
        // Strictly additive, and safe on the hook's own terms: the non-blocking
        // writer is a channel send, and before `install_logging` has run there is
        // no subscriber, which makes the macro a no-op (the hook is installed
        // first on purpose). `epoch_s` is repeated from the file record on
        // purpose: it is the join key between this line's formatted timestamp and
        // `crash.log`'s raw epoch stamp, so the two files can be correlated
        // without any date arithmetic in the hook.
        match &written {
            Some(path) => tracing::error!(
                thread = %thread,
                location = %location,
                panic = %message,
                epoch_s = epoch,
                crash_log = %path.display(),
                "panic — full backtrace in crash.log"
            ),
            None => tracing::error!(
                thread = %thread,
                location = %location,
                panic = %message,
                epoch_s = epoch,
                "panic — and crash.log could not be written anywhere, so there is no backtrace on disk"
            ),
        }
        default_hook(info);
    }));
}

/// The panic payload as text (panics carry `&str` or `String`).
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_owned())
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One `crash.log` record. Pure (time passed in) so it can be tested.
fn crash_entry(
    epoch_secs: u64,
    thread: &str,
    location: &str,
    message: &str,
    backtrace: &str,
) -> String {
    format!(
        "=== panic (epoch {epoch_secs}s) ===\nthread: {thread}\nlocation: {location}\nmessage: {message}\nbacktrace:\n{backtrace}\n\n"
    )
}

/// Candidate log paths, most-preferred first: the per-user app-data dir, then
/// the temp dir as a guaranteed-writable fallback.
fn crash_log_paths() -> Vec<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    crash_log_paths_from(local.as_deref(), &std::env::temp_dir())
}

/// Pure ordering: per-user app-data first (kept out of the user's face and off
/// shared dirs), the temp dir as a guaranteed-writable fallback.
///
/// Borrowed, not owned: both inputs are only ever `join`ed, so a caller — and
/// the tests, which is why this split exists at all — can hand over a literal.
fn crash_log_paths_from(local_appdata: Option<&Path>, temp: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(local) = local_appdata {
        paths.push(local.join(crate::APP_DIR).join("crash.log"));
    }
    paths.push(temp.join("arkyve-crash.log"));
    paths
}

/// Appends the entry to the first writable candidate and returns which one it
/// was, so the `tracing` line in the hook can name the file. Best-effort: the
/// panic hook must never itself panic, so every failure is swallowed.
fn write_first_writable<'a>(paths: &'a [PathBuf], entry: &str) -> Option<&'a PathBuf> {
    paths.iter().find(|path| append(path, entry).is_ok())
}

fn append(path: &Path, entry: &str) -> std::io::Result<()> {
    use std::io::Write;
    // Best-effort: create the app-data parent so the preferred path is usable
    // even before capture has created the folder. Ignore the result — the panic
    // hook must never itself panic, and the open below reports real failures.
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Truncate rather than append once the cap is reached: see
    // `MAX_CRASH_LOG_BYTES`. A metadata error (the usual case: the file does not
    // exist yet) means "not oversized", never "start over".
    let oversized = std::fs::metadata(path).is_ok_and(|meta| meta.len() >= MAX_CRASH_LOG_BYTES);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(!oversized)
        .write(oversized)
        .truncate(oversized)
        .open(path)?;
    if oversized {
        file.write_all(
            format!("=== crash.log exceeded {MAX_CRASH_LOG_BYTES} bytes and was restarted ===\n")
                .as_bytes(),
        )?;
    }
    file.write_all(entry.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RAII scratch file: removed on drop, *including* when an assertion panics.
    /// The hand-rolled after-the-fact cleanup this replaces leaked on every
    /// failing run, and a leaked *good* file was worse than a leaked blocker —
    /// `append` opens for append, so a stale file made the next broken run pass.
    /// Named by test and pid so parallel tests and parallel `cargo test`
    /// invocations cannot collide.
    struct TempFile(PathBuf);

    impl TempFile {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "arkyve_{tag}_{}_{:?}.log",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn crash_entry_captures_thread_location_and_message() {
        let entry = crash_entry(
            42,
            "capture",
            "src/capture/pcap.rs:60",
            "recv failed",
            "<backtrace>",
        );
        assert!(entry.contains("epoch 42s"));
        assert!(entry.contains("thread: capture"));
        assert!(entry.contains("location: src/capture/pcap.rs:60"));
        assert!(entry.contains("message: recv failed"));
        assert!(entry.contains("<backtrace>"));
    }

    #[test]
    fn prefers_local_appdata_over_temp() {
        let paths = crash_log_paths_from(
            Some(Path::new("C:/Users/x/AppData/Local")),
            Path::new("C:/Temp"),
        );
        assert_eq!(paths.len(), 2);
        assert!(paths[0].ends_with("arkyve-refresh-shop/crash.log"));
        assert!(paths[1].ends_with("arkyve-crash.log"));
    }

    #[test]
    fn falls_back_to_temp_without_appdata() {
        let paths = crash_log_paths_from(None, Path::new("/tmp"));
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("arkyve-crash.log"));
    }

    #[test]
    fn write_first_writable_falls_back_past_an_unwritable_path() {
        // First candidate is under a path that cannot exist as a directory
        // (a real file sits in the middle), so `append`'s best-effort
        // `create_dir_all` fails, the open fails, and the fallback temp file is
        // used instead. The middle component is materialized as a file below so
        // the premise holds now that `append` creates parents.
        let blocker = TempFile::new("nope");
        std::fs::write(blocker.path(), b"not a directory").unwrap();
        let bad = blocker.path().join("inner/crash.log");
        let good = TempFile::new("crash_test");

        let candidates = [bad, good.path().to_owned()];
        let first = write_first_writable(&candidates, "entry-one\n");
        assert_eq!(first.map(PathBuf::as_path), Some(good.path()));
        assert!(write_first_writable(&candidates[1..], "entry-two\n").is_some());

        let body = std::fs::read_to_string(good.path()).unwrap();
        assert!(body.contains("entry-one"));
        assert!(body.contains("entry-two"));
    }

    #[test]
    fn write_first_writable_reports_when_no_candidate_is_writable() {
        // The state that makes the hook's "no backtrace on disk" line the only
        // record of the panic.
        let blocker = TempFile::new("all_bad");
        std::fs::write(blocker.path(), b"not a directory").unwrap();
        let candidates = [blocker.path().join("inner/crash.log")];
        assert!(write_first_writable(&candidates, "entry\n").is_none());
    }

    #[test]
    fn an_oversized_crash_log_is_restarted_instead_of_growing_without_bound() {
        let file = TempFile::new("cap");
        std::fs::write(
            file.path(),
            vec![b'x'; usize::try_from(MAX_CRASH_LOG_BYTES).expect("cap fits in usize")],
        )
        .unwrap();

        append(file.path(), "fresh-entry\n").unwrap();

        let body = std::fs::read_to_string(file.path()).unwrap();
        assert!(!body.contains("xxxx"), "the old content should be gone");
        assert!(body.contains("was restarted"));
        assert!(body.contains("fresh-entry"));
        // And an under-cap file still appends.
        append(file.path(), "second-entry\n").unwrap();
        let body = std::fs::read_to_string(file.path()).unwrap();
        assert!(body.contains("fresh-entry"));
        assert!(body.contains("second-entry"));
    }
}
