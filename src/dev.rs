use crate::ast::FileAST;
use crate::export::export_file;
use crate::parser::Parser;
use crate::planner::{ExecutionPlan, build_plan};
use crate::runtime::{Runtime, RuntimeOptions};
use crate::validator::validate_file;
use axum::body::Body;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use lasso::Rodeo;
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

#[derive(Debug)]
pub struct DevConfig {
    pub config_path: PathBuf,
    pub env_file: Option<PathBuf>,
    pub ui_port: Option<u16>,
    pub runtime_options: RuntimeOptions,
}

#[derive(RustEmbed)]
#[folder = "editor/dist/"]
struct EditorAssets;

type SharedRunner = Arc<Mutex<ProjectRunner>>;

pub async fn run_dev_server(config: DevConfig) -> Result<(), String> {
    let runner = Arc::new(Mutex::new(ProjectRunner::new(
        config.config_path,
        config.env_file,
        config.runtime_options,
    )));
    runner.lock().await.start().await?;

    let app = Router::new()
        .route("/api/editor/state", get(editor_state))
        .route("/api/editor/ws", get(editor_ws))
        .route("/api/editor/control", post(editor_control))
        .route("/api/editor/config", put(editor_config))
        .fallback(static_asset)
        .with_state(Arc::clone(&runner));

    let port = config.ui_port.unwrap_or(0);
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|err| format!("invalid editor bind address: {err}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|err| format!("failed to bind editor UI: {err}"))?;
    let actual_addr = listener
        .local_addr()
        .map_err(|err| format!("failed to read editor UI address: {err}"))?;

    println!("velogate runtime started from editor control");
    println!("visual editor: http://{actual_addr}");

    axum::serve(listener, app)
        .await
        .map_err(|err| format!("editor UI failed: {err}"))
}

async fn editor_state(State(runner): State<SharedRunner>) -> impl IntoResponse {
    json_result(runner.lock().await.state().await)
}

async fn editor_ws(State(runner): State<SharedRunner>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| editor_socket(socket, runner))
}

async fn editor_socket(mut socket: WebSocket, runner: SharedRunner) {
    while let Some(message) = socket.recv().await {
        let Ok(message) = message else {
            break;
        };
        let Message::Text(text) = message else {
            continue;
        };

        let reply = match serde_json::from_str::<EditorWsMessage>(&text) {
            Ok(EditorWsMessage::EndpointAdd { method, path }) => {
                match runner.lock().await.add_endpoint(method, path).await {
                    Ok(state) => json!({ "kind": "endpoint_added", "state": state }),
                    Err(error) => json!({ "kind": "error", "error": error }),
                }
            }
            Ok(EditorWsMessage::EndpointUpdate {
                endpoint_index,
                method,
                path,
            }) => match runner
                .lock()
                .await
                .update_endpoint(endpoint_index, method, path)
                .await
            {
                Ok(state) => json!({ "kind": "endpoint_updated", "state": state }),
                Err(error) => json!({ "kind": "error", "error": error }),
            },
            Ok(EditorWsMessage::EndpointGraphSave {
                endpoint_index,
                endpoint_source,
            }) => match runner
                .lock()
                .await
                .save_endpoint_graph(endpoint_index, endpoint_source)
                .await
            {
                Ok(state) => json!({ "kind": "endpoint_graph_saved", "state": state }),
                Err(error) => json!({ "kind": "error", "error": error }),
            },
            Ok(EditorWsMessage::EndpointOptionsUpdate {
                endpoint_index,
                options_source,
            }) => match runner
                .lock()
                .await
                .update_endpoint_options(endpoint_index, options_source)
                .await
            {
                Ok(state) => json!({ "kind": "endpoint_options_updated", "state": state }),
                Err(error) => json!({ "kind": "error", "error": error }),
            },
            Ok(EditorWsMessage::GatewayUpdate {
                gateway_name,
                gateway_source,
            }) => match runner
                .lock()
                .await
                .update_gateway(gateway_name, gateway_source)
                .await
            {
                Ok(state) => json!({ "kind": "gateway_updated", "state": state }),
                Err(error) => json!({ "kind": "error", "error": error }),
            },
            Err(error) => {
                json!({ "kind": "error", "error": format!("invalid websocket message: {error}") })
            }
        };

        if socket
            .send(Message::Text(reply.to_string().into()))
            .await
            .is_err()
        {
            break;
        }
    }
}

async fn editor_control(
    State(runner): State<SharedRunner>,
    Json(payload): Json<ControlRequest>,
) -> impl IntoResponse {
    let mut runner = runner.lock().await;
    let result = match payload.action.as_str() {
        "start" => runner.start().await,
        "stop" => runner.stop().await,
        "restart" => runner.restart().await,
        other => Err(format!("unknown control action `{other}`")),
    };

    match result {
        Ok(()) => json_result(runner.state().await),
        Err(message) => json_error(StatusCode::BAD_REQUEST, message),
    }
}

async fn editor_config(
    State(runner): State<SharedRunner>,
    Json(payload): Json<SaveConfigRequest>,
) -> impl IntoResponse {
    let mut runner = runner.lock().await;
    match runner.save_source(payload.source).await {
        Ok(()) => json_result(runner.state().await),
        Err(message) => json_error(StatusCode::BAD_REQUEST, message),
    }
}

async fn static_asset(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let asset = EditorAssets::get(path).or_else(|| EditorAssets::get("index.html"));

    match asset {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .body(Body::from(content.data.into_owned()))
                .expect("static asset response should be valid")
        }
        None => (StatusCode::NOT_FOUND, "editor asset not found").into_response(),
    }
}

fn json_result<T: Serialize>(value: T) -> Response {
    (StatusCode::OK, Json(value)).into_response()
}

fn json_error(status: StatusCode, message: String) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

#[derive(Debug)]
struct ProjectRunner {
    config_path: PathBuf,
    env_file: Option<PathBuf>,
    runtime_options: RuntimeOptions,
    running: Option<RunningRuntime>,
    last_error: Arc<Mutex<Option<String>>>,
}

#[derive(Debug)]
struct RunningRuntime {
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl ProjectRunner {
    fn new(
        config_path: PathBuf,
        env_file: Option<PathBuf>,
        runtime_options: RuntimeOptions,
    ) -> Self {
        Self {
            config_path,
            env_file,
            runtime_options,
            running: None,
            last_error: Arc::new(Mutex::new(None)),
        }
    }

    async fn start(&mut self) -> Result<(), String> {
        if self
            .running
            .as_ref()
            .is_some_and(|runtime| !runtime.task.is_finished())
        {
            return Ok(());
        }

        let parsed = parse_config(&self.config_path, self.env_file.as_deref())?;
        let runtime = Runtime::with_options(
            parsed.ast,
            parsed.interner,
            parsed.plan,
            self.runtime_options.clone(),
        );
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let last_error = Arc::clone(&self.last_error);

        *self.last_error.lock().await = None;
        let task = tokio::spawn(async move {
            let result = runtime
                .serve_with_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
            if let Err(err) = result {
                *last_error.lock().await = Some(err.to_string());
            }
        });

        self.running = Some(RunningRuntime {
            shutdown: Some(shutdown_tx),
            task,
        });
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), String> {
        let Some(mut runtime) = self.running.take() else {
            return Ok(());
        };

        if let Some(shutdown) = runtime.shutdown.take() {
            let _ = shutdown.send(());
        }
        runtime
            .task
            .await
            .map_err(|err| format!("runtime task failed: {err}"))?;
        Ok(())
    }

    async fn restart(&mut self) -> Result<(), String> {
        self.stop().await?;
        self.start().await
    }

    async fn save_source(&mut self, source: String) -> Result<(), String> {
        parse_source(&source, &self.config_path, self.env_file.as_deref())?;
        let source = format_gate_source(&source);
        parse_source(&source, &self.config_path, self.env_file.as_deref())?;
        fs::write(&self.config_path, source)
            .map_err(|err| format!("failed to write {}: {err}", self.config_path.display()))?;
        if self
            .running
            .as_ref()
            .is_some_and(|runtime| !runtime.task.is_finished())
        {
            self.restart().await?;
        }
        Ok(())
    }

    async fn state(&self) -> DevState {
        let model = load_model(&self.config_path, self.env_file.as_deref());
        DevState {
            config_path: self.config_path.display().to_string(),
            runtime_running: self
                .running
                .as_ref()
                .is_some_and(|runtime| !runtime.task.is_finished()),
            last_error: self.last_error.lock().await.clone(),
            model,
        }
    }

    async fn add_endpoint(&mut self, method: String, path: String) -> Result<DevState, String> {
        let method = normalize_method(&method)?;
        let path = normalize_path(&path)?;
        let source = fs::read_to_string(&self.config_path)
            .map_err(|err| format!("failed to read {}: {err}", self.config_path.display()))?;
        let endpoint = format!("\nendpoint \"{method} {path}\" {{\n    respond 200 {{}}\n}}\n");
        let next = format!("{}{}", source.trim_end(), endpoint);
        self.save_source(next).await?;
        Ok(self.state().await)
    }

    async fn update_endpoint(
        &mut self,
        endpoint_index: usize,
        method: String,
        path: String,
    ) -> Result<DevState, String> {
        let method = normalize_method(&method)?;
        let path = normalize_path(&path)?;
        let source = fs::read_to_string(&self.config_path)
            .map_err(|err| format!("failed to read {}: {err}", self.config_path.display()))?;
        let parsed = parse_source(&source, &self.config_path, self.env_file.as_deref())?;
        let Some(endpoint) = parsed.ast.endpoints.get(endpoint_index) else {
            return Err(format!("endpoint index {endpoint_index} is out of range"));
        };
        let old_header = format!("endpoint \"{} {}\"", endpoint.method, endpoint.path);
        let new_header = format!("endpoint \"{method} {path}\"");
        let Some(position) = source.find(&old_header) else {
            return Err(format!("failed to find endpoint header `{old_header}`"));
        };
        let mut next = source;
        next.replace_range(position..position + old_header.len(), &new_header);
        self.save_source(next).await?;
        Ok(self.state().await)
    }

    async fn save_endpoint_graph(
        &mut self,
        endpoint_index: usize,
        endpoint_source: String,
    ) -> Result<DevState, String> {
        let source = fs::read_to_string(&self.config_path)
            .map_err(|err| format!("failed to read {}: {err}", self.config_path.display()))?;
        parse_source(&source, &self.config_path, self.env_file.as_deref())?;
        let replacement = format!("\n{}\n", trim_outer_newlines(&endpoint_source));
        let range = endpoint_content_range(&source, endpoint_index)?;
        let mut next = source;
        next.replace_range(range, &replacement);
        self.save_source(next).await?;
        Ok(self.state().await)
    }

    async fn update_endpoint_options(
        &mut self,
        endpoint_index: usize,
        options_source: String,
    ) -> Result<DevState, String> {
        let source = fs::read_to_string(&self.config_path)
            .map_err(|err| format!("failed to read {}: {err}", self.config_path.display()))?;
        parse_source(&source, &self.config_path, self.env_file.as_deref())?;
        let replacement = if options_source.trim().is_empty() {
            "\n".to_string()
        } else {
            format!("\n{}\n", trim_outer_newlines(&options_source))
        };
        let range = endpoint_options_range(&source, endpoint_index)?;
        let mut next = source;
        next.replace_range(range, &replacement);
        self.save_source(next).await?;
        Ok(self.state().await)
    }

    async fn update_gateway(
        &mut self,
        gateway_name: String,
        gateway_source: String,
    ) -> Result<DevState, String> {
        let gateway_name = normalize_gateway_name(&gateway_name)?;
        let source = fs::read_to_string(&self.config_path)
            .map_err(|err| format!("failed to read {}: {err}", self.config_path.display()))?;
        let parsed = parse_source(&source, &self.config_path, self.env_file.as_deref())?;
        let replacement = format!("\n{}\n", trim_outer_newlines(&gateway_source));
        let range = gateway_content_range(&source)?;
        let mut next = source;
        let old_header = format!("gateway {:?}", parsed.ast.gateway.name);
        let new_header = format!("gateway {gateway_name:?}");
        let position = next
            .find(&old_header)
            .ok_or_else(|| format!("failed to find gateway header `{old_header}`"))?;
        next.replace_range(range, &replacement);
        next.replace_range(position..position + old_header.len(), &new_header);
        self.save_source(next).await?;
        Ok(self.state().await)
    }
}

fn gateway_content_range(source: &str) -> Result<std::ops::Range<usize>, String> {
    let gateway_start = source
        .match_indices("gateway")
        .find_map(|(index, _)| {
            let before = source[..index].chars().next_back();
            let after = source[index + "gateway".len()..].chars().next();
            let valid_before = before.is_none_or(|ch| !is_ident_char(ch));
            let valid_after = after.is_some_and(char::is_whitespace);
            (valid_before && valid_after).then_some(index)
        })
        .ok_or_else(|| "failed to find gateway block".to_string())?;
    let open_brace = source[gateway_start..]
        .find('{')
        .map(|offset| gateway_start + offset)
        .ok_or_else(|| "failed to find gateway body".to_string())?;
    let close_brace = matching_brace(source, open_brace)
        .ok_or_else(|| "failed to find gateway closing brace".to_string())?;
    Ok(open_brace + 1..close_brace)
}

fn endpoint_content_range(
    source: &str,
    endpoint_index: usize,
) -> Result<std::ops::Range<usize>, String> {
    let endpoint_start = nth_endpoint_offset(source, endpoint_index)
        .ok_or_else(|| format!("endpoint index {endpoint_index} is out of range"))?;
    let open_brace = source[endpoint_start..]
        .find('{')
        .map(|offset| endpoint_start + offset)
        .ok_or_else(|| format!("failed to find endpoint {endpoint_index} body"))?;
    let close_brace = matching_brace(source, open_brace)
        .ok_or_else(|| format!("failed to find endpoint {endpoint_index} closing brace"))?;
    Ok(open_brace + 1..close_brace)
}

fn endpoint_options_range(
    source: &str,
    endpoint_index: usize,
) -> Result<std::ops::Range<usize>, String> {
    let content = endpoint_content_range(source, endpoint_index)?;
    let body = &source[content.clone()];
    let options_end = first_executable_line_offset(body)
        .map(|offset| content.start + offset)
        .unwrap_or(content.end);
    Ok(content.start..options_end)
}

fn nth_endpoint_offset(source: &str, endpoint_index: usize) -> Option<usize> {
    source
        .match_indices("endpoint")
        .filter_map(|(index, _)| {
            let before = source[..index].chars().next_back();
            let after = source[index + "endpoint".len()..].chars().next();
            let valid_before = before.is_none_or(|ch| !is_ident_char(ch));
            let valid_after = after.is_some_and(char::is_whitespace);
            (valid_before && valid_after).then_some(index)
        })
        .nth(endpoint_index)
}

fn first_executable_line_offset(body: &str) -> Option<usize> {
    let mut offset = 0;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("fetch ")
            || trimmed.starts_with("let ")
            || trimmed.starts_with("command ")
            || trimmed.starts_with("sync ")
            || trimmed.starts_with("respond")
        {
            return Some(offset + line.len() - trimmed.len());
        }
        offset += line.len();
    }
    None
}

fn matching_brace(source: &str, open_brace: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (index, ch) in source[open_brace..].char_indices() {
        let absolute = open_brace + index;
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(absolute);
                }
            }
            _ => {}
        }
    }

    None
}

fn is_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

#[derive(Debug)]
struct ParsedConfig {
    ast: FileAST,
    interner: Rodeo,
    plan: ExecutionPlan,
}

fn load_model(config_path: &Path, env_file: Option<&Path>) -> ModelState {
    let source = match fs::read_to_string(config_path) {
        Ok(source) => source,
        Err(err) => {
            return ModelState {
                source: String::new(),
                parsed: None,
                error: Some(format!("failed to read {}: {err}", config_path.display())),
            };
        }
    };

    match parse_source(&source, config_path, env_file) {
        Ok(parsed) => ModelState {
            source,
            parsed: Some(ParsedModel {
                file: json!(export_file(&parsed.ast, &parsed.interner)),
                plan: json!(parsed.plan),
            }),
            error: None,
        },
        Err(message) => ModelState {
            source,
            parsed: None,
            error: Some(message),
        },
    }
}

fn parse_config(config_path: &Path, env_file: Option<&Path>) -> Result<ParsedConfig, String> {
    let source = fs::read_to_string(config_path)
        .map_err(|err| format!("failed to read {}: {err}", config_path.display()))?;
    parse_source(&source, config_path, env_file)
}

fn normalize_method(method: &str) -> Result<String, String> {
    let method = method.trim().to_ascii_uppercase();
    match method.as_str() {
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS" => Ok(method),
        _ => Err(format!("unsupported endpoint method `{method}`")),
    }
}

fn normalize_path(path: &str) -> Result<String, String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("endpoint path must not be empty".to_string());
    }
    if !path.starts_with('/') {
        return Err("endpoint path must start with `/`".to_string());
    }
    if path.contains('"') {
        return Err("endpoint path must not contain quotes".to_string());
    }
    Ok(path.to_string())
}

fn normalize_gateway_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("gateway name must not be empty".to_string());
    }
    if name.contains('"') {
        return Err("gateway name must not contain quotes".to_string());
    }
    Ok(name.to_string())
}

fn parse_source(
    source: &str,
    config_path: &Path,
    env_file: Option<&Path>,
) -> Result<ParsedConfig, String> {
    let mut parser = Parser::new(Rodeo::new());
    let mut ast = parser
        .parse(source)
        .map_err(|diagnostic| format!("syntax error: {}", diagnostic.message))?;
    resolve_gateway_paths(config_path, &mut ast);
    if let Some(env_file) = env_file {
        ast.gateway.env_file = Some(
            resolve_config_path(config_path, env_file)
                .display()
                .to_string(),
        );
    }

    let validation_errors = validate_file(&ast, &parser.interner, config_path);
    if !validation_errors.is_empty() {
        return Err(validation_errors
            .iter()
            .map(|error| error.message.as_str())
            .collect::<Vec<_>>()
            .join("; "));
    }

    let plan = build_plan(&ast, &parser.interner).map_err(plan_error_message)?;
    Ok(ParsedConfig {
        ast,
        interner: parser.interner,
        plan,
    })
}

fn resolve_gateway_paths(config_path: &Path, ast: &mut FileAST) {
    if let Some(env_file) = ast.gateway.env_file.as_deref() {
        let path = PathBuf::from(env_file);
        if path.is_relative() {
            ast.gateway.env_file = Some(
                resolve_config_path(config_path, &path)
                    .display()
                    .to_string(),
            );
        }
    }

    for proto in &mut ast.gateway.static_protos {
        let path = PathBuf::from(&proto.path);
        if path.is_relative() {
            proto.path = resolve_config_path(config_path, &path)
                .display()
                .to_string();
        }
    }
}

fn resolve_config_path(config_path: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }

    let resolved = config_path
        .parent()
        .map(|parent| parent.join(path))
        .unwrap_or_else(|| path.to_path_buf());
    if resolved.is_absolute() {
        resolved
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&resolved))
            .unwrap_or(resolved)
    }
}

fn trim_outer_newlines(value: &str) -> &str {
    value.trim_matches(|ch| ch == '\n' || ch == '\r')
}

fn format_gate_source(source: &str) -> String {
    let mut out = Vec::new();
    let mut indent = 0usize;
    let mut pending_blank = false;

    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            pending_blank = !out.is_empty();
            continue;
        }

        if pending_blank
            && !out.last().is_some_and(|line: &String| line.is_empty())
            && !starts_top_level_block(trimmed)
        {
            out.push(String::new());
        }
        if starts_top_level_block(trimmed)
            && !out.is_empty()
            && !out.last().is_some_and(|line: &String| line.is_empty())
        {
            out.push(String::new());
        }

        let current_indent = indent.saturating_sub(leading_closers(trimmed));
        out.push(format!("{}{}", " ".repeat(current_indent * 4), trimmed));
        indent = indent_after_line(indent, trimmed);
        pending_blank = false;
    }

    let mut formatted = out.join("\n");
    formatted.push('\n');
    formatted
}

fn starts_top_level_block(line: &str) -> bool {
    line.starts_with("gateway ") || line.starts_with("endpoint ")
}

fn leading_closers(line: &str) -> usize {
    line.chars()
        .take_while(|ch| matches!(ch, '}' | ']'))
        .count()
}

fn indent_after_line(indent: usize, line: &str) -> usize {
    let mut next = indent as isize;
    let mut chars = line.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' | '[' => next += 1,
            '}' | ']' => next -= 1,
            '/' if chars.peek() == Some(&'/') => break,
            _ => {}
        }
    }

    next.max(0) as usize
}

fn plan_error_message(error: crate::planner::PlanError) -> String {
    match error {
        crate::planner::PlanError::DuplicateVariable {
            endpoint,
            variable,
            first_step,
            second_step,
        } => format!(
            "endpoint `{endpoint}` defines `{variable}` twice: steps {first_step} and {second_step}"
        ),
        crate::planner::PlanError::UndefinedVariable {
            endpoint,
            variable,
            used_by,
        } => format!("endpoint `{endpoint}` uses undefined variable `{variable}` in `{used_by}`"),
        crate::planner::PlanError::Cycle { endpoint } => {
            format!("endpoint `{endpoint}` has a cyclic dependency graph")
        }
    }
}

#[derive(Debug, Deserialize)]
struct ControlRequest {
    action: String,
}

#[derive(Debug, Deserialize)]
struct SaveConfigRequest {
    source: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum EditorWsMessage {
    EndpointAdd {
        method: String,
        path: String,
    },
    EndpointUpdate {
        endpoint_index: usize,
        method: String,
        path: String,
    },
    EndpointGraphSave {
        endpoint_index: usize,
        endpoint_source: String,
    },
    EndpointOptionsUpdate {
        endpoint_index: usize,
        options_source: String,
    },
    GatewayUpdate {
        gateway_name: String,
        gateway_source: String,
    },
}

#[derive(Debug, Serialize)]
struct DevState {
    config_path: String,
    runtime_running: bool,
    last_error: Option<String>,
    model: ModelState,
}

#[derive(Debug, Serialize)]
struct ModelState {
    source: String,
    parsed: Option<ParsedModel>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct ParsedModel {
    file: Value,
    plan: Value,
}
