//! Structured logging.
//!
//! Two sinks hang off one `tracing` subscriber:
//!
//! * a bounded in-memory ring buffer ([`LogBuffer`]) that the TUI's log view reads, and
//! * (only when the TUI is *not* running) a formatter writing to stderr, so log
//!   lines can never corrupt the alternate-screen layout.

use std::collections::VecDeque;
use std::fmt::Debug;
use std::io::IsTerminal;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::LogLevel;

#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Time since the log buffer was created (≈ process start).
    pub elapsed: Duration,
    pub level: Level,
    pub message: String,
}

/// Bounded ring buffer of recent log lines.
#[derive(Debug)]
pub struct LogBuffer {
    entries: Mutex<VecDeque<LogEntry>>,
    capacity: usize,
    started: Instant,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
            started: Instant::now(),
        })
    }

    pub fn push(&self, level: Level, message: String) {
        let mut q = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if q.len() == self.capacity {
            q.pop_front();
        }
        q.push_back(LogEntry {
            elapsed: self.started.elapsed(),
            level,
            message,
        });
    }

    /// The most recent `n` entries, oldest first.
    pub fn recent(&self, n: usize) -> Vec<LogEntry> {
        let q = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        q.iter().skip(q.len().saturating_sub(n)).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

struct BufferLayer(Arc<LogBuffer>);

#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: Vec<String>,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields.push(format!("{}={value:?}", field.name()));
        }
    }
}

impl<S: Subscriber> Layer<S> for BufferLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut v = MessageVisitor::default();
        event.record(&mut v);
        let mut message = v.message;
        for f in v.fields {
            message.push(' ');
            message.push_str(&f);
        }
        self.0.push(*event.metadata().level(), message);
    }
}

fn targets(level: LogLevel) -> Targets {
    match level {
        LogLevel::Quiet => Targets::new()
            .with_target("share", LevelFilter::WARN)
            .with_default(LevelFilter::ERROR),
        LogLevel::Normal => Targets::new()
            .with_target("share", LevelFilter::INFO)
            .with_default(LevelFilter::WARN),
        LogLevel::Debug => Targets::new()
            .with_target("share", LevelFilter::DEBUG)
            .with_default(LevelFilter::WARN),
        LogLevel::Trace => Targets::new().with_default(LevelFilter::TRACE),
    }
}

/// Install the global subscriber. Safe to call more than once (later calls are ignored).
pub fn init(level: LogLevel, log_to_stderr: bool, buffer: Arc<LogBuffer>) {
    let stderr_layer = log_to_stderr.then(|| {
        fmt::layer()
            .with_target(false)
            .with_timer(fmt::time::uptime())
            .with_ansi(std::io::stderr().is_terminal())
            .with_writer(std::io::stderr)
            .with_filter(targets(level))
    });
    let _ = tracing_subscriber::registry()
        .with(BufferLayer(buffer).with_filter(targets(level)))
        .with(stderr_layer)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_drops_oldest() {
        let b = LogBuffer::new(3);
        for i in 0..5 {
            b.push(Level::INFO, format!("m{i}"));
        }
        let all = b.recent(10);
        assert_eq!(
            all.iter().map(|e| e.message.as_str()).collect::<Vec<_>>(),
            ["m2", "m3", "m4"]
        );
        assert_eq!(b.recent(2).len(), 2);
        assert_eq!(b.recent(2)[1].message, "m4");
    }
}
