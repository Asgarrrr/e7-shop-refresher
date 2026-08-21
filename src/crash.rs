//! Crash logging: a global panic hook that records every panic to a file.
//!
//! stdout/stderr are inert in the windowed build, so a panic on a worker or
//! capture thread would otherwise surface only as "session ended". The hook
//! appends each panic (thread, location, message, backtrace) to `crash.log`
//! (per-user app-data dir, falling back to temp) and emits a matching
//! `tracing` event so `logs\` and `crash.log` cross-reference each other.

use std::path::{Path, PathBuf};

/// Cap on `crash.log`: append-only across runs (unlike `logs\`'s five
/// rotated files), so it restarts with a marker instead of growing forever.
const MAX_CRASH_LOG_BYTES: u64 = 1 << 20;

/// Installs the global panic hook (call once, before anything can panic);
/// chains the previous hook so the console build still gets its stderr output.
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
        // Safe pre-`install_logging`: with no subscriber yet this is a no-op.
        // `epoch_s` repeats the file entry's epoch, joining this line to
        // `crash.log` without dates.
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

/// Candidate log paths, most-preferred first: per-user app-data dir, then temp dir as a guaranteed-writable fallback.
fn crash_log_paths() -> Vec<PathBuf> {
    let local = crate::absolute_env_dir("LOCALAPPDATA");
    crash_log_paths_from(local.as_deref(), &std::env::temp_dir(), std::process::id())
}

/// Pure version of [`crash_log_paths`], so a test can hand over a literal.
///
/// `pid` names the temp fallback: a *fixed* name in a world-writable directory
/// is an NTFS hard-link target an unprivileged process can plant in advance and
/// point at any file it can read, turning this append into an elevated
/// append-or-truncate of that file. A per-process name does not make that
/// impossible — a new hard link can still appear after `pid` is read — but it
/// removes the predictable name the attack needs ahead of time, and the pid is
/// otherwise-unpredictable to a process that has not already seen this one run.
fn crash_log_paths_from(local_appdata: Option<&Path>, temp: &Path, pid: u32) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(local) = local_appdata {
        paths.push(local.join(crate::APP_DIR).join("crash.log"));
    }
    paths.push(temp.join(format!("arkyve-crash-{pid}.log")));
    paths
}

/// Appends to the first writable candidate, returning which one so the
/// hook's `tracing` line can name it. Best-effort: every failure is
/// swallowed, since the panic hook must never itself panic.
fn write_first_writable<'a>(paths: &'a [PathBuf], entry: &str) -> Option<&'a PathBuf> {
    paths.iter().find(|path| append(path, entry).is_ok())
}

fn append(path: &Path, entry: &str) -> std::io::Result<()> {
    use std::io::Write;
    // Only the app-data candidate is gated, not the temp fallback: the latter
    // has no app-data root of its own to redirect (it is `temp_dir()` itself,
    // not a subdirectory this app created), and it is the ladder's guaranteed-
    // writable backstop — refusing it on top of the primary would leave a
    // panic with nowhere to land. Refused before `create_dir_all` gets a
    // chance to run: that call resolves reparse points same as everything
    // else, so checking after it would only notice a redirected parent once
    // the elevated write it exists to prevent had already gone through it.
    // `write_first_writable` treats this `Err` like any other and walks to
    // the next candidate.
    #[cfg(windows)]
    if let Some(parent) = path.parent()
        && parent
            .file_name()
            .is_some_and(|name| name == crate::APP_DIR)
        && !crate::dirhandle::is_plain_directory(parent)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} is not a plain directory (a reparse point, or not created yet)",
                parent.display()
            ),
        ));
    }
    // Best-effort: create the app-data parent so the path works before
    // capture creates the folder; the open below reports real failures.
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Truncate rather than append past `MAX_CRASH_LOG_BYTES`; a metadata
    // error (file doesn't exist yet) means "not oversized", never "start over".
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

    /// RAII scratch file: removed on drop, including on an assertion panic, and
    /// named by test and pid so parallel runs cannot collide — a leaked stale
    /// file lets a broken run pass silently.
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
            "src/capture/pcap/sys.rs:60",
            "recv failed",
            "<backtrace>",
        );
        assert!(entry.contains("epoch 42s"));
        assert!(entry.contains("thread: capture"));
        assert!(entry.contains("location: src/capture/pcap/sys.rs:60"));
        assert!(entry.contains("message: recv failed"));
        assert!(entry.contains("<backtrace>"));
    }

    #[test]
    fn prefers_local_appdata_over_temp() {
        let paths = crash_log_paths_from(
            Some(Path::new("C:/Users/x/AppData/Local")),
            Path::new("C:/Temp"),
            4242,
        );
        assert_eq!(paths.len(), 2);
        assert!(paths[0].ends_with("arkyve-refresh-shop/crash.log"));
        assert!(paths[1].ends_with("arkyve-crash-4242.log"));
    }

    #[test]
    fn falls_back_to_temp_without_appdata() {
        let paths = crash_log_paths_from(None, Path::new("/tmp"), 4242);
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("arkyve-crash-4242.log"));
    }

    #[test]
    fn the_temp_fallback_name_carries_the_process_id() {
        // A fixed name in a world-writable directory is a hard-link target
        // planted in advance; a per-process name removes the predictable name
        // that plant needs. `std::process::id()` and the literal here must
        // agree, not just both be "some number".
        let pid = std::process::id();
        let paths = crash_log_paths_from(None, Path::new("/tmp"), pid);
        assert!(
            paths[0]
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == format!("arkyve-crash-{pid}.log")),
            "expected the pid in the fallback file name: {:?}",
            paths[0]
        );
    }

    #[test]
    fn write_first_writable_falls_back_past_an_unwritable_path() {
        // First candidate sits under a path that can't exist as a directory
        // (a real file occupies the middle component below), so `append`'s
        // best-effort `create_dir_all` fails and the fallback temp file is used.
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

    /// RAII scratch directory: removed on drop including on an assertion
    /// panic, and named by test and pid so parallel tests cannot collide.
    #[cfg(windows)]
    struct TempDir(PathBuf);

    #[cfg(windows)]
    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "arkyve_crash_{tag}_{}_{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    #[cfg(windows)]
    impl Drop for TempDir {
        fn drop(&mut self) {
            // The junction goes first, with `remove_dir`, which unlinks a mount
            // point without touching what is behind it — harmless (and ignored)
            // when this instance never had one.
            let _ = std::fs::remove_dir(self.0.join(crate::APP_DIR));
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(windows)]
    use crate::dirhandle::junction;

    #[cfg(windows)]
    #[test]
    fn an_app_data_root_that_is_a_reparse_point_is_refused() {
        // The module header's attack, end to end: `home`'s app-data root is a
        // junction onto `victim`, which stands in for whatever the attacker
        // actually wants written or truncated.
        let home = TempDir::new("junction_home");
        let victim = TempDir::new("junction_victim");

        let root = home.path().join(crate::APP_DIR);
        if !junction(&root, victim.path()) {
            // Group policy, or a filesystem without reparse points: asserting
            // on a machine that cannot host the attack proves nothing.
            eprintln!("skipped: mklink /J is unavailable here");
            return;
        }

        let result = append(&root.join("crash.log"), "entry\n");

        assert!(
            result.is_err(),
            "append through a junctioned app-data root must be refused, not followed"
        );
        assert!(
            !victim.path().join("crash.log").exists(),
            "the entry was written through the junction into the victim directory"
        );
    }

    #[cfg(windows)]
    #[test]
    fn an_app_data_root_that_is_a_real_directory_is_not_refused() {
        // The other half of the pair: a check that refused everything would
        // pass the junction test just as well.
        let home = TempDir::new("real_home");
        let root = home.path().join(crate::APP_DIR);
        std::fs::create_dir_all(&root).unwrap();

        append(&root.join("crash.log"), "entry\n").unwrap();

        let body = std::fs::read_to_string(root.join("crash.log")).unwrap();
        assert!(body.contains("entry"));
    }
}
