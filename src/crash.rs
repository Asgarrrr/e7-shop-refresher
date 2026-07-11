//! Crash logging: a global panic hook that records every panic to a file.
//!
//! In the windowed build stdout/stderr are inert, and a panic on a worker task
//! or the capture thread is swallowed by the runtime — it would otherwise
//! surface only as a bare "session ended". The hook appends each panic (thread,
//! location, message, backtrace) to `crash.log`, preferring the exe's directory
//! and falling back to the temp dir when that isn't writable.

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

/// Candidate log paths, most-preferred first: next to the exe, then the temp
/// dir (in case the exe lives somewhere read-only, e.g. Program Files).
fn crash_log_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        paths.push(dir.join("crash.log"));
    }
    paths.push(std::env::temp_dir().join("arkyve-crash.log"));
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
    fn write_first_writable_falls_back_past_an_unwritable_path() {
        // First candidate is under a path that cannot exist as a directory
        // (a file component in the middle), so the open fails and the fallback
        // temp file is used instead.
        let bad = std::env::temp_dir().join("arkyve_nope.log/inner/crash.log");
        let good =
            std::env::temp_dir().join(format!("arkyve_crash_test_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&good);

        write_first_writable(&[bad, good.clone()], "entry-one\n");
        write_first_writable(std::slice::from_ref(&good), "entry-two\n");

        let body = std::fs::read_to_string(&good).unwrap();
        assert!(body.contains("entry-one"));
        assert!(body.contains("entry-two"));
        let _ = std::fs::remove_file(&good);
    }
}
