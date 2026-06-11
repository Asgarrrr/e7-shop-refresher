//! GitHub release version check.
//!
//! Fires once at GUI startup on a background thread; writes the latest
//! published tag into a shared `Option<String>` when (and only when) it
//! is newer than the running binary. The GUI reads that field and shows
//! a small footer banner. Offline / rate-limited / parse errors are
//! silent — this is a nice-to-have, never blocks anything.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/Asgarrrr/e7-shop-refresher/releases/latest";

pub const RELEASES_PAGE_URL: &str = "https://github.com/Asgarrrr/e7-shop-refresher/releases/latest";

/// Cache stays valid for 6 h. GitHub's anonymous API limit is 60 req/h
/// per IP; without this cache, a user iterating on calibration who
/// relaunches the app dozens of times would silently exhaust the
/// quota and never see an update banner.
const CACHE_TTL: Duration = Duration::from_secs(6 * 3600);

/// Shared cell. `None` = no update (or check not done yet); `Some(tag)` =
/// the latest tag string from GitHub (e.g. "v0.6.3" or "0.6.3").
#[derive(Debug, Clone, Default)]
pub struct UpdateStatus(Arc<Mutex<Option<String>>>);

impl UpdateStatus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Option<String> {
        self.0.lock().ok().and_then(|g| g.clone())
    }

    fn set(&self, tag: String) {
        if let Ok(mut g) = self.0.lock() {
            *g = Some(tag);
        }
    }
}

/// Spawns the check on a detached thread. Safe to call from `App::new`.
/// `cache_path` is a small JSON file ([`UpdateCache`]) the check reads
/// and writes — pass the same path across runs.
pub fn spawn_check(status: UpdateStatus, cache_path: PathBuf) {
    let current = env!("CARGO_PKG_VERSION");
    let spawn = thread::Builder::new()
        .name("update-check".into())
        .spawn(move || run_check(current, &status, &cache_path));
    if let Err(e) = spawn {
        warn!(error = %e, "failed to spawn update-check thread");
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct UpdateCache {
    /// Wall-clock time the last successful GET happened, seconds since
    /// epoch. Wall-clock (not Instant) so it survives process restarts.
    last_check_unix: u64,
    /// Last `tag_name` GitHub returned (regardless of whether it was
    /// newer than the running binary).
    latest_tag: String,
}

fn run_check(current: &str, status: &UpdateStatus, cache_path: &Path) {
    let now_unix = unix_now();

    // Fast path: cache fresh enough — skip the network call entirely and
    // just decide visibility from the cached tag. Misses don't escalate
    // (a stale cache just delays the banner by up to one TTL).
    if let Some(cache) = load_cache(cache_path)
        && now_unix.saturating_sub(cache.last_check_unix) < CACHE_TTL.as_secs()
    {
        debug!(
            cached_tag = %cache.latest_tag,
            age_secs = now_unix.saturating_sub(cache.last_check_unix),
            "using cached release info"
        );
        if is_newer(&cache.latest_tag, current) {
            status.set(cache.latest_tag);
        }
        return;
    }

    let body = match crate::http::get_text(LATEST_RELEASE_URL) {
        Ok(b) => b,
        Err(e) => {
            debug!(error = %e, "update check: GET failed");
            return;
        }
    };
    let latest = match parse_tag_name(&body) {
        Some(t) => t,
        None => {
            debug!("update check: tag_name not found in response");
            return;
        }
    };

    save_cache(
        cache_path,
        &UpdateCache {
            last_check_unix: now_unix,
            latest_tag: latest.clone(),
        },
    );

    if is_newer(&latest, current) {
        debug!(current, latest = %latest, "update available");
        status.set(latest);
    } else {
        debug!(current, latest = %latest, "no update available");
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn load_cache(path: &Path) -> Option<UpdateCache> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_cache(path: &Path, cache: &UpdateCache) {
    let Ok(json) = serde_json::to_string(cache) else {
        return;
    };
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(path, json) {
        debug!(error = %e, path = %path.display(), "update cache write failed");
    }
}

fn parse_tag_name(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let tag = v.get("tag_name")?.as_str()?;
    Some(tag.to_string())
}

/// Strict semver-prefix comparison. Strips a leading `v` and compares
/// the `MAJOR.MINOR.PATCH` triple as integers. Anything past the third
/// dot is ignored (pre-release suffixes are treated as the same triple
/// — fine for our use, we just want "show banner / don't").
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_triple(latest), parse_triple(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

fn parse_triple(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().strip_prefix('v').unwrap_or(s.trim());
    let mut parts = s.splitn(3, '.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    // Third part may carry a `-rc1` / `+build` suffix; cut at the first
    // non-digit so it still parses.
    let patch_raw = parts.next()?;
    let cut = patch_raw
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(patch_raw.len());
    let patch: u32 = patch_raw[..cut].parse().ok()?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_triple_strips_v_prefix() {
        assert_eq!(parse_triple("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_triple("1.2.3"), Some((1, 2, 3)));
    }

    #[test]
    fn parse_triple_handles_prerelease_suffix() {
        assert_eq!(parse_triple("0.6.2-rc1"), Some((0, 6, 2)));
        assert_eq!(parse_triple("1.0.0+build.5"), Some((1, 0, 0)));
    }

    #[test]
    fn parse_triple_rejects_garbage() {
        assert_eq!(parse_triple("1.2"), None);
        assert_eq!(parse_triple("abc"), None);
        assert_eq!(parse_triple(""), None);
    }

    #[test]
    fn is_newer_compares_patch_minor_major() {
        assert!(is_newer("v0.6.3", "0.6.2"));
        assert!(is_newer("0.7.0", "0.6.99"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.6.2", "0.6.2"));
        assert!(!is_newer("0.6.1", "0.6.2"));
        assert!(!is_newer("garbage", "0.6.2"));
    }

    #[test]
    fn parse_tag_name_extracts_field() {
        let body = r#"{"tag_name":"v0.7.0","name":"Release 0.7.0"}"#;
        assert_eq!(parse_tag_name(body), Some("v0.7.0".into()));
    }

    #[test]
    fn parse_tag_name_returns_none_on_missing_field() {
        let body = r#"{"name":"Release"}"#;
        assert_eq!(parse_tag_name(body), None);
    }

    #[test]
    fn parse_tag_name_returns_none_on_invalid_json() {
        assert_eq!(parse_tag_name("not json"), None);
    }

    #[test]
    fn update_status_round_trip() {
        let s = UpdateStatus::new();
        assert_eq!(s.snapshot(), None);
        s.set("v9.9.9".into());
        assert_eq!(s.snapshot(), Some("v9.9.9".into()));
    }

    #[test]
    fn cache_round_trip_through_disk() {
        let dir = std::env::temp_dir().join(format!("e7_update_cache_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("update_cache.json");
        let written = UpdateCache {
            last_check_unix: 1_700_000_000,
            latest_tag: "v0.7.0".into(),
        };
        save_cache(&path, &written);
        let read = load_cache(&path).expect("cache must round-trip");
        assert_eq!(read.last_check_unix, written.last_check_unix);
        assert_eq!(read.latest_tag, written.latest_tag);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_load_returns_none_for_missing_file() {
        let path = std::env::temp_dir().join("e7_definitely_missing_cache.json");
        let _ = std::fs::remove_file(&path);
        assert!(load_cache(&path).is_none());
    }

    #[test]
    fn cache_load_returns_none_for_garbage() {
        let path = std::env::temp_dir().join(format!(
            "e7_update_cache_garbage_{}.json",
            std::process::id()
        ));
        std::fs::write(&path, "not json at all").unwrap();
        assert!(load_cache(&path).is_none());
        let _ = std::fs::remove_file(&path);
    }
}
