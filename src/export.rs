use crate::ast::*;
use lasso::Rodeo;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

#[derive(Debug, Serialize)]
pub struct FileExport {
    pub gateway: GatewayExport,
    pub endpoints: Vec<EndpointExport>,
}

#[derive(Debug, Serialize)]
pub struct GatewayExport {
    pub name: String,
    pub port: u16,
    pub host: Option<String>,
    pub env_file: Option<String>,
    pub constants: BTreeMap<String, ExprExport>,
    pub static_dbs: Vec<StaticDbExport>,
    pub static_protos: Vec<StaticProtoExport>,
}

#[derive(Debug, Serialize)]
pub struct StaticDbExport {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct StaticProtoExport {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct EndpointExport {
    pub method: String,
    pub path: String,
    pub options: Vec<EndpointOptionExport>,
    pub steps: Vec<StepExport>,
    pub response_status: u16,
    pub response_body: Option<BTreeMap<String, ExprExport>>,
    pub response_headers: BTreeMap<String, ExprExport>,
    pub response_cookies: BTreeMap<String, ExprExport>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EndpointOptionExport {
    Secure {
        rules: Vec<SecureRuleExport>,
    },
    RateLimit {
        limit: u32,
        unit: String,
        window_ms: u64,
    },
}

#[derive(Debug, Serialize)]
pub struct SecureRuleExport {
    pub scheme: String,
    pub has_secret: bool,
    pub has_username: bool,
    pub has_password: bool,
    pub checks: Vec<ExprExport>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepExport {
    Let {
        var_name: String,
        value: ExprExport,
    },
    Command {
        var_name: String,
        command: String,
    },
    FetchHttp {
        var_name: String,
        config: HttpConfigExport,
    },
    CallGrpc {
        var_name: String,
        config: GrpcConfigExport,
    },
    QueryDb {
        var_name: String,
        config: DbQueryConfigExport,
    },
    Pipe {
        var_name: String,
        source: ExprExport,
        operations: Vec<PipeOpExport>,
    },
}

#[derive(Debug, Serialize)]
pub struct HttpConfigExport {
    pub url: ExprExport,
    pub method: Option<String>,
    pub body: Option<ExprExport>,
    pub timeout_ms: Option<u64>,
    pub retries: Option<u32>,
    pub delay_ms: Option<u64>,
    pub fallback: Option<ExprExport>,
}

#[derive(Debug, Serialize)]
pub struct GrpcConfigExport {
    pub service_method: String,
    pub proto_path: Option<String>,
    pub service: Option<String>,
    pub method: Option<String>,
    pub payload: ExprExport,
    pub timeout_ms: Option<u64>,
    pub fallback: Option<ExprExport>,
}

#[derive(Debug, Serialize)]
pub struct DbQueryConfigExport {
    pub db_source: ExprExport,
    pub sql: String,
    pub params: Vec<ExprExport>,
    pub timeout_ms: Option<u64>,
    pub fallback: Option<ExprExport>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PipeOpExport {
    Filter {
        param: String,
        condition: ExprExport,
    },
    Map {
        param: String,
        layout: BTreeMap<String, ExprExport>,
    },
    Take {
        count: ExprExport,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExprExport {
    Variable {
        name: String,
    },
    Number {
        value: f64,
    },
    String {
        value: String,
    },
    Boolean {
        value: bool,
    },
    Object {
        fields: BTreeMap<String, ExprExport>,
    },
    Array {
        items: Vec<ExprExport>,
    },
    PropertyAccess {
        object: Box<ExprExport>,
        field: String,
    },
    Call {
        callee: Box<ExprExport>,
        args: Vec<ExprExport>,
    },
    BinaryOp {
        left: Box<ExprExport>,
        op: &'static str,
        right: Box<ExprExport>,
    },
}

pub fn export_file(ast: &FileAST, interner: &Rodeo) -> FileExport {
    FileExport {
        gateway: GatewayExport {
            name: ast.gateway.name.clone(),
            port: ast.gateway.port,
            host: ast.gateway.host.clone(),
            env_file: ast.gateway.env_file.clone(),
            constants: ast
                .gateway
                .constants
                .iter()
                .map(|constant| {
                    (
                        sym(interner, constant.name),
                        export_expr(&constant.value, interner),
                    )
                })
                .collect(),
            static_dbs: ast
                .gateway
                .static_dbs
                .iter()
                .map(|db| StaticDbExport {
                    name: sym(interner, db.name),
                    url: db.url.clone(),
                })
                .collect(),
            static_protos: ast
                .gateway
                .static_protos
                .iter()
                .map(|proto| StaticProtoExport {
                    name: sym(interner, proto.name),
                    path: proto.path.clone(),
                })
                .collect(),
        },
        endpoints: ast
            .endpoints
            .iter()
            .map(|endpoint| export_endpoint(endpoint, interner))
            .collect(),
    }
}

pub fn export_dot(ast: &FileAST, interner: &Rodeo) -> String {
    let mut out = String::from("digraph velogate {\n  rankdir=LR;\n");
    out.push_str(&format!(
        "  gateway [label=\"gateway {}:{}\"];\n",
        dot_escape(&ast.gateway.name),
        ast.gateway.port
    ));

    for endpoint in &ast.endpoints {
        let endpoint_node = format!("endpoint_{}_{}", endpoint.method, endpoint.path)
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>();
        out.push_str(&format!(
            "  {endpoint_node} [label=\"{} {}\"];\n  gateway -> {endpoint_node};\n",
            dot_escape(&endpoint.method),
            dot_escape(&endpoint.path)
        ));

        for step in &endpoint.steps {
            let (var, deps) = step_var_and_deps(step, interner);
            out.push_str(&format!("  \"{var}\" [shape=box];\n"));
            out.push_str(&format!("  {endpoint_node} -> \"{var}\";\n"));
            for dep in deps {
                out.push_str(&format!("  \"{dep}\" -> \"{var}\";\n"));
            }
        }
    }

    out.push_str("}\n");
    out
}

pub fn export_openapi(ast: &FileAST, interner: &Rodeo) -> Value {
    let mut paths = Map::new();
    let mut security_schemes = Map::new();

    for endpoint in &ast.endpoints {
        let path = openapi_path(&endpoint.path);
        let method = endpoint.method.to_ascii_lowercase();
        let path_item = paths.entry(path).or_insert_with(|| json!({}));
        let Value::Object(methods) = path_item else {
            continue;
        };

        let (security, route_schemes) = openapi_security(endpoint, interner);
        for (name, scheme) in route_schemes {
            security_schemes.entry(name).or_insert(scheme);
        }

        let mut operation = Map::new();
        operation.insert(
            "operationId".to_string(),
            json!(operation_id(&endpoint.method, &endpoint.path)),
        );
        operation.insert(
            "summary".to_string(),
            json!(format!("{} {}", endpoint.method, endpoint.path)),
        );
        operation.insert(
            "parameters".to_string(),
            json!(path_parameters(&endpoint.path)),
        );
        if !security.is_empty() {
            operation.insert("security".to_string(), Value::Array(security));
        }
        if let Some(rate_limit) = openapi_rate_limit(endpoint, interner) {
            operation.insert("x-rate-limit".to_string(), rate_limit);
        }
        if matches!(endpoint.method.as_str(), "POST" | "PUT" | "PATCH") {
            operation.insert(
                "requestBody".to_string(),
                json!({
                    "required": false,
                    "content": {
                        "application/json": {
                            "schema": {
                                "type": "object",
                                "additionalProperties": true
                            }
                        }
                    }
                }),
            );
        }
        operation.insert(
            "responses".to_string(),
            json!({
                endpoint.response.status.to_string(): {
                    "description": response_description(endpoint.response.status),
                    "content": {
                        "application/json": {
                            "schema": response_schema(&endpoint.response, interner)
                        }
                    }
                }
            }),
        );

        methods.insert(method, Value::Object(operation));
    }

    json!({
        "openapi": "3.1.0",
        "info": {
            "title": ast.gateway.name,
            "version": env!("CARGO_PKG_VERSION")
        },
        "servers": [openapi_server(ast)],
        "paths": paths,
        "components": {
            "securitySchemes": security_schemes
        }
    })
}

fn export_endpoint(endpoint: &Endpoint, interner: &Rodeo) -> EndpointExport {
    EndpointExport {
        method: endpoint.method.clone(),
        path: endpoint.path.clone(),
        options: endpoint
            .options
            .iter()
            .map(|option| export_endpoint_option(option, interner))
            .collect(),
        steps: endpoint
            .steps
            .iter()
            .map(|step| export_step(step, interner))
            .collect(),
        response_status: endpoint.response.status,
        response_body: endpoint.response.body.as_ref().map(|body| {
            body.iter()
                .map(|(key, value)| (key.clone(), export_expr(value, interner)))
                .collect()
        }),
        response_headers: endpoint
            .response
            .headers
            .iter()
            .map(|(key, value)| (key.clone(), export_expr(value, interner)))
            .collect(),
        response_cookies: endpoint
            .response
            .cookies
            .iter()
            .map(|(key, value)| (key.clone(), export_expr(value, interner)))
            .collect(),
    }
}

fn openapi_server(ast: &FileAST) -> Value {
    let host = ast.gateway.host.as_deref().unwrap_or("127.0.0.1");
    json!({ "url": format!("http://{host}:{}", ast.gateway.port) })
}

fn openapi_path(path: &str) -> String {
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

fn path_parameters(path: &str) -> Vec<Value> {
    path.split('/')
        .filter_map(|segment| segment.strip_prefix(':'))
        .filter(|name| !name.is_empty())
        .map(|name| {
            json!({
                "name": name,
                "in": "path",
                "required": true,
                "schema": { "type": "string" }
            })
        })
        .collect()
}

fn openapi_security(endpoint: &Endpoint, interner: &Rodeo) -> (Vec<Value>, Vec<(String, Value)>) {
    let mut requirement = Map::new();
    let mut schemes = Vec::new();

    for option in &endpoint.options {
        let EndpointOption::Secure(rules) = option else {
            continue;
        };
        for rule in rules {
            let scheme = sym(interner, rule.scheme);
            let name = security_scheme_name(&scheme);
            requirement.insert(name.clone(), json!([]));
            schemes.push((name, security_scheme(&scheme)));
        }
    }

    let requirements = if requirement.is_empty() {
        Vec::new()
    } else {
        vec![Value::Object(requirement)]
    };

    (requirements, schemes)
}

fn security_scheme_name(scheme: &str) -> String {
    match scheme.to_ascii_lowercase().as_str() {
        "jwt" => "jwtAuth".to_string(),
        "basic" => "basicAuth".to_string(),
        other => format!("{other}Auth"),
    }
}

fn security_scheme(scheme: &str) -> Value {
    match scheme.to_ascii_lowercase().as_str() {
        "jwt" => json!({ "type": "http", "scheme": "bearer", "bearerFormat": "JWT" }),
        "basic" => json!({ "type": "http", "scheme": "basic" }),
        _ => json!({ "type": "apiKey", "in": "header", "name": "Authorization" }),
    }
}

fn openapi_rate_limit(endpoint: &Endpoint, interner: &Rodeo) -> Option<Value> {
    endpoint.options.iter().find_map(|option| match option {
        EndpointOption::RateLimit {
            limit,
            unit,
            window_ms,
        } => Some(json!({
            "limit": limit,
            "unit": sym(interner, *unit),
            "window_ms": window_ms,
            "key": "ip"
        })),
        EndpointOption::Secure(_) => None,
    })
}

fn response_schema(response: &EndpointResponse, interner: &Rodeo) -> Value {
    match &response.body {
        Some(body) => object_schema(body, interner),
        None => json!({ "type": "object", "additionalProperties": true }),
    }
}

fn object_schema(
    fields: &std::collections::HashMap<String, Expression>,
    interner: &Rodeo,
) -> Value {
    let mut properties = Map::new();
    for (key, value) in fields {
        properties.insert(key.clone(), expr_schema(value, interner));
    }
    json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": true
    })
}

fn expr_schema(expr: &Expression, interner: &Rodeo) -> Value {
    match expr {
        Expression::String(_) => json!({ "type": "string" }),
        Expression::Number(_) => json!({ "type": "number" }),
        Expression::Boolean(_) => json!({ "type": "boolean" }),
        Expression::Object(fields) => object_schema(fields, interner),
        Expression::Array(items) => {
            let item_schema = items
                .first()
                .map(|item| expr_schema(item, interner))
                .unwrap_or_else(|| json!({}));
            json!({ "type": "array", "items": item_schema })
        }
        Expression::Variable(_)
        | Expression::PropertyAccess(_, _)
        | Expression::Call { .. }
        | Expression::BinaryOp(_, _, _) => json!({}),
    }
}

fn response_description(status: u16) -> &'static str {
    match status {
        100..=199 => "Informational response",
        200..=299 => "Successful response",
        300..=399 => "Redirect response",
        400..=499 => "Client error response",
        500..=599 => "Server error response",
        _ => "Response",
    }
}

fn operation_id(method: &str, path: &str) -> String {
    let mut out = method.to_ascii_lowercase();
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        out.push('_');
        out.push_str(segment.trim_start_matches(':'));
    }
    out.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect()
}

fn export_step(step: &Step, interner: &Rodeo) -> StepExport {
    match step {
        Step::Let { var_name, value } => StepExport::Let {
            var_name: sym(interner, *var_name),
            value: export_expr(value, interner),
        },
        Step::Command { var_name, command } => StepExport::Command {
            var_name: sym(interner, *var_name),
            command: command.clone(),
        },
        Step::FetchHttp { var_name, config } => StepExport::FetchHttp {
            var_name: sym(interner, *var_name),
            config: HttpConfigExport {
                url: export_expr(&config.url, interner),
                method: config.method.clone(),
                body: config.body.as_ref().map(|expr| export_expr(expr, interner)),
                timeout_ms: config.timeout_ms,
                retries: config.retries,
                delay_ms: config.delay_ms,
                fallback: config
                    .fallback
                    .as_ref()
                    .map(|expr| export_expr(expr, interner)),
            },
        },
        Step::CallGrpc { var_name, config } => StepExport::CallGrpc {
            var_name: sym(interner, *var_name),
            config: GrpcConfigExport {
                service_method: config.service_method.clone(),
                proto_path: config.proto_path.clone(),
                service: config.service.clone(),
                method: config.method.clone(),
                payload: export_expr(&config.payload, interner),
                timeout_ms: config.timeout_ms,
                fallback: config
                    .fallback
                    .as_ref()
                    .map(|expr| export_expr(expr, interner)),
            },
        },
        Step::QueryDb { var_name, config } => StepExport::QueryDb {
            var_name: sym(interner, *var_name),
            config: DbQueryConfigExport {
                db_source: export_expr(&config.db_source, interner),
                sql: config.sql.clone(),
                params: config
                    .params
                    .iter()
                    .map(|expr| export_expr(expr, interner))
                    .collect(),
                timeout_ms: config.timeout_ms,
                fallback: config
                    .fallback
                    .as_ref()
                    .map(|expr| export_expr(expr, interner)),
            },
        },
        Step::Pipe {
            var_name,
            source,
            operations,
        } => StepExport::Pipe {
            var_name: sym(interner, *var_name),
            source: export_expr(source, interner),
            operations: operations
                .iter()
                .map(|op| export_pipe_op(op, interner))
                .collect(),
        },
    }
}

fn export_endpoint_option(option: &EndpointOption, interner: &Rodeo) -> EndpointOptionExport {
    match option {
        EndpointOption::Secure(rules) => EndpointOptionExport::Secure {
            rules: rules
                .iter()
                .map(|rule| SecureRuleExport {
                    scheme: sym(interner, rule.scheme),
                    has_secret: rule.secret.is_some(),
                    has_username: rule.username.is_some(),
                    has_password: rule.password.is_some(),
                    checks: rule
                        .checks
                        .iter()
                        .map(|expr| export_expr(expr, interner))
                        .collect(),
                })
                .collect(),
        },
        EndpointOption::RateLimit {
            limit,
            unit,
            window_ms,
        } => EndpointOptionExport::RateLimit {
            limit: *limit,
            unit: sym(interner, *unit),
            window_ms: *window_ms,
        },
    }
}

fn export_pipe_op(op: &PipeOp, interner: &Rodeo) -> PipeOpExport {
    match op {
        PipeOp::Filter { param, condition } => PipeOpExport::Filter {
            param: sym(interner, *param),
            condition: export_expr(condition, interner),
        },
        PipeOp::Map { param, layout } => PipeOpExport::Map {
            param: sym(interner, *param),
            layout: layout
                .iter()
                .map(|(key, value)| (key.clone(), export_expr(value, interner)))
                .collect(),
        },
        PipeOp::Take(count) => PipeOpExport::Take {
            count: export_expr(count, interner),
        },
    }
}

fn export_expr(expr: &Expression, interner: &Rodeo) -> ExprExport {
    match expr {
        Expression::Variable(name) => ExprExport::Variable {
            name: sym(interner, *name),
        },
        Expression::Number(value) => ExprExport::Number { value: *value },
        Expression::String(value) => ExprExport::String {
            value: value.clone(),
        },
        Expression::Boolean(value) => ExprExport::Boolean { value: *value },
        Expression::Object(fields) => ExprExport::Object {
            fields: fields
                .iter()
                .map(|(key, value)| (key.clone(), export_expr(value, interner)))
                .collect(),
        },
        Expression::Array(items) => ExprExport::Array {
            items: items
                .iter()
                .map(|expr| export_expr(expr, interner))
                .collect(),
        },
        Expression::PropertyAccess(object, field) => ExprExport::PropertyAccess {
            object: Box::new(export_expr(object, interner)),
            field: sym(interner, *field),
        },
        Expression::Call { callee, args } => ExprExport::Call {
            callee: Box::new(export_expr(callee, interner)),
            args: args.iter().map(|arg| export_expr(arg, interner)).collect(),
        },
        Expression::BinaryOp(left, op, right) => ExprExport::BinaryOp {
            left: Box::new(export_expr(left, interner)),
            op: op.as_str(),
            right: Box::new(export_expr(right, interner)),
        },
    }
}

fn step_var_and_deps(step: &Step, interner: &Rodeo) -> (String, Vec<String>) {
    let mut deps = Vec::new();
    let var = match step {
        Step::Let { var_name, value } => {
            collect_expr_deps(value, interner, &mut deps);
            sym(interner, *var_name)
        }
        Step::Command { var_name, .. } => sym(interner, *var_name),
        Step::FetchHttp { var_name, config } => {
            collect_expr_deps(&config.url, interner, &mut deps);
            if let Some(body) = &config.body {
                collect_expr_deps(body, interner, &mut deps);
            }
            sym(interner, *var_name)
        }
        Step::CallGrpc { var_name, config } => {
            collect_expr_deps(&config.payload, interner, &mut deps);
            sym(interner, *var_name)
        }
        Step::QueryDb { var_name, config } => {
            collect_expr_deps(&config.db_source, interner, &mut deps);
            for param in &config.params {
                collect_expr_deps(param, interner, &mut deps);
            }
            sym(interner, *var_name)
        }
        Step::Pipe {
            var_name,
            source,
            operations,
        } => {
            collect_expr_deps(source, interner, &mut deps);
            for op in operations {
                match op {
                    PipeOp::Filter { param, condition } => {
                        let bound = sym(interner, *param);
                        collect_expr_deps(condition, interner, &mut deps);
                        deps.retain(|dep| dep != &bound);
                    }
                    PipeOp::Map { param, layout } => {
                        let bound = sym(interner, *param);
                        for expr in layout.values() {
                            collect_expr_deps(expr, interner, &mut deps);
                        }
                        deps.retain(|dep| dep != &bound);
                    }
                    PipeOp::Take(count) => collect_expr_deps(count, interner, &mut deps),
                }
            }
            sym(interner, *var_name)
        }
    };
    deps.sort();
    deps.dedup();
    (var, deps)
}

fn collect_expr_deps(expr: &Expression, interner: &Rodeo, deps: &mut Vec<String>) {
    match expr {
        Expression::Variable(name) => deps.push(sym(interner, *name)),
        Expression::PropertyAccess(object, _) => collect_expr_deps(object, interner, deps),
        Expression::Call { callee, args } => {
            collect_expr_deps(callee, interner, deps);
            for arg in args {
                collect_expr_deps(arg, interner, deps);
            }
        }
        Expression::BinaryOp(left, _, right) => {
            collect_expr_deps(left, interner, deps);
            collect_expr_deps(right, interner, deps);
        }
        Expression::Object(fields) => {
            for expr in fields.values() {
                collect_expr_deps(expr, interner, deps);
            }
        }
        Expression::Array(items) => {
            for expr in items {
                collect_expr_deps(expr, interner, deps);
            }
        }
        Expression::Number(_) | Expression::String(_) | Expression::Boolean(_) => {}
    }
}

fn sym(interner: &Rodeo, name: Sym) -> String {
    interner.resolve(&name).to_string()
}

fn dot_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

impl BinaryOperator {
    fn as_str(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Mod => "%",
            Self::Eq => "==",
            Self::Neq => "!=",
            Self::Gt => ">",
            Self::Lt => "<",
            Self::Gte => ">=",
            Self::Lte => "<=",
            Self::And => "&&",
            Self::Or => "||",
        }
    }
}
