//! Event sink — where accepted events go after the wire edge.
//!
//! The real implementation is the WAL ([`crate::wal::WalSink`]): `append`
//! returning `Ok` is the durability promise that justifies a 2xx to the
//! client, so no implementation may ack before its write is durable.
//! [`MemorySink`] exists for tests and [`LogSink`] for running without
//! persistence.

use std::sync::Mutex;

use crate::capture::event::CapturedEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkError {
    /// Transient failure → 503, which posthog-js retries.
    Retryable,
    /// The batch itself can never be stored → 400, never retried.
    Fatal,
}

#[async_trait::async_trait]
pub trait EventSink: Send + Sync {
    async fn append(&self, events: Vec<CapturedEvent>) -> Result<(), SinkError>;
}

/// Explicit bound: no unbounded growth in the ingest path, even in the stub.
pub const MEMORY_SINK_MAX_EVENTS: usize = 1_000_000;

#[derive(Default)]
pub struct MemorySink {
    events: Mutex<Vec<CapturedEvent>>,
}

impl MemorySink {
    pub fn snapshot(&self) -> Vec<CapturedEvent> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl EventSink for MemorySink {
    async fn append(&self, mut batch: Vec<CapturedEvent>) -> Result<(), SinkError> {
        let mut events = self.events.lock().unwrap();
        if events.len() + batch.len() > MEMORY_SINK_MAX_EVENTS {
            return Err(SinkError::Retryable);
        }
        events.append(&mut batch);
        Ok(())
    }
}

/// Logs and drops. Only for running without persistence.
pub struct LogSink;

#[async_trait::async_trait]
impl EventSink for LogSink {
    async fn append(&self, events: Vec<CapturedEvent>) -> Result<(), SinkError> {
        for e in &events {
            tracing::info!(event = %e.event, distinct_id = %e.distinct_id, "event (log sink, not persisted)");
        }
        Ok(())
    }
}
