use super::types::{RateLimitPolicy, RateLimitState};
use crate::ast::{Endpoint, EndpointOption};
use crate::runtime::types::{RateLimitOptions, RuntimeMetrics};
use dashmap::mapref::entry::Entry;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::time::Instant;

pub(super) fn rate_limited(policy: &RateLimitPolicy, key: &str) -> bool {
    let now = Instant::now();
    let call = policy.calls.fetch_add(1, Ordering::Relaxed);
    if call.is_multiple_of(1024)
        || policy.tracked_clients.load(Ordering::Relaxed) >= policy.max_tracked_clients
    {
        let before = policy.states.len();
        policy
            .states
            .retain(|_, state| now.duration_since(state.started_at) < policy.cleanup_after);
        let removed = before.saturating_sub(policy.states.len());
        let _ =
            policy
                .tracked_clients
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    Some(current.saturating_sub(removed))
                });
    }

    let mut state = match policy.states.entry(key.to_string()) {
        Entry::Occupied(entry) => entry.into_ref(),
        Entry::Vacant(entry) => {
            if policy.tracked_clients.fetch_add(1, Ordering::Relaxed) >= policy.max_tracked_clients
            {
                policy.tracked_clients.fetch_sub(1, Ordering::Relaxed);
                return true;
            }
            entry.insert(RateLimitState {
                started_at: now,
                used: 0,
            })
        }
    };

    if now.duration_since(state.started_at) >= policy.window {
        state.started_at = now;
        state.used = 0;
    }

    if state.used >= policy.limit {
        true
    } else {
        state.used += 1;
        false
    }
}

pub(crate) fn endpoint_rate_limit_policy(
    endpoint: &Endpoint,
    options: &RateLimitOptions,
    metrics: Arc<RuntimeMetrics>,
) -> Option<RateLimitPolicy> {
    endpoint.options.iter().find_map(|option| match option {
        EndpointOption::RateLimit {
            limit, window_ms, ..
        } => Some(RateLimitPolicy {
            limit: *limit,
            window: Duration::from_millis(*window_ms),
            states: Arc::new(dashmap::DashMap::new()),
            trusted_proxies: Arc::new(options.trusted_proxies.clone()),
            max_tracked_clients: options.max_tracked_clients,
            cleanup_after: Duration::from_millis(*window_ms).max(options.cleanup_interval),
            calls: Arc::new(AtomicU64::new(0)),
            tracked_clients: Arc::new(AtomicUsize::new(0)),
            metrics: Arc::clone(&metrics),
        }),
        EndpointOption::Secure(_) => None,
    })
}
