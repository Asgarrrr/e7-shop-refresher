//! Outbound pings: Discord webhook fire-and-forget.
//!
//! The bot worker calls into here when a stop condition fires. The POST
//! happens on a detached thread so a slow or down webhook can't delay
//! the runner's shutdown.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct RunSummary {
    pub reason: String,
    pub elapsed: Duration,
    pub refreshes: u32,
    pub mystic_bought: u32,
    pub covenant_bought: u32,
    pub gold_spent: u64,
}

impl RunSummary {
    pub fn to_discord_message(&self) -> String {
        format!(
            "**e7-shop-refresher** — run finished\n\
             • Reason: `{}`\n\
             • Duration: {}\n\
             • Refreshes: {}\n\
             • Mystic medals: {}\n\
             • Covenant bookmarks: {}\n\
             • Gold spent: {}",
            self.reason,
            format_duration(self.elapsed),
            self.refreshes,
            self.mystic_bought,
            self.covenant_bought,
            format_gold(self.gold_spent),
        )
    }
}

/// Synchronous POST of a run summary. Called from the worker thread
/// AT THE COMPLETION BOUNDARY, before any `suspend_to_sleep` — firing
/// asynchronously would race the OS shutting down the network stack
/// and silently drop the webhook.
///
/// Blocks for up to the WinHTTP default timeouts (~30 s receive).
/// Acceptable here: the worker is between rounds, the user is by
/// definition waiting for "the run finished" feedback, and a slow
/// webhook delaying sleep by a few seconds beats no notification at all.
///
/// Callers MUST pre-trim the URL (use `NotificationsConfig::webhook_url`);
/// an empty string is treated as "disabled" and noops.
pub fn deliver_summary_blocking(webhook_url: &str, summary: RunSummary) {
    if webhook_url.is_empty() {
        return;
    }
    let content = summary.to_discord_message();
    if let Err(e) = post(webhook_url, &content) {
        warn!(error = %e, "discord webhook failed during completion dispatch");
    }
}

/// Fire-and-forget Discord POST for the Setup/Run-tab "Send test"
/// button. Reports the outcome through `status` so the UI can show
/// "Sending… → Sent ✓ / Failed". Unlike [`deliver_summary_blocking`],
/// this is async (detached thread) because nothing downstream depends
/// on it completing — the user is at the keyboard waiting on UI text.
/// Callers MUST pre-trim the URL.
pub fn send_discord_test(webhook_url: String, content: String, status: WebhookTestStatus) {
    if webhook_url.is_empty() {
        return;
    }
    if !status.try_begin() {
        return;
    }
    let status_for_thread = status.clone();
    let spawn = thread::Builder::new()
        .name("discord-webhook-test".into())
        .spawn(move || {
            let outcome = match post(&webhook_url, &content) {
                Ok(()) => TestEventKind::Ok,
                Err(e) => TestEventKind::Err(e),
            };
            status_for_thread.set(outcome);
        });
    if let Err(e) = spawn {
        warn!(error = %e, "failed to spawn discord-webhook-test thread");
        status.set(TestEventKind::Err(format!("spawn failed: {e}")));
    }
}

/// `Ok(())` on Discord's 200/204; `Err(message)` for any other outcome.
/// Message is suitable for surfacing in the GUI (short, human-readable).
fn post(url: &str, content: &str) -> std::result::Result<(), String> {
    let body = json!({ "content": content }).to_string();
    match crate::http::post_json(url, &body) {
        Ok((status, _)) if status == 204 || status == 200 => {
            info!(status, "discord webhook delivered");
            Ok(())
        }
        Ok((status, body)) => {
            warn!(status, body = %body, "discord webhook returned non-2xx");
            // Body can be hundreds of bytes of HTML on an error page — cap
            // for the GUI label.
            let snippet = body.chars().take(140).collect::<String>();
            Err(format!("HTTP {status}: {snippet}"))
        }
        Err(e) => {
            warn!(error = %e, "discord webhook failed");
            Err(e.to_string())
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct WebhookTestStatus {
    inner: Arc<Mutex<Option<TestEvent>>>,
}

#[derive(Debug, Clone)]
pub struct TestEvent {
    pub kind: TestEventKind,
    pub at: Instant,
}

#[derive(Debug, Clone)]
pub enum TestEventKind {
    Pending,
    Ok,
    Err(String),
}

/// Stale-success messages fade out so a "Sent ✓" from five minutes ago
/// doesn't look like a fresh confirmation. Pending stays visible
/// indefinitely (it's waiting on the network, not a snapshot in time).
const RESULT_VISIBLE_FOR: Duration = Duration::from_secs(8);

impl WebhookTestStatus {
    pub fn new() -> Self {
        Self::default()
    }

    fn set(&self, kind: TestEventKind) {
        if let Ok(mut g) = self.inner.lock() {
            *g = Some(TestEvent {
                kind,
                at: Instant::now(),
            });
        }
    }

    /// Returns `true` if we transitioned to Pending; `false` if a previous
    /// test is still in flight. Atomic check-and-set under the inner Mutex
    /// so a double-click can't spawn two concurrent POSTs.
    fn try_begin(&self) -> bool {
        let Ok(mut g) = self.inner.lock() else {
            return false;
        };
        if matches!(g.as_ref().map(|e| &e.kind), Some(TestEventKind::Pending)) {
            return false;
        }
        *g = Some(TestEvent {
            kind: TestEventKind::Pending,
            at: Instant::now(),
        });
        true
    }

    /// `None` once a finished result has faded; `Some` while pending or
    /// within [`RESULT_VISIBLE_FOR`] of a finished result.
    pub fn visible(&self) -> Option<TestEventKind> {
        let g = self.inner.lock().ok()?;
        let ev = g.as_ref()?;
        match &ev.kind {
            TestEventKind::Pending => Some(TestEventKind::Pending),
            other if ev.at.elapsed() < RESULT_VISIBLE_FOR => Some(other.clone()),
            _ => None,
        }
    }
}

fn format_duration(d: Duration) -> String {
    let total = d.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    match (h, m, s) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, m, _) => format!("{h}h{m:02}m"),
    }
}

fn format_gold(n: u64) -> String {
    if n == 0 {
        return "0g".into();
    }
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out.push('g');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_handles_canonical_cases() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0s");
        assert_eq!(format_duration(Duration::from_secs(45)), "45s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m05s");
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h00m");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1h02m");
    }

    #[test]
    fn format_gold_inserts_thousands_separators() {
        assert_eq!(format_gold(0), "0g");
        assert_eq!(format_gold(1), "1g");
        assert_eq!(format_gold(999), "999g");
        assert_eq!(format_gold(1_000), "1,000g");
        assert_eq!(format_gold(280_000), "280,000g");
        assert_eq!(format_gold(1_234_567), "1,234,567g");
    }

    #[test]
    fn summary_message_contains_all_fields() {
        let s = RunSummary {
            reason: "stop_when_mystic_medals".into(),
            elapsed: Duration::from_secs(125),
            refreshes: 12,
            mystic_bought: 3,
            covenant_bought: 1,
            gold_spent: 1_025_000,
        };
        let msg = s.to_discord_message();
        assert!(msg.contains("stop_when_mystic_medals"));
        assert!(msg.contains("2m05s"));
        assert!(msg.contains("12"));
        assert!(msg.contains("1,025,000g"));
    }

    #[test]
    fn deliver_summary_blocking_is_noop_for_empty_url() {
        // Empty URL must short-circuit before any network call; this test
        // would block on a real HTTP attempt otherwise.
        deliver_summary_blocking(
            "",
            RunSummary {
                reason: "test".into(),
                elapsed: Duration::from_secs(1),
                refreshes: 0,
                mystic_bought: 0,
                covenant_bought: 0,
                gold_spent: 0,
            },
        );
    }

    #[test]
    fn webhook_test_status_pending_visible() {
        let s = WebhookTestStatus::new();
        s.set(TestEventKind::Pending);
        assert!(matches!(s.visible(), Some(TestEventKind::Pending)));
    }

    #[test]
    fn webhook_test_status_ok_visible_immediately() {
        let s = WebhookTestStatus::new();
        s.set(TestEventKind::Ok);
        assert!(matches!(s.visible(), Some(TestEventKind::Ok)));
    }

    #[test]
    fn webhook_test_status_err_carries_message() {
        let s = WebhookTestStatus::new();
        s.set(TestEventKind::Err("HTTP 401".into()));
        match s.visible() {
            Some(TestEventKind::Err(msg)) => assert_eq!(msg, "HTTP 401"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn webhook_test_status_empty_until_first_event() {
        assert!(WebhookTestStatus::new().visible().is_none());
    }
}
