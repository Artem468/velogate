use super::types::SecureFailure;
use crate::ast::{Endpoint, EndpointOption, Expression, SecureRule};
use crate::runtime::{Value, Vars, as_string, eval_expr, insert_value_var, sym, truthy};
use axum::http::HeaderMap;
use axum::http::header;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use lasso::Rodeo;
use serde_json::json;

pub(crate) fn authorize_endpoint(
    endpoint: &Endpoint,
    vars: &mut Vars,
    headers: &HeaderMap,
    interner: &Rodeo,
) -> Result<(), SecureFailure> {
    for option in &endpoint.options {
        let EndpointOption::Secure(rules) = option else {
            continue;
        };

        for rule in rules {
            authorize_rule(rule, vars, headers, interner)?;
        }
    }

    Ok(())
}

fn authorize_rule(
    rule: &SecureRule,
    vars: &mut Vars,
    headers: &HeaderMap,
    interner: &Rodeo,
) -> Result<(), SecureFailure> {
    let scheme = sym(interner, rule.scheme);
    match scheme {
        "JWT" => {
            let secret = eval_secure_string(rule.secret.as_ref(), vars, interner, "secret")?;
            let token = bearer_token(headers).ok_or_else(|| SecureFailure {
                message: "missing or invalid bearer token".to_string(),
            })?;
            let claims = decode_jwt_claims(token, &secret)?;
            insert_value_var(vars, interner, "jwt", claims);
            evaluate_checks(&rule.checks, vars, interner)
        }
        "Basic" => {
            let (username, password) = basic_credentials(headers).ok_or_else(|| SecureFailure {
                message: "missing or invalid basic authorization".to_string(),
            })?;
            if let Some(expected) = eval_optional_secure_string(&rule.username, vars, interner)?
                && username.as_str() != expected.as_str()
            {
                return Err(SecureFailure {
                    message: "basic username rejected".to_string(),
                });
            }
            if let Some(expected) = eval_optional_secure_string(&rule.password, vars, interner)?
                && password.as_str() != expected.as_str()
            {
                return Err(SecureFailure {
                    message: "basic password rejected".to_string(),
                });
            }
            insert_value_var(
                vars,
                interner,
                "basic",
                json!({ "username": username, "password": password }),
            );
            evaluate_checks(&rule.checks, vars, interner)
        }
        _ => Err(SecureFailure {
            message: format!("unsupported secure scheme `{scheme}`"),
        }),
    }
}

fn evaluate_checks(
    checks: &[Expression],
    vars: &Vars,
    interner: &Rodeo,
) -> Result<(), SecureFailure> {
    for check in checks {
        let passed = eval_expr(check, vars, interner)
            .map(|value| truthy(&value))
            .map_err(|err| SecureFailure {
                message: format!("secure check failed to evaluate: {err}"),
            })?;
        if !passed {
            return Err(SecureFailure {
                message: "secure check rejected request".to_string(),
            });
        }
    }

    Ok(())
}

fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let encoded = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Basic ")?
        .trim();
    let decoded = BASE64.decode(encoded).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (username, password) = decoded.split_once(':')?;
    Some((username.to_string(), password.to_string()))
}

fn eval_secure_string(
    expr: Option<&Expression>,
    vars: &Vars,
    interner: &Rodeo,
    field: &str,
) -> Result<String, SecureFailure> {
    let expr = expr.ok_or_else(|| SecureFailure {
        message: format!("secure rule requires `{field}`"),
    })?;
    eval_expr(expr, vars, interner)
        .map(as_string)
        .map_err(|err| SecureFailure {
            message: format!("secure `{field}` failed to evaluate: {err}"),
        })
}

fn eval_optional_secure_string(
    expr: &Option<Expression>,
    vars: &Vars,
    interner: &Rodeo,
) -> Result<Option<String>, SecureFailure> {
    expr.as_ref()
        .map(|expr| {
            eval_expr(expr, vars, interner)
                .map(as_string)
                .map_err(|err| SecureFailure {
                    message: format!("secure credential failed to evaluate: {err}"),
                })
        })
        .transpose()
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

fn decode_jwt_claims(token: &str, secret: &str) -> Result<Value, SecureFailure> {
    let validation = Validation::new(Algorithm::HS256);
    decode::<Value>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .map(|data| data.claims)
    .map_err(|err| SecureFailure {
        message: format!("invalid JWT: {err}"),
    })
}
