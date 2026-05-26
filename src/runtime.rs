use crate::ast::{BinaryOperator, Endpoint, Expression, FileAST, HttpConfig, PipeOp, Step, Sym};
use crate::planner::{EndpointPlan, ExecutionPlan};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodFilter, on};
use axum::{Json, Router};
use lasso::Rodeo;
use reqwest::Client;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;

#[derive(Clone)]
pub struct Runtime {
    ast: Arc<FileAST>,
    interner: Arc<Rodeo>,
    plan: Arc<ExecutionPlan>,
    client: Client,
}

#[derive(Debug)]
pub enum RuntimeError {
    InvalidMethod { method: String, path: String },
    InvalidBindAddress(String),
    Bind(std::io::Error),
    Serve(std::io::Error),
    Execution(String),
}

type Vars = HashMap<Sym, Value>;
type RuntimeResult<T> = Result<T, RuntimeError>;

impl Runtime {
    pub fn new(ast: FileAST, interner: Rodeo, plan: ExecutionPlan) -> Self {
        Self {
            ast: Arc::new(ast),
            interner: Arc::new(interner),
            plan: Arc::new(plan),
            client: Client::new(),
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
            });
            let method =
                method_filter(&endpoint.method).ok_or_else(|| RuntimeError::InvalidMethod {
                    method: endpoint.method.clone(),
                    path: endpoint.path.clone(),
                })?;
            let handler_runtime = Arc::clone(&endpoint_runtime);

            router = router.route(
                &endpoint.path,
                on(method, move || {
                    let runtime = Arc::clone(&handler_runtime);
                    async move { runtime.handle().await }
                }),
            );
        }

        Ok(router)
    }
}

struct EndpointRuntime {
    endpoint: Endpoint,
    plan: EndpointPlan,
    interner: Arc<Rodeo>,
    client: Client,
    static_dbs: Vars,
}

impl EndpointRuntime {
    async fn handle(self: Arc<Self>) -> Response {
        match self.execute().await {
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

    async fn execute(&self) -> RuntimeResult<Value> {
        let mut vars = self.static_dbs.clone();

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

                tasks.spawn(async move {
                    let var = step_var(&step);
                    let value = execute_step(&step, &ctx, &interner, &client).await?;
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
    client: &Client,
) -> RuntimeResult<Value> {
    match step {
        Step::Let { value, .. } => eval_expr(value, vars, interner),
        Step::FetchHttp { config, .. } => fetch_http(config, vars, interner, client).await,
        Step::Pipe {
            source, operations, ..
        } => execute_pipe(source, operations, vars, interner),
        Step::CallGrpc { config, .. } => config
            .fallback
            .as_ref()
            .map(|fallback| eval_expr(fallback, vars, interner))
            .unwrap_or_else(|| {
                Err(RuntimeError::Execution(
                    "grpc calls are not implemented in this runtime yet".to_string(),
                ))
            }),
        Step::QueryDb { config, .. } => config
            .fallback
            .as_ref()
            .map(|fallback| eval_expr(fallback, vars, interner))
            .unwrap_or_else(|| {
                Err(RuntimeError::Execution(
                    "database queries are not implemented in this runtime yet".to_string(),
                ))
            }),
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
        Expression::PropertyAccess(object, method)
            if matches!(&**object, Expression::Variable(name) if sym(interner, *name) == "db")
                && sym(interner, *method) == "query" =>
        {
            Ok(json!({
                "unsupported": "db::query",
                "args": args
                    .iter()
                    .map(|arg| eval_expr(arg, vars, interner))
                    .collect::<RuntimeResult<Vec<_>>>()?,
            }))
        }
        _ => Err(RuntimeError::Execution(
            "only len() and db::query(...) calls are supported".to_string(),
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

fn step_var(step: &Step) -> Sym {
    match step {
        Step::Let { var_name, .. }
        | Step::FetchHttp { var_name, .. }
        | Step::CallGrpc { var_name, .. }
        | Step::QueryDb { var_name, .. }
        | Step::Pipe { var_name, .. } => *var_name,
    }
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
}
