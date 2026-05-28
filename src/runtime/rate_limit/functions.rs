use super::types::{RateLimitPolicy, RateLimitState};
use crate::ast::{Endpoint, EndpointOption};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

pub(super) async fn rate_limited(policy: &RateLimitPolicy, key: &str) -> bool {
    let mut states = policy.states.lock().await;
    let now = Instant::now();
    let state = states
        .entry(key.to_string())
        .or_insert_with(|| RateLimitState {
            started_at: now,
            used: 0,
        });

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

pub(crate) fn endpoint_rate_limit_policy(endpoint: &Endpoint) -> Option<RateLimitPolicy> {
    endpoint.options.iter().find_map(|option| match option {
        EndpointOption::RateLimit {
            limit, window_ms, ..
        } => Some(RateLimitPolicy {
            limit: *limit,
            window: Duration::from_millis(*window_ms),
            states: Arc::new(Mutex::new(HashMap::new())),
        }),
        EndpointOption::Secure(_) => None,
    })
}
