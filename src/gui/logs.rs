use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

const MAX_LOG_LINES: usize = 500;

#[derive(Debug, Clone)]
pub struct LogLine {
    pub level: Level,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct LogBuffer(Arc<Mutex<VecDeque<LogLine>>>);

impl LogBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<LogLine> {
        self.0
            .lock()
            .expect("log mutex poisoned")
            .iter()
            .cloned()
            .collect()
    }

    pub fn clear(&self) {
        self.0.lock().expect("log mutex poisoned").clear();
    }

    fn push(&self, line: LogLine) {
        let mut g = self.0.lock().expect("log mutex poisoned");
        if g.len() >= MAX_LOG_LINES {
            g.pop_front();
        }
        g.push_back(line);
    }
}

/// Captures events with their `Level` preserved, so the panel can filter
/// by severity (a plain fmt-writer drops level metadata at format time).
impl<S> Layer<S> for LogBuffer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let target = metadata.target();
        let body = visitor.into_body();
        let text = if body.is_empty() {
            target.to_string()
        } else if target.is_empty() {
            body
        } else {
            format!("{target}  {body}")
        };
        self.push(LogLine {
            level: *metadata.level(),
            text,
        });
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
    fields: Vec<(String, String)>,
}

impl MessageVisitor {
    fn into_body(self) -> String {
        let mut out = self.message.unwrap_or_default();
        for (k, v) in self.fields {
            if !out.is_empty() {
                out.push(' ');
            }
            let _ = write!(out, "{k}={v}");
        }
        out
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let name = field.name();
        if name == "message" {
            self.message = Some(format!("{value:?}"));
        } else {
            self.fields.push((name.to_string(), format!("{value:?}")));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        let name = field.name();
        if name == "message" {
            self.message = Some(value.to_string());
        } else {
            self.fields.push((name.to_string(), value.to_string()));
        }
    }
}
