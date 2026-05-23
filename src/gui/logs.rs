use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

const MAX_LOG_LINES: usize = 500;

#[derive(Debug, Clone, Default)]
pub struct LogBuffer(Arc<Mutex<VecDeque<String>>>);

impl LogBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<String> {
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

    fn push(&self, line: &str) {
        let mut g = self.0.lock().expect("log mutex poisoned");
        if g.len() >= MAX_LOG_LINES {
            g.pop_front();
        }
        g.push_back(line.to_string());
    }
}

pub struct LogWriter(LogBuffer);

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Ok(s) = std::str::from_utf8(buf) {
            for line in s.lines() {
                let trimmed = line.trim_end();
                if !trimmed.is_empty() {
                    self.0.push(trimmed);
                }
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = LogWriter;
    fn make_writer(&'a self) -> Self::Writer {
        LogWriter(self.clone())
    }
}
