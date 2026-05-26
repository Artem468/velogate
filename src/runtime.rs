use crate::ast::{
    BinaryOperator, DbQueryConfig, Endpoint, EndpointOption, Expression, FileAST, GrpcConfig,
    HttpConfig, PipeOp, Step, Sym,
};
use crate::planner::{EndpointPlan, ExecutionPlan};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodFilter, on};
use axum::{Json, Router};
use lasso::Rodeo;
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor};
use prost_types::{ListValue, Struct, Value as ProstValue, value::Kind};
use reqwest::Client;
use serde_json::{Map, Value as JsonValue, json};
use sqlx::any::{AnyPoolOptions, install_default_drivers};
use sqlx::{AnyPool, Column, Row, TypeInfo};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Once;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tonic::Status;
use tonic::client::Grpc;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::transport::{Channel, Endpoint as TonicEndpoint};
use tonic_prost::ProstCodec;
use tower::{Layer, Service};

#[derive(Clone)]
pub struct Runtime {
    ast: Arc<FileAST>,
    interner: Arc<Rodeo>,
    plan: Arc<ExecutionPlan>,
    client: Client,
    db_urls: Arc<HashMap<String, String>>,
    db_pools: Arc<Mutex<HashMap<String, AnyPool>>>,
    proto_paths: Arc<HashMap<String, String>>,
    proto_pools: Arc<Mutex<HashMap<String, DescriptorPool>>>,
}

#[derive(Debug)]
pub enum RuntimeError {
    InvalidMethod { method: String, path: String },
    InvalidBindAddress(String),
    Bind(std::io::Error),
    Serve(std::io::Error),
    Execution(String),
}

type Vars = HashMap<Sym, JsonValue>;
type Value = JsonValue;
type RuntimeResult<T> = Result<T, RuntimeError>;
static SQLX_DRIVERS: Once = Once::new();

#[derive(Clone)]
struct SecurePolicy {
    schemes: Vec<String>,
}

#[derive(Clone)]
struct RateLimitPolicy {
    limit: u32,
    window: Duration,
    state: Arc<Mutex<RateLimitState>>,
}

struct RateLimitState {
    started_at: Instant,
    used: u32,
}

impl Runtime {
    pub fn new(ast: FileAST, interner: Rodeo, plan: ExecutionPlan) -> Self {
        Self {
            db_urls: Arc::new(db_urls(&ast, &interner)),
            proto_paths: Arc::new(proto_paths(&ast, &interner)),
            ast: Arc::new(ast),
            interner: Arc::new(interner),
            plan: Arc::new(plan),
            client: Client::new(),
            db_pools: Arc::new(Mutex::new(HashMap::new())),
            proto_pools: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn bind_addr(&self) -> RuntimeResult<SocketAddr> {
        let host = self
            .ast
            .gateway
            .host
            .clone()
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let port = self.ast.gateway.port;
        format!("{host}:{port}")
            .parse()
            .map_err(|_| RuntimeError::InvalidBindAddress(format!("{host}:{port}")))
    }

    pub async fn serve(self) -> RuntimeResult<()> {
        let addr = self.bind_addr()?;
        let router = self.router()?;
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(RuntimeError::Bind)?;

        println!("velogate listening on http://{addr}");
        axum::serve(listener, router)
            .await
            .map_err(RuntimeError::Serve)
    }

    pub fn router(&self) -> RuntimeResult<Router> {
        let mut router = Router::new();

        for (idx, endpoint) in self.ast.endpoints.iter().enumerate() {
            let plan = self
                .plan
                .endpoints
                .get(idx)
                .ok_or_else(|| RuntimeError::Execution(format!("missing plan for endpoint {idx}")))?
                .clone();
            let endpoint_runtime = Arc::new(EndpointRuntime {
                endpoint: endpoint.clone(),
                plan,
                interner: Arc::clone(&self.interner),
                client: self.client.clone(),
                static_dbs: static_dbs(&self.ast),
                db_urls: Arc::clone(&self.db_urls),
                db_pools: Arc::clone(&self.db_pools),
                proto_paths: Arc::clone(&self.proto_paths),
                proto_pools: Arc::clone(&self.proto_pools),
            });
            let method =
                method_filter(&endpoint.method).ok_or_else(|| RuntimeError::InvalidMethod {
                    method: endpoint.method.clone(),
                    path: endpoint.path.clone(),
                })?;
            let handler_runtime = Arc::clone(&endpoint_runtime);
            let mut method_router = on(
                method,
                move |Path(path_params): Path<HashMap<String, String>>,
                      Query(query_params): Query<HashMap<String, String>>,
                      headers: HeaderMap| {
                    let runtime = Arc::clone(&handler_runtime);
                    async move { runtime.handle(path_params, query_params, headers).await }
                },
            );

            if let Some(policy) = endpoint_rate_limit_policy(endpoint) {
                method_router = method_router.layer(VelogateRateLimitLayer::new(policy));
            }

            if let Some(policy) = endpoint_secure_policy(endpoint, &self.interner) {
                method_router =
                    method_router.layer(middleware::from_fn_with_state(policy, secure_middleware));
            }

            router = router.route(&axum_route_path(&endpoint.path), method_router);
        }

        Ok(router)
    }
}

async fn secure_middleware(
    State(policy): State<SecurePolicy>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if policy.schemes.iter().any(|scheme| scheme == "BearerJWT") {
        let authorized = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|token| !token.trim().is_empty());

        if !authorized {
            let body = json!({
                "error": "unauthorized",
                "message": "missing or invalid bearer token",
            });
            return (StatusCode::UNAUTHORIZED, Json(body)).into_response();
        }
    }

    next.run(request).await
}

#[derive(Clone)]
struct VelogateRateLimitLayer {
    policy: RateLimitPolicy,
}

impl VelogateRateLimitLayer {
    fn new(policy: RateLimitPolicy) -> Self {
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

#[derive(Clone)]
struct VelogateRateLimitService<S> {
    inner: S,
    policy: RateLimitPolicy,
}

impl<S> Service<Request<Body>> for VelogateRateLimitService<S>
where
    S: Service<Request<Body>, Response = Response> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let mut inner = self.inner.clone();
        let policy = self.policy.clone();

        Box::pin(async move {
            if rate_limited(&policy).await {
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

async fn rate_limited(policy: &RateLimitPolicy) -> bool {
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

struct EndpointRuntime {
    endpoint: Endpoint,
    plan: EndpointPlan,
    interner: Arc<Rodeo>,
    client: Client,
    static_dbs: Vars,
    db_urls: Arc<HashMap<String, String>>,
    db_pools: Arc<Mutex<HashMap<String, AnyPool>>>,
    proto_paths: Arc<HashMap<String, String>>,
    proto_pools: Arc<Mutex<HashMap<String, DescriptorPool>>>,
}

struct StepRuntimeDeps<'a> {
    client: &'a Client,
    db_urls: &'a HashMap<String, String>,
    db_pools: &'a Mutex<HashMap<String, AnyPool>>,
    proto_paths: &'a HashMap<String, String>,
    proto_pools: &'a Mutex<HashMap<String, DescriptorPool>>,
}

impl EndpointRuntime {
    async fn handle(
        self: Arc<Self>,
        path_params: HashMap<String, String>,
        query_params: HashMap<String, String>,
        headers: HeaderMap,
    ) -> Response {
        match self.execute(path_params, query_params, &headers).await {
            Ok(value) => {
                let status =
                    StatusCode::from_u16(self.endpoint.response_status).unwrap_or(StatusCode::OK);
                (status, Json(value)).into_response()
            }
            Err(err) => {
                let body = json!({
                    "error": "runtime_error",
                    "message": err.to_string(),
                });
                (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
            }
        }
    }

    async fn execute(
        &self,
        path_params: HashMap<String, String>,
        query_params: HashMap<String, String>,
        headers: &HeaderMap,
    ) -> RuntimeResult<Value> {
        let mut vars = self.static_dbs.clone();
        vars.extend(request_vars(
            &path_params,
            &query_params,
            headers,
            &self.interner,
        ));

        for layer in &self.plan.layers {
            let snapshot = Arc::new(vars.clone());
            let mut tasks = JoinSet::new();

            for step_idx in layer {
                let step = self.endpoint.steps.get(*step_idx).ok_or_else(|| {
                    RuntimeError::Execution(format!("missing step {step_idx} in endpoint plan"))
                })?;
                let step = step.clone();
                let ctx = Arc::clone(&snapshot);
                let interner = Arc::clone(&self.interner);
                let client = self.client.clone();
                let db_urls = Arc::clone(&self.db_urls);
                let db_pools = Arc::clone(&self.db_pools);
                let proto_paths = Arc::clone(&self.proto_paths);
                let proto_pools = Arc::clone(&self.proto_pools);

                tasks.spawn(async move {
                    let var = step_var(&step);
                    let deps = StepRuntimeDeps {
                        client: &client,
                        db_urls: &db_urls,
                        db_pools: &db_pools,
                        proto_paths: &proto_paths,
                        proto_pools: &proto_pools,
                    };
                    let value = execute_step(&step, &ctx, &interner, &deps).await?;
                    RuntimeResult::Ok((var, value))
                });
            }

            while let Some(result) = tasks.join_next().await {
                let (var, value) = result
                    .map_err(|err| RuntimeError::Execution(format!("step task failed: {err}")))??;
                vars.insert(var, value);
            }
        }

        eval_response(&self.endpoint, &vars, &self.interner)
    }
}

async fn execute_step(
    step: &Step,
    vars: &Vars,
    interner: &Rodeo,
    deps: &StepRuntimeDeps<'_>,
) -> RuntimeResult<Value> {
    match step {
        Step::Let { value, .. } => eval_expr(value, vars, interner),
        Step::FetchHttp { config, .. } => fetch_http(config, vars, interner, deps.client).await,
        Step::Pipe {
            source, operations, ..
        } => execute_pipe(source, operations, vars, interner),
        Step::CallGrpc { config, .. } => {
            call_grpc(config, vars, interner, deps.proto_paths, deps.proto_pools).await
        }
        Step::QueryDb { config, .. } => {
            query_db(config, vars, interner, deps.db_urls, deps.db_pools).await
        }
    }
}

async fn fetch_http(
    config: &HttpConfig,
    vars: &Vars,
    interner: &Rodeo,
    client: &Client,
) -> RuntimeResult<Value> {
    let url = as_string(eval_expr(&config.url, vars, interner)?);
    let attempts = config.retries.unwrap_or(0).saturating_add(1);
    let mut last_error = None;

    for attempt in 0..attempts {
        let mut request = client.get(&url);
        if let Some(timeout_ms) = config.timeout_ms {
            request = request.timeout(Duration::from_millis(timeout_ms));
        }

        match request.send().await {
            Ok(response) => match response.error_for_status() {
                Ok(response) => {
                    let bytes = response.bytes().await.map_err(|err| {
                        RuntimeError::Execution(format!(
                            "failed to read response from {url}: {err}"
                        ))
                    })?;
                    return Ok(serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                        Value::String(String::from_utf8_lossy(&bytes).into())
                    }));
                }
                Err(err) => last_error = Some(err.to_string()),
            },
            Err(err) => last_error = Some(err.to_string()),
        }

        if attempt + 1 < attempts
            && let Some(delay_ms) = config.delay_ms
        {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }

    if let Some(fallback) = &config.fallback {
        return eval_expr(fallback, vars, interner);
    }

    Err(RuntimeError::Execution(format!(
        "fetch {url} failed: {}",
        last_error.unwrap_or_else(|| "unknown error".to_string())
    )))
}

async fn query_db(
    config: &DbQueryConfig,
    vars: &Vars,
    interner: &Rodeo,
    db_urls: &HashMap<String, String>,
    db_pools: &Mutex<HashMap<String, AnyPool>>,
) -> RuntimeResult<Value> {
    let source = as_string(eval_expr(&config.db_source, vars, interner)?);
    let url = db_urls.get(&source).cloned().unwrap_or(source);
    let params = config
        .params
        .iter()
        .map(|param| eval_expr(param, vars, interner))
        .collect::<RuntimeResult<Vec<_>>>()?;

    let work = async {
        let pool = get_db_pool(&url, db_pools).await?;
        let mut query = sqlx::query(&config.sql);

        for param in params {
            query = bind_json_value(query, param);
        }

        let rows = query
            .fetch_all(&pool)
            .await
            .map_err(|err| RuntimeError::Execution(format!("database query failed: {err}")))?;
        rows.into_iter()
            .map(row_to_json)
            .collect::<RuntimeResult<Vec<_>>>()
            .map(Value::Array)
    };

    let result = if let Some(timeout_ms) = config.timeout_ms {
        tokio::time::timeout(Duration::from_millis(timeout_ms), work)
            .await
            .map_err(|_| RuntimeError::Execution("database query timed out".to_string()))?
    } else {
        work.await
    };

    match result {
        Ok(value) => Ok(value),
        Err(err) => match &config.fallback {
            Some(fallback) => eval_expr(fallback, vars, interner),
            None => Err(err),
        },
    }
}

async fn get_db_pool(
    url: &str,
    db_pools: &Mutex<HashMap<String, AnyPool>>,
) -> RuntimeResult<AnyPool> {
    SQLX_DRIVERS.call_once(install_default_drivers);

    if let Some(pool) = db_pools.lock().await.get(url).cloned() {
        return Ok(pool);
    }

    let pool = AnyPoolOptions::new()
        .max_connections(5)
        .connect(url)
        .await
        .map_err(|err| RuntimeError::Execution(format!("failed to connect to database: {err}")))?;
    db_pools.lock().await.insert(url.to_string(), pool.clone());
    Ok(pool)
}

fn bind_json_value<'q>(
    query: sqlx::query::Query<'q, sqlx::Any, sqlx::any::AnyArguments<'q>>,
    value: Value,
) -> sqlx::query::Query<'q, sqlx::Any, sqlx::any::AnyArguments<'q>> {
    match value {
        Value::Null => query.bind(Option::<String>::None),
        Value::Bool(value) => query.bind(value),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                query.bind(value)
            } else if let Some(value) = value.as_u64().and_then(|value| i64::try_from(value).ok()) {
                query.bind(value)
            } else {
                query.bind(value.as_f64().unwrap_or_default())
            }
        }
        Value::String(value) => query.bind(value),
        Value::Array(value) => query.bind(Value::Array(value).to_string()),
        Value::Object(value) => query.bind(Value::Object(value).to_string()),
    }
}

fn row_to_json(row: sqlx::any::AnyRow) -> RuntimeResult<Value> {
    let mut object = Map::new();

    for (idx, column) in row.columns().iter().enumerate() {
        let value = decode_column(&row, idx).map_err(|err| {
            RuntimeError::Execution(format!(
                "failed to decode column `{}` ({}) as JSON value: {err}",
                column.name(),
                column.type_info().name()
            ))
        })?;
        object.insert(column.name().to_string(), value);
    }

    Ok(Value::Object(object))
}

fn decode_column(row: &sqlx::any::AnyRow, idx: usize) -> Result<Value, sqlx::Error> {
    if let Ok(value) = row.try_get::<Option<i64>, _>(idx) {
        return Ok(value.map_or(Value::Null, |value| json!(value)));
    }
    if let Ok(value) = row.try_get::<Option<f64>, _>(idx) {
        return Ok(value.map_or(Value::Null, |value| json!(value)));
    }
    if let Ok(value) = row.try_get::<Option<bool>, _>(idx) {
        return Ok(value.map(Value::Bool).unwrap_or(Value::Null));
    }
    if let Ok(value) = row.try_get::<Option<String>, _>(idx) {
        return Ok(value.map_or(Value::Null, Value::String));
    }
    if let Ok(value) = row.try_get::<Option<Vec<u8>>, _>(idx) {
        return Ok(value
            .map(|value| Value::String(String::from_utf8_lossy(&value).into()))
            .unwrap_or(Value::Null));
    }

    row.try_get::<Option<String>, _>(idx)
        .map(|value| value.map_or(Value::Null, Value::String))
}

async fn call_grpc(
    config: &GrpcConfig,
    vars: &Vars,
    interner: &Rodeo,
    proto_paths: &HashMap<String, String>,
    proto_pools: &Mutex<HashMap<String, DescriptorPool>>,
) -> RuntimeResult<Value> {
    let payload = eval_expr(&config.payload, vars, interner)?;

    let work = async {
        let channel = grpc_channel(&config.service_method).await?;
        if config.proto_path.is_some() {
            call_grpc_dynamic(channel, config, payload, proto_paths, proto_pools).await
        } else {
            let request = GrpcRequest::parse(&config.service_method)?;
            call_grpc_struct(channel, &request.method, payload).await
        }
    };

    let result = if let Some(timeout_ms) = config.timeout_ms {
        tokio::time::timeout(Duration::from_millis(timeout_ms), work)
            .await
            .map_err(|_| RuntimeError::Execution("grpc call timed out".to_string()))?
    } else {
        work.await
    };

    match result {
        Ok(value) => Ok(value),
        Err(err) => match &config.fallback {
            Some(fallback) => eval_expr(fallback, vars, interner),
            None => Err(err),
        },
    }
}

async fn grpc_channel(target: &str) -> RuntimeResult<Channel> {
    let endpoint = grpc_endpoint(target)?;
    TonicEndpoint::from_shared(endpoint)
        .map_err(|err| RuntimeError::Execution(format!("invalid grpc endpoint: {err}")))?
        .connect()
        .await
        .map_err(|err| RuntimeError::Execution(format!("failed to connect grpc: {err}")))
}

fn grpc_endpoint(target: &str) -> RuntimeResult<String> {
    let endpoint = if let Some(rest) = target.strip_prefix("http://") {
        let host = rest.split_once('/').map_or(rest, |(host, _)| host);
        format!("http://{host}")
    } else if let Some(rest) = target.strip_prefix("https://") {
        let host = rest.split_once('/').map_or(rest, |(host, _)| host);
        format!("https://{host}")
    } else if let Some((host, _)) = target.split_once('/') {
        format!("http://{host}")
    } else {
        format!("http://{target}")
    };

    Ok(endpoint)
}

struct GrpcRequest {
    method: String,
}

impl GrpcRequest {
    fn parse(value: &str) -> RuntimeResult<Self> {
        let rest = if let Some(rest) = value.strip_prefix("http://") {
            rest
        } else if let Some(rest) = value.strip_prefix("https://") {
            rest
        } else {
            value
        };

        let Some((target, method)) = rest.split_once('/') else {
            return Err(RuntimeError::Execution(
                "grpc method must look like `host:port/package.Service/Method`".to_string(),
            ));
        };

        if target.is_empty() || method.is_empty() {
            return Err(RuntimeError::Execution(
                "grpc target and method must not be empty".to_string(),
            ));
        }

        Ok(Self {
            method: method.to_string(),
        })
    }
}

async fn call_grpc_dynamic(
    channel: Channel,
    config: &GrpcConfig,
    payload: Value,
    proto_paths: &HashMap<String, String>,
    proto_pools: &Mutex<HashMap<String, DescriptorPool>>,
) -> RuntimeResult<Value> {
    let proto_source = config
        .proto_path
        .as_deref()
        .ok_or_else(|| RuntimeError::Execution("missing grpc proto source".to_string()))?;
    let proto_path = proto_paths
        .get(proto_source)
        .map(String::as_str)
        .unwrap_or(proto_source);
    let service_name = config
        .service
        .as_deref()
        .ok_or_else(|| RuntimeError::Execution("missing grpc service name".to_string()))?;
    let method_name = config
        .method
        .as_deref()
        .ok_or_else(|| RuntimeError::Execution("missing grpc method name".to_string()))?;

    let method = load_grpc_method(proto_path, service_name, method_name, proto_pools).await?;
    if method.is_client_streaming() || method.is_server_streaming() {
        return Err(RuntimeError::Execution(
            "only unary grpc methods are supported".to_string(),
        ));
    }

    let input = dynamic_message_from_json(method.input(), payload)?;
    let output = method.output();
    let path = http::uri::PathAndQuery::from_maybe_shared(format!(
        "/{}/{}",
        method.parent_service().full_name(),
        method.name()
    ))
    .map_err(|err| RuntimeError::Execution(format!("invalid grpc method path: {err}")))?;

    let mut grpc = Grpc::new(channel);
    let response = grpc
        .unary(
            tonic::Request::new(input),
            path,
            DynamicGrpcCodec::new(output),
        )
        .await
        .map_err(|err| RuntimeError::Execution(format!("grpc call failed: {err}")))?;

    dynamic_message_to_json(&response.into_inner())
}

async fn load_grpc_method(
    proto_path: &str,
    service_name: &str,
    method_name: &str,
    proto_pools: &Mutex<HashMap<String, DescriptorPool>>,
) -> RuntimeResult<prost_reflect::MethodDescriptor> {
    let pool = get_proto_pool(proto_path, proto_pools).await?;
    let service = pool.get_service_by_name(service_name).ok_or_else(|| {
        RuntimeError::Execution(format!("grpc service `{service_name}` not found in proto"))
    })?;

    service
        .methods()
        .find(|method| method.name() == method_name)
        .ok_or_else(|| {
            RuntimeError::Execution(format!(
                "grpc method `{method_name}` not found in service `{service_name}`"
            ))
        })
}

async fn get_proto_pool(
    proto_path: &str,
    proto_pools: &Mutex<HashMap<String, DescriptorPool>>,
) -> RuntimeResult<DescriptorPool> {
    if let Some(pool) = proto_pools.lock().await.get(proto_path).cloned() {
        return Ok(pool);
    }

    let path = std::path::PathBuf::from(proto_path);
    let include = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let descriptor_set = protox::compile([&path], [include]).map_err(|err| {
        RuntimeError::Execution(format!("failed to compile proto `{proto_path}`: {err}"))
    })?;
    let pool = DescriptorPool::from_file_descriptor_set(descriptor_set).map_err(|err| {
        RuntimeError::Execution(format!("failed to load proto descriptors: {err}"))
    })?;
    proto_pools
        .lock()
        .await
        .insert(proto_path.to_string(), pool.clone());
    Ok(pool)
}

fn dynamic_message_from_json(
    desc: MessageDescriptor,
    value: Value,
) -> RuntimeResult<DynamicMessage> {
    let json = value.to_string();
    let mut deserializer = serde_json::Deserializer::from_str(&json);
    let message = DynamicMessage::deserialize(desc, &mut deserializer).map_err(|err| {
        RuntimeError::Execution(format!("failed to encode grpc payload from JSON: {err}"))
    })?;
    deserializer.end().map_err(|err| {
        RuntimeError::Execution(format!("invalid trailing JSON in grpc payload: {err}"))
    })?;
    Ok(message)
}

fn dynamic_message_to_json(message: &DynamicMessage) -> RuntimeResult<Value> {
    serde_json::to_value(message)
        .map_err(|err| RuntimeError::Execution(format!("failed to decode grpc response: {err}")))
}

#[derive(Clone)]
struct DynamicGrpcCodec {
    output: MessageDescriptor,
}

impl DynamicGrpcCodec {
    fn new(output: MessageDescriptor) -> Self {
        Self { output }
    }
}

impl Codec for DynamicGrpcCodec {
    type Encode = DynamicMessage;
    type Decode = DynamicMessage;
    type Encoder = DynamicGrpcEncoder;
    type Decoder = DynamicGrpcDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        DynamicGrpcEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        DynamicGrpcDecoder {
            output: self.output.clone(),
        }
    }
}

#[derive(Clone)]
struct DynamicGrpcEncoder;

impl Encoder for DynamicGrpcEncoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn encode(&mut self, item: Self::Item, buf: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        item.encode(buf)
            .map_err(|err| Status::internal(err.to_string()))
    }
}

#[derive(Clone)]
struct DynamicGrpcDecoder {
    output: MessageDescriptor,
}

impl Decoder for DynamicGrpcDecoder {
    type Item = DynamicMessage;
    type Error = Status;

    fn decode(&mut self, buf: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        DynamicMessage::decode(self.output.clone(), buf)
            .map(Some)
            .map_err(|err| Status::internal(err.to_string()))
    }
}

async fn call_grpc_struct(channel: Channel, method: &str, payload: Value) -> RuntimeResult<Value> {
    let mut grpc = Grpc::new(channel);
    let path = http::uri::PathAndQuery::from_maybe_shared(format!("/{method}"))
        .map_err(|err| RuntimeError::Execution(format!("invalid grpc method path: {err}")))?;
    let codec = ProstCodec::<Struct, Struct>::default();
    let request = tonic::Request::new(json_to_prost_struct(payload)?);
    let response = grpc
        .unary(request, path, codec)
        .await
        .map_err(|err| RuntimeError::Execution(format!("grpc call failed: {err}")))?;

    Ok(prost_struct_to_json(response.into_inner()))
}

fn json_to_prost_struct(value: Value) -> RuntimeResult<Struct> {
    match json_to_prost_value(value)?.kind {
        Some(Kind::StructValue(value)) => Ok(value),
        _ => Err(RuntimeError::Execution(
            "grpc payload must be a JSON object for google.protobuf.Struct calls".to_string(),
        )),
    }
}

fn json_to_prost_value(value: Value) -> RuntimeResult<ProstValue> {
    let kind = match value {
        Value::Null => Kind::NullValue(0),
        Value::Bool(value) => Kind::BoolValue(value),
        Value::Number(value) => Kind::NumberValue(value.as_f64().unwrap_or_default()),
        Value::String(value) => Kind::StringValue(value),
        Value::Array(items) => Kind::ListValue(ListValue {
            values: items
                .into_iter()
                .map(json_to_prost_value)
                .collect::<RuntimeResult<Vec<_>>>()?,
        }),
        Value::Object(fields) => Kind::StructValue(Struct {
            fields: fields
                .into_iter()
                .map(|(key, value)| json_to_prost_value(value).map(|value| (key, value)))
                .collect::<RuntimeResult<BTreeMap<_, _>>>()?,
        }),
    };

    Ok(ProstValue { kind: Some(kind) })
}

fn prost_struct_to_json(value: Struct) -> Value {
    Value::Object(
        value
            .fields
            .into_iter()
            .map(|(key, value)| (key, prost_value_to_json(value)))
            .collect(),
    )
}

fn prost_value_to_json(value: ProstValue) -> Value {
    match value.kind {
        Some(Kind::NullValue(_)) | None => Value::Null,
        Some(Kind::NumberValue(value)) => json!(value),
        Some(Kind::StringValue(value)) => Value::String(value),
        Some(Kind::BoolValue(value)) => Value::Bool(value),
        Some(Kind::StructValue(value)) => prost_struct_to_json(value),
        Some(Kind::ListValue(value)) => {
            Value::Array(value.values.into_iter().map(prost_value_to_json).collect())
        }
    }
}

fn execute_pipe(
    source: &Expression,
    operations: &[PipeOp],
    vars: &Vars,
    interner: &Rodeo,
) -> RuntimeResult<Value> {
    let mut current = eval_expr(source, vars, interner)?;

    for op in operations {
        match op {
            PipeOp::Filter { param, condition } => {
                let items = take_array(current, "filter")?;
                let mut filtered = Vec::new();
                for item in items {
                    let mut scoped = vars.clone();
                    scoped.insert(*param, item.clone());
                    if truthy(&eval_expr(condition, &scoped, interner)?) {
                        filtered.push(item);
                    }
                }
                current = Value::Array(filtered);
            }
            PipeOp::Map { param, layout } => {
                let items = take_array(current, "map")?;
                let mut mapped = Vec::with_capacity(items.len());
                for item in items {
                    let mut scoped = vars.clone();
                    scoped.insert(*param, item);
                    let mut object = Map::new();
                    for (key, expr) in layout {
                        object.insert(key.clone(), eval_expr(expr, &scoped, interner)?);
                    }
                    mapped.push(Value::Object(object));
                }
                current = Value::Array(mapped);
            }
            PipeOp::Take(count) => {
                let mut items = take_array(current, "take")?;
                items.truncate(*count);
                current = Value::Array(items);
            }
        }
    }

    Ok(current)
}

fn eval_response(endpoint: &Endpoint, vars: &Vars, interner: &Rodeo) -> RuntimeResult<Value> {
    let mut object = Map::new();
    for (key, expr) in &endpoint.response_body {
        object.insert(key.clone(), eval_expr(expr, vars, interner)?);
    }
    Ok(Value::Object(object))
}

fn eval_expr(expr: &Expression, vars: &Vars, interner: &Rodeo) -> RuntimeResult<Value> {
    match expr {
        Expression::Variable(name) => vars.get(name).cloned().ok_or_else(|| {
            RuntimeError::Execution(format!(
                "undefined runtime variable `{}`",
                sym(interner, *name)
            ))
        }),
        Expression::Number(value) => Ok(json!(value)),
        Expression::String(value) => Ok(Value::String(value.clone())),
        Expression::Boolean(value) => Ok(Value::Bool(*value)),
        Expression::Object(fields) => {
            let mut object = Map::new();
            for (key, value) in fields {
                object.insert(key.clone(), eval_expr(value, vars, interner)?);
            }
            Ok(Value::Object(object))
        }
        Expression::Array(items) => items
            .iter()
            .map(|item| eval_expr(item, vars, interner))
            .collect::<RuntimeResult<Vec<_>>>()
            .map(Value::Array),
        Expression::PropertyAccess(object, field) => {
            let object = eval_expr(object, vars, interner)?;
            get_property(&object, sym(interner, *field))
        }
        Expression::Call { callee, args } => eval_call(callee, args, vars, interner),
        Expression::BinaryOp(left, op, right) => {
            let left = eval_expr(left, vars, interner)?;
            let right = eval_expr(right, vars, interner)?;
            eval_binary(left, *op, right)
        }
    }
}

fn eval_call(
    callee: &Expression,
    args: &[Expression],
    vars: &Vars,
    interner: &Rodeo,
) -> RuntimeResult<Value> {
    match callee {
        Expression::PropertyAccess(object, method) if sym(interner, *method) == "len" => {
            if !args.is_empty() {
                return Err(RuntimeError::Execution(
                    "len() does not take arguments".to_string(),
                ));
            }
            let value = eval_expr(object, vars, interner)?;
            match value {
                Value::Array(items) => Ok(json!(items.len())),
                Value::Object(object) => Ok(json!(object.len())),
                Value::String(value) => Ok(json!(value.chars().count())),
                other => Err(RuntimeError::Execution(format!(
                    "len() is not supported for {}",
                    value_type(&other)
                ))),
            }
        }
        _ => Err(RuntimeError::Execution(
            "only len() calls are supported inside expressions".to_string(),
        )),
    }
}

fn eval_binary(left: Value, op: BinaryOperator, right: Value) -> RuntimeResult<Value> {
    match op {
        BinaryOperator::Add => {
            if matches!(left, Value::String(_)) || matches!(right, Value::String(_)) {
                Ok(Value::String(format!(
                    "{}{}",
                    as_string(left),
                    as_string(right)
                )))
            } else {
                Ok(json!(as_f64(&left)? + as_f64(&right)?))
            }
        }
        BinaryOperator::Sub => Ok(json!(as_f64(&left)? - as_f64(&right)?)),
        BinaryOperator::Mul => Ok(json!(as_f64(&left)? * as_f64(&right)?)),
        BinaryOperator::Div => Ok(json!(as_f64(&left)? / as_f64(&right)?)),
        BinaryOperator::Mod => Ok(json!(as_f64(&left)? % as_f64(&right)?)),
        BinaryOperator::Eq => Ok(Value::Bool(left == right)),
        BinaryOperator::Neq => Ok(Value::Bool(left != right)),
        BinaryOperator::Gt => Ok(Value::Bool(as_f64(&left)? > as_f64(&right)?)),
        BinaryOperator::Lt => Ok(Value::Bool(as_f64(&left)? < as_f64(&right)?)),
        BinaryOperator::Gte => Ok(Value::Bool(as_f64(&left)? >= as_f64(&right)?)),
        BinaryOperator::Lte => Ok(Value::Bool(as_f64(&left)? <= as_f64(&right)?)),
        BinaryOperator::And => Ok(Value::Bool(truthy(&left) && truthy(&right))),
        BinaryOperator::Or => Ok(Value::Bool(truthy(&left) || truthy(&right))),
    }
}

fn get_property(value: &Value, field: &str) -> RuntimeResult<Value> {
    match value {
        Value::Object(object) => Ok(object.get(field).cloned().unwrap_or(Value::Null)),
        Value::Array(items) if field == "len" => Ok(json!(items.len())),
        Value::String(value) if field == "len" => Ok(json!(value.chars().count())),
        other => Err(RuntimeError::Execution(format!(
            "cannot read property `{field}` from {}",
            value_type(other)
        ))),
    }
}

fn take_array(value: Value, operation: &str) -> RuntimeResult<Vec<Value>> {
    match value {
        Value::Array(items) => Ok(items),
        other => Err(RuntimeError::Execution(format!(
            "pipe {operation} expects array, got {}",
            value_type(&other)
        ))),
    }
}

fn as_f64(value: &Value) -> RuntimeResult<f64> {
    value.as_f64().ok_or_else(|| {
        RuntimeError::Execution(format!("expected number, got {}", value_type(value)))
    })
}

fn as_string(value: Value) -> String {
    match value {
        Value::String(value) => value,
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Bool(value) => *value,
        Value::Null => false,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn static_dbs(ast: &FileAST) -> Vars {
    ast.gateway
        .static_dbs
        .iter()
        .map(|db| (db.name, Value::String(db.url.clone())))
        .collect()
}

fn request_vars(
    path_params: &HashMap<String, String>,
    query_params: &HashMap<String, String>,
    headers: &HeaderMap,
    interner: &Rodeo,
) -> Vars {
    let mut vars = Vars::new();

    for (key, value) in path_params {
        insert_string_var(&mut vars, interner, key, value.clone());
    }

    insert_object_var(
        &mut vars,
        interner,
        "query",
        normalized_object(query_params),
    );
    insert_object_var(
        &mut vars,
        interner,
        "headers",
        headers
            .iter()
            .filter_map(|(name, value)| {
                value.to_str().ok().map(|value| {
                    (
                        normalize_key(name.as_str()),
                        Value::String(value.to_string()),
                    )
                })
            })
            .collect(),
    );
    insert_object_var(
        &mut vars,
        interner,
        "cookies",
        headers
            .get(header::COOKIE)
            .and_then(|value| value.to_str().ok())
            .map(parse_cookies)
            .unwrap_or_default(),
    );

    vars
}

fn insert_string_var(vars: &mut Vars, interner: &Rodeo, name: &str, value: String) {
    if let Some(sym) = interner.get(name) {
        vars.insert(sym, Value::String(value));
    }
}

fn insert_object_var(vars: &mut Vars, interner: &Rodeo, name: &str, value: Map<String, Value>) {
    if let Some(sym) = interner.get(name) {
        vars.insert(sym, Value::Object(value));
    }
}

fn normalized_object(params: &HashMap<String, String>) -> Map<String, Value> {
    params
        .iter()
        .map(|(key, value)| (normalize_key(key), Value::String(value.clone())))
        .collect()
}

fn parse_cookies(header: &str) -> Map<String, Value> {
    header
        .split(';')
        .filter_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            Some((normalize_key(key), Value::String(value.to_string())))
        })
        .collect()
}

fn normalize_key(key: &str) -> String {
    key.replace('-', "_")
}

fn axum_route_path(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            segment
                .strip_prefix(':')
                .map(|name| format!("{{{name}}}"))
                .unwrap_or_else(|| segment.to_string())
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn db_urls(ast: &FileAST, interner: &Rodeo) -> HashMap<String, String> {
    ast.gateway
        .static_dbs
        .iter()
        .map(|db| (sym(interner, db.name).to_string(), db.url.clone()))
        .collect()
}

fn proto_paths(ast: &FileAST, interner: &Rodeo) -> HashMap<String, String> {
    ast.gateway
        .static_protos
        .iter()
        .map(|proto| (sym(interner, proto.name).to_string(), proto.path.clone()))
        .collect()
}

fn step_var(step: &Step) -> Sym {
    match step {
        Step::Let { var_name, .. }
        | Step::FetchHttp { var_name, .. }
        | Step::CallGrpc { var_name, .. }
        | Step::QueryDb { var_name, .. }
        | Step::Pipe { var_name, .. } => *var_name,
    }
}

fn endpoint_rate_limit_policy(endpoint: &Endpoint) -> Option<RateLimitPolicy> {
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

fn endpoint_secure_policy(endpoint: &Endpoint, interner: &Rodeo) -> Option<SecurePolicy> {
    endpoint.options.iter().find_map(|option| match option {
        EndpointOption::Secure(schemes) => Some(SecurePolicy {
            schemes: schemes
                .iter()
                .map(|scheme| sym(interner, *scheme).to_string())
                .collect(),
        }),
        EndpointOption::RateLimit { .. } => None,
    })
}

fn method_filter(method: &str) -> Option<MethodFilter> {
    match method {
        "GET" => Some(MethodFilter::GET),
        "POST" => Some(MethodFilter::POST),
        "PUT" => Some(MethodFilter::PUT),
        "PATCH" => Some(MethodFilter::PATCH),
        "DELETE" => Some(MethodFilter::DELETE),
        "HEAD" => Some(MethodFilter::HEAD),
        "OPTIONS" => Some(MethodFilter::OPTIONS),
        "TRACE" => Some(MethodFilter::TRACE),
        _ => None,
    }
}

fn sym(interner: &Rodeo, name: Sym) -> &str {
    interner.resolve(&name)
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMethod { method, path } => {
                write!(f, "unsupported HTTP method `{method}` for `{path}`")
            }
            Self::InvalidBindAddress(addr) => write!(f, "invalid bind address `{addr}`"),
            Self::Bind(err) => write!(f, "failed to bind listener: {err}"),
            Self::Serve(err) => write!(f, "server failed: {err}"),
            Self::Execution(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RuntimeError {}

#[cfg(test)]
mod tests {
    use super::Runtime;
    use crate::parser::Parser;
    use crate::planner::build_plan;
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use lasso::Rodeo;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    #[tokio::test]
    async fn generated_axum_route_executes_planned_layers() {
        let source = r#"
            gateway "test" { port: 0 }

            endpoint "GET /dashboard" {
                let user = { "id": 42, "role": "admin", "name": "Ada" };
                let orders = [
                    { "uuid": "a", "status": "completed", "total": 1000, "items": [1, 2] },
                    { "uuid": "b", "status": "pending", "total": 100, "items": [1] },
                    { "uuid": "c", "status": "completed", "total": 90, "items": [1, 2, 3] }
                ];
                let top_orders = orders
                    | filter(order => order.status == "completed" || order.total > 500)
                    | map(order => {
                        "id": order.uuid,
                        "amount": order.total / 10,
                        "items_count": order.items.len()
                    })
                    | take(1);

                respond 201 {
                    "user_name": user.name,
                    "is_admin": user.role == "admin",
                    "latest_orders": top_orders
                }
            }
        "#;

        let mut parser = Parser::new(Rodeo::new());
        let ast = parser.parse(source).expect("test DSL should parse");
        let plan = build_plan(&ast, &parser.interner).expect("test DSL should plan");
        let router = Runtime::new(ast, parser.interner, plan)
            .router()
            .expect("router should build");

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/dashboard")
                    .body(axum::body::Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");

        assert_eq!(response.status(), StatusCode::CREATED);

        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body should read");
        let actual: Value = serde_json::from_slice(&body).expect("body should be JSON");

        assert_eq!(
            actual,
            json!({
                "user_name": "Ada",
                "is_admin": true,
                "latest_orders": [
                    {
                        "id": "a",
                        "amount": 100.0,
                        "items_count": 2
                    }
                ]
            })
        );
    }

    #[tokio::test]
    async fn generated_axum_route_executes_database_query() {
        let source = r#"
            gateway "test" {
                port: 0,
                databases: [
                    sqlite "main" { url: "sqlite::memory:" }
                ],
            }

            endpoint "GET /db" {
                let rows = db::query("main", "select ? as answer, ? as label", 42, "ok");

                respond 200 {
                    "rows": rows
                }
            }
        "#;

        let mut parser = Parser::new(Rodeo::new());
        let ast = parser.parse(source).expect("test DSL should parse");
        let plan = build_plan(&ast, &parser.interner).expect("test DSL should plan");
        let router = Runtime::new(ast, parser.interner, plan)
            .router()
            .expect("router should build");

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/db")
                    .body(axum::body::Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");

        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body should read");
        let actual: Value = serde_json::from_slice(&body).expect("body should be JSON");

        assert_eq!(
            actual,
            json!({
                "rows": [
                    {
                        "answer": 42.0,
                        "label": "ok"
                    }
                ]
            })
        );
    }

    #[tokio::test]
    async fn secure_endpoint_requires_bearer_token() {
        let source = r#"
            gateway "test" { port: 0 }

            endpoint "GET /secure" {
                secure: [BearerJWT],
                respond 200 { "ok": true }
            }
        "#;

        let router = test_router(source);

        let unauthorized = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/secure")
                    .body(axum::body::Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let authorized = router
            .oneshot(
                Request::builder()
                    .uri("/secure")
                    .header("authorization", "Bearer token")
                    .body(axum::body::Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");
        assert_eq!(authorized.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn rate_limit_endpoint_returns_429_after_limit() {
        let source = r#"
            gateway "test" { port: 0 }

            endpoint "GET /limited" {
                rate_limit: 1/rps window 1s,
                respond 200 { "ok": true }
            }
        "#;

        let router = test_router(source);

        let first = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/limited")
                    .body(axum::body::Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");
        assert_eq!(first.status(), StatusCode::OK);

        let second = router
            .oneshot(
                Request::builder()
                    .uri("/limited")
                    .body(axum::body::Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn request_context_exposes_path_query_headers_and_cookies() {
        let source = r#"
            gateway "test" { port: 0 }

            endpoint "GET /api/v1/todos/:id" {
                respond 200 {
                    "id": id,
                    "arg": query.arg,
                    "trace": headers.x_trace_id,
                    "session": cookies.session
                }
            }
        "#;

        let router = test_router(source);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/todos/42?arg=1")
                    .header("x-trace-id", "abc")
                    .header("cookie", "session=s1; theme=dark")
                    .body(axum::body::Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");

        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body should read");
        let actual: Value = serde_json::from_slice(&body).expect("body should be JSON");

        assert_eq!(
            actual,
            json!({
                "id": "42",
                "arg": "1",
                "trace": "abc",
                "session": "s1"
            })
        );
    }

    #[tokio::test]
    async fn modulo_operator_evaluates_remainder() {
        let source = r#"
            gateway "test" { port: 0 }

            endpoint "GET /math" {
                let value = 10 % 4;
                let mixed = 10 + 5 % 3 * 2;

                respond 200 {
                    "value": value,
                    "mixed": mixed,
                    "is_odd": value == 1
                }
            }
        "#;

        let router = test_router(source);

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/math")
                    .body(axum::body::Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");

        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body should read");
        let actual: Value = serde_json::from_slice(&body).expect("body should be JSON");

        assert_eq!(
            actual,
            json!({
                "value": 2.0,
                "mixed": 14.0,
                "is_odd": false
            })
        );
    }

    fn test_router(source: &str) -> axum::Router {
        let mut parser = Parser::new(Rodeo::new());
        let ast = parser.parse(source).expect("test DSL should parse");
        let plan = build_plan(&ast, &parser.interner).expect("test DSL should plan");
        Runtime::new(ast, parser.interner, plan)
            .router()
            .expect("router should build")
    }
}
