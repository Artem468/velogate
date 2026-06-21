use std::path::{Path, PathBuf};
use std::process::Command;

const CONFIG: &str = "tests/fixtures/dump.gate";

#[test]
fn dump_json_matches_fixture() {
    assert_dump_matches("json", "tests/fixtures/dump.json");
}

#[test]
fn dump_openapi_matches_fixture() {
    assert_dump_matches("openapi", "tests/fixtures/dump.openapi.json");
}

#[test]
fn dump_graph_matches_fixture() {
    assert_dump_matches("graph", "tests/fixtures/dump.graph.dot");
}

fn assert_dump_matches(format: &str, fixture: &str) {
    let output = Command::new(binary_path())
        .args(["dump", "--config", CONFIG, "--format", format])
        .output()
        .unwrap_or_else(|err| panic!("failed to run velogate dump {format}: {err}"));

    assert!(
        output.status.success(),
        "velogate dump {format} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let actual = normalize_newlines(&String::from_utf8(output.stdout).expect("stdout is utf-8"));
    let expected =
        normalize_newlines(&std::fs::read_to_string(fixture).expect("fixture should read"));

    assert_eq!(actual, expected, "dump {format} changed");
}

fn binary_path() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_velogate")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/velogate"))
}

fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n")
}
