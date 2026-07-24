//! Crash logging: a global panic hook that records every panic to a file.
//!
//! In the windowed build stdout/stderr are inert, and a panic on a worker task
//! or the capture thread is swallowed by the runtime — it would otherwise
//! surface only as a bare "session ended". The hook appends each panic (thread,
//! location, message, backtrace) to `crash.log`, preferring the per-user
//! app-data dir and falling back to the temp dir when that isn't writable.

use std::path::PathBuf;

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
        let entry = crash_entry(
            epoch_secs(),
            &thread,
            &location,
            &panic_message(info.payload()),
            &backtrace,
        );
        write_first_writable(&crash_log_paths(), &entry);
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

/// One crash.log record. Pure (time passed in) so it can be tested.
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
    crash_log_paths_from(local, std::env::temp_dir())
}

/// Pure ordering: per-user app-data first (kept out of the user's face and off
/// shared dirs), the temp dir as a guaranteed-writable fallback.
fn crash_log_paths_from(local_appdata: Option<PathBuf>, temp: PathBuf) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(local) = local_appdata {
        paths.push(local.join(crate::APP_DIR).join("crash.log"));
    }
    paths.push(temp.join("arkyve-crash.log"));
    paths
}

/// Appends the entry to the first writable candidate. Best-effort: the panic
/// hook must never itself panic, so every failure is swallowed.
fn write_first_writable(paths: &[PathBuf], entry: &str) {
    for path in paths {
        if append(path, entry).is_ok() {
            return;
        }
    }
}

fn append(path: &std::path::Path, entry: &str) -> std::io::Result<()> {
    use std::io::Write;
    // Best-effort: create the app-data parent so the preferred path is usable
    // even before capture has created the folder. Ignore the result — the panic
    // hook must never itself panic, and the open below reports real failures.
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(entry.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_entry_captures_thread_location_and_message() {
        let entry = crash_entry(
            42,
            "capture",
            "src/capture/windivert.rs:60",
            "recv failed",
            "<backtrace>",
        );
        assert!(entry.contains("epoch 42s"));
        assert!(entry.contains("thread: capture"));
        assert!(entry.contains("location: src/capture/windivert.rs:60"));
        assert!(entry.contains("message: recv failed"));
        assert!(entry.contains("<backtrace>"));
    }

    #[test]
    fn prefers_local_appdata_over_temp() {
        let paths = crash_log_paths_from(
            Some(PathBuf::from("C:/Users/x/AppData/Local")),
            PathBuf::from("C:/Temp"),
        );
        assert_eq!(paths.len(), 2);
        assert!(paths[0].ends_with("arkyve-refresh-shop/crash.log"));
        assert!(paths[1].ends_with("arkyve-crash.log"));
    }

    #[test]
    fn falls_back_to_temp_without_appdata() {
        let paths = crash_log_paths_from(None, PathBuf::from("/tmp"));
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
        let blocker = std::env::temp_dir().join(format!("arkyve_nope_{}.log", std::process::id()));
        std::fs::write(&blocker, b"not a directory").unwrap();
        let bad = blocker.join("inner/crash.log");
        let good =
            std::env::temp_dir().join(format!("arkyve_crash_test_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&good);

        write_first_writable(&[bad, good.clone()], "entry-one\n");
        write_first_writable(std::slice::from_ref(&good), "entry-two\n");

        let body = std::fs::read_to_string(&good).unwrap();
        assert!(body.contains("entry-one"));
        assert!(body.contains("entry-two"));
        let _ = std::fs::remove_file(&good);
        let _ = std::fs::remove_file(&blocker);
    }
}
