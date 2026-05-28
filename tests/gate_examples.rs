use lasso::Rodeo;
use std::fs;
use std::path::Path;
use velogate::parser::Parser;
use velogate::planner::build_plan;

const EXAMPLES: &[&str] = &[
    "examples/main.gate",
    "examples/cases/01_read_routes.gate",
    "examples/cases/02_write_body.gate",
    "examples/cases/03_jwt_secure.gate",
    "examples/cases/04_basic_env_constants.gate",
    "examples/cases/05_builtins_and_variable_take.gate",
    "examples/cases/06_sync_and_command.gate",
];

#[test]
fn gate_examples_parse_and_plan() {
    for path in EXAMPLES {
        parse_and_plan(Path::new(path));
    }
}

fn parse_and_plan(path: &Path) {
    let source = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let mut parser = Parser::new(Rodeo::new());
    let ast = parser
        .parse(&source)
        .unwrap_or_else(|err| panic!("failed to parse {}: {err:?}", path.display()));
    build_plan(&ast, &parser.interner)
        .unwrap_or_else(|err| panic!("failed to plan {}: {err:?}", path.display()));
}
