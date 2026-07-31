//! Every knob [`ImageControllerConfig::from_env`] reads must be renderable by
//! the chart that runs the binary.
//!
//! A configuration option the server reads and no chart writes is not a
//! configuration option — it is a default with documentation. That failure has
//! already cost this deployment once: `DJINN_MAX_BUILD_PODS` was read by the
//! pod-permit gate, rendered by no chart, and its absence fail-closed every
//! dispatch while `build_capacity` still reported healthy.
//!
//! The required set is **derived** from `config.rs` rather than restated here,
//! so adding an env constant without wiring it into the Deployment fails this
//! test rather than shipping an unreachable knob — which is exactly how
//! `DJINN_IMAGE_LAUNCHER_AUTHORITY_PROTOCOL` could otherwise repeat the
//! hardcoded-declaration bug it exists to fix.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("the crate must live three levels below the repository root")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{relative} must exist and be readable: {error}"))
}

/// Every `pub const …: &str = "DJINN_…";` declared in `config::env`.
///
/// Whole-line comments are dropped so a commented-out constant is not counted
/// as a requirement.
fn declared_env_vars(source: &str) -> Vec<String> {
    let module = source
        .split_once("pub mod env {")
        .expect("config.rs must declare the env module")
        .1;
    let module = module
        .split_once("\n}\n")
        .expect("the env module must be closed")
        .0;

    module
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("//"))
        .filter_map(|line| {
            let (_, rest) = line.split_once("&str = \"")?;
            let (value, _) = rest.split_once('"')?;
            value.starts_with("DJINN_").then(|| value.to_string())
        })
        .collect()
}

/// MUTATION: delete the `DJINN_IMAGE_LAUNCHER_AUTHORITY_PROTOCOL` block from
/// `deployment-server.yaml` — a deployment could then no longer select the
/// protocol no matter what `values.yaml` says. This test names the variable and
/// fails.
#[test]
fn every_image_controller_env_var_is_rendered_by_the_server_deployment() {
    let config = read("server/crates/djinn-image-controller/src/config.rs");
    let deployment = read("deploy/helm/djinn/templates/deployment-server.yaml");

    let declared = declared_env_vars(&config);

    // Non-vacuity: a parser that matched nothing would pass the loop below
    // silently, which is the whole failure mode this file is about.
    assert!(
        declared.len() >= 15,
        "the env-constant scan found only {} variables — it stopped parsing config.rs: {declared:?}",
        declared.len()
    );
    assert!(
        declared
            .iter()
            .any(|v| v == "DJINN_IMAGE_LAUNCHER_AUTHORITY_PROTOCOL"),
        "the launcher-protocol knob must be among the derived requirements: {declared:?}"
    );

    for variable in declared {
        assert!(
            deployment.contains(&format!("name: {variable}")),
            "{variable} is read by ImageControllerConfig::from_env but no chart renders it — \
             a deployment cannot set it, so it is a hardcoded default wearing a config's name"
        );
    }
}

/// The knob is reachable from `values.yaml` too, and ships unset so an upgrade
/// changes no artifact and rebuilds no image.
///
/// MUTATION: default `launcherAuthorityProtocol` to `resize-v2` in
/// `values.yaml`. The emptiness assertion fails — which is the point: that
/// default would silently re-tag and rebuild every catalog image on upgrade,
/// and hand quota ownership to Kubernetes on clusters whose launcher still
/// writes leaf `cpu.max`.
#[test]
fn the_chart_exposes_the_protocol_and_defaults_it_to_unset() {
    let values = read("deploy/helm/djinn/values.yaml");
    let deployment = read("deploy/helm/djinn/templates/deployment-server.yaml");

    let line = values
        .lines()
        .find(|line| line.trim_start().starts_with("launcherAuthorityProtocol:"))
        .expect("values.yaml must expose imagePipeline.controller.launcherAuthorityProtocol");
    let value = line.split_once(':').unwrap().1.trim().trim_matches('"');
    assert!(
        value.is_empty(),
        "the chart must ship no protocol selection, got {value:?} — the built-in default is \
         what every existing deployment already builds"
    );

    assert!(
        deployment.contains(".Values.imagePipeline.controller.launcherAuthorityProtocol"),
        "the Deployment must render the value the chart exposes, not a literal"
    );
}
