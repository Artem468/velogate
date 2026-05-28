use axum::body::Body;
use axum::http::Request;
use axum::response::Response;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

#[derive(Clone)]
pub(crate) struct RateLimitPolicy {
    pub(super) limit: u32,
    pub(super) window: Duration,
    pub(super) states: Arc<Mutex<HashMap<String, RateLimitState>>>,
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
