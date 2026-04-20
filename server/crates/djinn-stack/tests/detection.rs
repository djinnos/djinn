//! Fixture-driven detection tests.
//!
//! Each fixture under `tests/fixtures/<name>/` is a plain directory
//! tree (marker manifests + a few source files) plus an
//! `expected.json` that captures the stable shape of the resulting
//! [`Stack`] — modulo fields whose value is environment-dependent
//! (`detected_at`, byte counts, percentages).

use std::path::PathBuf;

use djinn_stack::{detect, Stack};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Expected {
    primary_language: Option<String>,
    /// Names in byte-share-descending order. The test asserts the
    /// detected language-name vector equals this, without checking
    /// exact byte counts.
    languages: Vec<String>,
    package_managers: Vec<String>,
    monorepo_tools: Vec<String>,
    is_monorepo: bool,
    test_runners: Vec<String>,
    frameworks: Vec<String>,
    runtimes: ExpectedRuntimes,
    manifest_signals: ExpectedSignals,
}

#[derive(Debug, Deserialize)]
struct ExpectedRuntimes {
    node: Option<String>,
    rust: Option<String>,
    python: Option<String>,
    go: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExpectedSignals {
    has_package_json: bool,
    has_cargo_toml: bool,
    has_pyproject_toml: bool,
    has_go_mod: bool,
    has_pnpm_workspace: bool,
    has_turbo_json: bool,
    has_devcontainer: bool,
    has_devcontainer_lock: bool,
}

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

async fn assert_fixture(name: &str) {
    let dir = fixtures_root().join(name);
    let stack: Stack = detect(&dir).await.expect("detect() succeeds");
    let raw = std::fs::read_to_string(dir.join("expected.json")).expect("read expected.json");
    let expected: Expected = serde_json::from_str(&raw).expect("parse expected.json");

    assert_eq!(
        stack.primary_language, expected.primary_language,
        "{name}: primary_language"
    );
    let detected_lang_names: Vec<String> =
        stack.languages.iter().map(|l| l.name.clone()).collect();
    assert_eq!(
        detected_lang_names, expected.languages,
        "{name}: language byte-share order"
    );
    assert_eq!(
        stack.package_managers, expected.package_managers,
        "{name}: package_managers"
    );
    assert_eq!(
        stack.monorepo_tools, expected.monorepo_tools,
        "{name}: monorepo_tools"
    );
    assert_eq!(stack.is_monorepo, expected.is_monorepo, "{name}: is_monorepo");
    assert_eq!(stack.test_runners, expected.test_runners, "{name}: test_runners");
    assert_eq!(stack.frameworks, expected.frameworks, "{name}: frameworks");
    assert_eq!(stack.runtimes.node, expected.runtimes.node, "{name}: node runtime");
    assert_eq!(stack.runtimes.rust, expected.runtimes.rust, "{name}: rust runtime");
    assert_eq!(stack.runtimes.python, expected.runtimes.python, "{name}: python runtime");
    assert_eq!(stack.runtimes.go, expected.runtimes.go, "{name}: go runtime");

    let got = &stack.manifest_signals;
    assert_eq!(got.has_package_json, expected.manifest_signals.has_package_json);
    assert_eq!(got.has_cargo_toml, expected.manifest_signals.has_cargo_toml);
    assert_eq!(got.has_pyproject_toml, expected.manifest_signals.has_pyproject_toml);
    assert_eq!(got.has_go_mod, expected.manifest_signals.has_go_mod);
    assert_eq!(got.has_pnpm_workspace, expected.manifest_signals.has_pnpm_workspace);
    assert_eq!(got.has_turbo_json, expected.manifest_signals.has_turbo_json);
    assert_eq!(got.has_devcontainer, expected.manifest_signals.has_devcontainer);
    assert_eq!(got.has_devcontainer_lock, expected.manifest_signals.has_devcontainer_lock);
}

#[tokio::test]
async fn rust_only_fixture() {
    assert_fixture("rust_only").await;
}

#[tokio::test]
async fn ts_pnpm_fixture() {
    assert_fixture("ts_pnpm").await;
}

#[tokio::test]
async fn polyglot_fixture() {
    assert_fixture("polyglot").await;
}
