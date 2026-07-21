//! Per-token rate limiting (SPEC.md "Security and tenancy").
//!
//! Fixed one-second window per token. The whole table resets when the wall
//! second ticks, which bounds memory to the number of distinct tokens seen in
//! a single second — no eviction machinery, no unbounded growth (the ingest
//! path takes no unbounded allocation, CLAUDE.md).
//!
//! Abuse protection, not fairness: the default ceiling is generous. Exceeding
//! it returns 429, which posthog-js treats as retry-after on its backoff.

use std::collections::HashMap;
use std::sync::Mutex;

/// Default per-token events/second ceiling. Generous — this is a firewall
/// against a runaway client, not a quota.
pub const DEFAULT_MAX_PER_SEC: u32 = 10_000;

/// Cap on distinct tokens tracked within one window. Past this we fail open
/// (allow) rather than allocate without bound.
const MAX_TRACKED_TOKENS: usize = 50_000;

pub struct RateLimiter {
    max_per_sec: u32,
    inner: Mutex<Window>,
}

struct Window {
    second: i64,
    counts: HashMap<String, u32>,
}

impl RateLimiter {
    pub fn new(max_per_sec: u32) -> Self {
        Self {
            max_per_sec,
            inner: Mutex::new(Window {
                second: 0,
                counts: HashMap::new(),
            }),
        }
    }

    /// Record `n` events for `token` at wall-clock `now_secs`. Returns true if
    /// within the ceiling, false if the token is now over budget this second.
    pub fn allow(&self, token: &str, n: u32, now_secs: i64) -> bool {
        let mut w = self.inner.lock().unwrap();
        if now_secs != w.second {
            w.second = now_secs;
            w.counts.clear();
        }
        // Fail open if we're tracking too many tokens — never block real
        // traffic to satisfy the memory bound.
        if w.counts.len() >= MAX_TRACKED_TOKENS && !w.counts.contains_key(token) {
            return true;
        }
        let entry = w.counts.entry(token.to_string()).or_insert(0);
        *entry = entry.saturating_add(n);
        *entry <= self.max_per_sec
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_ceiling_allowed() {
        let rl = RateLimiter::new(100);
        assert!(rl.allow("phc_t", 50, 1000));
        assert!(rl.allow("phc_t", 50, 1000)); // 100 total, == ceiling
    }

    #[test]
    fn over_ceiling_rejected() {
        let rl = RateLimiter::new(100);
        assert!(rl.allow("phc_t", 100, 1000));
        assert!(!rl.allow("phc_t", 1, 1000)); // 101 > 100
    }

    #[test]
    fn window_reset_on_new_second() {
        let rl = RateLimiter::new(100);
        assert!(rl.allow("phc_t", 100, 1000));
        assert!(!rl.allow("phc_t", 1, 1000));
        // New second: budget refreshed.
        assert!(rl.allow("phc_t", 100, 1001));
    }

    #[test]
    fn tokens_are_independent() {
        let rl = RateLimiter::new(100);
        assert!(rl.allow("phc_a", 100, 1000));
        assert!(rl.allow("phc_b", 100, 1000)); // b has its own budget
        assert!(!rl.allow("phc_a", 1, 1000));
    }
}
