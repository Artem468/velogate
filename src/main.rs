use ariadne::{Color, Label, Report, ReportKind, Source};
use clap::{Args, Parser as ClapParser, Subcommand};
use lasso::Rodeo;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use velogate::ast::{EndpointOption, FileAST};
use velogate::export::export_file;
use velogate::linter::{LintWarning, lint_file};
use velogate::parser::{ParseDiagnostic, Parser};
use velogate::planner::{ExecutionPlan, PlanError, build_plan, export_plan_dot};
use velogate::runtime::Runtime;
use velogate::validator::{ValidationError, validate_file};

#[derive(ClapParser, Debug)]
#[command(name = "velogate")]
#[command(author = "Your Name")]
#[command(version = "0.1.0")]
#[command(about = "High-performance declarative API Gateway & BFF compiler", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Validate a .gate file without starting the server.
    Check(CheckArgs),

    /// Format a .gate file in place or check formatting.
    Fmt(FmtArgs),

    /// Run static lint warnings after syntax, semantic and planning checks.
    Lint(CheckArgs),

    /// Print endpoint routes, auth, rate limits and step dependencies.
    Routes(CheckArgs),

    /// Inspect config health and resolved inputs.
    Doctor(CheckArgs),

    /// Start the API gateway runtime.
    Start(StartArgs),

    /// Export internal representation.
    Dump(DumpArgs),
}

#[derive(Args, Debug)]
struct CheckArgs {
    /// Path to a .gate config file.
    #[arg(short, long, value_name = "FILE")]
    config: PathBuf,
}

#[derive(Args, Debug)]
struct FmtArgs {
    /// Path to a .gate config file.
    #[arg(short, long, value_name = "FILE")]
    config: PathBuf,

    /// Check formatting without writing changes.
    #[arg(long)]
    check: bool,
}

#[derive(Args, Debug)]
struct StartArgs {
    /// Path to a .gate config file.
    #[arg(short, long, value_name = "FILE")]
    config: PathBuf,

    /// Tokio worker thread count.
    #[arg(short, long)]
    workers: Option<usize>,

    /// Path to an environment config file.
    #[arg(long, value_name = "ENV_FILE")]
    env_file: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum DumpFormat {
    Ast,
    Json,
    Graph,
    Plan,
}

#[derive(Args, Debug)]
struct DumpArgs {
    /// Path to a .gate config file.
    #[arg(short, long, value_name = "FILE")]
    config: PathBuf,

    /// Output format.
    #[arg(short, long, value_enum, default_value_t = DumpFormat::Ast)]
    format: DumpFormat,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(()) => ExitCode::FAILURE,
    }
}

fn run(cli: Cli) -> Result<(), ()> {
    match cli.command {
        Commands::Check(args) => {
            let _ = parse_config(&args.config)?;
            println!("syntax and plan ok: {}", args.config.display());
            Ok(())
        }
        Commands::Fmt(args) => fmt_config(args),
        Commands::Lint(args) => lint(args),
        Commands::Routes(args) => routes(args),
        Commands::Doctor(args) => doctor(args),
        Commands::Start(args) => start(args),
        Commands::Dump(args) => dump(args),
    }
}

fn fmt_config(args: FmtArgs) -> Result<(), ()> {
    let source = fs::read_to_string(&args.config).map_err(|err| {
        eprintln!("failed to read {}: {err}", args.config.display());
    })?;
    let formatted = format_gate_source(&source);

    if args.check {
        if source == formatted {
            println!("format ok: {}", args.config.display());
            return Ok(());
        }
        eprintln!("format check failed: {}", args.config.display());
        return Err(());
    }

    if source != formatted {
        fs::write(&args.config, formatted).map_err(|err| {
            eprintln!("failed to write {}: {err}", args.config.display());
        })?;
        println!("formatted: {}", args.config.display());
    } else {
        println!("format ok: {}", args.config.display());
    }

    Ok(())
}

fn lint(args: CheckArgs) -> Result<(), ()> {
    let parsed = parse_config(&args.config)?;
    let warnings = lint_file(&parsed.ast, &parsed.parser.interner);
    emit_lint_warnings(&warnings);
    println!(
        "lint ok: {} ({} endpoint(s), {} planned step(s), {} warning(s))",
        args.config.display(),
        parsed.ast.endpoints.len(),
        parsed
            .plan
            .endpoints
            .iter()
            .map(|endpoint| endpoint.steps.len())
            .sum::<usize>(),
        warnings.len()
    );
    Ok(())
}

fn routes(args: CheckArgs) -> Result<(), ()> {
    let ParsedConfig { ast, parser, plan } = parse_config(&args.config)?;

    for (idx, endpoint) in ast.endpoints.iter().enumerate() {
        println!("{} {}", endpoint.method, endpoint.path);
        println!("  response: {}", endpoint.response.status);
        println!("  auth: {}", route_auth(endpoint, &parser.interner));
        println!(
            "  rate_limit: {}",
            route_rate_limit(endpoint, &parser.interner)
        );

        if let Some(endpoint_plan) = plan.endpoints.get(idx) {
            println!("  steps:");
            if endpoint_plan.steps.is_empty() {
                println!("    - none");
            }
            for step in &endpoint_plan.steps {
                let deps = if step.dependencies.is_empty() {
                    "none".to_string()
                } else {
                    step.dependencies
                        .iter()
                        .map(|dep| endpoint_plan.steps[*dep].produces.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                println!(
                    "    - #{} {} {:?} deps: {}",
                    step.index, step.produces, step.kind, deps
                );
            }

            println!("  layers:");
            if endpoint_plan.layers.is_empty() {
                println!("    - none");
            }
            for (layer_idx, layer) in endpoint_plan.layers.iter().enumerate() {
                let names = layer
                    .iter()
                    .map(|step_idx| endpoint_plan.steps[*step_idx].produces.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("    - layer {layer_idx}: {names}");
            }
        }
    }

    Ok(())
}

fn doctor(args: CheckArgs) -> Result<(), ()> {
    let ParsedConfig { ast, plan, .. } = parse_config(&args.config)?;
    let host = ast.gateway.host.as_deref().unwrap_or("127.0.0.1");

    println!("doctor ok: {}", args.config.display());
    println!("gateway: {}", ast.gateway.name);
    println!("bind: {host}:{}", ast.gateway.port);
    println!(
        "env_file: {}",
        ast.gateway.env_file.as_deref().unwrap_or("none")
    );
    println!("constants: {}", ast.gateway.constants.len());
    println!("databases: {}", ast.gateway.static_dbs.len());
    println!("protos: {}", ast.gateway.static_protos.len());
    println!("endpoints: {}", ast.endpoints.len());
    println!(
        "planned_steps: {}",
        plan.endpoints
            .iter()
            .map(|endpoint| endpoint.steps.len())
            .sum::<usize>()
    );

    Ok(())
}

fn dump(args: DumpArgs) -> Result<(), ()> {
    let ParsedConfig { ast, parser, plan } = parse_config(&args.config)?;
    let export = export_file(&ast, &parser.interner);

    match args.format {
        DumpFormat::Ast => println!("{export:#?}"),
        DumpFormat::Json => {
            let json = serde_json::to_string_pretty(&export).map_err(|err| {
                eprintln!("failed to serialize AST as JSON: {err}");
            })?;
            println!("{json}");
        }
        DumpFormat::Graph => print!("{}", export_plan_dot(&plan)),
        DumpFormat::Plan => {
            let json = serde_json::to_string_pretty(&plan).map_err(|err| {
                eprintln!("failed to serialize execution plan as JSON: {err}");
            })?;
            println!("{json}");
        }
    }

    Ok(())
}

fn start(args: StartArgs) -> Result<(), ()> {
    let ParsedConfig {
        mut ast,
        parser,
        plan,
    } = parse_config(&args.config)?;
    if let Some(env_file) = args.env_file {
        ast.gateway.env_file = Some(
            resolve_config_path(&args.config, &env_file)
                .display()
                .to_string(),
        );
    }

    let mut runtime_builder = tokio::runtime::Builder::new_multi_thread();
    runtime_builder.enable_all();

    if let Some(worker_threads) = args.workers {
        runtime_builder.worker_threads(worker_threads);
    }

    let rt = runtime_builder.build().map_err(|err| {
        eprintln!("failed to initialize Tokio runtime: {err}");
    })?;

    rt.block_on(async {
        let actual_workers = tokio::runtime::Handle::current().metrics().num_workers();
        tracing::info!(workers = actual_workers, "velogate runtime started");

        let runtime = Runtime::new(ast, parser.interner, plan);
        if let Err(err) = runtime.serve().await {
            tracing::error!(error = %err, "velogate runtime failed");
        }
        tracing::info!("velogate runtime stopped");
    });

    Ok(())
}

fn format_gate_source(source: &str) -> String {
    let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::new();
    let mut blank_count = 0usize;

    for line in normalized.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_count += 1;
            if blank_count <= 1 && !out.is_empty() {
                out.push('\n');
            }
            continue;
        }

        blank_count = 0;
        out.push_str(trimmed);
        out.push('\n');
    }

    out
}

fn route_auth(endpoint: &velogate::ast::Endpoint, interner: &Rodeo) -> String {
    let schemes = endpoint
        .options
        .iter()
        .filter_map(|option| match option {
            EndpointOption::Secure(rules) => Some(
                rules
                    .iter()
                    .map(|rule| interner.resolve(&rule.scheme))
                    .collect::<Vec<_>>(),
            ),
            EndpointOption::RateLimit { .. } => None,
        })
        .flatten()
        .collect::<Vec<_>>();

    if schemes.is_empty() {
        "none".to_string()
    } else {
        schemes.join(", ")
    }
}

fn route_rate_limit(endpoint: &velogate::ast::Endpoint, interner: &Rodeo) -> String {
    endpoint
        .options
        .iter()
        .find_map(|option| match option {
            EndpointOption::RateLimit {
                limit,
                unit,
                window_ms,
            } => Some(format!(
                "{limit}/{} window {window_ms}ms by ip",
                interner.resolve(unit)
            )),
            EndpointOption::Secure(_) => None,
        })
        .unwrap_or_else(|| "none".to_string())
}

struct ParsedConfig {
    parser: Parser,
    ast: FileAST,
    plan: ExecutionPlan,
}

fn parse_config(path: &Path) -> Result<ParsedConfig, ()> {
    let source = fs::read_to_string(path).map_err(|err| {
        eprintln!("failed to read {}: {err}", path.display());
    })?;

    let mut parser = Parser::new(Rodeo::new());
    match parser.parse(&source) {
        Ok(mut ast) => {
            resolve_gateway_paths(path, &mut ast);
            let validation_errors = validate_file(&ast, &parser.interner, path);
            if !validation_errors.is_empty() {
                emit_validation_errors(&validation_errors);
                return Err(());
            }
            let plan = build_plan(&ast, &parser.interner).map_err(|err| {
                emit_plan_error(&err);
            })?;
            Ok(ParsedConfig { parser, ast, plan })
        }
        Err(diagnostic) => {
            emit_parse_error(path, &source, &diagnostic);
            Err(())
        }
    }
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
        path.to_path_buf()
    } else {
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
}

fn emit_plan_error(error: &PlanError) {
    match error {
        PlanError::DuplicateVariable {
            endpoint,
            variable,
            first_step,
            second_step,
        } => {
            eprintln!(
                "plan error: endpoint `{endpoint}` defines `{variable}` twice: steps {first_step} and {second_step}"
            );
        }
        PlanError::UndefinedVariable {
            endpoint,
            variable,
            used_by,
        } => {
            eprintln!(
                "plan error: endpoint `{endpoint}` uses undefined variable `{variable}` in `{used_by}`"
            );
        }
        PlanError::Cycle { endpoint } => {
            eprintln!("plan error: endpoint `{endpoint}` has a cyclic dependency graph");
        }
    }
}

fn emit_validation_errors(errors: &[ValidationError]) {
    for error in errors {
        eprintln!("validation error: {}", error.message);
    }
}

fn emit_lint_warnings(warnings: &[LintWarning]) {
    for warning in warnings {
        eprintln!("lint warning [{}]: {}", warning.rule, warning.message);
    }
}

fn emit_parse_error(path: &Path, source: &str, diagnostic: &ParseDiagnostic) {
    let file_id = path.display().to_string();
    let display_span = byte_span_to_char_span(source, diagnostic.span.clone());
    let expected = if diagnostic.expected.is_empty() {
        None
    } else {
        Some(format!(
            "expected one of: {}",
            diagnostic.expected.join(", ")
        ))
    };

    let mut report = Report::build(ReportKind::Error, (file_id.as_str(), display_span.clone()))
        .with_message("syntax error")
        .with_label(
            Label::new((file_id.as_str(), display_span))
                .with_message(diagnostic.message.clone())
                .with_color(Color::Red),
        );

    if let Some(expected) = expected {
        report = report.with_note(expected);
    }

    if let Err(err) = report
        .finish()
        .eprint((file_id.as_str(), Source::from(source)))
    {
        eprintln!("failed to render diagnostic: {err}");
        eprintln!("syntax error: {}", diagnostic.message);
    }
}

fn byte_span_to_char_span(source: &str, span: std::ops::Range<usize>) -> std::ops::Range<usize> {
    byte_offset_to_char_offset(source, span.start)..byte_offset_to_char_offset(source, span.end)
}

fn byte_offset_to_char_offset(source: &str, offset: usize) -> usize {
    let offset = offset.min(source.len());
    source[..offset].chars().count()
}
