use crate::ast::{
    Endpoint, EndpointOption, Expression, FileAST, GrpcConfig, HttpConfig, PipeOp, Step, Sym,
};
use lasso::Rodeo;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub struct LintWarning {
    pub rule: &'static str,
    pub message: String,
}

pub fn lint_file(ast: &FileAST, interner: &Rodeo) -> Vec<LintWarning> {
    let mut warnings = Vec::new();
    let endpoint_reads = endpoint_reads(ast);
    let used_constants = used_constants(ast, &endpoint_reads);

    lint_gateway(ast, interner, &used_constants, &mut warnings);
    lint_endpoints(ast, interner, &mut warnings);

    warnings
}

fn lint_gateway(
    ast: &FileAST,
    interner: &Rodeo,
    used_constants: &HashSet<Sym>,
    warnings: &mut Vec<LintWarning>,
) {
    if ast.gateway.host.as_deref() == Some("0.0.0.0") {
        warn(
            warnings,
            "public_bind",
            "gateway binds to 0.0.0.0; make sure this is intended",
        );
    }

    for constant in &ast.gateway.constants {
        if !used_constants.contains(&constant.name) {
            warn(
                warnings,
                "unused_constant",
                format!(
                    "gateway constant `{}` is not used by any endpoint",
                    interner.resolve(&constant.name)
                ),
            );
        }
    }

    let used_dbs = used_databases(ast, interner);
    for db in &ast.gateway.static_dbs {
        if !used_dbs.contains(&db.name) {
            warn(
                warnings,
                "unused_database",
                format!(
                    "database `{}` is declared but not used",
                    interner.resolve(&db.name)
                ),
            );
        }
    }

    let used_protos = used_protos(ast, interner);
    for proto in &ast.gateway.static_protos {
        if !used_protos.contains(&proto.name) {
            warn(
                warnings,
                "unused_proto",
                format!(
                    "proto `{}` is declared but not used",
                    interner.resolve(&proto.name)
                ),
            );
        }
    }
}

fn lint_endpoints(ast: &FileAST, interner: &Rodeo, warnings: &mut Vec<LintWarning>) {
    for endpoint in &ast.endpoints {
        let endpoint_name = format!("{} {}", endpoint.method, endpoint.path);
        if !has_security(endpoint) {
            warn(
                warnings,
                "public_endpoint",
                format!("endpoint `{endpoint_name}` has no security rule"),
            );
        }

        for step in &endpoint.steps {
            match step {
                Step::FetchHttp { var_name, config } => {
                    lint_http_fetch(&endpoint_name, interner.resolve(var_name), config, warnings);
                }
                Step::QueryDb { var_name, config } => {
                    if config.timeout_ms.is_none() {
                        warn(
                            warnings,
                            "db_without_timeout",
                            format!(
                                "db query `{}` in endpoint `{endpoint_name}` has no timeout",
                                interner.resolve(var_name)
                            ),
                        );
                    }
                }
                Step::CallGrpc { var_name, config } => {
                    lint_grpc_call(&endpoint_name, interner.resolve(var_name), config, warnings);
                }
                Step::Command { var_name, .. } => {
                    warn(
                        warnings,
                        "command_step",
                        format!(
                            "command `{}` in endpoint `{endpoint_name}` executes a local shell command",
                            interner.resolve(var_name)
                        ),
                    );
                }
                Step::Let { .. } | Step::Pipe { .. } => {}
            }
        }
    }
}

fn lint_http_fetch(
    endpoint_name: &str,
    var_name: &str,
    config: &HttpConfig,
    warnings: &mut Vec<LintWarning>,
) {
    if config.timeout_ms.is_none() {
        warn(
            warnings,
            "fetch_without_timeout",
            format!("fetch `{var_name}` in endpoint `{endpoint_name}` has no timeout"),
        );
    }

    if config.retries.unwrap_or_default() > 0 && config.delay_ms.is_none() {
        warn(
            warnings,
            "retry_without_delay",
            format!("fetch `{var_name}` in endpoint `{endpoint_name}` retries without delay"),
        );
    }

    if config.fallback.is_some() && config.retries.unwrap_or_default() == 0 {
        warn(
            warnings,
            "fallback_without_retry",
            format!("fetch `{var_name}` in endpoint `{endpoint_name}` has fallback but no retry"),
        );
    }
}

fn lint_grpc_call(
    endpoint_name: &str,
    var_name: &str,
    config: &GrpcConfig,
    warnings: &mut Vec<LintWarning>,
) {
    if config.timeout_ms.is_none() {
        warn(
            warnings,
            "grpc_without_timeout",
            format!("grpc call `{var_name}` in endpoint `{endpoint_name}` has no timeout"),
        );
    }

    if config.fallback.is_some() && config.timeout_ms.is_none() {
        warn(
            warnings,
            "fallback_without_timeout",
            format!(
                "grpc call `{var_name}` in endpoint `{endpoint_name}` has fallback but no timeout"
            ),
        );
    }
}

fn endpoint_reads(ast: &FileAST) -> HashSet<Sym> {
    let mut reads = HashSet::new();
    for endpoint in &ast.endpoints {
        collect_endpoint_reads(endpoint, &mut reads);
    }
    reads
}

fn collect_endpoint_reads(endpoint: &Endpoint, reads: &mut HashSet<Sym>) {
    for option in &endpoint.options {
        if let EndpointOption::Secure(rules) = option {
            for rule in rules {
                collect_optional_expr_reads(rule.secret.as_ref(), reads, &HashSet::new());
                collect_optional_expr_reads(rule.username.as_ref(), reads, &HashSet::new());
                collect_optional_expr_reads(rule.password.as_ref(), reads, &HashSet::new());
                for check in &rule.checks {
                    collect_expr_reads(check, reads, &HashSet::new());
                }
            }
        }
    }

    for step in &endpoint.steps {
        collect_step_reads(step, reads);
    }

    if let Some(body) = &endpoint.response.body {
        for expr in body.values() {
            collect_expr_reads(expr, reads, &HashSet::new());
        }
    }
    for expr in endpoint.response.headers.values() {
        collect_expr_reads(expr, reads, &HashSet::new());
    }
    for expr in endpoint.response.cookies.values() {
        collect_expr_reads(expr, reads, &HashSet::new());
    }
}

fn collect_step_reads(step: &Step, reads: &mut HashSet<Sym>) {
    match step {
        Step::Let { value, .. } => collect_expr_reads(value, reads, &HashSet::new()),
        Step::Command { .. } => {}
        Step::FetchHttp { config, .. } => {
            collect_expr_reads(&config.url, reads, &HashSet::new());
            collect_optional_expr_reads(config.body.as_ref(), reads, &HashSet::new());
            collect_optional_expr_reads(config.fallback.as_ref(), reads, &HashSet::new());
        }
        Step::CallGrpc { config, .. } => {
            collect_expr_reads(&config.payload, reads, &HashSet::new());
            collect_optional_expr_reads(config.fallback.as_ref(), reads, &HashSet::new());
        }
        Step::QueryDb { config, .. } => {
            collect_expr_reads(&config.db_source, reads, &HashSet::new());
            for param in &config.params {
                collect_expr_reads(param, reads, &HashSet::new());
            }
            collect_optional_expr_reads(config.fallback.as_ref(), reads, &HashSet::new());
        }
        Step::Pipe {
            source, operations, ..
        } => {
            collect_expr_reads(source, reads, &HashSet::new());
            for op in operations {
                collect_pipe_op_reads(op, reads);
            }
        }
    }
}

fn collect_pipe_op_reads(op: &PipeOp, reads: &mut HashSet<Sym>) {
    match op {
        PipeOp::Closure { param, value, .. } => collect_bound_expr_reads(*param, value, reads),
        PipeOp::Reduce {
            initial,
            acc,
            param,
            value,
            ..
        } => {
            collect_expr_reads(initial, reads, &HashSet::new());
            let mut bound = HashSet::new();
            bound.insert(*acc);
            bound.insert(*param);
            collect_expr_reads(value, reads, &bound);
        }
        PipeOp::Expr { value, .. } => {
            collect_expr_reads(value, reads, &HashSet::new());
        }
        PipeOp::None { .. } => {}
    }
}

fn collect_bound_expr_reads(param: Sym, expr: &Expression, reads: &mut HashSet<Sym>) {
    let mut bound = HashSet::new();
    bound.insert(param);
    collect_expr_reads(expr, reads, &bound);
}

fn collect_optional_expr_reads(
    expr: Option<&Expression>,
    reads: &mut HashSet<Sym>,
    bound: &HashSet<Sym>,
) {
    if let Some(expr) = expr {
        collect_expr_reads(expr, reads, bound);
    }
}

fn collect_expr_reads(expr: &Expression, reads: &mut HashSet<Sym>, bound: &HashSet<Sym>) {
    match expr {
        Expression::Variable(name) => {
            if !bound.contains(name) {
                reads.insert(*name);
            }
        }
        Expression::PropertyAccess(object, _) => collect_expr_reads(object, reads, bound),
        Expression::Call { callee, args } => {
            collect_call_callee_reads(callee, reads, bound);
            for arg in args {
                collect_expr_reads(arg, reads, bound);
            }
        }
        Expression::BinaryOp(left, _, right) => {
            collect_expr_reads(left, reads, bound);
            collect_expr_reads(right, reads, bound);
        }
        Expression::Object(fields) => {
            for expr in fields.values() {
                collect_expr_reads(expr, reads, bound);
            }
        }
        Expression::Array(items) => {
            for expr in items {
                collect_expr_reads(expr, reads, bound);
            }
        }
        Expression::Null
        | Expression::Number(_)
        | Expression::String(_)
        | Expression::Boolean(_) => {}
    }
}

fn collect_call_callee_reads(callee: &Expression, reads: &mut HashSet<Sym>, bound: &HashSet<Sym>) {
    match callee {
        Expression::PropertyAccess(object, _) => collect_expr_reads(object, reads, bound),
        Expression::Variable(_) => {}
        _ => collect_expr_reads(callee, reads, bound),
    }
}

fn used_constants(ast: &FileAST, endpoint_reads: &HashSet<Sym>) -> HashSet<Sym> {
    let constants = ast
        .gateway
        .constants
        .iter()
        .map(|constant| constant.name)
        .collect::<HashSet<_>>();
    let constant_reads = ast
        .gateway
        .constants
        .iter()
        .map(|constant| {
            let mut reads = HashSet::new();
            collect_expr_reads(&constant.value, &mut reads, &HashSet::new());
            (constant.name, reads)
        })
        .collect::<HashMap<_, _>>();
    let mut used = endpoint_reads
        .intersection(&constants)
        .copied()
        .collect::<HashSet<_>>();
    let mut changed = true;

    while changed {
        changed = false;
        for used_constant in used.clone() {
            if let Some(reads) = constant_reads.get(&used_constant) {
                for read in reads {
                    if constants.contains(read) && used.insert(*read) {
                        changed = true;
                    }
                }
            }
        }
    }

    used
}

fn used_databases(ast: &FileAST, interner: &Rodeo) -> HashSet<Sym> {
    let mut used = HashSet::new();

    for endpoint in &ast.endpoints {
        for step in &endpoint.steps {
            if let Step::QueryDb { config, .. } = step {
                match &config.db_source {
                    Expression::Variable(name) => {
                        used.insert(*name);
                    }
                    Expression::String(name) => {
                        for db in &ast.gateway.static_dbs {
                            if name == interner.resolve(&db.name) || name == &db.url {
                                used.insert(db.name);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    used
}

fn used_protos(ast: &FileAST, interner: &Rodeo) -> HashSet<Sym> {
    let mut used = HashSet::new();

    for endpoint in &ast.endpoints {
        for step in &endpoint.steps {
            if let Step::CallGrpc { config, .. } = step
                && let Some(proto_path) = &config.proto_path
            {
                for proto in &ast.gateway.static_protos {
                    if proto_path == interner.resolve(&proto.name) || proto_path == &proto.path {
                        used.insert(proto.name);
                    }
                }
            }
        }
    }

    used
}

fn has_security(endpoint: &Endpoint) -> bool {
    endpoint
        .options
        .iter()
        .any(|option| matches!(option, EndpointOption::Secure(_)))
}

fn warn(warnings: &mut Vec<LintWarning>, rule: &'static str, message: impl Into<String>) {
    warnings.push(LintWarning {
        rule,
        message: message.into(),
    });
}

#[cfg(test)]
mod tests {
    use super::lint_file;
    use crate::parser::Parser;
    use lasso::Rodeo;

    #[test]
    fn warns_about_public_fetch_without_timeout_and_unused_constant() {
        let source = r#"
            gateway "api" {
                port: 8080,
                constants: {
                    "api_base": "https://example.test",
                    "unused": "x"
                }
            }

            endpoint "GET /x" {
                fetch api_base + "/items" as items;
                respond 200 { "items": items }
            }
        "#;

        let warnings = lint(source);

        assert!(has_warning(&warnings, "public_endpoint"));
        assert!(has_warning(&warnings, "fetch_without_timeout"));
        assert!(has_warning(&warnings, "unused_constant"));
    }

    #[test]
    fn does_not_warn_about_used_database_and_proto() {
        let source = r#"
            gateway "api" {
                port: 8080,
                databases: [
                    sqlite "main" { url: "sqlite::memory:" }
                ],
                protos: [
                    proto "profile_proto" { path: "profile.proto" }
                ]
            }

            endpoint "GET /db" {
                secure: [ Basic { username: "a", password: "b" } ],
                let rows = db::query("main", "select 1");
                let profile = grpc::call(
                    "http://profiles:50051",
                    "profile_proto",
                    "profile.Profile",
                    "Get",
                    { "id": "1" }
                ) { timeout: 10ms };
                respond 200 { "rows": rows, "profile": profile }
            }
        "#;

        let warnings = lint(source);

        assert!(!has_warning(&warnings, "unused_database"));
        assert!(!has_warning(&warnings, "unused_proto"));
    }

    fn lint(source: &str) -> Vec<String> {
        let mut parser = Parser::new(Rodeo::new());
        let ast = parser.parse(source).expect("source should parse");
        lint_file(&ast, &parser.interner)
            .into_iter()
            .map(|warning| warning.rule.to_string())
            .collect()
    }

    fn has_warning(warnings: &[String], rule: &str) -> bool {
        warnings.iter().any(|warning| warning == rule)
    }
}
