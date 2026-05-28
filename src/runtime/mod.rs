use crate::ast::{
    BinaryOperator, DbQueryConfig, Endpoint, Expression, FileAST, GrpcConfig, HttpConfig, PipeOp,
    Step,
};
use crate::planner::ExecutionPlan;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query};
use axum::http::header::{HeaderName, HeaderValue, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::on;
use axum::{Json, Router};
use lasso::Rodeo;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor};
use prost_types::{ListValue, Struct, Value as ProstValue, value::Kind};
use reqwest::{Client, Method};
use serde_json::{Map, json};
use sqlx::any::{AnyPoolOptions, install_default_drivers};
use sqlx::{AnyPool, Column, Row, TypeInfo};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tonic::client::Grpc;
use tonic::transport::{Channel, Endpoint as TonicEndpoint};
use tonic_prost::ProstCodec;
use tracing::{debug, error, info, warn};

mod functions;
mod rate_limit;
mod security;
mod traits;
mod types;

use functions::{
    axum_route_path, db_urls, gateway_vars, insert_value_var, method_filter, proto_paths,
    request_body_value, request_vars, sym,
};
use rate_limit::{VelogateRateLimitLayer, endpoint_rate_limit_policy};
use security::authorize_endpoint;
use types::{
    DynamicGrpcCodec, EndpointRuntime, GrpcRequest, RuntimeResult, SQLX_DRIVERS, StepRuntimeDeps,
    Value, Vars,
};
pub use types::{Runtime, RuntimeError};

struct EvaluatedResponse {
    body: Option<Value>,
    headers: HeaderMap,
    cookies: Vec<String>,
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

        info!(%addr, "velogate listening");
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .map_err(RuntimeError::Serve)
    }

    pub fn router(&self) -> RuntimeResult<Router> {
        let mut router = Router::new();
        let static_vars = gateway_vars(&self.ast, &self.interner)?;

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
                static_vars: static_vars.clone(),
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

            if let Some(policy) = endpoint_rate_limit_policy(endpoint) {
                method_router = method_router.layer(VelogateRateLimitLayer::new(policy));
            }

            router = router.route(&axum_route_path(&endpoint.path), method_router);
        }

        Ok(router)
    }
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
        debug!(%endpoint, "handling request");
        let mut vars = self.static_vars.clone();
        vars.extend(request_vars(
            &path_params,
            &query_params,
            &headers,
            body,
            &self.interner,
        ));

        if let Err(err) = authorize_endpoint(&self.endpoint, &mut vars, &headers, &self.interner) {
            warn!(%endpoint, message = %err.message, "request rejected by security rule");
            let body = json!({
                "error": "unauthorized",
                "message": err.message,
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
                let status = error_status(&err);
                error!(%endpoint, %status, error = %err, "request failed");
                let body = json!({
                    "error": error_code(&err),
                    "message": err.to_string(),
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

                tasks.spawn(async move {
                    let var = step.var_name();
                    let deps = StepRuntimeDeps {
                        client: &client,
                        db_urls: &db_urls,
                        db_pools: &db_pools,
                        proto_paths: &proto_paths,
                        proto_pools: &proto_pools,
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
    let method_name = config.method.as_deref().unwrap_or("GET").to_uppercase();
    let method = Method::from_bytes(method_name.as_bytes()).map_err(|err| {
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
        .map_err(|err| RuntimeError::Database(format!("failed to connect to database: {err}")))?;
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
                let mut scoped = vars.clone();
                for item in items {
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
                let mut scoped = vars.clone();
                for item in items {
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
                let count = as_usize(eval_expr(count, vars, interner)?)?;
                items.truncate(count);
                current = Value::Array(items);
            }
        }
    }

    Ok(current)
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
        | RuntimeError::Execution(_) => StatusCode::INTERNAL_SERVER_ERROR,
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
        | RuntimeError::Execution(_) => "runtime_error",
    }
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

fn eval_args(args: &[Expression], vars: &Vars, interner: &Rodeo) -> RuntimeResult<Vec<Value>> {
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
