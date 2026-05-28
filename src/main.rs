use ariadne::{Color, Label, Report, ReportKind, Source};
use clap::{Args, Parser as ClapParser, Subcommand};
use lasso::Rodeo;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use velogate::ast::FileAST;
use velogate::export::export_file;
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
        Commands::Start(args) => start(args),
        Commands::Dump(args) => dump(args),
    }
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
