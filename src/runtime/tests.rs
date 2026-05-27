use super::Runtime;
use crate::parser::Parser;
use crate::planner::build_plan;
use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use jsonwebtoken::{EncodingKey, Header, encode};
use lasso::Rodeo;
use serde_json::{Value, json};
use std::fs;
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

#[tokio::test]
async fn generated_axum_route_executes_database_query() {
    let source = r#"
            gateway "test" {
                port: 0,
                databases: [
                    sqlite "main" { url: "sqlite::memory:" }
                ],
            }

            endpoint "GET /db" {
                let rows = db::query("main", "select ? as answer, ? as label", 42, "ok");

                respond 200 {
                    "rows": rows
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
                .uri("/db")
                .body(axum::body::Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body should read");
    let actual: Value = serde_json::from_slice(&body).expect("body should be JSON");

    assert_eq!(
        actual,
        json!({
            "rows": [
                {
                    "answer": 42.0,
                    "label": "ok"
                }
            ]
        })
    );
}

#[tokio::test]
async fn basic_secure_rule_verifies_credentials_and_custom_checks() {
    let source = r#"
            gateway "test" { port: 0 }

            endpoint "GET /secure" {
                secure: [
                    Basic {
                        username: "admin",
                        password: "secret",
                        checks: [
                            basic.username == "admin"
                        ]
                    }
                ],
                respond 200 { "user": basic.username }
            }
        "#;

    let router = test_router(source);

    let unauthorized = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/secure")
                .body(axum::body::Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let authorized = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/secure")
                .header("authorization", basic_auth("admin", "secret"))
                .body(axum::body::Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");
    assert_eq!(authorized.status(), StatusCode::OK);

    let rejected = router
        .oneshot(
            Request::builder()
                .uri("/secure")
                .header("authorization", basic_auth("admin", "bad"))
                .body(axum::body::Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn jwt_secure_rule_verifies_token_and_custom_checks() {
    let source = r#"
            gateway "test" { port: 0 }

            endpoint "GET /secure/:id" {
                secure: [
                    JWT {
                        secret: "test-secret",
                        checks: [
                            jwt.role == "admin",
                            jwt.sub == id
                        ]
                    }
                ],

                respond 200 {
                    "sub": jwt.sub,
                    "role": jwt.role
                }
            }
        "#;

    let router = test_router(source);
    let token = test_jwt(json!({
        "sub": "42",
        "role": "admin",
        "exp": 4102444800u64
    }));

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/secure/42")
                .header("authorization", format!("Bearer {token}"))
                .body(axum::body::Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body should read");
    let actual: Value = serde_json::from_slice(&body).expect("body should be JSON");
    assert_eq!(actual, json!({ "sub": "42", "role": "admin" }));

    let forbidden_token = test_jwt(json!({
        "sub": "42",
        "role": "user",
        "exp": 4102444800u64
    }));
    let forbidden = router
        .oneshot(
            Request::builder()
                .uri("/secure/42")
                .header("authorization", format!("Bearer {forbidden_token}"))
                .body(axum::body::Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");

    assert_eq!(forbidden.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn gateway_env_file_and_constants_are_available_to_endpoints() {
    let env_path = std::env::temp_dir().join(format!(
        "velogate-test-{}-constants.env",
        std::process::id()
    ));
    fs::write(&env_path, "API_BASE=https://example.test\nTOKEN=abc\n")
        .expect("env file should write");
    let env_path = env_path.to_string_lossy().replace('\\', "/");
    let source = format!(
        r#"
            gateway "test" {{
                port: 0,
                env_file: "{env_path}",
                constants: {{
                    "api_base": env.API_BASE,
                    "limit": 25
                }}
            }}

            endpoint "GET /constants" {{
                respond 200 {{
                    "api_base": api_base,
                    "limit": limit,
                    "token": env.TOKEN
                }}
            }}
        "#
    );

    let router = test_router(&source);

    let response = router
        .oneshot(
            Request::builder()
                .uri("/constants")
                .body(axum::body::Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body should read");
    let actual: Value = serde_json::from_slice(&body).expect("body should be JSON");
    assert_eq!(
        actual,
        json!({
            "api_base": "https://example.test",
            "limit": 25.0,
            "token": "abc"
        })
    );

    let _ = fs::remove_file(env_path);
}

#[tokio::test]
async fn rate_limit_endpoint_returns_429_after_limit() {
    let source = r#"
            gateway "test" { port: 0 }

            endpoint "GET /limited" {
                rate_limit: 1/rps window 1s,
                respond 200 { "ok": true }
            }
        "#;

    let router = test_router(source);

    let first = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/limited")
                .body(axum::body::Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");
    assert_eq!(first.status(), StatusCode::OK);

    let second = router
        .oneshot(
            Request::builder()
                .uri("/limited")
                .body(axum::body::Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn request_context_exposes_path_query_headers_and_cookies() {
    let source = r#"
            gateway "test" { port: 0 }

            endpoint "GET /api/v1/todos/:id" {
                respond 200 {
                    "id": id,
                    "arg": query.arg,
                    "trace": headers.x_trace_id,
                    "session": cookies.session
                }
            }
        "#;

    let router = test_router(source);

    let response = router
        .oneshot(
            Request::builder()
                .uri("/api/v1/todos/42?arg=1")
                .header("x-trace-id", "abc")
                .header("cookie", "session=s1; theme=dark")
                .body(axum::body::Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body should read");
    let actual: Value = serde_json::from_slice(&body).expect("body should be JSON");

    assert_eq!(
        actual,
        json!({
            "id": "42",
            "arg": "1",
            "trace": "abc",
            "session": "s1"
        })
    );
}

#[tokio::test]
async fn endpoint_can_read_json_request_body_for_write_methods() {
    let source = r#"
            gateway "test" { port: 0 }

            endpoint "PATCH /api/v1/todos/:id" {
                respond 200 {
                    "id": id,
                    "title": body.title,
                    "done": body.done
                }
            }
        "#;

    let router = test_router(source);

    let response = router
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/todos/42")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    r#"{"title":"ship body support","done":true}"#,
                ))
                .expect("request should build"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body should read");
    let actual: Value = serde_json::from_slice(&body).expect("body should be JSON");

    assert_eq!(
        actual,
        json!({
            "id": "42",
            "title": "ship body support",
            "done": true
        })
    );
}

#[tokio::test]
async fn modulo_operator_evaluates_remainder() {
    let source = r#"
            gateway "test" { port: 0 }

            endpoint "GET /math" {
                let value = 10 % 4;
                let mixed = 10 + 5 % 3 * 2;

                respond 200 {
                    "value": value,
                    "mixed": mixed,
                    "is_odd": value == 1
                }
            }
        "#;

    let router = test_router(source);

    let response = router
        .oneshot(
            Request::builder()
                .uri("/math")
                .body(axum::body::Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::OK);

    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body should read");
    let actual: Value = serde_json::from_slice(&body).expect("body should be JSON");

    assert_eq!(
        actual,
        json!({
            "value": 2.0,
            "mixed": 14.0,
            "is_odd": false
        })
    );
}

#[tokio::test]
async fn builtins_and_take_can_read_variables() {
    let source = r#"
        gateway "test" {
            port: 0,
            constants: {
                "default_take": 2,
                "allowed": ["admin", "ops"]
            }
        }

        endpoint "GET /builtins/:role" {
            let names = [
                { "name": " Ada " },
                { "name": "Grace" },
                { "name": "Linus" }
            ];
            let selected = names
                | filter(item => item.name.trim().lower().contains("a"))
                | map(item => {
                    "raw": item.name,
                    "label": format("user: {}", item.name.trim().upper())
                })
                | take(default_take);

            respond 200 {
                "allowed": contains(allowed, role),
                "prefix": starts_with("admin-user", "admin"),
                "selected": selected
            }
        }
    "#;

    let router = test_router(source);

    let response = router
        .oneshot(
            Request::builder()
                .uri("/builtins/admin")
                .body(axum::body::Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body should read");
    let actual: Value = serde_json::from_slice(&body).expect("body should be JSON");

    assert_eq!(
        actual,
        json!({
            "allowed": true,
            "prefix": true,
            "selected": [
                { "raw": " Ada ", "label": "user: ADA" },
                { "raw": "Grace", "label": "user: GRACE" }
            ]
        })
    );
}

fn test_router(source: &str) -> axum::Router {
    let mut parser = Parser::new(Rodeo::new());
    let ast = parser.parse(source).expect("test DSL should parse");
    let plan = build_plan(&ast, &parser.interner).expect("test DSL should plan");
    Runtime::new(ast, parser.interner, plan)
        .router()
        .expect("router should build")
}

fn test_jwt(claims: Value) -> String {
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(b"test-secret"),
    )
    .expect("test JWT should encode")
}

fn basic_auth(username: &str, password: &str) -> String {
    format!(
        "Basic {}",
        BASE64.encode(format!("{username}:{password}").as_bytes())
    )
}
