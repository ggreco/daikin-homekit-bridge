//! In-memory activity log: a bounded ring buffer of structured events that the
//! admin web UI polls. Every event is also emitted through `tracing`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

const CAPACITY: usize = 1000;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    /// Monotonic sequence number (also used by the UI to de-duplicate).
    pub seq: u64,
    /// RFC3339 timestamp.
    pub ts: String,
    pub level: Level,
    /// Originating device name, if any.
    pub device: Option<String>,
    pub message: String,
}

#[derive(Clone)]
pub struct Activity {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    events: VecDeque<Event>,
    next_seq: u64,
}

impl Activity {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                events: VecDeque::with_capacity(CAPACITY),
                next_seq: 1,
            })),
        }
    }

    fn push(&self, level: Level, device: Option<String>, message: String) {
        match level {
            Level::Info => tracing::info!(device = ?device, "{message}"),
            Level::Warn => tracing::warn!(device = ?device, "{message}"),
            Level::Error => tracing::error!(device = ?device, "{message}"),
        }

        let ts = OffsetDateTime::now_local()
            .unwrap_or_else(|_| OffsetDateTime::now_utc())
            .format(&Rfc3339)
            .unwrap_or_default();

        let mut inner = self.inner.lock().unwrap();
        let seq = inner.next_seq;
        inner.next_seq += 1;
        if inner.events.len() == CAPACITY {
            inner.events.pop_front();
        }
        inner.events.push_back(Event {
            seq,
            ts,
            level,
            device,
            message,
        });
    }

    pub fn info(&self, device: Option<&str>, message: impl Into<String>) {
        self.push(Level::Info, device.map(str::to_string), message.into());
    }

    pub fn warn(&self, device: Option<&str>, message: impl Into<String>) {
        self.push(Level::Warn, device.map(str::to_string), message.into());
    }

    pub fn error(&self, device: Option<&str>, message: impl Into<String>) {
        self.push(Level::Error, device.map(str::to_string), message.into());
    }

    /// Returns up to `limit` of the most recent events, oldest first.
    pub fn recent(&self, limit: usize) -> Vec<Event> {
        let inner = self.inner.lock().unwrap();
        let len = inner.events.len();
        let start = len.saturating_sub(limit);
        inner.events.iter().skip(start).cloned().collect()
    }
}

impl Default for Activity {
    fn default() -> Self {
        Self::new()
    }
}
