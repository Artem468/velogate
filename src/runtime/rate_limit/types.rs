use axum::body::Body;
use axum::http::Request;
use axum::response::Response;
use dashmap::DashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::time::Duration;
use tokio::time::Instant;

#[derive(Clone)]
pub(crate) struct RateLimitPolicy {
    pub(super) limit: u32,
    pub(super) window: Duration,
    pub(super) states: Arc<DashMap<String, RateLimitState>>,
    pub(super) trusted_proxies: Arc<Vec<ipnet::IpNet>>,
    pub(super) max_tracked_clients: usize,
    pub(super) cleanup_after: Duration,
    pub(super) calls: Arc<AtomicU64>,
    pub(super) tracked_clients: Arc<AtomicUsize>,
    pub(super) metrics: Arc<crate::runtime::types::RuntimeMetrics>,
}

pub(crate) struct RateLimitState {
    pub(super) started_at: Instant,
    pub(super) used: u32,
}

#[derive(Clone)]
pub(crate) struct VelogateRateLimitLayer {
    pub(super) policy: RateLimitPolicy,
}

#[derive(Clone)]
pub(crate) struct VelogateRateLimitService<S> {
    pub(super) inner: S,
    pub(super) policy: RateLimitPolicy,
}

pub(super) type RateLimitFuture<E> = Pin<Box<dyn Future<Output = Result<Response, E>> + Send>>;

pub(super) type RateLimitRequest = Request<Body>;
