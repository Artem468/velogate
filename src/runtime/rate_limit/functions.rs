use super::types::{RateLimitPolicy, RateLimitState};
use crate::ast::{Endpoint, EndpointOption};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

pub(super) async fn rate_limited(policy: &RateLimitPolicy) -> bool {
    let mut state = policy.state.lock().await;
    let now = Instant::now();

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
            state: Arc::new(Mutex::new(RateLimitState {
                started_at: Instant::now(),
                used: 0,
            })),
        }),
        EndpointOption::Secure(_) => None,
    })
}
