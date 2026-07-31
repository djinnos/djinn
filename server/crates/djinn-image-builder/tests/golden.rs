//! Golden-file tests: render a representative `EnvironmentConfig` and
//! compare the Dockerfile output to a committed fixture. Regressions
//! show up as a readable diff in the PR.
//!
//! To regenerate the fixture after an intentional generator change:
//! `REGENERATE_GOLDEN=1 cargo test -p djinn-image-builder --test golden`.

use std::fs;
use std::path::PathBuf;

use djinn_image_builder::{AgentWorkerImage, DEFAULT_LAUNCHER_PROTOCOL, generate_dockerfile};
use djinn_launcher_protocol::LauncherAuthorityProtocol;
use djinn_stack::environment::EnvironmentConfig;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden_fixtures")
}

fn agent_worker() -> AgentWorkerImage {
    AgentWorkerImage::new("djinn/agent-worker", "sha256-golden")
}

fn load_config(name: &str) -> EnvironmentConfig {
    let config_path = fixture_dir().join(format!("{name}.config.json"));
    let config_raw =
        fs::read_to_string(&config_path).unwrap_or_else(|e| panic!("read {config_path:?}: {e}"));
    serde_json::from_str(&config_raw).unwrap_or_else(|e| panic!("parse {config_path:?}: {e}"))
}

fn run_golden(name: &str) {
    let dockerfile_path = fixture_dir().join(format!("{name}.Dockerfile"));
    let config = load_config(name);

    // The committed fixtures are what a deployment that configures nothing
    // builds. Rendering them under the default is what makes them evidence that
    // making the protocol configurable changed no existing artifact.
    let rendered = generate_dockerfile(&config, &agent_worker(), DEFAULT_LAUNCHER_PROTOCOL)
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

/// Configuring `resize-v2` changes the artifact in exactly one way: it declares
/// `resize-v2`. Diffed against the committed `leaf-v1` fixture rather than a
/// second fixture, so the assertion is "nothing else moved" and not "these two
/// files happen to differ".
///
/// MUTATION: hardcode the declaration in `emit_launcher_protocol` again — the
/// rendered output equals the leaf-v1 fixture and the first assertion fails,
/// naming `resize-v2` as absent.
#[test]
fn configuring_resize_v2_changes_only_the_declaration() {
    for name in ["single_rust", "polyglot_monorepo"] {
        let config = load_config(name);
        let leaf = fs::read_to_string(fixture_dir().join(format!("{name}.Dockerfile")))
            .unwrap_or_else(|e| panic!("read {name} fixture: {e}"));

        let resize = generate_dockerfile(
            &config,
            &agent_worker(),
            LauncherAuthorityProtocol::ResizeV2,
        )
        .unwrap_or_else(|e| panic!("generate {name}: {e}"))
        .dockerfile;

        assert!(
            resize.contains("resize-v2"),
            "{name}: the configured protocol never reached the artifact:\n{resize}"
        );
        assert!(
            !resize.contains("leaf-v1"),
            "{name}: the artifact still claims leaf-v1 somewhere:\n{resize}"
        );
        pretty_assertions::assert_eq!(
            leaf.replace("leaf-v1", "resize-v2"),
            resize,
            "{}: selecting a protocol must change the declaration and nothing else",
            name
        );
    }
}
