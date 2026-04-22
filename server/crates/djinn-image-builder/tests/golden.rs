//! Golden-file tests: render a representative `EnvironmentConfig` and
//! compare the Dockerfile output to a committed fixture. Regressions
//! show up as a readable diff in the PR.
//!
//! To regenerate the fixture after an intentional generator change:
//! `REGENERATE_GOLDEN=1 cargo test -p djinn-image-builder --test golden`.

use std::fs;
use std::path::PathBuf;

use djinn_image_builder::{AgentWorkerImage, generate_dockerfile};
use djinn_stack::environment::EnvironmentConfig;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden_fixtures")
}

fn agent_worker() -> AgentWorkerImage {
    AgentWorkerImage::new("djinn/agent-worker", "sha256-golden")
}

fn run_golden(name: &str) {
    let dir = fixture_dir();
    let config_path = dir.join(format!("{name}.config.json"));
    let dockerfile_path = dir.join(format!("{name}.Dockerfile"));

    let config_raw =
        fs::read_to_string(&config_path).unwrap_or_else(|e| panic!("read {config_path:?}: {e}"));
    let config: EnvironmentConfig = serde_json::from_str(&config_raw)
        .unwrap_or_else(|e| panic!("parse {config_path:?}: {e}"));

    let rendered = generate_dockerfile(&config, &agent_worker())
        .unwrap_or_else(|e| panic!("generate {name}: {e}"));

    if std::env::var("REGENERATE_GOLDEN").is_ok() {
        fs::write(&dockerfile_path, &rendered.dockerfile).unwrap();
        return;
    }

    let expected = fs::read_to_string(&dockerfile_path)
        .unwrap_or_else(|e| panic!("read {dockerfile_path:?}: {e}"));
    pretty_assertions::assert_eq!(expected, rendered.dockerfile);
}

#[test]
fn polyglot_monorepo_two_rust_toolchains() {
    run_golden("polyglot_monorepo");
}

#[test]
fn single_rust_minimal() {
    run_golden("single_rust");
}
