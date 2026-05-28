use crate::ast::FileAST;
use crate::lexer::{LexicalError, Token, lex};

use lalrpop_util::ParseError;
use lalrpop_util::lalrpop_mod;
use lasso::Rodeo;
use std::ops::Range;

lalrpop_mod!(pub(crate) grammar);

#[derive(Debug)]
pub struct Parser {
    pub interner: Rodeo,
}

#[derive(Debug, Clone)]
pub struct ParseDiagnostic {
    pub span: Range<usize>,
    pub message: String,
    pub expected: Vec<String>,
}

impl Parser {
    pub fn new(interner: Rodeo) -> Self {
        Self { interner }
    }

    pub fn parse(&mut self, source: &str) -> Result<FileAST, ParseDiagnostic> {
        let lexer = lex(source);

        grammar::ManifestParser::new()
            .parse(&mut self.interner, lexer)
            .map_err(|err| ParseDiagnostic::from_lalrpop(err, source.len()))
    }
}

impl ParseDiagnostic {
    fn from_lalrpop(err: ParseError<usize, Token, LexicalError>, source_len: usize) -> Self {
        match err {
            ParseError::InvalidToken { location } => Self {
                span: point_span(location, source_len),
                message: "invalid token".to_string(),
                expected: Vec::new(),
            },
            ParseError::UnrecognizedEof { location, expected } => Self {
                span: point_span(location, source_len),
                message: "unexpected end of file".to_string(),
                expected,
            },
            ParseError::UnrecognizedToken {
                token: (start, token, end),
                expected,
            } => Self {
                span: normalize_span(start..end, source_len),
                message: format!("unexpected token {token:?}"),
                expected,
            },
            ParseError::ExtraToken {
                token: (start, token, end),
            } => Self {
                span: normalize_span(start..end, source_len),
                message: format!("extra token {token:?}"),
                expected: Vec::new(),
            },
            ParseError::User { error } => Self {
                span: normalize_span(error.span, source_len),
                message: error.message,
                expected: Vec::new(),
            },
        }
    }
}

fn point_span(location: usize, source_len: usize) -> Range<usize> {
    let location = location.min(source_len);
    normalize_span(location..location.saturating_add(1), source_len)
}

fn normalize_span(span: Range<usize>, source_len: usize) -> Range<usize> {
    let start = span.start.min(source_len);
    let mut end = span.end.min(source_len);
    if start == end && end < source_len {
        end += 1;
    }
    start..end
}

#[cfg(test)]
mod tests {
    use super::Parser;
    use crate::ast::{EndpointOption, Expression, Step};
    use lasso::Rodeo;

    #[test]
    fn parses_gateway_and_endpoint() {
        let source = r#"
            gateway "api" {
                port: 8080,
                databases: [
                    postgres "main" { url: "postgres://localhost/app" }
                ],
            }

            endpoint "GET /v1/users" {
                let limit = 10;
                let users = fetch "https://example.com/users";
                respond 200 {
                    "ok": true,
                    "limit": limit,
                    "users": users
                }
            }
        "#;

        let mut parser = Parser::new(Rodeo::new());
        let ast = parser.parse(source).expect("valid DSL should parse");

        assert_eq!(ast.gateway.name, "api");
        assert_eq!(ast.gateway.port, 8080);
        assert_eq!(ast.gateway.static_dbs.len(), 1);
        assert_eq!(ast.endpoints.len(), 1);

        let endpoint = &ast.endpoints[0];
        assert_eq!(endpoint.method, "GET");
        assert_eq!(endpoint.path, "/v1/users");
        assert_eq!(endpoint.response.status, 200);
        assert_eq!(endpoint.steps.len(), 2);
        assert!(matches!(endpoint.steps[0], Step::Let { .. }));
        assert!(matches!(endpoint.steps[1], Step::FetchHttp { .. }));
        let body = endpoint
            .response
            .body
            .as_ref()
            .expect("response body should be present");
        assert!(matches!(body.get("ok"), Some(Expression::Boolean(true))));
    }

    #[test]
    fn parses_db_query_and_comparison_expression() {
        let source = r#"
            gateway "api" { port: 8080 }

            endpoint "GET /v1/users" {
                let min_age = 18;
                let rows = db::query("sqlite::memory:", "select * from users where age >= ?", min_age);
                respond 200 {
                    "adult": min_age >= 18,
                    "rows": rows
                }
            }
        "#;

        let mut parser = Parser::new(Rodeo::new());
        let ast = parser.parse(source).expect("valid DSL should parse");

        assert_eq!(ast.endpoints[0].steps.len(), 2);
        assert!(matches!(ast.endpoints[0].steps[1], Step::QueryDb { .. }));
    }

    #[test]
    fn parses_fetch_method_and_body_config() {
        let source = r#"
            gateway "api" { port: 8080 }

            endpoint "POST /v1/proxy" {
                let created = fetch "https://example.com/items" {
                    method: "PATCH",
                    body: { "name": body.name, "active": true },
                    timeout: 100ms
                };
                respond 200 { "created": created }
            }
        "#;

        let mut parser = Parser::new(Rodeo::new());
        let ast = parser.parse(source).expect("valid DSL should parse");

        let Step::FetchHttp { config, .. } = &ast.endpoints[0].steps[0] else {
            panic!("step should be HTTP fetch");
        };
        assert_eq!(config.method.as_deref(), Some("PATCH"));
        assert!(config.body.is_some());
        assert_eq!(config.timeout_ms, Some(100));
    }

    #[test]
    fn parses_jwt_secure_rule_with_custom_checks() {
        let source = r#"
            gateway "api" { port: 8080 }

            endpoint "GET /v1/users/:id" {
                secure: [
                    JWT {
                        secret: "dev-secret",
                        checks: [
                            jwt.role == "admin",
                            jwt.sub == id
                        ]
                    }
                ],

                respond 200 { "sub": jwt.sub }
            }
        "#;

        let mut parser = Parser::new(Rodeo::new());
        let ast = parser.parse(source).expect("valid DSL should parse");

        let EndpointOption::Secure(rules) = &ast.endpoints[0].options[0] else {
            panic!("option should be secure");
        };
        assert_eq!(rules.len(), 1);
        assert!(rules[0].secret.is_some());
        assert_eq!(rules[0].checks.len(), 2);
    }

    #[test]
    fn parses_gateway_env_file_constants_and_basic_secure_rule() {
        let source = r#"
            gateway "api" {
                port: 8080,
                env_file: ".env",
                constants: {
                    "api_base": env.API_BASE,
                    "default_limit": 10
                }
            }

            endpoint "GET /v1/admin" {
                secure: [
                    Basic {
                        username: "admin",
                        password: "secret",
                        checks: [
                            basic.username == "admin"
                        ]
                    }
                ],

                respond 200 {
                    "url": api_base,
                    "limit": default_limit
                }
            }
        "#;

        let mut parser = Parser::new(Rodeo::new());
        let ast = parser.parse(source).expect("valid DSL should parse");

        assert_eq!(ast.gateway.env_file.as_deref(), Some(".env"));
        assert_eq!(ast.gateway.constants.len(), 2);
        let EndpointOption::Secure(rules) = &ast.endpoints[0].options[0] else {
            panic!("option should be secure");
        };
        assert!(rules[0].username.is_some());
        assert!(rules[0].password.is_some());
        assert_eq!(rules[0].checks.len(), 1);
    }

    #[test]
    fn parses_grpc_call_step() {
        let source = r#"
            gateway "api" { port: 8080 }

            endpoint "POST /v1/profile" {
                let payload = { "id": 42 };
                let profile = grpc::call("http://profiles:50051/profile.Profile/Get", payload);
                respond 200 { "profile": profile }
            }
        "#;

        let mut parser = Parser::new(Rodeo::new());
        let ast = parser.parse(source).expect("valid DSL should parse");

        assert_eq!(ast.endpoints[0].steps.len(), 2);
        assert!(matches!(ast.endpoints[0].steps[1], Step::CallGrpc { .. }));
    }

    #[test]
    fn parses_grpc_call_step_with_proto_path() {
        let source = r#"
            gateway "api" {
                port: 8080,
                protos: [
                    proto "profile_proto" { path: "./proto/profile.proto" }
                ],
            }

            endpoint "POST /v1/profile" {
                let payload = { "id": 42 };
                let profile = grpc::call(
                    "http://profiles:50051",
                    "profile_proto",
                    "profile.Profile",
                    "Get",
                    payload
                );
                respond 200 { "profile": profile }
            }
        "#;

        let mut parser = Parser::new(Rodeo::new());
        let ast = parser.parse(source).expect("valid DSL should parse");

        assert_eq!(ast.gateway.static_protos.len(), 1);
        assert_eq!(ast.endpoints[0].steps.len(), 2);
        assert!(matches!(ast.endpoints[0].steps[1], Step::CallGrpc { .. }));
    }

    #[test]
    fn parses_empty_endpoint_as_default_response() {
        let source = r#"
            gateway "api" { port: 8080 }

            endpoint "GET /api/v1/todos/:id" {
            }
        "#;

        let mut parser = Parser::new(Rodeo::new());
        let ast = parser.parse(source).expect("empty endpoint should parse");

        assert_eq!(ast.endpoints[0].method, "GET");
        assert_eq!(ast.endpoints[0].path, "/api/v1/todos/:id");
        assert_eq!(ast.endpoints[0].response.status, 200);
        assert!(ast.endpoints[0].response.body.is_none());
    }

    #[test]
    fn parses_response_body_headers_and_cookies() {
        let source = r#"
            gateway "api" { port: 8080 }

            endpoint "GET /x" {
                let token = "abc";
                respond 202
                    headers { "x-trace": token }
                    cookies { "session": token }
                    body { "ok": true }
            }
        "#;

        let mut parser = Parser::new(Rodeo::new());
        let ast = parser.parse(source).expect("response parts should parse");
        let response = &ast.endpoints[0].response;

        assert_eq!(response.status, 202);
        assert!(response.headers.contains_key("x-trace"));
        assert!(response.cookies.contains_key("session"));
        assert!(
            response
                .body
                .as_ref()
                .is_some_and(|body| body.contains_key("ok"))
        );
    }

    #[test]
    fn parses_status_only_response() {
        let source = r#"
            gateway "api" { port: 8080 }

            endpoint "DELETE /x" {
                respond 204
            }
        "#;

        let mut parser = Parser::new(Rodeo::new());
        let ast = parser
            .parse(source)
            .expect("status-only response should parse");
        let response = &ast.endpoints[0].response;

        assert_eq!(response.status, 204);
        assert!(response.body.is_none());
        assert!(response.headers.is_empty());
        assert!(response.cookies.is_empty());
    }
}
