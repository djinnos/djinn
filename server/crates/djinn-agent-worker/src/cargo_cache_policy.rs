//! Cargo cache policy resolver — project-agnostic detection of per-project
//! cargo build strategy.
//!
//! Detects from workspace layout, `.cargo/config.toml`, and verification
//! command patterns.  Pure/unit-testable; never mutates any project file.

use std::path::{Path, PathBuf};

use djinn_stack::environment::EnvironmentConfig;

/// Resolved per-project cargo cache strategy.
///
/// All fields are derived from detection, not hardcoded.  The warm and worker
/// paths use the same `CargoCachePolicy` so their feature sets, sccache vs
/// incremental, and command shapes stay consistent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoCachePolicy {
    /// Whether the project uses a cargo workspace (multiple crates) or a
    /// single crate.
    pub workspace: bool,
    /// Feature flags to pass to warm and worker compiles.  Empty means
    /// default features only.
    pub features: Vec<String>,
    /// Whether to pass `--all-features` to cargo commands.
    pub all_features: bool,
    /// Whether the project pins `rustc-wrapper = "sccache"` in
    /// `.cargo/config.toml`.
    pub sccache: bool,
    /// Whether to enable incremental compilation (`CARGO_INCREMENTAL=1`).
    /// Set to `false` when sccache is detected because sccache disables
    /// incremental anyway.
    pub incremental: bool,
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

impl Default for CargoCachePolicy {
    fn default() -> Self {
        Self {
            workspace: false,
            features: Vec::new(),
            all_features: false,
            sccache: false,
            incremental: true,
            warm_commands: Vec::new(),
        }
    }
}

/// One warm-base compile step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoWarmCommand {
    /// Human label for logs/metrics.
    pub label: &'static str,
    /// Cargo subcommand and arguments (e.g. `["clippy", "--workspace"]`).
    pub args: Vec<String>,
}

/// Resolve the cargo cache policy for a project root.
///
/// Reads (but never writes):
/// * `Cargo.toml` — workspace section detection.
/// * `.cargo/config.toml` — `rustc-wrapper` and `build.features`.
/// * `env_config` — Rust workspace root and verification rules for command
///   pattern detection.
///
/// Returns `None` when no cargo workspace exists (non-Rust repo).
pub fn resolve_cargo_cache_policy(
    project_root: &Path,
    env_config: Option<&EnvironmentConfig>,
) -> Option<CargoCachePolicy> {
    let workspace_dir = resolve_cargo_workspace_dir(project_root, env_config)?;

    let is_workspace = detect_workspace_layout(&workspace_dir);
    let cargo_config = read_cargo_config_toml(&workspace_dir);
    let sccache = cargo_config
        .as_ref()
        .map(|c| c.rustc_wrapper.as_deref() == Some("sccache"))
        .unwrap_or(false);
    let config_features = cargo_config
        .as_ref()
        .map(|c| c.features.clone())
        .unwrap_or_default();
    let all_features = detect_all_features_from_env_config(env_config)
        || config_features.contains(&"all-features".to_string());

    let features = if all_features {
        Vec::new()
    } else {
        config_features
    };

    let warm_commands = build_warm_commands(is_workspace, all_features, &features);

    Some(CargoCachePolicy {
        workspace: is_workspace,
        features,
        all_features,
        sccache,
        incremental: !sccache,
        warm_commands,
    })
}

// ---------------------------------------------------------------------------
// Internal detection helpers
// ---------------------------------------------------------------------------

/// Parsed subset of `.cargo/config.toml` relevant to cache policy.
#[derive(Debug, Default, PartialEq, Eq)]
struct CargoConfigToml {
    rustc_wrapper: Option<String>,
    features: Vec<String>,
}

fn read_cargo_config_toml(workspace_dir: &Path) -> Option<CargoConfigToml> {
    let path = workspace_dir.join(".cargo").join("config.toml");
    let text = std::fs::read_to_string(&path).ok()?;
    let raw: toml::Table = toml::from_str(&text).ok()?;

    let mut out = CargoConfigToml::default();

    if let Some(build) = raw.get("build").and_then(|v| v.as_table()) {
        if let Some(wrapper) = build.get("rustc-wrapper").and_then(|v| v.as_str()) {
            out.rustc_wrapper = Some(wrapper.to_string());
        }
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

fn detect_all_features_from_env_config(env_config: Option<&EnvironmentConfig>) -> bool {
    let Some(cfg) = env_config else {
        return false;
    };
    // Scan verification rules for `--all-features` in any command string.
    // Verification rules are no longer in EnvironmentConfig, but we keep
    // the hook scanning path for CI command patterns in lifecycle hooks.
    for hook in &cfg.lifecycle.pre_verification {
        if hook_contains(hook, "--all-features") {
            return true;
        }
    }
    for hook in &cfg.lifecycle.pre_task {
        if hook_contains(hook, "--all-features") {
            return true;
        }
    }
    for hook in &cfg.lifecycle.post_build {
        if hook_contains(hook, "--all-features") {
            return true;
        }
    }
    false
}

fn hook_contains(hook: &djinn_stack::environment::HookCommand, needle: &str) -> bool {
    use djinn_stack::environment::HookCommand;
    match hook {
        HookCommand::Shell(s) => s.contains(needle),
        HookCommand::Exec(argv) => argv.iter().any(|a| a.contains(needle)),
        HookCommand::Parallel(map) => map.values().any(|h| hook_contains(h, needle)),
    }
}

fn build_warm_commands(
    is_workspace: bool,
    all_features: bool,
    features: &[String],
) -> Vec<CargoWarmCommand> {
    let feature_args = if all_features {
        vec!["--all-features".to_string()]
    } else if !features.is_empty() {
        let joined = features.join(",");
        vec!["--features".to_string(), joined]
    } else {
        Vec::new()
    };

    let mut clippy_args = vec!["clippy".to_string()];
    if is_workspace {
        clippy_args.push("--workspace".to_string());
    }
    clippy_args.push("--all-targets".to_string());
    clippy_args.extend(feature_args.clone());

    let mut build_args = vec!["build".to_string()];
    if is_workspace {
        build_args.push("--workspace".to_string());
    }
    build_args.push("--all-targets".to_string());
    build_args.extend(feature_args.clone());

    let mut test_args = vec!["test".to_string()];
    if is_workspace {
        test_args.push("--workspace".to_string());
    }
    test_args.push("--all-targets".to_string());
    test_args.extend(feature_args);
    test_args.push("--no-run".to_string());

    vec![
        CargoWarmCommand {
            label: "clippy",
            args: clippy_args,
        },
        CargoWarmCommand {
            label: "build (clippy fallback)",
            args: build_args,
        },
        CargoWarmCommand {
            label: "test --no-run",
            args: test_args,
        },
    ]
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
            agent_mcp_defaults: Default::default(),
            global_skills: vec![],
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
        assert!(!policy.sccache);
        assert!(policy.incremental);
        assert_eq!(policy.features, Vec::<String>::new());

        assert_eq!(policy.warm_commands.len(), 3);
        assert_eq!(policy.warm_commands[0].label, "clippy");
        assert_eq!(
            policy.warm_commands[0].args,
            vec!["clippy", "--all-targets"]
        );
        assert_eq!(
            policy.warm_commands[2].args,
            vec!["test", "--all-targets", "--no-run"]
        );
    }

    // (b) workspace with `--all-features` CI
    #[test]
    fn workspace_with_all_features_ci() {
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
        assert!(policy.all_features);
        assert!(!policy.sccache);
        assert!(policy.incremental);
        assert_eq!(policy.features, Vec::<String>::new());

        assert_eq!(
            policy.warm_commands[0].args,
            vec!["clippy", "--workspace", "--all-targets", "--all-features"]
        );
        assert_eq!(
            policy.warm_commands[2].args,
            vec![
                "test",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--no-run"
            ]
        );
    }

    // (c) workspace with pinned `rustc-wrapper = "sccache"`
    #[test]
    fn workspace_with_sccache_wrapper() {
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
        assert!(policy.sccache);
        assert!(!policy.incremental);
        assert_eq!(policy.features, Vec::<String>::new());

        assert_eq!(
            policy.warm_commands[0].args,
            vec!["clippy", "--workspace", "--all-targets"]
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

        // Verify warm commands include --features for named feature sets
        assert_eq!(
            policy.warm_commands[0].args,
            vec!["clippy", "--all-targets", "--features", "foo,bar"]
        );
        assert_eq!(
            policy.warm_commands[1].args,
            vec!["build", "--all-targets", "--features", "foo,bar"]
        );
        assert_eq!(
            policy.warm_commands[2].args,
            vec!["test", "--all-targets", "--features", "foo,bar", "--no-run"]
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

        // Verify warm commands include --features for named feature sets
        assert_eq!(
            policy.warm_commands[0].args,
            vec!["clippy", "--all-targets", "--features", "foo,bar"]
        );
        assert_eq!(
            policy.warm_commands[1].args,
            vec!["build", "--all-targets", "--features", "foo,bar"]
        );
        assert_eq!(
            policy.warm_commands[2].args,
            vec!["test", "--all-targets", "--features", "foo,bar", "--no-run"]
        );

        // Verify features() returns the correct CLI args
        assert_eq!(
            policy.features(),
            vec!["--features".to_string(), "foo,bar".to_string()]
        );
    }
}
