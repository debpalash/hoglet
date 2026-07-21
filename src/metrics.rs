//! Operational metrics (SPEC.md "self-observability").
//!
//! Our telemetry for the operator running Hoglet — ingest counters and
//! uptime, in Prometheus text format at `/metrics`. This is not the
//! observability *product* we said we'd never build (traces/logs/metrics for
//! the user's app); it's how an operator watches the binary itself.
//!
//! Counters are cheap atomics incremented on the request path. No RSS here —
//! the OS (`ps`, cgroups) owns that truth.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Metrics {
    /// Events accepted into the pipeline (post-parse, pre-sink).
    pub captured: AtomicU64,
    /// Events durably acknowledged by the sink.
    pub acked: AtomicU64,
    /// Requests rejected (4xx: bad token, malformed, rate-limited).
    pub rejected: AtomicU64,
    /// Retryable sink failures (503).
    pub sink_errors: AtomicU64,
    /// Process start, seconds since epoch — set once at boot.
    start_epoch: AtomicU64,
}

impl Metrics {
    pub fn new(now_epoch: u64) -> Self {
        let m = Self::default();
        m.start_epoch.store(now_epoch, Ordering::Relaxed);
        m
    }

    pub fn inc_captured(&self, n: u64) {
        self.captured.fetch_add(n, Ordering::Relaxed);
    }
    pub fn inc_acked(&self, n: u64) {
        self.acked.fetch_add(n, Ordering::Relaxed);
    }
    pub fn inc_rejected(&self) {
        self.rejected.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_sink_errors(&self) {
        self.sink_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Prometheus text exposition.
    pub fn render(&self, now_epoch: u64) -> String {
        let uptime = now_epoch.saturating_sub(self.start_epoch.load(Ordering::Relaxed));
        let g = |o: &AtomicU64| o.load(Ordering::Relaxed);
        format!(
            "# HELP hoglet_events_captured_total Events accepted into the pipeline.\n\
             # TYPE hoglet_events_captured_total counter\n\
             hoglet_events_captured_total {}\n\
             # HELP hoglet_events_acked_total Events durably acknowledged.\n\
             # TYPE hoglet_events_acked_total counter\n\
             hoglet_events_acked_total {}\n\
             # HELP hoglet_requests_rejected_total Requests rejected with 4xx.\n\
             # TYPE hoglet_requests_rejected_total counter\n\
             hoglet_requests_rejected_total {}\n\
             # HELP hoglet_sink_errors_total Retryable sink failures (503).\n\
             # TYPE hoglet_sink_errors_total counter\n\
             hoglet_sink_errors_total {}\n\
             # HELP hoglet_uptime_seconds Seconds since process start.\n\
             # TYPE hoglet_uptime_seconds gauge\n\
             hoglet_uptime_seconds {}\n",
            g(&self.captured),
            g(&self.acked),
            g(&self.rejected),
            g(&self.sink_errors),
            uptime,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_prometheus_text() {
        let m = Metrics::new(1000);
        m.inc_captured(5);
        m.inc_acked(5);
        m.inc_rejected();
        let out = m.render(1010);
        assert!(out.contains("hoglet_events_captured_total 5"));
        assert!(out.contains("hoglet_events_acked_total 5"));
        assert!(out.contains("hoglet_requests_rejected_total 1"));
        assert!(out.contains("hoglet_uptime_seconds 10"));
    }
}
