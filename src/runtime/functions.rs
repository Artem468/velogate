use super::{RuntimeError, RuntimeResult, Value, Vars, eval_expr};
use crate::ast::{FileAST, Sym};
use axum::body::Bytes;
use axum::http::{HeaderMap, header};
use axum::routing::MethodFilter;
use lasso::Rodeo;
use serde_json::Map;
use std::collections::HashMap;

pub(super) fn gateway_vars(ast: &FileAST, interner: &Rodeo) -> RuntimeResult<Vars> {
    let mut vars: Vars = ast
        .gateway
        .static_dbs
        .iter()
        .map(|db| (db.name, Value::String(db.url.clone())))
        .collect();

    insert_value_var(
        &mut vars,
        interner,
        "env",
        Value::Object(load_env_file(ast.gateway.env_file.as_deref())?),
    );

    for constant in &ast.gateway.constants {
        let value = eval_expr(&constant.value, &vars, interner).map_err(|err| {
            RuntimeError::Execution(format!(
                "failed to evaluate gateway constant `{}`: {err}",
                sym(interner, constant.name)
            ))
        })?;
        vars.insert(constant.name, value);
    }

    Ok(vars)
}

fn load_env_file(path: Option<&str>) -> RuntimeResult<Map<String, Value>> {
    let Some(path) = path else {
        return Ok(Map::new());
    };
    let contents = std::fs::read_to_string(path).map_err(|err| {
        RuntimeError::Execution(format!("failed to read env_file `{path}`: {err}"))
    })?;
    Ok(parse_env_file(&contents))
}

fn parse_env_file(contents: &str) -> Map<String, Value> {
    contents
        .lines()
        .filter_map(parse_env_line)
        .map(|(key, value)| (normalize_key(&key), Value::String(value)))
        .collect()
}

fn parse_env_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").unwrap_or(line);
    let (key, value) = line.split_once('=')?;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    Some((key.to_string(), unquote_env_value(value.trim())))
}

fn unquote_env_value(value: &str) -> String {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

pub(super) fn request_vars(
    path_params: &HashMap<String, String>,
    query_params: &HashMap<String, String>,
    headers: &HeaderMap,
    body: Value,
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
    if let Some(sym) = interner.get("body") {
        vars.insert(sym, body);
    }

    vars
}

pub(super) fn request_body_value(body: &Bytes) -> Value {
    if body.is_empty() {
        return Value::Null;
    }

    serde_json::from_slice(body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(body).into()))
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

pub(super) fn insert_value_var(vars: &mut Vars, interner: &Rodeo, name: &str, value: Value) {
    if let Some(sym) = interner.get(name) {
        vars.insert(sym, value);
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

pub(super) fn normalize_key(key: &str) -> String {
    key.replace('-', "_")
}

pub(super) fn axum_route_path(path: &str) -> String {
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

pub(super) fn db_urls(ast: &FileAST, interner: &Rodeo) -> HashMap<String, String> {
    ast.gateway
        .static_dbs
        .iter()
        .map(|db| (sym(interner, db.name).to_string(), db.url.clone()))
        .collect()
}

pub(super) fn proto_paths(ast: &FileAST, interner: &Rodeo) -> HashMap<String, String> {
    ast.gateway
        .static_protos
        .iter()
        .map(|proto| (sym(interner, proto.name).to_string(), proto.path.clone()))
        .collect()
}

pub(super) fn method_filter(method: &str) -> Option<MethodFilter> {
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

pub(super) fn sym(interner: &Rodeo, name: Sym) -> &str {
    interner.resolve(&name)
}
