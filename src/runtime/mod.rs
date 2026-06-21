use crate::ast::{
    BinaryOperator, DbQueryConfig, Endpoint, Expression, FileAST, GrpcConfig, HttpConfig, PipeOp,
    Step, Sym,
};
use crate::planner::ExecutionPlan;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query};
use axum::http::header::{HeaderName, HeaderValue, SET_COOKIE};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::on;
use axum::{Json, Router};
use dashmap::DashMap;
use lasso::Rodeo;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor};
use prost_types::{ListValue, Struct, Value as ProstValue, value::Kind};
use reqwest::{Client, Method as ReqwestMethod};
use serde_json::{Map, json};
use sqlx::any::{AnyPoolOptions, install_default_drivers};
use sqlx::{AnyPool, Column, Row, TypeInfo};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command as ProcessCommand;
use tokio::task::JoinSet;
use tonic::client::Grpc;
use tonic::transport::{Channel, Endpoint as TonicEndpoint};
use tonic_prost::ProstCodec;
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer, ExposeHeaders};
use tracing::{debug, error, info, warn};

mod functions;
mod rate_limit;
mod security;
mod traits;
mod types;

use functions::{
    axum_route_path, db_urls, gateway_vars, insert_request_vars, insert_value_var, method_filter,
    proto_paths, request_body_value, sym,
};
use rate_limit::{VelogateRateLimitLayer, endpoint_rate_limit_policy};
use security::authorize_endpoint;
pub use types::{CommandOptions, RateLimitOptions, Runtime, RuntimeError, RuntimeOptions};
use types::{
    DynamicGrpcCodec, EndpointRuntime, GrpcRequest, RuntimeMetrics, RuntimeResult, SQLX_DRIVERS,
    StepRuntimeDeps, Value, Vars,
};

struct EvaluatedResponse {
    body: Option<Value>,
    headers: HeaderMap,
    cookies: Vec<String>,
}

impl Runtime {
    pub fn new(ast: FileAST, interner: Rodeo, plan: ExecutionPlan) -> Self {
        Self::with_options(ast, interner, plan, RuntimeOptions::default())
    }

    pub fn with_options(
        ast: FileAST,
        interner: Rodeo,
        plan: ExecutionPlan,
        options: RuntimeOptions,
    ) -> Self {
        let command_slots = options.command.max_concurrency.max(1);
        Self {
            db_urls: Arc::new(db_urls(&ast, &interner)),
            proto_paths: Arc::new(proto_paths(&ast, &interner)),
            ast: Arc::new(ast),
            interner: Arc::new(interner),
            plan: Arc::new(plan),
            client: Client::new(),
            db_pools: Arc::new(DashMap::new()),
            proto_pools: Arc::new(DashMap::new()),
            options: Arc::new(options),
            command_slots: Arc::new(tokio::sync::Semaphore::new(command_slots)),
            metrics: Arc::new(RuntimeMetrics::default()),
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
        self.serve_with_shutdown(std::future::pending::<()>()).await
    }

    pub async fn serve_with_shutdown<F>(self, shutdown: F) -> RuntimeResult<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let addr = self.bind_addr()?;
        let router = self.router()?;
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(RuntimeError::Bind)?;

        info!(%addr, "velogate listening");
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(RuntimeError::Serve)
    }

    pub fn router(&self) -> RuntimeResult<Router> {
        let mut router = Router::new();
        let static_vars = gateway_vars(&self.ast, &self.interner)?;
        let mut registered_routes = std::collections::HashSet::new();

        for (idx, endpoint) in self.ast.endpoints.iter().enumerate() {
            validate_runtime_route(&endpoint.path)?;
            if !registered_routes.insert((endpoint.method.clone(), route_pattern(&endpoint.path))) {
                return Err(RuntimeError::RouteConflict(format!(
                    "conflicting route `{} {}`",
                    endpoint.method, endpoint.path
                )));
            }
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
                static_vars: static_vars.clone(),
                db_urls: Arc::clone(&self.db_urls),
                db_pools: Arc::clone(&self.db_pools),
                proto_paths: Arc::clone(&self.proto_paths),
                proto_pools: Arc::clone(&self.proto_pools),
                options: Arc::clone(&self.options),
                command_slots: Arc::clone(&self.command_slots),
                metrics: Arc::clone(&self.metrics),
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
                      headers: HeaderMap,
                      body: Bytes| {
                    let runtime = Arc::clone(&handler_runtime);
                    async move {
                        runtime
                            .handle(
                                path_params,
                                query_params,
                                headers,
                                request_body_value(&body),
                            )
                            .await
                    }
                },
            );

            if let Some(policy) = endpoint_rate_limit_policy(
                endpoint,
                &self.options.rate_limit,
                Arc::clone(&self.metrics),
            ) {
                method_router = method_router.layer(VelogateRateLimitLayer::new(policy));
            }

            router = router.route(&axum_route_path(&endpoint.path), method_router);
        }

        router = self.add_operational_routes(router, &mut registered_routes)?;
        if let Some(cors_layer) = self.cors_layer()? {
            router = router.layer(cors_layer);
        }
        Ok(router)
    }

    fn cors_layer(&self) -> RuntimeResult<Option<CorsLayer>> {
        let Some(cors) = self.ast.gateway.cors.as_ref() else {
            return Ok(None);
        };

        let mut layer = CorsLayer::new()
            .allow_origin(cors_allow_origin(&cors.origins)?)
            .allow_methods(cors_allow_methods(&cors.methods)?)
            .allow_headers(cors_allow_headers(&cors.headers)?)
            .expose_headers(cors_expose_headers(&cors.expose_headers)?)
            .allow_credentials(cors.credentials);

        if let Some(max_age) = cors.max_age_seconds {
            layer = layer.max_age(Duration::from_secs(max_age));
        }

        Ok(Some(layer))
    }

    fn add_operational_routes(
        &self,
        mut router: Router,
        registered: &mut std::collections::HashSet<(String, String)>,
    ) -> RuntimeResult<Router> {
        for (path, kind) in [
            (self.options.health_path.as_deref(), "health"),
            (self.options.readiness_path.as_deref(), "readiness"),
            (self.options.metrics_path.as_deref(), "metrics"),
        ] {
            let Some(path) = path else { continue };
            if !path.starts_with('/')
                || path.contains(':')
                || path.contains('{')
                || path.contains('}')
                || path.contains('*')
            {
                return Err(RuntimeError::Config(format!(
                    "{kind} path must be a static absolute path, got `{path}`"
                )));
            }
            if !registered.insert(("GET".to_string(), route_pattern(path))) {
                return Err(RuntimeError::RouteConflict(format!(
                    "{kind} path `{path}` conflicts with an endpoint"
                )));
            }

            let metrics = Arc::clone(&self.metrics);
            router = if kind == "metrics" {
                router.route(
                    path,
                    axum::routing::get(move || async move {
                        Json(json!({
                            "requests": metrics.requests.load(AtomicOrdering::Relaxed),
                            "failures": metrics.failures.load(AtomicOrdering::Relaxed),
                            "rate_limited": metrics.rate_limited.load(AtomicOrdering::Relaxed),
                            "commands_started": metrics.commands_started.load(AtomicOrdering::Relaxed),
                            "commands_rejected": metrics.commands_rejected.load(AtomicOrdering::Relaxed),
                        }))
                    }),
                )
            } else {
                router.route(
                    path,
                    axum::routing::get(|| async { Json(json!({ "status": "ok" })) }),
                )
            };
        }
        Ok(router)
    }
}

fn route_pattern(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if segment.starts_with(':') || (segment.starts_with('{') && segment.ends_with('}')) {
                "{}"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn cors_allow_origin(origins: &[String]) -> RuntimeResult<AllowOrigin> {
    if origins.iter().any(|origin| origin == "*") {
        return Ok(AllowOrigin::any());
    }
    let origins = origins
        .iter()
        .map(|origin| {
            HeaderValue::from_str(origin).map_err(|err| {
                RuntimeError::Config(format!("invalid gateway.cors origin `{origin}`: {err}"))
            })
        })
        .collect::<RuntimeResult<Vec<_>>>()?;
    Ok(AllowOrigin::list(origins))
}

fn cors_allow_methods(methods: &[String]) -> RuntimeResult<AllowMethods> {
    if methods.is_empty() || methods.iter().any(|method| method == "*") {
        return Ok(AllowMethods::any());
    }
    let methods = methods
        .iter()
        .map(|method| {
            Method::from_bytes(method.as_bytes()).map_err(|err| {
                RuntimeError::Config(format!("invalid gateway.cors method `{method}`: {err}"))
            })
        })
        .collect::<RuntimeResult<Vec<_>>>()?;
    Ok(AllowMethods::list(methods))
}

fn cors_allow_headers(headers: &[String]) -> RuntimeResult<AllowHeaders> {
    if headers.is_empty() || headers.iter().any(|header| header == "*") {
        return Ok(AllowHeaders::any());
    }
    Ok(AllowHeaders::list(cors_header_names(
        "gateway.cors.headers",
        headers,
    )?))
}

fn cors_expose_headers(headers: &[String]) -> RuntimeResult<ExposeHeaders> {
    if headers.is_empty() {
        return Ok(ExposeHeaders::default());
    }
    if headers.iter().any(|header| header == "*") {
        return Ok(ExposeHeaders::any());
    }
    Ok(ExposeHeaders::list(cors_header_names(
        "gateway.cors.expose_headers",
        headers,
    )?))
}

fn cors_header_names(field: &str, headers: &[String]) -> RuntimeResult<Vec<HeaderName>> {
    headers
        .iter()
        .map(|header| {
            HeaderName::from_bytes(header.as_bytes()).map_err(|err| {
                RuntimeError::Config(format!("{field} contains invalid header `{header}`: {err}"))
            })
        })
        .collect()
}

fn validate_runtime_route(path: &str) -> RuntimeResult<()> {
    if !path.starts_with('/') || path.contains(['{', '}', '*']) {
        return Err(RuntimeError::Config(format!(
            "invalid endpoint route `{path}`"
        )));
    }
    for parameter in path
        .split('/')
        .filter_map(|segment| segment.strip_prefix(':'))
    {
        if parameter.is_empty()
            || !parameter
                .chars()
                .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        {
            return Err(RuntimeError::Config(format!(
                "invalid endpoint route parameter `:{parameter}` in `{path}`"
            )));
        }
    }
    Ok(())
}

impl EndpointRuntime {
    async fn handle(
        self: Arc<Self>,
        path_params: HashMap<String, String>,
        query_params: HashMap<String, String>,
        headers: HeaderMap,
        body: Value,
    ) -> Response {
        let endpoint = format!("{} {}", self.endpoint.method, self.endpoint.path);
        let request_id = self.metrics.requests.fetch_add(1, AtomicOrdering::Relaxed) + 1;
        debug!(%endpoint, request_id, "handling request");
        let mut vars = self.static_vars.clone();
        vars.reserve(path_params.len() + 4);
        insert_request_vars(
            &mut vars,
            &path_params,
            &query_params,
            &headers,
            body,
            &self.interner,
        );

        if let Err(err) = authorize_endpoint(&self.endpoint, &mut vars, &headers, &self.interner) {
            warn!(%endpoint, message = %err.message, "request rejected by security rule");
            let body = json!({
                "error": "unauthorized",
                "message": "authorization failed",
            });
            return (StatusCode::UNAUTHORIZED, Json(body)).into_response();
        }

        match self.execute(vars).await {
            Ok(evaluated) => {
                let status =
                    StatusCode::from_u16(self.endpoint.response.status).unwrap_or(StatusCode::OK);
                let mut response = if let Some(body) = evaluated.body {
                    (status, Json(body)).into_response()
                } else {
                    let mut response = Response::new(Body::empty());
                    *response.status_mut() = status;
                    response
                };
                response.headers_mut().extend(evaluated.headers);
                for cookie in evaluated.cookies {
                    match HeaderValue::from_str(&cookie) {
                        Ok(value) => {
                            response.headers_mut().append(SET_COOKIE, value);
                        }
                        Err(err) => {
                            error!(%endpoint, %err, cookie, "failed to append response cookie")
                        }
                    }
                }
                response
            }
            Err(err) => {
                self.metrics.failures.fetch_add(1, AtomicOrdering::Relaxed);
                let status = error_status(&err);
                error!(%endpoint, %status, request_id, error = %err, "request failed");
                let body = json!({
                    "error": error_code(&err),
                    "message": public_error_message(&err),
                    "request_id": format!("{request_id:016x}"),
                });
                (status, Json(body)).into_response()
            }
        }
    }

    async fn execute(&self, mut vars: Vars) -> RuntimeResult<EvaluatedResponse> {
        for layer in &self.plan.layers {
            if let [step_idx] = layer.as_slice() {
                let step = self.endpoint.steps.get(*step_idx).ok_or_else(|| {
                    RuntimeError::Execution(format!("missing step {step_idx} in endpoint plan"))
                })?;
                let deps = StepRuntimeDeps {
                    client: &self.client,
                    db_urls: &self.db_urls,
                    db_pools: &self.db_pools,
                    proto_paths: &self.proto_paths,
                    proto_pools: &self.proto_pools,
                    options: &self.options,
                    command_slots: &self.command_slots,
                    metrics: &self.metrics,
                };
                debug!(
                    step = step_idx,
                    var = sym(&self.interner, step.var_name()),
                    "executing step"
                );
                let value = execute_step(step, &vars, &self.interner, &deps).await?;
                vars.insert(step.var_name(), value);
                continue;
            }

            let snapshot = Arc::new(vars.clone());
            let mut tasks = JoinSet::new();

            for step_idx in layer {
                let step_idx_value = *step_idx;
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
                let options = Arc::clone(&self.options);
                let command_slots = Arc::clone(&self.command_slots);
                let metrics = Arc::clone(&self.metrics);

                tasks.spawn(async move {
                    let var = step.var_name();
                    let deps = StepRuntimeDeps {
                        client: &client,
                        db_urls: &db_urls,
                        db_pools: &db_pools,
                        proto_paths: &proto_paths,
                        proto_pools: &proto_pools,
                        options: &options,
                        command_slots: &command_slots,
                        metrics: &metrics,
                    };
                    debug!(
                        step = step_idx_value,
                        var = sym(&interner, var),
                        "executing step"
                    );
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
        Step::Command { command, .. } => execute_command(command, deps).await,
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

async fn execute_command(command: &str, deps: &StepRuntimeDeps<'_>) -> RuntimeResult<Value> {
    if !deps.options.command.enabled {
        deps.metrics
            .commands_rejected
            .fetch_add(1, AtomicOrdering::Relaxed);
        return Err(RuntimeError::Execution(
            "command execution is disabled; enable it explicitly at startup".to_string(),
        ));
    }

    let permit = deps.command_slots.acquire().await.map_err(|_| {
        RuntimeError::Execution("command concurrency limiter is closed".to_string())
    })?;
    deps.metrics
        .commands_started
        .fetch_add(1, AtomicOrdering::Relaxed);

    let mut process = shell_command(command);
    process
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process.spawn().map_err(|err| {
        RuntimeError::Execution(format!("command `{command}` failed to start: {err}"))
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RuntimeError::Execution("failed to capture command stdout".to_string()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| RuntimeError::Execution("failed to capture command stderr".to_string()))?;
    let max_output = deps.options.command.max_output_bytes;

    let work = async move {
        let stdout_task = read_limited(stdout, max_output);
        let stderr_task = read_limited(stderr, max_output);
        let (status, stdout, stderr) = tokio::try_join!(child.wait(), stdout_task, stderr_task)
            .map_err(|err| RuntimeError::Execution(format!("command execution failed: {err}")))?;
        RuntimeResult::Ok((status, stdout, stderr))
    };
    let (status, stdout, stderr) = tokio::time::timeout(deps.options.command.timeout, work)
        .await
        .map_err(|_| RuntimeError::Timeout("command execution timed out".to_string()))??;
    drop(permit);

    Ok(json!({
        "success": status.success(),
        "status": status.code(),
        "stdout": String::from_utf8_lossy(&stdout).trim_end().to_string(),
        "stderr": String::from_utf8_lossy(&stderr).trim_end().to_string(),
    }))
}

async fn read_limited<R>(reader: R, max_bytes: usize) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    reader
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > max_bytes {
        return Err(std::io::Error::other(format!(
            "command output exceeded {max_bytes} bytes"
        )));
    }
    Ok(bytes)
}

#[cfg(target_os = "windows")]
fn shell_command(command: &str) -> ProcessCommand {
    let mut process = ProcessCommand::new("powershell");
    process.args(["-NoProfile", "-Command", command]);
    process
}

#[cfg(not(target_os = "windows"))]
fn shell_command(command: &str) -> ProcessCommand {
    let mut process = ProcessCommand::new("sh");
    process.args(["-c", command]);
    process
}

async fn fetch_http(
    config: &HttpConfig,
    vars: &Vars,
    interner: &Rodeo,
    client: &Client,
) -> RuntimeResult<Value> {
    let url = as_string(eval_expr(&config.url, vars, interner)?);
    let method_name = config.method.as_deref().unwrap_or("GET").to_uppercase();
    let method = ReqwestMethod::from_bytes(method_name.as_bytes()).map_err(|err| {
        RuntimeError::BadRequest(format!(
            "invalid HTTP method `{method_name}` for fetch: {err}"
        ))
    })?;
    let request_body = config
        .body
        .as_ref()
        .map(|body| eval_expr(body, vars, interner))
        .transpose()?;
    let attempts = config.retries.unwrap_or(0).saturating_add(1);
    let mut last_error = None;

    for attempt in 0..attempts {
        debug!(%method, %url, attempt = attempt + 1, attempts, "sending upstream HTTP request");
        let mut request = client.request(method.clone(), &url);
        if let Some(body) = &request_body {
            request = request.json(body);
        }
        if let Some(timeout_ms) = config.timeout_ms {
            request = request.timeout(Duration::from_millis(timeout_ms));
        }

        match request.send().await {
            Ok(response) => match response.error_for_status() {
                Ok(response) => {
                    let bytes = response.bytes().await.map_err(|err| {
                        RuntimeError::Upstream(format!("failed to read response from {url}: {err}"))
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

    let message = format!(
        "fetch {url} failed: {}",
        last_error.unwrap_or_else(|| "unknown error".to_string())
    );
    if message.contains("timed out") || message.contains("timeout") {
        Err(RuntimeError::Timeout(message))
    } else {
        Err(RuntimeError::Upstream(message))
    }
}

async fn query_db(
    config: &DbQueryConfig,
    vars: &Vars,
    interner: &Rodeo,
    db_urls: &HashMap<String, String>,
    db_pools: &DashMap<String, AnyPool>,
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
            .map_err(|err| RuntimeError::Database(format!("database query failed: {err}")))?;
        rows.into_iter()
            .map(row_to_json)
            .collect::<RuntimeResult<Vec<_>>>()
            .map(Value::Array)
    };

    let result = if let Some(timeout_ms) = config.timeout_ms {
        tokio::time::timeout(Duration::from_millis(timeout_ms), work)
            .await
            .map_err(|_| RuntimeError::Timeout("database query timed out".to_string()))?
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

async fn get_db_pool(url: &str, db_pools: &DashMap<String, AnyPool>) -> RuntimeResult<AnyPool> {
    SQLX_DRIVERS.call_once(install_default_drivers);

    if let Some(pool) = db_pools.get(url).map(|pool| pool.clone()) {
        return Ok(pool);
    }

    let pool = AnyPoolOptions::new()
        .max_connections(5)
        .connect(url)
        .await
        .map_err(|err| RuntimeError::Database(format!("failed to connect to database: {err}")))?;
    Ok(db_pools
        .entry(url.to_string())
        .or_insert_with(|| pool.clone())
        .clone())
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
    let mut object = Map::with_capacity(row.columns().len());

    for (idx, column) in row.columns().iter().enumerate() {
        let value = decode_column(&row, idx).map_err(|err| {
            RuntimeError::Database(format!(
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
    proto_pools: &DashMap<String, DescriptorPool>,
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
            .map_err(|_| RuntimeError::Timeout("grpc call timed out".to_string()))?
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
        .map_err(|err| RuntimeError::BadRequest(format!("invalid grpc endpoint: {err}")))?
        .connect()
        .await
        .map_err(|err| RuntimeError::Grpc(format!("failed to connect grpc: {err}")))
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

async fn call_grpc_dynamic(
    channel: Channel,
    config: &GrpcConfig,
    payload: Value,
    proto_paths: &HashMap<String, String>,
    proto_pools: &DashMap<String, DescriptorPool>,
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
    proto_pools: &DashMap<String, DescriptorPool>,
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
    proto_pools: &DashMap<String, DescriptorPool>,
) -> RuntimeResult<DescriptorPool> {
    if let Some(pool) = proto_pools.get(proto_path).map(|pool| pool.clone()) {
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
    Ok(proto_pools
        .entry(proto_path.to_string())
        .or_insert_with(|| pool.clone())
        .clone())
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
        let (name, invocation) = match op {
            PipeOp::Closure { name, param, value } => {
                let name = sym(interner, *name);
                (
                    name,
                    PipeInvocation::Closure {
                        param: *param,
                        value,
                    },
                )
            }
            PipeOp::Expr { name, value } => {
                let name = sym(interner, *name);
                (name, PipeInvocation::Expr { value })
            }
            PipeOp::Reduce {
                name,
                initial,
                acc,
                param,
                value,
            } => {
                let name = sym(interner, *name);
                (
                    name,
                    PipeInvocation::Reduce {
                        initial,
                        acc: *acc,
                        param: *param,
                        value,
                    },
                )
            }
            PipeOp::None { name } => {
                let name = sym(interner, *name);
                (name, PipeInvocation::None)
            }
        };
        current = execute_pipe_op(PipeRuntimeCtx {
            name,
            current,
            invocation,
            vars,
            interner,
        })?;
    }

    Ok(current)
}

enum PipeInvocation<'a> {
    Closure {
        param: Sym,
        value: &'a Expression,
    },
    Expr {
        value: &'a Expression,
    },
    None,
    Reduce {
        initial: &'a Expression,
        acc: Sym,
        param: Sym,
        value: &'a Expression,
    },
}

struct PipeRuntimeCtx<'a> {
    name: &'a str,
    current: Value,
    invocation: PipeInvocation<'a>,
    vars: &'a Vars,
    interner: &'a Rodeo,
}

impl<'a> PipeRuntimeCtx<'a> {
    fn shape_name(&self) -> &'static str {
        match self.invocation {
            PipeInvocation::Closure { .. } => "closure",
            PipeInvocation::Expr { .. } => "expression",
            PipeInvocation::None => "empty",
            PipeInvocation::Reduce { .. } => "reduce",
        }
    }

    fn closure(self) -> RuntimeResult<(Value, Sym, &'a Expression, &'a Vars, &'a Rodeo)> {
        match self.invocation {
            PipeInvocation::Closure { param, value } => {
                Ok((self.current, param, value, self.vars, self.interner))
            }
            _ => Err(self.invalid_shape("closure")),
        }
    }

    fn expr(self) -> RuntimeResult<(&'a str, Value, &'a Expression, &'a Vars, &'a Rodeo)> {
        match self.invocation {
            PipeInvocation::Expr { value } => {
                Ok((self.name, self.current, value, self.vars, self.interner))
            }
            _ => Err(self.invalid_shape("expression")),
        }
    }

    fn none(self) -> RuntimeResult<Value> {
        match self.invocation {
            PipeInvocation::None => Ok(self.current),
            _ => Err(self.invalid_shape("empty")),
        }
    }

    fn reduce(
        self,
    ) -> RuntimeResult<(
        Value,
        &'a Expression,
        Sym,
        Sym,
        &'a Expression,
        &'a Vars,
        &'a Rodeo,
    )> {
        match self.invocation {
            PipeInvocation::Reduce {
                initial,
                acc,
                param,
                value,
            } => Ok((
                self.current,
                initial,
                acc,
                param,
                value,
                self.vars,
                self.interner,
            )),
            _ => Err(self.invalid_shape("reduce")),
        }
    }

    fn invalid_shape(&self, expected: &str) -> RuntimeError {
        RuntimeError::Execution(format!(
            "pipe operation `{}` expects {expected} syntax, got {} syntax",
            self.name,
            self.shape_name()
        ))
    }
}

macro_rules! runtime_pipe_ops {
    ($($name:literal => $handler:ident,)*) => {
        fn execute_pipe_op(ctx: PipeRuntimeCtx<'_>) -> RuntimeResult<Value> {
            match ctx.name {
                $($name => $handler(ctx),)*
                _ => {
                    let name = ctx.name;
                    let shape = ctx.shape_name();
                    Err(RuntimeError::Execution(format!(
                        "unsupported pipe {shape} operation `{name}`"
                    )))
                }
            }
        }
    };
}

runtime_pipe_ops! {
    "filter" => pipe_filter,
    "map" => pipe_map,
    "sort" => pipe_sort,
    "group_by" => pipe_group_by,
    "sum" => pipe_sum,
    "avg" => pipe_avg,
    "min" => pipe_min,
    "max" => pipe_max,
    "unique" => pipe_unique,
    "flat_map" => pipe_flat_map,
    "limit" => pipe_limit,
    "take" => pipe_limit,
    "offset" => pipe_offset,
    "count" => pipe_count,
    "first" => pipe_first,
    "last" => pipe_last,
    "reduce" => pipe_reduce,
}

fn pipe_filter(ctx: PipeRuntimeCtx<'_>) -> RuntimeResult<Value> {
    let (current, param, value, vars, interner) = ctx.closure()?;
    let items = take_array(current, "filter")?;
    let mut filtered = Vec::new();
    let mut scoped = ScopedVars::with_capacity(vars, 1);
    for item in items {
        scoped.set(param, item.clone());
        if truthy(&eval_expr(value, &scoped, interner)?) {
            filtered.push(item);
        }
    }
    Ok(Value::Array(filtered))
}

fn pipe_map(ctx: PipeRuntimeCtx<'_>) -> RuntimeResult<Value> {
    let (current, param, value, vars, interner) = ctx.closure()?;
    let items = take_array(current, "map")?;
    let mut mapped = Vec::with_capacity(items.len());
    let mut scoped = ScopedVars::with_capacity(vars, 1);
    for item in items {
        scoped.set(param, item);
        mapped.push(eval_expr(value, &scoped, interner)?);
    }
    Ok(Value::Array(mapped))
}

fn pipe_sort(ctx: PipeRuntimeCtx<'_>) -> RuntimeResult<Value> {
    let (current, param, value, vars, interner) = ctx.closure()?;
    let items = take_array(current, "sort")?;
    let mut keyed = Vec::with_capacity(items.len());
    let mut scoped = ScopedVars::with_capacity(vars, 1);
    for item in items {
        scoped.set(param, item.clone());
        keyed.push((eval_expr(value, &scoped, interner)?, item));
    }
    keyed.sort_by(|(left, _), (right, _)| compare_json(left, right));
    Ok(Value::Array(
        keyed.into_iter().map(|(_, item)| item).collect(),
    ))
}

fn pipe_group_by(ctx: PipeRuntimeCtx<'_>) -> RuntimeResult<Value> {
    let (current, param, value, vars, interner) = ctx.closure()?;
    let mut groups = Map::new();
    let mut scoped = ScopedVars::with_capacity(vars, 1);
    for item in take_array(current, "group_by")? {
        scoped.set(param, item.clone());
        let key = as_string(eval_expr(value, &scoped, interner)?);
        groups
            .entry(key)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("group bucket should be array")
            .push(item);
    }
    Ok(Value::Object(groups))
}

fn pipe_sum(ctx: PipeRuntimeCtx<'_>) -> RuntimeResult<Value> {
    let (current, param, value, vars, interner) = ctx.closure()?;
    Ok(json!(
        numeric_stats(take_array(current, "sum")?, param, value, vars, interner)?.sum
    ))
}

fn pipe_avg(ctx: PipeRuntimeCtx<'_>) -> RuntimeResult<Value> {
    let (current, param, value, vars, interner) = ctx.closure()?;
    let stats = numeric_stats(take_array(current, "avg")?, param, value, vars, interner)?;
    Ok(json!(if stats.count == 0 {
        0.0
    } else {
        stats.sum / stats.count as f64
    }))
}

fn pipe_min(ctx: PipeRuntimeCtx<'_>) -> RuntimeResult<Value> {
    let (current, param, value, vars, interner) = ctx.closure()?;
    Ok(
        numeric_stats(take_array(current, "min")?, param, value, vars, interner)?
            .min
            .map_or(Value::Null, |value| json!(value)),
    )
}

fn pipe_max(ctx: PipeRuntimeCtx<'_>) -> RuntimeResult<Value> {
    let (current, param, value, vars, interner) = ctx.closure()?;
    Ok(
        numeric_stats(take_array(current, "max")?, param, value, vars, interner)?
            .max
            .map_or(Value::Null, |value| json!(value)),
    )
}

fn pipe_unique(ctx: PipeRuntimeCtx<'_>) -> RuntimeResult<Value> {
    let (current, param, value, vars, interner) = ctx.closure()?;
    let mut seen = BTreeSet::<String>::new();
    let mut unique = Vec::new();
    let mut scoped = ScopedVars::with_capacity(vars, 1);
    for item in take_array(current, "unique")? {
        scoped.set(param, item.clone());
        let key = eval_expr(value, &scoped, interner)?;
        let key = serde_json::to_string(&key).map_err(|err| {
            RuntimeError::Execution(format!("failed to serialize unique key: {err}"))
        })?;
        if seen.insert(key) {
            unique.push(item);
        }
    }
    Ok(Value::Array(unique))
}

fn pipe_flat_map(ctx: PipeRuntimeCtx<'_>) -> RuntimeResult<Value> {
    let (current, param, value, vars, interner) = ctx.closure()?;
    let mut flattened = Vec::new();
    let mut scoped = ScopedVars::with_capacity(vars, 1);
    for item in take_array(current, "flat_map")? {
        scoped.set(param, item);
        match eval_expr(value, &scoped, interner)? {
            Value::Array(items) => flattened.extend(items),
            value => flattened.push(value),
        }
    }
    Ok(Value::Array(flattened))
}

fn pipe_limit(ctx: PipeRuntimeCtx<'_>) -> RuntimeResult<Value> {
    let (name, current, value, vars, interner) = ctx.expr()?;
    let mut items = take_array(current, name)?;
    let count = as_usize(eval_expr(value, vars, interner)?)?;
    items.truncate(count);
    Ok(Value::Array(items))
}

fn pipe_offset(ctx: PipeRuntimeCtx<'_>) -> RuntimeResult<Value> {
    let (_, current, value, vars, interner) = ctx.expr()?;
    let items = take_array(current, "offset")?;
    let count = as_usize(eval_expr(value, vars, interner)?)?;
    Ok(Value::Array(items.into_iter().skip(count).collect()))
}

fn pipe_count(ctx: PipeRuntimeCtx<'_>) -> RuntimeResult<Value> {
    let current = ctx.none()?;
    Ok(json!(take_array(current, "count")?.len()))
}

fn pipe_first(ctx: PipeRuntimeCtx<'_>) -> RuntimeResult<Value> {
    let current = ctx.none()?;
    Ok(take_array(current, "first")?
        .into_iter()
        .next()
        .unwrap_or(Value::Null))
}

fn pipe_last(ctx: PipeRuntimeCtx<'_>) -> RuntimeResult<Value> {
    let current = ctx.none()?;
    Ok(take_array(current, "last")?
        .into_iter()
        .last()
        .unwrap_or(Value::Null))
}

fn pipe_reduce(ctx: PipeRuntimeCtx<'_>) -> RuntimeResult<Value> {
    let (current, initial, acc, param, value, vars, interner) = ctx.reduce()?;
    let mut acc_value = eval_expr(initial, vars, interner)?;
    let mut scoped = ScopedVars::with_capacity(vars, 2);
    for item in take_array(current, "reduce")? {
        scoped.set(acc, acc_value);
        scoped.set(param, item);
        acc_value = eval_expr(value, &scoped, interner)?;
    }
    Ok(acc_value)
}

struct NumericStats {
    count: usize,
    sum: f64,
    min: Option<f64>,
    max: Option<f64>,
}

fn numeric_stats(
    items: Vec<Value>,
    param: Sym,
    expr: &Expression,
    vars: &Vars,
    interner: &Rodeo,
) -> RuntimeResult<NumericStats> {
    let mut stats = NumericStats {
        count: 0,
        sum: 0.0,
        min: None,
        max: None,
    };
    let mut scoped = ScopedVars::with_capacity(vars, 1);
    for item in items {
        scoped.set(param, item);
        let value = as_f64(&eval_expr(expr, &scoped, interner)?)?;
        stats.count += 1;
        stats.sum += value;
        stats.min = Some(stats.min.map_or(value, |min| min.min(value)));
        stats.max = Some(stats.max.map_or(value, |max| max.max(value)));
    }
    Ok(stats)
}

fn compare_json(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left
            .as_f64()
            .partial_cmp(&right.as_f64())
            .unwrap_or(Ordering::Equal),
        (Value::String(left), Value::String(right)) => left.cmp(right),
        (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
        _ => as_string(left.clone()).cmp(&as_string(right.clone())),
    }
}

fn eval_response(
    endpoint: &Endpoint,
    vars: &Vars,
    interner: &Rodeo,
) -> RuntimeResult<EvaluatedResponse> {
    let body = endpoint
        .response
        .body
        .as_ref()
        .map(|fields| {
            let mut body = Map::new();
            for (key, expr) in fields {
                body.insert(key.clone(), eval_expr(expr, vars, interner)?);
            }
            RuntimeResult::Ok(Value::Object(body))
        })
        .transpose()?;
    let mut headers = HeaderMap::new();
    for (name, expr) in &endpoint.response.headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|err| {
            RuntimeError::BadRequest(format!("invalid response header `{name}`: {err}"))
        })?;
        let value = as_string(eval_expr(expr, vars, interner)?);
        let value = HeaderValue::from_str(&value).map_err(|err| {
            RuntimeError::BadRequest(format!("invalid value for response header `{name}`: {err}"))
        })?;
        headers.insert(name, value);
    }

    let mut cookies = Vec::with_capacity(endpoint.response.cookies.len());
    for (name, expr) in &endpoint.response.cookies {
        let value = as_string(eval_expr(expr, vars, interner)?);
        cookies.push(format!("{name}={value}; Path=/"));
    }

    Ok(EvaluatedResponse {
        body,
        headers,
        cookies,
    })
}

fn error_status(error: &RuntimeError) -> StatusCode {
    match error {
        RuntimeError::BadRequest(_) => StatusCode::BAD_REQUEST,
        RuntimeError::Upstream(_) | RuntimeError::Grpc(_) => StatusCode::BAD_GATEWAY,
        RuntimeError::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
        RuntimeError::InvalidMethod { .. }
        | RuntimeError::InvalidBindAddress(_)
        | RuntimeError::Bind(_)
        | RuntimeError::Serve(_)
        | RuntimeError::Config(_)
        | RuntimeError::Database(_)
        | RuntimeError::Execution(_)
        | RuntimeError::RouteConflict(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn error_code(error: &RuntimeError) -> &'static str {
    match error {
        RuntimeError::BadRequest(_) => "bad_request",
        RuntimeError::Upstream(_) => "upstream_error",
        RuntimeError::Timeout(_) => "timeout",
        RuntimeError::Database(_) => "database_error",
        RuntimeError::Grpc(_) => "grpc_error",
        RuntimeError::Config(_) => "config_error",
        RuntimeError::InvalidMethod { .. }
        | RuntimeError::InvalidBindAddress(_)
        | RuntimeError::Bind(_)
        | RuntimeError::Serve(_)
        | RuntimeError::Execution(_)
        | RuntimeError::RouteConflict(_) => "runtime_error",
    }
}

fn public_error_message(error: &RuntimeError) -> &'static str {
    match error {
        RuntimeError::BadRequest(_) => "request could not be processed",
        RuntimeError::Upstream(_) => "upstream service failed",
        RuntimeError::Timeout(_) => "operation timed out",
        RuntimeError::Database(_) => "database operation failed",
        RuntimeError::Grpc(_) => "grpc service failed",
        RuntimeError::InvalidMethod { .. }
        | RuntimeError::InvalidBindAddress(_)
        | RuntimeError::Bind(_)
        | RuntimeError::Serve(_)
        | RuntimeError::Config(_)
        | RuntimeError::Execution(_)
        | RuntimeError::RouteConflict(_) => "internal server error",
    }
}

trait VarLookup {
    fn get_var(&self, name: Sym) -> Option<&Value>;
}

impl VarLookup for Vars {
    fn get_var(&self, name: Sym) -> Option<&Value> {
        self.get(&name)
    }
}

struct ScopedVars<'a> {
    base: &'a Vars,
    bindings: Vec<(Sym, Value)>,
}

impl<'a> ScopedVars<'a> {
    fn with_capacity(base: &'a Vars, capacity: usize) -> Self {
        Self {
            base,
            bindings: Vec::with_capacity(capacity),
        }
    }

    fn set(&mut self, name: Sym, value: Value) {
        if let Some((_, existing)) = self
            .bindings
            .iter_mut()
            .rev()
            .find(|(binding_name, _)| *binding_name == name)
        {
            *existing = value;
        } else {
            self.bindings.push((name, value));
        }
    }
}

impl VarLookup for ScopedVars<'_> {
    fn get_var(&self, name: Sym) -> Option<&Value> {
        self.bindings
            .iter()
            .rev()
            .find_map(|(binding_name, value)| (*binding_name == name).then_some(value))
            .or_else(|| self.base.get(&name))
    }
}

fn eval_expr<V: VarLookup + ?Sized>(
    expr: &Expression,
    vars: &V,
    interner: &Rodeo,
) -> RuntimeResult<Value> {
    match expr {
        Expression::Null => Ok(Value::Null),
        Expression::Variable(name) => vars.get_var(*name).cloned().ok_or_else(|| {
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
    vars: &(impl VarLookup + ?Sized),
    interner: &Rodeo,
) -> RuntimeResult<Value> {
    match callee {
        Expression::PropertyAccess(object, method) => {
            let method = sym(interner, *method);
            let value = eval_expr(object, vars, interner)?;
            let args = eval_args(args, vars, interner)?;
            eval_method_call(value, method, args)
        }
        Expression::Variable(function) => {
            let function = sym(interner, *function);
            let args = eval_args(args, vars, interner)?;
            eval_builtin_call(function, args)
        }
        _ => Err(RuntimeError::Execution(
            "only built-in function and method calls are supported inside expressions".to_string(),
        )),
    }
}

fn eval_args(
    args: &[Expression],
    vars: &(impl VarLookup + ?Sized),
    interner: &Rodeo,
) -> RuntimeResult<Vec<Value>> {
    args.iter()
        .map(|arg| eval_expr(arg, vars, interner))
        .collect()
}

fn eval_builtin_call(function: &str, args: Vec<Value>) -> RuntimeResult<Value> {
    match function {
        "len" => {
            require_arg_count(function, &args, 1)?;
            value_len(&args[0])
        }
        "contains" => {
            require_arg_count(function, &args, 2)?;
            contains_value(&args[0], &args[1]).map(Value::Bool)
        }
        "starts_with" => {
            require_arg_count(function, &args, 2)?;
            Ok(Value::Bool(
                as_string_ref(&args[0]).starts_with(&*as_string_ref(&args[1])),
            ))
        }
        "ends_with" => {
            require_arg_count(function, &args, 2)?;
            Ok(Value::Bool(
                as_string_ref(&args[0]).ends_with(&*as_string_ref(&args[1])),
            ))
        }
        "lower" => {
            require_arg_count(function, &args, 1)?;
            Ok(Value::String(as_string_ref(&args[0]).to_lowercase()))
        }
        "upper" => {
            require_arg_count(function, &args, 1)?;
            Ok(Value::String(as_string_ref(&args[0]).to_uppercase()))
        }
        "trim" => {
            require_arg_count(function, &args, 1)?;
            Ok(Value::String(as_string_ref(&args[0]).trim().to_string()))
        }
        "replace" => {
            require_arg_count(function, &args, 3)?;
            Ok(Value::String(as_string_ref(&args[0]).replace(
                &*as_string_ref(&args[1]),
                &as_string_ref(&args[2]),
            )))
        }
        "split" => {
            require_arg_count(function, &args, 2)?;
            Ok(Value::Array(
                as_string_ref(&args[0])
                    .split(&*as_string_ref(&args[1]))
                    .map(|item| Value::String(item.to_string()))
                    .collect(),
            ))
        }
        "join" => {
            require_arg_count(function, &args, 2)?;
            join_array(&args[0], &as_string_ref(&args[1]))
        }
        "format" => format_string(&args),
        "string" => {
            require_arg_count(function, &args, 1)?;
            Ok(Value::String(as_string_ref(&args[0]).into_owned()))
        }
        "number" => {
            require_arg_count(function, &args, 1)?;
            Ok(json!(as_f64(&args[0])?))
        }
        "bool" => {
            require_arg_count(function, &args, 1)?;
            Ok(Value::Bool(truthy(&args[0])))
        }
        "is_null" => {
            require_arg_count(function, &args, 1)?;
            Ok(Value::Bool(args[0].is_null()))
        }
        "is_empty" => {
            require_arg_count(function, &args, 1)?;
            Ok(Value::Bool(is_empty(&args[0])))
        }
        _ => Err(RuntimeError::Execution(format!(
            "unsupported built-in function `{function}`"
        ))),
    }
}

fn eval_method_call(receiver: Value, method: &str, args: Vec<Value>) -> RuntimeResult<Value> {
    match method {
        "len" => {
            require_arg_count(method, &args, 0)?;
            value_len(&receiver)
        }
        "contains" => {
            require_arg_count(method, &args, 1)?;
            contains_value(&receiver, &args[0]).map(Value::Bool)
        }
        "starts_with" => {
            require_arg_count(method, &args, 1)?;
            Ok(Value::Bool(
                as_string(receiver).starts_with(&*as_string_ref(&args[0])),
            ))
        }
        "ends_with" => {
            require_arg_count(method, &args, 1)?;
            Ok(Value::Bool(
                as_string(receiver).ends_with(&*as_string_ref(&args[0])),
            ))
        }
        "lower" => {
            require_arg_count(method, &args, 0)?;
            Ok(Value::String(as_string(receiver).to_lowercase()))
        }
        "upper" => {
            require_arg_count(method, &args, 0)?;
            Ok(Value::String(as_string(receiver).to_uppercase()))
        }
        "trim" => {
            require_arg_count(method, &args, 0)?;
            Ok(Value::String(as_string(receiver).trim().to_string()))
        }
        "replace" => {
            require_arg_count(method, &args, 2)?;
            Ok(Value::String(as_string(receiver).replace(
                &*as_string_ref(&args[0]),
                &as_string_ref(&args[1]),
            )))
        }
        "split" => {
            require_arg_count(method, &args, 1)?;
            Ok(Value::Array(
                as_string(receiver)
                    .split(&*as_string_ref(&args[0]))
                    .map(|item| Value::String(item.to_string()))
                    .collect(),
            ))
        }
        "join" => {
            require_arg_count(method, &args, 1)?;
            join_array(&receiver, &as_string_ref(&args[0]))
        }
        _ => Err(RuntimeError::Execution(format!(
            "unsupported method `{method}` for {}",
            value_type(&receiver)
        ))),
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

fn require_arg_count(name: &str, args: &[Value], expected: usize) -> RuntimeResult<()> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(RuntimeError::Execution(format!(
            "`{name}` expects {expected} argument(s), got {}",
            args.len()
        )))
    }
}

fn value_len(value: &Value) -> RuntimeResult<Value> {
    match value {
        Value::Array(items) => Ok(json!(items.len())),
        Value::Object(object) => Ok(json!(object.len())),
        Value::String(value) => Ok(json!(value.chars().count())),
        other => Err(RuntimeError::Execution(format!(
            "len() is not supported for {}",
            value_type(other)
        ))),
    }
}

fn contains_value(container: &Value, needle: &Value) -> RuntimeResult<bool> {
    match container {
        Value::String(value) => Ok(value.contains(&*as_string_ref(needle))),
        Value::Array(items) => Ok(items.iter().any(|item| item == needle)),
        Value::Object(object) => Ok(object.contains_key(&*as_string_ref(needle))),
        other => Err(RuntimeError::Execution(format!(
            "contains() is not supported for {}",
            value_type(other)
        ))),
    }
}

fn join_array(value: &Value, separator: &str) -> RuntimeResult<Value> {
    let Value::Array(items) = value else {
        return Err(RuntimeError::Execution(format!(
            "join() expects array, got {}",
            value_type(value)
        )));
    };
    Ok(Value::String(
        items
            .iter()
            .cloned()
            .map(as_string)
            .collect::<Vec<_>>()
            .join(separator),
    ))
}

fn format_string(args: &[Value]) -> RuntimeResult<Value> {
    if args.is_empty() {
        return Err(RuntimeError::Execution(
            "`format` expects at least 1 argument".to_string(),
        ));
    }

    let mut output = as_string(args[0].clone());
    for value in &args[1..] {
        let replacement = as_string_ref(value);
        if output.contains("{}") {
            output = output.replacen("{}", &replacement, 1);
        } else {
            output.push_str(&replacement);
        }
    }
    Ok(Value::String(output))
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(value) => value.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(object) => object.is_empty(),
        Value::Bool(_) | Value::Number(_) => false,
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

fn as_usize(value: Value) -> RuntimeResult<usize> {
    match value {
        Value::Number(value) => {
            if let Some(value) = value.as_u64().and_then(|value| usize::try_from(value).ok()) {
                return Ok(value);
            }
            let Some(value) = value.as_f64() else {
                return Err(RuntimeError::Execution(
                    "take() expects a non-negative integer".to_string(),
                ));
            };
            if value >= 0.0 && value.fract() == 0.0 && value <= usize::MAX as f64 {
                Ok(value as usize)
            } else {
                Err(RuntimeError::Execution(
                    "take() expects a non-negative integer".to_string(),
                ))
            }
        }
        other => Err(RuntimeError::Execution(format!(
            "take() expects number, got {}",
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

fn as_string_ref(value: &Value) -> Cow<'_, str> {
    match value {
        Value::String(value) => Cow::Borrowed(value),
        Value::Number(value) => Cow::Owned(value.to_string()),
        Value::Bool(value) => Cow::Owned(value.to_string()),
        Value::Null => Cow::Borrowed("null"),
        other => Cow::Owned(other.to_string()),
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

#[cfg(test)]
mod tests;
