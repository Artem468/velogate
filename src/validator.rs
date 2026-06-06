use crate::ast::{EndpointOption, FileAST};
use lasso::Rodeo;
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ValidationError {
    pub message: String,
}

pub fn validate_file(ast: &FileAST, interner: &Rodeo, config_path: &Path) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    validate_gateway(ast, interner, config_path, &mut errors);
    validate_endpoints(ast, interner, &mut errors);

    errors
}

fn validate_gateway(
    ast: &FileAST,
    interner: &Rodeo,
    config_path: &Path,
    errors: &mut Vec<ValidationError>,
) {
    match ast.gateway.port_raw {
        Some(port) if (1..=u16::MAX as i64).contains(&port) => {}
        Some(port) => push(
            errors,
            format!("gateway.port must be in range 1..65535, got {port}"),
        ),
        None => push(errors, "gateway.port is required"),
    }

    if let Some(env_file) = ast.gateway.env_file.as_deref() {
        validate_existing_file(config_path, env_file, "gateway.env_file", errors);
    }

    validate_unique_symbols(
        ast.gateway.constants.iter().map(|constant| constant.name),
        interner,
        "gateway constant",
        errors,
    );
    validate_unique_symbols(
        ast.gateway.static_dbs.iter().map(|db| db.name),
        interner,
        "database",
        errors,
    );
    validate_unique_symbols(
        ast.gateway.static_protos.iter().map(|proto| proto.name),
        interner,
        "proto",
        errors,
    );

    for proto in &ast.gateway.static_protos {
        validate_existing_file(config_path, &proto.path, "proto.path", errors);
    }
}

fn validate_endpoints(ast: &FileAST, interner: &Rodeo, errors: &mut Vec<ValidationError>) {
    let mut routes = HashSet::new();

    for endpoint in &ast.endpoints {
        validate_endpoint_path(endpoint, errors);
        if !is_valid_method(&endpoint.method) {
            push(
                errors,
                format!(
                    "endpoint `{}` uses unsupported HTTP method `{}`",
                    endpoint.path, endpoint.method
                ),
            );
        }

        let route_pattern = normalized_route_pattern(&endpoint.path);
        if !routes.insert((endpoint.method.as_str(), route_pattern.clone())) {
            push(
                errors,
                format!(
                    "conflicting endpoint route `{} {}`; route pattern `{route_pattern}` is already registered",
                    endpoint.method, endpoint.path,
                ),
            );
        }

        if !(100..=599).contains(&endpoint.response.status_raw) {
            push(
                errors,
                format!(
                    "endpoint `{} {}` response status must be in range 100..599, got {}",
                    endpoint.method, endpoint.path, endpoint.response.status_raw
                ),
            );
        }

        for option in &endpoint.options {
            if let EndpointOption::RateLimit { limit, unit, .. } = option {
                if *limit == 0 {
                    push(
                        errors,
                        format!(
                            "endpoint `{} {}` rate_limit must be greater than zero",
                            endpoint.method, endpoint.path
                        ),
                    );
                }

                let unit = interner.resolve(unit);
                if !matches!(unit, "rps" | "rpm" | "rph") {
                    push(
                        errors,
                        format!(
                            "endpoint `{} {}` uses unknown rate_limit unit `{unit}`",
                            endpoint.method, endpoint.path
                        ),
                    );
                }
            }
        }
    }
}

fn validate_endpoint_path(endpoint: &crate::ast::Endpoint, errors: &mut Vec<ValidationError>) {
    if !endpoint.path.starts_with('/') || endpoint.path.contains(['{', '}', '*']) {
        push(
            errors,
            format!(
                "endpoint `{} {}` path must be absolute and use `:name` for parameters",
                endpoint.method, endpoint.path
            ),
        );
    }

    let mut params = HashSet::new();
    for segment in endpoint
        .path
        .split('/')
        .filter_map(|segment| segment.strip_prefix(':'))
    {
        if segment.is_empty()
            || !segment
                .chars()
                .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        {
            push(
                errors,
                format!(
                    "endpoint `{} {}` has invalid path parameter `:{segment}`",
                    endpoint.method, endpoint.path
                ),
            );
        } else if !params.insert(segment) {
            push(
                errors,
                format!(
                    "endpoint `{} {}` repeats path parameter `:{segment}`",
                    endpoint.method, endpoint.path
                ),
            );
        }
    }
}

fn normalized_route_pattern(path: &str) -> String {
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

fn validate_unique_symbols(
    names: impl Iterator<Item = crate::ast::Sym>,
    interner: &Rodeo,
    kind: &str,
    errors: &mut Vec<ValidationError>,
) {
    let mut seen = HashSet::new();
    for name in names {
        if !seen.insert(name) {
            push(
                errors,
                format!("duplicate {kind} `{}`", interner.resolve(&name)),
            );
        }
    }
}

fn validate_existing_file(
    config_path: &Path,
    path: &str,
    field: &str,
    errors: &mut Vec<ValidationError>,
) {
    let path = Path::new(path);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_path
            .parent()
            .map(|parent| parent.join(path))
            .unwrap_or_else(|| path.to_path_buf())
    };

    if !path.is_file() {
        push(
            errors,
            format!("{field} points to missing file `{}`", path.display()),
        );
    }
}

fn is_valid_method(method: &str) -> bool {
    matches!(
        method,
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS" | "TRACE"
    )
}

fn push(errors: &mut Vec<ValidationError>, message: impl Into<String>) {
    errors.push(ValidationError {
        message: message.into(),
    });
}

#[cfg(test)]
mod tests {
    use super::validate_file;
    use crate::parser::Parser;
    use lasso::Rodeo;
    use std::path::Path;

    #[test]
    fn rejects_missing_port_bad_method_bad_status_and_unknown_rate_unit() {
        let source = r#"
            gateway "api" {}

            endpoint "BREW /x" {
                rate_limit: 1/banana window 1s,
                respond 99 {}
            }
        "#;

        let errors = validate(source);
        assert!(has_error(&errors, "gateway.port is required"));
        assert!(has_error(&errors, "unsupported HTTP method `BREW`"));
        assert!(has_error(
            &errors,
            "response status must be in range 100..599"
        ));
        assert!(has_error(&errors, "unknown rate_limit unit `banana`"));
    }

    #[test]
    fn rejects_duplicate_routes_and_gateway_names() {
        let source = r#"
            gateway "api" {
                port: 8080,
                constants: {
                    "api": "a",
                    "api": "b"
                },
                databases: [
                    sqlite "main" { url: "sqlite::memory:" },
                    sqlite "main" { url: "sqlite::memory:" }
                ],
                protos: [
                    proto "profile" { path: "missing.proto" },
                    proto "profile" { path: "missing.proto" }
                ],
            }

            endpoint "GET /x" { respond 200 {} }
            endpoint "GET /x" { respond 200 {} }
        "#;

        let errors = validate(source);
        assert!(has_error(&errors, "duplicate gateway constant `api`"));
        assert!(has_error(&errors, "duplicate database `main`"));
        assert!(has_error(&errors, "duplicate proto `profile`"));
        assert!(has_error(&errors, "conflicting endpoint route `GET /x`"));
        assert!(has_error(&errors, "proto.path points to missing file"));
    }

    #[test]
    fn rejects_routes_with_equivalent_dynamic_patterns() {
        let source = r#"
            gateway "api" { port: 8080 }
            endpoint "GET /users/:id" { respond 200 {} }
            endpoint "GET /users/:name" { respond 200 {} }
        "#;

        let errors = validate(source);
        assert!(has_error(
            &errors,
            "route pattern `/users/{}` is already registered"
        ));
    }

    #[test]
    fn rejects_missing_env_file() {
        let source = r#"
            gateway "api" {
                port: 8080,
                env_file: "missing.env"
            }
        "#;

        let errors = validate(source);
        assert!(has_error(
            &errors,
            "gateway.env_file points to missing file"
        ));
    }

    fn validate(source: &str) -> Vec<String> {
        let mut parser = Parser::new(Rodeo::new());
        let ast = parser.parse(source).expect("source should parse");
        validate_file(&ast, &parser.interner, Path::new("test.gate"))
            .into_iter()
            .map(|error| error.message)
            .collect()
    }

    fn has_error(errors: &[String], needle: &str) -> bool {
        errors.iter().any(|error| error.contains(needle))
    }
}
