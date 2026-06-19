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
/// * `.cargo/config.toml` — `rustc-wrapper` and `build.features`.
/// * `env_config` — Rust workspace root and verification rules for command
///   pattern detection.
///
/// Returns `None` when no cargo workspace exists (non-Rust repo).
pub fn resolve_cargo_cache_policy(
    project_root: &Path,
    env_config: Option<&EnvironmentConfig>,
) -> Option<CargoCachePolicy> {
    if let Some(cfg) = env_config {
        if let Some(djinn_stack::environment::CargoCachePolicy::Explicit(override_policy)) =
            &cfg.cargo_cache_policy
        {
            return Some(CargoCachePolicy {
                workspace: override_policy.workspace,
                features: override_policy.features.clone(),
                all_features: override_policy.all_features,
                sccache: override_policy.sccache,
                incremental: override_policy.incremental,
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
    }

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
    let all_features_args: Vec<String> = if all_features {
        vec!["--all-features".to_string()]
    } else {
        Vec::new()
    };

    let named_features_args: Vec<String> = if !all_features && !features.is_empty() {
        let joined = features.join(",");
        vec!["--features".to_string(), joined]
    } else {
        Vec::new()
    };

    // Base non-feature args shared by all clippy/build commands.
    let mut base_args = Vec::new();
    if is_workspace {
        base_args.push("--workspace".to_string());
    }
    base_args.push("--all-targets".to_string());

    let mut commands = Vec::new();

    if all_features && is_workspace {
        // Dual-pass warm: all-features first (for CI/verification deps),
        // then default-features (for worker default-feature reuse).
        let mut clippy_all = vec!["clippy".to_string()];
        clippy_all.extend(base_args.clone());
        commands.push(CargoWarmCommand {
            label: "clippy (all-features)",
            args: clippy_all,
            feature_args: all_features_args.clone(),
        });

        let mut clippy_default = vec!["clippy".to_string()];
        clippy_default.extend(base_args.clone());
        commands.push(CargoWarmCommand {
            label: "clippy (default-features)",
            args: clippy_default,
            feature_args: Vec::new(),
        });

        let mut build_all = vec!["build".to_string()];
        build_all.extend(base_args);
        commands.push(CargoWarmCommand {
            label: "build (clippy fallback)",
            args: build_all,
            feature_args: all_features_args,
        });
    } else {
        // Single-pass warm: clippy with detected features, build fallback.
        let detected_features = if all_features {
            all_features_args.clone()
        } else {
            named_features_args.clone()
        };

        let mut clippy_args = vec!["clippy".to_string()];
        clippy_args.extend(base_args.clone());
        commands.push(CargoWarmCommand {
            label: "clippy",
            args: clippy_args,
            feature_args: detected_features.clone(),
        });

        let mut build_args = vec!["build".to_string()];
        build_args.extend(base_args);
        commands.push(CargoWarmCommand {
            label: "build (clippy fallback)",
            args: build_args,
            feature_args: detected_features,
        });
    }

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

        // Single crate, default features: 2 commands (clippy + build fallback),
        // no test --no-run, no feature flags in args.
        assert_eq!(policy.warm_commands.len(), 2);
        assert_eq!(policy.warm_commands[0].label, "clippy");
        assert_eq!(
            policy.warm_commands[0].args,
            vec!["clippy", "--all-targets"]
        );
        assert!(policy.warm_commands[0].feature_args.is_empty());
        assert_eq!(policy.warm_commands[1].label, "build (clippy fallback)");
        assert_eq!(policy.warm_commands[1].args, vec!["build", "--all-targets"]);
        assert!(policy.warm_commands[1].feature_args.is_empty());
    }

    // (b) workspace with `--all-features` CI → dual-pass warm
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

        // Dual-pass: 3 commands — clippy (all-features), clippy (default-features),
        // build (clippy fallback). No test --no-run.
        assert_eq!(policy.warm_commands.len(), 3);

        // Pass 1: all-features clippy
        assert_eq!(policy.warm_commands[0].label, "clippy (all-features)");
        assert_eq!(
            policy.warm_commands[0].args,
            vec!["clippy", "--workspace", "--all-targets"]
        );
        assert_eq!(policy.warm_commands[0].feature_args, vec!["--all-features"]);

        // Pass 2: default-features clippy (no feature flags)
        assert_eq!(policy.warm_commands[1].label, "clippy (default-features)");
        assert_eq!(
            policy.warm_commands[1].args,
            vec!["clippy", "--workspace", "--all-targets"]
        );
        assert!(policy.warm_commands[1].feature_args.is_empty());

        // Pass 3: build fallback (all-features)
        assert_eq!(policy.warm_commands[2].label, "build (clippy fallback)");
        assert_eq!(
            policy.warm_commands[2].args,
            vec!["build", "--workspace", "--all-targets"]
        );
        assert_eq!(policy.warm_commands[2].feature_args, vec!["--all-features"]);
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

        // Workspace, default features, sccache: 2 commands, no feature flags.
        assert_eq!(policy.warm_commands.len(), 2);
        assert_eq!(policy.warm_commands[0].label, "clippy");
        assert_eq!(
            policy.warm_commands[0].args,
            vec!["clippy", "--workspace", "--all-targets"]
        );
        assert!(policy.warm_commands[0].feature_args.is_empty());
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

        // Named features: 2 commands, features in feature_args, not in args.
        assert_eq!(policy.warm_commands.len(), 2);
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

        // Named features: 2 commands, features in feature_args, not in args.
        assert_eq!(policy.warm_commands.len(), 2);
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
                sccache: false,
                incremental: true,
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
        assert!(!policy.sccache);
        assert!(policy.incremental);
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
}
