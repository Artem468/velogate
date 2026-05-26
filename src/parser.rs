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
    use crate::ast::{Expression, Step};
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
        assert_eq!(endpoint.response_status, 200);
        assert_eq!(endpoint.steps.len(), 2);
        assert!(matches!(endpoint.steps[0], Step::Let { .. }));
        assert!(matches!(endpoint.steps[1], Step::FetchHttp { .. }));
        assert!(matches!(
            endpoint.response_body.get("ok"),
            Some(Expression::Boolean(true))
        ));
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
}
