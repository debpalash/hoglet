//! Event sink — where accepted events go after the wire edge.
//!
//! The WAL (claims.md claim 2) will be the real implementation. Until it
//! lands, [`MemorySink`] holds events for tests and [`LogSink`] serves the
//! running binary. The contract stays the same either way: `append` returning
//! `Ok` is the durability promise that justifies a 2xx to the client, so no
//! future implementation may ack before its write is durable.

use std::sync::Mutex;

use crate::capture::event::CapturedEvent;

/// A sink failure is retryable by contract → handler returns 503, which
/// posthog-js retries.
#[derive(Debug)]
pub struct SinkFull;

pub trait EventSink: Send + Sync {
    fn append(&self, events: Vec<CapturedEvent>) -> Result<(), SinkFull>;
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

impl EventSink for MemorySink {
    fn append(&self, mut batch: Vec<CapturedEvent>) -> Result<(), SinkFull> {
        let mut events = self.events.lock().unwrap();
        if events.len() + batch.len() > MEMORY_SINK_MAX_EVENTS {
            return Err(SinkFull);
        }
        events.append(&mut batch);
        Ok(())
    }
}

/// Sink for the running binary until the WAL exists: logs and drops.
pub struct LogSink;

impl EventSink for LogSink {
    fn append(&self, events: Vec<CapturedEvent>) -> Result<(), SinkFull> {
        for e in &events {
            tracing::info!(event = %e.event, distinct_id = %e.distinct_id, "event (no WAL yet, not persisted)");
        }
        Ok(())
    }
}
