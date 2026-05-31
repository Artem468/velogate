use super::functions::rate_limited;
use super::types::{
    RateLimitFuture, RateLimitPolicy, RateLimitRequest, VelogateRateLimitLayer,
    VelogateRateLimitService,
};
use axum::Json;
use axum::extract::ConnectInfo;
use axum::http::StatusCode;
use axum::http::header::{HeaderName, HeaderValue};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::net::SocketAddr;
use std::task::{Context, Poll};
use tower::{Layer, Service};

impl VelogateRateLimitLayer {
    pub(crate) fn new(policy: RateLimitPolicy) -> Self {
        Self { policy }
    }
}

impl<S> Layer<S> for VelogateRateLimitLayer {
    type Service = VelogateRateLimitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        VelogateRateLimitService {
            inner,
            policy: self.policy.clone(),
        }
    }
}

impl<S> Service<RateLimitRequest> for VelogateRateLimitService<S>
where
    S: Service<RateLimitRequest, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = RateLimitFuture<Self::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: RateLimitRequest) -> Self::Future {
        let mut inner = self.inner.clone();
        let policy = self.policy.clone();

        Box::pin(async move {
            let key = client_ip_key(&request);
            if rate_limited(&policy, &key) {
                let body = json!({
                    "error": "rate_limited",
                    "message": "request rate limit exceeded",
                });
                return Ok((StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response());
            }

            inner.call(request).await
        })
    }
}

fn client_ip_key(request: &RateLimitRequest) -> String {
    header_ip(request, "x-forwarded-for")
        .or_else(|| header_ip(request, "x-real-ip"))
        .or_else(|| {
            request
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(addr)| addr.ip().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn header_ip(request: &RateLimitRequest, name: &'static str) -> Option<String> {
    let name = HeaderName::from_static(name);
    request.headers().get(name).and_then(header_value_to_ip)
}

fn header_value_to_ip(value: &HeaderValue) -> Option<String> {
    value
        .to_str()
        .ok()?
        .split(',')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}
