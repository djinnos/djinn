//! Cargo cache policy resolver — project-agnostic detection of per-project
//! cargo build strategy.
//!
//! Detects from workspace layout and `.cargo/config.toml`.  Pure/unit-
//! testable; never mutates any project file.

use std::path::{Path, PathBuf};

use djinn_stack::environment::EnvironmentConfig;

/// Resolved per-project cargo cache strategy.
///
/// All fields are derived from detection, not hardcoded.  The warm and worker
/// paths use the same `CargoCachePolicy` so their feature sets and command
/// shapes stay consistent. (sccache/incremental are no longer policy knobs —
/// the platform forces `CARGO_INCREMENTAL=1` + `RUSTC_WRAPPER=""` on every
/// warm/verify/worker pod, see PR #874.)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CargoCachePolicy {
    /// Whether the project uses a cargo workspace (multiple crates) or a
    /// single crate.
    pub workspace: bool,
    /// Feature flags to pass to warm and worker compiles.  Empty means
    /// default features only.
    pub features: Vec<String>,
    /// Whether to pass `--all-features` to cargo commands.
    pub all_features: bool,
    /// The warm-base commands derived from the project's detected shape.
    pub warm_commands: Vec<CargoWarmCommand>,
}

impl CargoCachePolicy {
    /// Return the feature flags as CLI arguments for cargo commands.
    ///
    /// * Empty vec → default features (no extra flags).
    /// * `["--all-features"]` → all features enabled.
    /// * `["--features", "foo,bar"]` → named features from `.cargo/config.toml`.
    pub fn features(&self) -> Vec<String> {
        if self.all_features {
            vec!["--all-features".to_string()]
        } else if !self.features.is_empty() {
            let joined = self.features.join(",");
            vec!["--features".to_string(), joined]
        } else {
            Vec::new()
        }
    }
}

/// One warm-base compile step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoWarmCommand {
    /// Human label for logs/metrics.
    pub label: &'static str,
    /// Cargo subcommand and non-feature arguments
    /// (e.g. `["clippy", "--workspace", "--all-targets"]`).
    pub args: Vec<String>,
    /// Feature-flag arguments carried by this specific command.
    /// Separate from `args` so that `warm_cargo_target_base` can build the
    /// full argv without double-chaining `policy.features()`.
    pub feature_args: Vec<String>,
}

/// Resolve the cargo cache policy for a project root.
///
/// Reads (but never writes):
/// * `Cargo.toml` — workspace section detection.
/// * `.cargo/config.toml` — `build.features`.
/// * `env_config` — Rust workspace root for workspace directory resolution.
///
/// Returns `None` when no cargo workspace exists (non-Rust repo).
pub fn resolve_cargo_cache_policy(
    project_root: &Path,
    env_config: Option<&EnvironmentConfig>,
) -> Option<CargoCachePolicy> {
    if let Some(cfg) = env_config
        && let Some(djinn_stack::environment::CargoCachePolicy::Explicit(override_policy)) =
            &cfg.cargo_cache_policy
    {
        return Some(CargoCachePolicy {
            workspace: override_policy.workspace,
            features: override_policy.features.clone(),
            all_features: override_policy.all_features,
            warm_commands: override_policy
                .warm_commands
                .iter()
                .map(|command| CargoWarmCommand {
                    label: "override",
                    args: command.args.clone(),
                    feature_args: Vec::new(),
                })
                .collect(),
        });
    }

    let workspace_dir = resolve_cargo_workspace_dir(project_root, env_config)?;

    let is_workspace = detect_workspace_layout(&workspace_dir);
    let cargo_config = read_cargo_config_toml(&workspace_dir);
    let config_features = cargo_config
        .as_ref()
        .map(|c| c.features.clone())
        .unwrap_or_default();
    // Only the explicit `.cargo/config.toml` `build.features = "all-features"`
    // override can opt into an all-features warm.
    let all_features = config_features.contains(&"all-features".to_string());

    let features = if all_features {
        Vec::new()
    } else {
        config_features
    };

    let nextest = detect_nextest(&workspace_dir);

    let warm_commands = build_warm_commands(is_workspace, all_features, &features, nextest);

    Some(CargoCachePolicy {
        workspace: is_workspace,
        features,
        all_features,
        warm_commands,
    })
}

// ---------------------------------------------------------------------------
// Internal detection helpers
// ---------------------------------------------------------------------------

/// Parsed subset of `.cargo/config.toml` relevant to cache policy.
#[derive(Debug, Default, PartialEq, Eq)]
struct CargoConfigToml {
    features: Vec<String>,
}

fn read_cargo_config_toml(workspace_dir: &Path) -> Option<CargoConfigToml> {
    let path = workspace_dir.join(".cargo").join("config.toml");
    let text = std::fs::read_to_string(&path).ok()?;
    let raw: toml::Table = toml::from_str(&text).ok()?;

    let mut out = CargoConfigToml::default();

    if let Some(build) = raw.get("build").and_then(|v| v.as_table()) {
        if let Some(features) = build.get("features").and_then(|v| v.as_str()) {
            out.features = features.split_whitespace().map(|s| s.to_string()).collect();
        }
        if let Some(features_arr) = build.get("features").and_then(|v| v.as_array()) {
            out.features = features_arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
        }
    }

    Some(out)
}

fn detect_workspace_layout(workspace_dir: &Path) -> bool {
    let cargo_toml = workspace_dir.join("Cargo.toml");
    let text = match std::fs::read_to_string(&cargo_toml) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let raw: toml::Table = match toml::from_str(&text) {
        Ok(t) => t,
        Err(_) => return false,
    };
    raw.contains_key("workspace")
}

// All-features warm is now opt-in only via `.cargo/config.toml`
// `build.features = "all-features"`.
/// Detect whether the project uses `cargo-nextest` (a `.config/nextest.toml` or
/// `nextest.toml` at the cargo workspace root). Drives whether the test-compile
/// warm step uses `cargo nextest run --no-run` vs `cargo test --no-run`.
fn detect_nextest(workspace_dir: &Path) -> bool {
    djinn_stack::test_runners::NEXTEST_CONFIG_PATHS
        .iter()
        .any(|rel| workspace_dir.join(rel).is_file())
}

/// Build the test-compile warm command. Compiles (does NOT run) the test
/// binaries with the SAME shape the worker/verify `cargo test`/nextest uses, so
/// they reuse the warm base instead of cold-building test deps on first run.
///
/// * nextest present → `cargo nextest run <base> [--features ...] --no-run`
/// * otherwise → `cargo test <base> [--features ...] --no-run`
fn build_test_warm_command(
    base_args: &[String],
    feature_args: &[String],
    nextest: bool,
) -> CargoWarmCommand {
    let mut args = if nextest {
        vec!["nextest".to_string(), "run".to_string()]
    } else {
        vec!["test".to_string()]
    };
    args.extend(base_args.iter().cloned());
    args.push("--no-run".to_string());
    CargoWarmCommand {
        label: if nextest {
            "nextest (--no-run)"
        } else {
            "test (--no-run)"
        },
        args,
        feature_args: feature_args.to_vec(),
    }
}

fn build_warm_commands(
    is_workspace: bool,
    all_features: bool,
    features: &[String],
    nextest: bool,
) -> Vec<CargoWarmCommand> {
    // Single-pass warm: always one clippy + one build fallback, matching
    // the worker's feature set exactly.  The former dual-pass
    // (all-features + default-features for workspace) was removed — there is
    // no longer an in-cluster consumer for the all-features warm pass on
    // workspace projects.
    let detected_features: Vec<String> = if all_features {
        vec!["--all-features".to_string()]
    } else if !features.is_empty() {
        let joined = features.join(",");
        vec!["--features".to_string(), joined]
    } else {
        Vec::new()
    };

    // Base non-feature args shared by all clippy/build/test commands.
    let mut base_args = Vec::new();
    if is_workspace {
        base_args.push("--workspace".to_string());
    }
    base_args.push("--all-targets".to_string());

    let mut commands = Vec::new();

    let mut clippy_args = vec!["clippy".to_string()];
    clippy_args.extend(base_args.clone());
    commands.push(CargoWarmCommand {
        label: "clippy",
        args: clippy_args,
        feature_args: detected_features.clone(),
    });

    let mut build_args = vec!["build".to_string()];
    build_args.extend(base_args.clone());
    commands.push(CargoWarmCommand {
        label: "build (clippy fallback)",
        args: build_args,
        feature_args: detected_features.clone(),
    });

    // Test-compile warm step (compile, not run) so worker `cargo
    // test`/nextest reuse the warm test binaries. Uses the same feature
    // set as the worker test command — matching the consistency invariant.
    // Always appended last so it never displaces the clippy/build fallback
    // ordering above.
    commands.push(build_test_warm_command(
        &base_args,
        &detected_features,
        nextest,
    ));

    commands
}

// ---------------------------------------------------------------------------
// resolve_cargo_workspace_dir (mirrors main.rs logic, kept local for purity)
// ---------------------------------------------------------------------------

fn resolve_cargo_workspace_dir(
    project_root: &Path,
    env_config: Option<&EnvironmentConfig>,
) -> Option<PathBuf> {
    if let Some(cfg) = env_config {
        for ws in &cfg.workspaces {
            if ws.language.eq_ignore_ascii_case("rust") {
                let dir = project_root.join(&ws.root);
                if dir.join("Cargo.toml").is_file() {
                    return Some(dir);
                }
            }
        }
    }

    if project_root.join("Cargo.toml").is_file() {
        return Some(project_root.to_path_buf());
    }

    if let Ok(entries) = std::fs::read_dir(project_root) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if dir.is_dir() && dir.join("Cargo.toml").is_file() {
                return Some(dir);
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_env_config_with_rust_workspace(root: &str) -> EnvironmentConfig {
        EnvironmentConfig {
            schema_version: 1,
            source: djinn_stack::environment::ConfigSource::AutoDetected,
            languages: djinn_stack::environment::Languages::default(),
            workspaces: vec![djinn_stack::environment::Workspace {
                slug: None,
                name: None,
                tags: vec![],
                root: root.into(),
                language: "rust".into(),
                toolchain: None,
                version: None,
                package_manager: None,
            }],
            system_packages: vec![],
            env: Default::default(),
            lifecycle: djinn_stack::environment::LifecycleHooks::default(),
            cargo_cache_policy: None,
            agent_mcp_defaults: Default::default(),
            global_skills: vec![],
            build_resources: None,
        }
    }

    // (a) single-crate default features
    #[test]
    fn single_crate_default_features() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "single"
version = "0.1.0"
"#,
        )
        .expect("write Cargo.toml");
        fs::create_dir_all(root.join("src")).expect("mkdir src");
        fs::write(root.join("src/main.rs"), "fn main() {}").expect("write main.rs");

        let policy = resolve_cargo_cache_policy(root, None).expect("policy");
        assert!(!policy.workspace);
        assert!(!policy.all_features);
        assert_eq!(policy.features, Vec::<String>::new());

        // Single crate, default features: clippy + build fallback + test compile,
        // no feature flags in args.
        assert_eq!(policy.warm_commands.len(), 3);
        assert_eq!(policy.warm_commands[0].label, "clippy");
        assert_eq!(
            policy.warm_commands[0].args,
            vec!["clippy", "--all-targets"]
        );
        assert!(policy.warm_commands[0].feature_args.is_empty());
        assert_eq!(policy.warm_commands[1].label, "build (clippy fallback)");
        assert_eq!(policy.warm_commands[1].args, vec!["build", "--all-targets"]);
        assert!(policy.warm_commands[1].feature_args.is_empty());
        // Test-compile step (no nextest config → cargo test --no-run).
        assert_eq!(policy.warm_commands[2].label, "test (--no-run)");
        assert_eq!(
            policy.warm_commands[2].args,
            vec!["test", "--all-targets", "--no-run"]
        );
        assert!(policy.warm_commands[2].feature_args.is_empty());
    }

    // (b) workspace with `--all-features` lifecycle hook → single default-
    // features pass
    #[test]
    fn workspace_with_lifecycle_hook_no_longer_triggers_all_features() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["crate-a", "crate-b"]
"#,
        )
        .expect("write Cargo.toml");
        fs::create_dir_all(root.join("crate-a/src")).expect("mkdir crate-a/src");
        fs::write(
            root.join("crate-a/Cargo.toml"),
            r#"[package]
name = "crate-a"
version = "0.1.0"
"#,
        )
        .expect("write crate-a/Cargo.toml");

        let mut cfg = make_env_config_with_rust_workspace(".");
        cfg.lifecycle.pre_verification = vec![djinn_stack::environment::HookCommand::Shell(
            "cargo test --workspace --all-features".into(),
        )];

        let policy = resolve_cargo_cache_policy(root, Some(&cfg)).expect("policy");
        assert!(policy.workspace);
        // Lifecycle hook scanning removed — all_features only comes from
        // .cargo/config.toml features override now.
        assert!(!policy.all_features);
        assert_eq!(policy.features, Vec::<String>::new());

        // Single default-features pass: clippy + build + test --no-run.
        assert_eq!(policy.warm_commands.len(), 3);

        assert_eq!(policy.warm_commands[0].label, "clippy");
        assert_eq!(
            policy.warm_commands[0].args,
            vec!["clippy", "--workspace", "--all-targets"]
        );
        assert!(policy.warm_commands[0].feature_args.is_empty());

        assert_eq!(policy.warm_commands[1].label, "build (clippy fallback)");
        assert_eq!(
            policy.warm_commands[1].args,
            vec!["build", "--workspace", "--all-targets"]
        );
        assert!(policy.warm_commands[1].feature_args.is_empty());

        assert_eq!(policy.warm_commands[2].label, "test (--no-run)");
        assert_eq!(
            policy.warm_commands[2].args,
            vec!["test", "--workspace", "--all-targets", "--no-run"]
        );
        assert!(policy.warm_commands[2].feature_args.is_empty());
    }

    // (c) workspace with a pinned `rustc-wrapper = "sccache"` in
    // `.cargo/config.toml` — the wrapper is no longer a policy knob (the
    // platform forces RUSTC_WRAPPER=""), so it must not affect resolution.
    #[test]
    fn workspace_with_sccache_wrapper_in_config_is_ignored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["crate-a"]
"#,
        )
        .expect("write Cargo.toml");
        fs::create_dir_all(root.join(".cargo")).expect("mkdir .cargo");
        fs::write(
            root.join(".cargo/config.toml"),
            r#"[build]
rustc-wrapper = "sccache"
"#,
        )
        .expect("write config.toml");
        fs::create_dir_all(root.join("crate-a/src")).expect("mkdir crate-a/src");
        fs::write(
            root.join("crate-a/Cargo.toml"),
            r#"[package]
name = "crate-a"
version = "0.1.0"
"#,
        )
        .expect("write crate-a/Cargo.toml");

        let policy = resolve_cargo_cache_policy(root, None).expect("policy");
        assert!(policy.workspace);
        assert!(!policy.all_features);
        assert_eq!(policy.features, Vec::<String>::new());

        // Workspace, default features: clippy + build + test, no feature flags.
        // (The repo's `rustc-wrapper = "sccache"` is ignored — the platform
        // forces RUSTC_WRAPPER="" — so the warm command shape is unaffected.)
        assert_eq!(policy.warm_commands.len(), 3);
        assert_eq!(policy.warm_commands[0].label, "clippy");
        assert_eq!(
            policy.warm_commands[0].args,
            vec!["clippy", "--workspace", "--all-targets"]
        );
        assert!(policy.warm_commands[0].feature_args.is_empty());
        assert_eq!(policy.warm_commands[2].label, "test (--no-run)");
        assert_eq!(
            policy.warm_commands[2].args,
            vec!["test", "--workspace", "--all-targets", "--no-run"]
        );
    }

    #[test]
    fn non_rust_repo_returns_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(root.join("package.json"), "{}").expect("write package.json");
        assert!(resolve_cargo_cache_policy(root, None).is_none());
    }

    #[test]
    fn cargo_config_features_array() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "feat"
version = "0.1.0"
"#,
        )
        .expect("write Cargo.toml");
        fs::create_dir_all(root.join(".cargo")).expect("mkdir .cargo");
        fs::write(
            root.join(".cargo/config.toml"),
            r#"[build]
features = ["foo", "bar"]
"#,
        )
        .expect("write config.toml");

        let policy = resolve_cargo_cache_policy(root, None).expect("policy");
        assert_eq!(policy.features, vec!["foo", "bar"]);
        assert!(!policy.all_features);

        // Named features: clippy + build + test, features in feature_args, not in args.
        assert_eq!(policy.warm_commands.len(), 3);
        assert_eq!(
            policy.warm_commands[0].args,
            vec!["clippy", "--all-targets"]
        );
        assert_eq!(
            policy.warm_commands[0].feature_args,
            vec!["--features", "foo,bar"]
        );
        assert_eq!(policy.warm_commands[1].args, vec!["build", "--all-targets"]);
        assert_eq!(
            policy.warm_commands[1].feature_args,
            vec!["--features", "foo,bar"]
        );
        assert_eq!(policy.warm_commands[2].label, "test (--no-run)");
        assert_eq!(
            policy.warm_commands[2].args,
            vec!["test", "--all-targets", "--no-run"]
        );
        assert_eq!(
            policy.warm_commands[2].feature_args,
            vec!["--features", "foo,bar"]
        );

        // Verify features() returns the correct CLI args
        assert_eq!(
            policy.features(),
            vec!["--features".to_string(), "foo,bar".to_string()]
        );
    }

    #[test]
    fn cargo_config_features_string() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "feat"
version = "0.1.0"
"#,
        )
        .expect("write Cargo.toml");
        fs::create_dir_all(root.join(".cargo")).expect("mkdir .cargo");
        fs::write(
            root.join(".cargo/config.toml"),
            r#"[build]
features = "foo bar"
"#,
        )
        .expect("write config.toml");

        let policy = resolve_cargo_cache_policy(root, None).expect("policy");
        assert_eq!(policy.features, vec!["foo", "bar"]);
        assert!(!policy.all_features);

        // Named features: clippy + build + test, features in feature_args, not in args.
        assert_eq!(policy.warm_commands.len(), 3);
        assert_eq!(
            policy.warm_commands[0].args,
            vec!["clippy", "--all-targets"]
        );
        assert_eq!(
            policy.warm_commands[0].feature_args,
            vec!["--features", "foo,bar"]
        );
        assert_eq!(policy.warm_commands[1].args, vec!["build", "--all-targets"]);
        assert_eq!(
            policy.warm_commands[1].feature_args,
            vec!["--features", "foo,bar"]
        );
        assert_eq!(policy.warm_commands[2].label, "test (--no-run)");
        assert_eq!(
            policy.warm_commands[2].feature_args,
            vec!["--features", "foo,bar"]
        );

        // Verify features() returns the correct CLI args
        assert_eq!(
            policy.features(),
            vec!["--features".to_string(), "foo,bar".to_string()]
        );
    }

    #[test]
    fn explicit_env_config_override_takes_precedence_over_detection() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["crate-a"]
"#,
        )
        .expect("write Cargo.toml");
        fs::create_dir_all(root.join(".cargo")).expect("mkdir .cargo");
        fs::write(
            root.join(".cargo/config.toml"),
            r#"[build]
rustc-wrapper = "sccache"
features = ["detected"]
"#,
        )
        .expect("write config.toml");
        fs::create_dir_all(root.join("crate-a/src")).expect("mkdir crate-a/src");
        fs::write(
            root.join("crate-a/Cargo.toml"),
            r#"[package]
name = "crate-a"
version = "0.1.0"
"#,
        )
        .expect("write crate-a/Cargo.toml");

        let mut cfg = make_env_config_with_rust_workspace(".");
        cfg.lifecycle.pre_verification = vec![djinn_stack::environment::HookCommand::Shell(
            "cargo test --workspace --all-features".into(),
        )];
        cfg.cargo_cache_policy = Some(djinn_stack::environment::CargoCachePolicy::Explicit(
            djinn_stack::environment::CargoCachePolicyOverride {
                workspace: false,
                features: vec!["override-a".into(), "override-b".into()],
                all_features: false,
                warm_commands: vec![djinn_stack::environment::CargoWarmCommand {
                    label: "explicit warm".into(),
                    args: vec![
                        "check".into(),
                        "--features".into(),
                        "override-a,override-b".into(),
                    ],
                }],
            },
        ));

        let policy = resolve_cargo_cache_policy(root, Some(&cfg)).expect("policy");
        assert!(!policy.workspace);
        assert_eq!(policy.features, vec!["override-a", "override-b"]);
        assert!(!policy.all_features);
        assert_eq!(
            policy.features(),
            vec![
                "--features".to_string(),
                "override-a,override-b".to_string()
            ]
        );
        assert_eq!(policy.warm_commands.len(), 1);
        assert_eq!(policy.warm_commands[0].label, "override");
        assert_eq!(
            policy.warm_commands[0].args,
            vec!["check", "--features", "override-a,override-b"]
        );
    }

    // ====================================================================
    // Drift prevention tests — warm set is superset of worker feature set
    // ====================================================================

    /// Helper: extract the set of distinct `feature_args` from warm commands.
    fn feature_sets(commands: &[CargoWarmCommand]) -> Vec<Vec<String>> {
        let mut sets: Vec<Vec<String>> = commands.iter().map(|c| c.feature_args.clone()).collect();
        sets.sort();
        sets.dedup();
        sets
    }

    // (d1) build_warm_commands: workspace + all_features → single-pass with
    //      --all-features (dual-pass was removed)
    #[test]
    fn build_warm_commands_workspace_all_features_single_pass() {
        let cmds = build_warm_commands(true, true, &[], false);

        // clippy, build, test — all with --all-features in feature_args
        assert_eq!(cmds.len(), 3);

        assert_eq!(cmds[0].label, "clippy");
        assert_eq!(cmds[0].args, vec!["clippy", "--workspace", "--all-targets"]);
        assert_eq!(cmds[0].feature_args, vec!["--all-features"]);

        assert_eq!(cmds[1].label, "build (clippy fallback)");
        assert_eq!(cmds[1].args, vec!["build", "--workspace", "--all-targets"]);
        assert_eq!(cmds[1].feature_args, vec!["--all-features"]);

        assert_eq!(cmds[2].label, "test (--no-run)");
        assert_eq!(
            cmds[2].args,
            vec!["test", "--workspace", "--all-targets", "--no-run"]
        );
        assert_eq!(cmds[2].feature_args, vec!["--all-features"]);
    }

    // (d1b) build_warm_commands: nextest detected → nextest --no-run test step
    #[test]
    fn build_warm_commands_uses_nextest_when_detected() {
        let cmds = build_warm_commands(true, false, &[], true);
        let test = cmds.last().expect("test step");
        assert_eq!(test.label, "nextest (--no-run)");
        assert_eq!(
            test.args,
            vec!["nextest", "run", "--workspace", "--all-targets", "--no-run"]
        );
    }

    // (d2) build_warm_commands: workspace + default features only
    #[test]
    fn build_warm_commands_workspace_default_features_only() {
        let cmds = build_warm_commands(true, false, &[], false);

        // clippy (default), build (default), test (default)
        assert_eq!(cmds.len(), 3);

        assert_eq!(cmds[0].label, "clippy");
        assert_eq!(cmds[0].args, vec!["clippy", "--workspace", "--all-targets"]);
        assert!(
            cmds[0].feature_args.is_empty(),
            "default-features clippy must have empty feature_args"
        );

        assert_eq!(cmds[1].label, "build (clippy fallback)");
        assert_eq!(cmds[1].args, vec!["build", "--workspace", "--all-targets"]);
        assert!(
            cmds[1].feature_args.is_empty(),
            "default-features build must have empty feature_args"
        );

        assert_eq!(cmds[2].label, "test (--no-run)");
        assert_eq!(
            cmds[2].args,
            vec!["test", "--workspace", "--all-targets", "--no-run"]
        );
        assert!(cmds[2].feature_args.is_empty());
    }

    // (d3) build_warm_commands: single crate + default features
    #[test]
    fn build_warm_commands_single_crate_default_features() {
        let cmds = build_warm_commands(false, false, &[], false);

        // clippy (default), build (default), test (default) — no --workspace
        assert_eq!(cmds.len(), 3);

        assert_eq!(cmds[0].label, "clippy");
        assert_eq!(cmds[0].args, vec!["clippy", "--all-targets"]);
        assert!(
            cmds[0].feature_args.is_empty(),
            "single-crate default clippy must have empty feature_args"
        );

        assert_eq!(cmds[1].label, "build (clippy fallback)");
        assert_eq!(cmds[1].args, vec!["build", "--all-targets"]);
        assert!(
            cmds[1].feature_args.is_empty(),
            "single-crate default build must have empty feature_args"
        );

        assert_eq!(cmds[2].label, "test (--no-run)");
        assert_eq!(cmds[2].args, vec!["test", "--all-targets", "--no-run"]);
    }

    // (d4) build_warm_commands: single crate + all_features (non-workspace)
    //      → single-pass with --all-features in feature_args
    #[test]
    fn build_warm_commands_single_crate_all_features_single_pass() {
        let cmds = build_warm_commands(false, true, &[], false);

        // clippy, build, test — all with --all-features in feature_args
        assert_eq!(cmds.len(), 3);
        assert_eq!(cmds[0].label, "clippy");
        assert_eq!(cmds[0].args, vec!["clippy", "--all-targets"]);
        assert_eq!(cmds[0].feature_args, vec!["--all-features"]);

        assert_eq!(cmds[1].label, "build (clippy fallback)");
        assert_eq!(cmds[1].args, vec!["build", "--all-targets"]);
        assert_eq!(cmds[1].feature_args, vec!["--all-features"]);

        assert_eq!(cmds[2].label, "test (--no-run)");
        assert_eq!(cmds[2].args, vec!["test", "--all-targets", "--no-run"]);
        assert_eq!(cmds[2].feature_args, vec!["--all-features"]);
    }

    // (d5) Feature-set invariant: each warm set's feature_args match the
    //      expected feature set for that configuration.
    #[test]
    fn warm_set_feature_args_match_expected_configuration() {
        // All-features workspace → must have --all-features (single pass)
        let cmds_af = build_warm_commands(true, true, &[], false);
        let sets_af = feature_sets(&cmds_af);
        assert!(
            sets_af.contains(&vec!["--all-features".to_string()]),
            "all-features warm set must include --all-features pass, got {:?}",
            sets_af
        );
        assert_eq!(
            sets_af.len(),
            1,
            "all-features warm set must have exactly one feature set, got {:?}",
            sets_af
        );

        // Default-features workspace → must have empty feature_args
        let cmds_def = build_warm_commands(true, false, &[], false);
        let sets_def = feature_sets(&cmds_def);
        assert!(
            sets_def.contains(&Vec::<String>::new()),
            "default-features warm set must include empty feature_args, got {:?}",
            sets_def
        );

        // Default-features single crate → must have empty feature_args
        let cmds_single = build_warm_commands(false, false, &[], false);
        let sets_single = feature_sets(&cmds_single);
        assert!(
            sets_single.contains(&Vec::<String>::new()),
            "single-crate default warm set must include empty feature_args, got {:?}",
            sets_single
        );
    }

    // (d6) Superset invariant for named features:
    //      features in feature_args, NOT in args.
    #[test]
    fn warm_commands_named_features_in_feature_args_not_in_args() {
        let named = vec!["foo".to_string(), "bar".to_string()];
        let cmds = build_warm_commands(true, false, &named, false);

        // clippy, build, test — features in feature_args, not in args.
        assert_eq!(cmds.len(), 3);
        for cmd in &cmds {
            assert!(
                !cmd.args
                    .iter()
                    .any(|a| a == "--features" || a == "--all-features"),
                "args must NOT contain feature flags: {:?}",
                cmd.args
            );
        }
        assert_eq!(cmds[0].feature_args, vec!["--features", "foo,bar"]);
        assert_eq!(cmds[1].feature_args, vec!["--features", "foo,bar"]);
        assert_eq!(cmds[2].feature_args, vec!["--features", "foo,bar"]);
    }

    // (d7) Integration: workspace + (ignored) sccache config + --all-features
    //      lifecycle hook → lifecycle hook no longer triggers all_features,
    //      policy.all_features=false, single default-features pass. The repo's
    //      `rustc-wrapper = "sccache"` is ignored.
    #[test]
    fn integration_workspace_lifecycle_hook_no_all_features() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
    members = ["crate-a"]
    "#,
        )
        .expect("write Cargo.toml");
        fs::create_dir_all(root.join(".cargo")).expect("mkdir .cargo");
        fs::write(
            root.join(".cargo/config.toml"),
            r#"[build]
    	rustc-wrapper = "sccache"
    "#,
        )
        .expect("write config.toml");
        fs::create_dir_all(root.join("crate-a/src")).expect("mkdir crate-a/src");
        fs::write(
            root.join("crate-a/Cargo.toml"),
            r#"[package]
    name = "crate-a"
    version = "0.1.0"
    "#,
        )
        .expect("write crate-a/Cargo.toml");

        let mut cfg = make_env_config_with_rust_workspace(".");
        cfg.lifecycle.pre_verification = vec![djinn_stack::environment::HookCommand::Shell(
            "cargo test --workspace --all-features".into(),
        )];

        let policy = resolve_cargo_cache_policy(root, Some(&cfg)).expect("policy");
        // Lifecycle hook scanning removed — all_features only comes from
        // .cargo/config.toml features override now.
        assert!(
            !policy.all_features,
            "lifecycle hook should no longer trigger all-features"
        );

        // Single default-features pass: clippy + build + test-compile = 3 warm commands
        assert_eq!(policy.warm_commands.len(), 3);

        // Verify all commands have empty feature_args (default features)
        let sets = feature_sets(&policy.warm_commands);
        assert_eq!(
            sets.len(),
            1,
            "only one distinct feature set expected, got {:?}",
            sets
        );
        assert!(
            sets.contains(&Vec::<String>::new()),
            "warm set must have empty feature_args (default features), got {:?}",
            sets
        );

        // Verify args do NOT contain feature flags (they're in feature_args)
        for cmd in &policy.warm_commands {
            assert!(
                !cmd.args
                    .iter()
                    .any(|a| a == "--features" || a == "--all-features"),
                "args must NOT contain feature flags: {:?}",
                cmd.args
            );
        }
    }

    // (d8) Non-djinn project: workspace but NO --all-features, NO sccache
    //      → default-features-only warm commands.
    #[test]
    fn non_djinn_workspace_produces_default_features_only_warm() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["crate-a"]
"#,
        )
        .expect("write Cargo.toml");
        fs::create_dir_all(root.join("crate-a/src")).expect("mkdir crate-a/src");
        fs::write(
            root.join("crate-a/Cargo.toml"),
            r#"[package]
name = "crate-a"
version = "0.1.0"
"#,
        )
        .expect("write crate-a/Cargo.toml");

        // No env_config → no lifecycle hooks, no all-features
        let policy = resolve_cargo_cache_policy(root, None).expect("policy");
        assert!(!policy.all_features);

        // clippy + build + test = 3 commands, all with empty feature_args
        assert_eq!(policy.warm_commands.len(), 3);
        for cmd in &policy.warm_commands {
            assert!(
                cmd.feature_args.is_empty(),
                "non-djinn project warm commands must have empty feature_args: {:?}",
                cmd
            );
        }

        let sets = feature_sets(&policy.warm_commands);
        assert_eq!(sets.len(), 1, "only one distinct feature set expected");
        assert_eq!(sets[0], Vec::<String>::new());
    }
}
