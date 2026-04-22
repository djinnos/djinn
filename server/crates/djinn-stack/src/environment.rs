//! `EnvironmentConfig` — per-project runtime configuration.
//!
//! Persisted as JSON in `projects.environment_config` (migration 10). Replaces
//! the pre-cut-over `.devcontainer/devcontainer.json` read path: a djinn-owned
//! schema that the UI authors, the image-controller hashes + renders into a
//! Dockerfile, and the worker reads from a ConfigMap at Pod start.
//!
//! This module is additive for P1 — no consumers wire it up yet. P3 brings the
//! image-builder; P5 is the atomic cut-over that makes this the source of
//! truth. The Dolt column exists from migration 10 on, defaulting to `'{}'`;
//! P5's boot hook treats that emptiness as the reseed trigger.
//!
//! ## Validation invariants
//!
//! Values from this struct end up in shell `RUN` lines inside the generated
//! Dockerfile (e.g. `TOOLCHAINS="$default_toolchain"` in `install-rust.sh`),
//! so every string that flows into a `RUN` is restricted to a conservative
//! character set (`[A-Za-z0-9._-]` — no shell metacharacters, no whitespace).
//! `HookCommand` values are *not* restricted that way — they're commands by
//! construction — but list lengths are capped.

use std::collections::{BTreeMap, HashSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Current schema version. Bumped for every breaking shape change. The worker
/// rejects configs with `schema_version` greater than this (forward-incompat
/// canary — see `risks & gotchas` in the plan).
pub const SCHEMA_VERSION: u32 = 1;

// ---- caps -----------------------------------------------------------------

const MAX_WORKSPACES: usize = 64;
const MAX_ENV_ENTRIES: usize = 256;
const MAX_SYSTEM_PACKAGES: usize = 256;
const MAX_HOOKS_PER_PHASE: usize = 64;
const MAX_VERIFICATION_RULES: usize = 128;
const MAX_LANGUAGE_LIST: usize = 64;
const MAX_STRING_LEN: usize = 512;
const MAX_HOOK_SHELL_LEN: usize = 16 * 1024;

#[derive(Debug, Error)]
pub enum EnvironmentConfigError {
    #[error("schema_version {found} is higher than supported ({supported})")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error("{field}: value {value:?} contains disallowed characters (allowed: [A-Za-z0-9._-])")]
    UnsafeIdentifier { field: String, value: String },
    #[error("{field}: value is empty")]
    EmptyValue { field: String },
    #[error("{field}: length {len} exceeds max {max}")]
    TooLong { field: String, len: usize, max: usize },
    #[error("{field}: list length {len} exceeds max {max}")]
    ListTooLong {
        field: String,
        len: usize,
        max: usize,
    },
    #[error("duplicate workspace slug {slug:?}")]
    DuplicateWorkspaceSlug { slug: String },
    #[error("env var key {key:?} is not a valid identifier ([A-Za-z_][A-Za-z0-9_]*)")]
    InvalidEnvKey { key: String },
    #[error("env var {key:?}: value contains disallowed newline/NUL")]
    InvalidEnvValue { key: String },
    #[error("{field}: verification rule has empty commands list")]
    EmptyVerificationCommands { field: String },
}

pub type EnvResult<T> = std::result::Result<T, EnvironmentConfigError>;

// ---- top-level ------------------------------------------------------------

/// How the config landed in the column.
///
/// * `AutoDetected` — written by the P5 boot reseed hook from stack detection.
///   Re-writing from detection is OK (config may still be overwritten on the
///   next detector pass until the user edits it).
/// * `UserEdited` — saved via the MCP tool or UI. Never reseeded from stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ConfigSource {
    AutoDetected,
    UserEdited,
}

impl Default for ConfigSource {
    fn default() -> Self {
        Self::AutoDetected
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentConfig {
    /// `0` (the default) is the "needs reseed" sentinel — the P5 boot hook
    /// treats any config with `schema_version < 1` as an un-seeded row and
    /// rewrites it from `projects.stack`. `validate()` rejects 0 so that
    /// user-submitted configs must declare a real version.
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub source: ConfigSource,
    #[serde(default)]
    pub base: BaseImage,
    #[serde(default)]
    pub languages: Languages,
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub system_packages: SystemPackages,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub lifecycle: LifecycleHooks,
    #[serde(default)]
    pub verification: Verification,
}

impl EnvironmentConfig {
    /// Minimal valid config — what the column's default `'{}'` parses into
    /// once the P5 reseed hook tags the source.
    pub fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            source: ConfigSource::AutoDetected,
            base: BaseImage::default(),
            languages: Languages::default(),
            workspaces: Vec::new(),
            system_packages: SystemPackages::default(),
            env: BTreeMap::new(),
            lifecycle: LifecycleHooks::default(),
            verification: Verification::default(),
        }
    }

    /// Seed a fresh config from a freshly-detected [`crate::schema::Stack`].
    /// Called by the P5 boot reseed hook for every project whose
    /// `environment_config` column is still `'{}'`.
    ///
    /// Populates:
    /// * `schema_version`, `source = AutoDetected`
    /// * `base` — default debian/bookworm-slim
    /// * `languages.*` — one entry per language the stack detected a
    ///   runtime for. Rust is populated even when `stack.runtimes.rust`
    ///   is `None`, as long as `Stack.workspaces` has a Rust entry
    ///   (covers the bare "Cargo.toml without rust-toolchain.toml"
    ///   case). `rust-analyzer` is included in components by default
    ///   so the warm-graph SCIP pipeline works out of the box.
    /// * `workspaces` — one entry per `StackWorkspace`, with
    ///   toolchain/version routed to the right field per language.
    /// * `env`, `system_packages`, `lifecycle`, `verification` —
    ///   empty (user fills them in via the UI).
    pub fn from_stack(stack: &crate::schema::Stack) -> Self {
        let mut cfg = Self::empty();
        cfg.source = ConfigSource::AutoDetected;

        // Detect which languages appear in workspaces or runtimes so we
        // only populate `languages.*` blocks that the image will actually
        // install.
        let has_rust = stack.runtimes.rust.is_some()
            || stack.workspaces.iter().any(|w| w.language == "rust");
        let has_node = stack.runtimes.node.is_some()
            || stack.workspaces.iter().any(|w| w.language == "node");
        let has_python = stack.runtimes.python.is_some()
            || stack.workspaces.iter().any(|w| w.language == "python");
        let has_go = stack.runtimes.go.is_some()
            || stack.workspaces.iter().any(|w| w.language == "go");

        if has_rust {
            cfg.languages.rust = Some(RustLanguage {
                default_toolchain: stack
                    .runtimes
                    .rust
                    .clone()
                    .unwrap_or_else(|| "stable".to_string()),
                components: vec!["rust-analyzer".to_string()],
                targets: vec![],
            });
        }
        if has_node {
            let default_version = stack.runtimes.node.clone().unwrap_or_else(|| "22".to_string());
            // Pick the first package manager the stack saw among the
            // Node set, else pnpm (matches djinn's own default).
            let default_pm = stack
                .package_managers
                .iter()
                .find(|p| matches!(p.as_str(), "pnpm" | "yarn" | "bun" | "npm"))
                .cloned()
                .or_else(|| Some("pnpm".to_string()));
            cfg.languages.node = Some(NodeLanguage {
                default_version,
                default_package_manager: default_pm,
                scip_indexer: Some("scip-typescript".to_string()),
            });
        }
        if has_python {
            cfg.languages.python = Some(PythonLanguage {
                default_version: stack
                    .runtimes
                    .python
                    .clone()
                    .unwrap_or_else(|| "3.12".to_string()),
                scip_indexer: Some("scip-python".to_string()),
            });
        }
        if has_go {
            cfg.languages.go = Some(GoLanguage {
                default_version: stack
                    .runtimes
                    .go
                    .clone()
                    .unwrap_or_else(|| "1.22".to_string()),
                scip_indexer: Some("scip-go".to_string()),
            });
        }

        // Workspace entries — route StackWorkspace.toolchain to the
        // right field per language: Rust uses `toolchain`, others use
        // `version`.
        cfg.workspaces = stack
            .workspaces
            .iter()
            .map(|ws| {
                let (toolchain, version) = match ws.language.as_str() {
                    "rust" => (ws.toolchain.clone(), None),
                    _ => (None, ws.toolchain.clone()),
                };
                Workspace {
                    slug: ws.slug.clone(),
                    root: ws.root.clone(),
                    language: ws.language.clone(),
                    toolchain,
                    version,
                    package_manager: ws.package_manager.clone(),
                }
            })
            .collect();

        cfg
    }

    /// Validate the config. Called from the MCP `_set` tool and from
    /// `project_settings_validate` before any Dolt write.
    pub fn validate(&self) -> EnvResult<()> {
        if self.schema_version == 0 {
            return Err(EnvironmentConfigError::EmptyValue {
                field: "schema_version".into(),
            });
        }
        if self.schema_version > SCHEMA_VERSION {
            return Err(EnvironmentConfigError::UnsupportedSchemaVersion {
                found: self.schema_version,
                supported: SCHEMA_VERSION,
            });
        }
        self.base.validate()?;
        self.languages.validate()?;
        validate_workspaces(&self.workspaces)?;
        self.system_packages.validate()?;
        validate_env(&self.env)?;
        self.lifecycle.validate()?;
        self.verification.validate()?;
        Ok(())
    }
}

impl Default for EnvironmentConfig {
    fn default() -> Self {
        Self::empty()
    }
}

// ---- base image ----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Distro {
    Debian,
    Alpine,
}

impl Default for Distro {
    fn default() -> Self {
        Self::Debian
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BaseImage {
    #[serde(default)]
    pub distro: Distro,
    #[serde(default = "BaseImage::default_variant")]
    pub variant: String,
}

impl BaseImage {
    fn default_variant() -> String {
        "bookworm-slim".to_owned()
    }

    fn validate(&self) -> EnvResult<()> {
        validate_identifier("base.variant", &self.variant)
    }
}

impl Default for BaseImage {
    fn default() -> Self {
        Self {
            distro: Distro::default(),
            variant: Self::default_variant(),
        }
    }
}

// ---- languages ----------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Languages {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rust: Option<RustLanguage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeLanguage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<PythonLanguage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub go: Option<GoLanguage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub java: Option<JavaLanguage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruby: Option<RubyLanguage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dotnet: Option<DotnetLanguage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clang: Option<ClangLanguage>,
}

impl Languages {
    fn validate(&self) -> EnvResult<()> {
        if let Some(r) = &self.rust {
            r.validate()?;
        }
        if let Some(n) = &self.node {
            n.validate()?;
        }
        if let Some(p) = &self.python {
            p.validate()?;
        }
        if let Some(g) = &self.go {
            g.validate()?;
        }
        if let Some(j) = &self.java {
            j.validate()?;
        }
        if let Some(r) = &self.ruby {
            r.validate()?;
        }
        if let Some(d) = &self.dotnet {
            d.validate()?;
        }
        if let Some(c) = &self.clang {
            c.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RustLanguage {
    pub default_toolchain: String,
    #[serde(default)]
    pub components: Vec<String>,
    #[serde(default)]
    pub targets: Vec<String>,
}

impl RustLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_identifier("languages.rust.default_toolchain", &self.default_toolchain)?;
        validate_identifier_list("languages.rust.components", &self.components)?;
        validate_identifier_list("languages.rust.targets", &self.targets)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeLanguage {
    pub default_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_package_manager: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scip_indexer: Option<String>,
}

impl NodeLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_identifier("languages.node.default_version", &self.default_version)?;
        if let Some(pm) = &self.default_package_manager {
            validate_identifier("languages.node.default_package_manager", pm)?;
        }
        if let Some(idx) = &self.scip_indexer {
            validate_identifier("languages.node.scip_indexer", idx)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PythonLanguage {
    pub default_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scip_indexer: Option<String>,
}

impl PythonLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_identifier("languages.python.default_version", &self.default_version)?;
        if let Some(idx) = &self.scip_indexer {
            validate_identifier("languages.python.scip_indexer", idx)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GoLanguage {
    pub default_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scip_indexer: Option<String>,
}

impl GoLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_identifier("languages.go.default_version", &self.default_version)?;
        if let Some(idx) = &self.scip_indexer {
            validate_identifier("languages.go.scip_indexer", idx)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JavaLanguage {
    pub default_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scip_indexer: Option<String>,
}

impl JavaLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_identifier("languages.java.default_version", &self.default_version)?;
        if let Some(idx) = &self.scip_indexer {
            validate_identifier("languages.java.scip_indexer", idx)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RubyLanguage {
    pub default_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scip_indexer: Option<String>,
}

impl RubyLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_identifier("languages.ruby.default_version", &self.default_version)?;
        if let Some(idx) = &self.scip_indexer {
            validate_identifier("languages.ruby.scip_indexer", idx)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DotnetLanguage {
    pub default_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scip_indexer: Option<String>,
}

impl DotnetLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_identifier("languages.dotnet.default_version", &self.default_version)?;
        if let Some(idx) = &self.scip_indexer {
            validate_identifier("languages.dotnet.scip_indexer", idx)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClangLanguage {
    pub default_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scip_indexer: Option<String>,
}

impl ClangLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_identifier("languages.clang.default_version", &self.default_version)?;
        if let Some(idx) = &self.scip_indexer {
            validate_identifier("languages.clang.scip_indexer", idx)?;
        }
        Ok(())
    }
}

// ---- workspaces ---------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub slug: String,
    pub root: String,
    pub language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_manager: Option<String>,
}

fn validate_workspaces(workspaces: &[Workspace]) -> EnvResult<()> {
    if workspaces.len() > MAX_WORKSPACES {
        return Err(EnvironmentConfigError::ListTooLong {
            field: "workspaces".into(),
            len: workspaces.len(),
            max: MAX_WORKSPACES,
        });
    }
    let mut seen: HashSet<&str> = HashSet::with_capacity(workspaces.len());
    for ws in workspaces {
        if ws.slug.is_empty() {
            return Err(EnvironmentConfigError::EmptyValue {
                field: "workspaces[*].slug".into(),
            });
        }
        if !seen.insert(ws.slug.as_str()) {
            return Err(EnvironmentConfigError::DuplicateWorkspaceSlug {
                slug: ws.slug.clone(),
            });
        }
        validate_identifier("workspaces[*].slug", &ws.slug)?;
        // `root` is a path within the repo; allow `/` and be lenient.
        validate_path("workspaces[*].root", &ws.root)?;
        validate_identifier("workspaces[*].language", &ws.language)?;
        if let Some(t) = &ws.toolchain {
            validate_identifier("workspaces[*].toolchain", t)?;
        }
        if let Some(v) = &ws.version {
            validate_identifier("workspaces[*].version", v)?;
        }
        if let Some(pm) = &ws.package_manager {
            validate_identifier("workspaces[*].package_manager", pm)?;
        }
    }
    Ok(())
}

// ---- system packages ----------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SystemPackages {
    #[serde(default)]
    pub apt: Vec<String>,
    #[serde(default)]
    pub apk: Vec<String>,
}

impl SystemPackages {
    fn validate(&self) -> EnvResult<()> {
        validate_package_list("system_packages.apt", &self.apt)?;
        validate_package_list("system_packages.apk", &self.apk)?;
        Ok(())
    }
}

fn validate_package_list(field: &str, pkgs: &[String]) -> EnvResult<()> {
    if pkgs.len() > MAX_SYSTEM_PACKAGES {
        return Err(EnvironmentConfigError::ListTooLong {
            field: field.into(),
            len: pkgs.len(),
            max: MAX_SYSTEM_PACKAGES,
        });
    }
    for pkg in pkgs {
        // Debian/Alpine package names: [A-Za-z0-9._-]+ (add `+` for C++ pkgs
        // like libstdc++-dev — allow a superset of the identifier set).
        validate_package_name(field, pkg)?;
    }
    Ok(())
}

fn validate_package_name(field: &str, value: &str) -> EnvResult<()> {
    if value.is_empty() {
        return Err(EnvironmentConfigError::EmptyValue {
            field: field.into(),
        });
    }
    if value.len() > MAX_STRING_LEN {
        return Err(EnvironmentConfigError::TooLong {
            field: field.into(),
            len: value.len(),
            max: MAX_STRING_LEN,
        });
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
    {
        return Err(EnvironmentConfigError::UnsafeIdentifier {
            field: field.into(),
            value: value.into(),
        });
    }
    Ok(())
}

// ---- env vars -----------------------------------------------------------

fn validate_env(env: &BTreeMap<String, String>) -> EnvResult<()> {
    if env.len() > MAX_ENV_ENTRIES {
        return Err(EnvironmentConfigError::ListTooLong {
            field: "env".into(),
            len: env.len(),
            max: MAX_ENV_ENTRIES,
        });
    }
    for (k, v) in env {
        if !is_valid_env_key(k) {
            return Err(EnvironmentConfigError::InvalidEnvKey { key: k.clone() });
        }
        if v.contains('\n') || v.contains('\r') || v.contains('\0') {
            return Err(EnvironmentConfigError::InvalidEnvValue { key: k.clone() });
        }
        if v.len() > MAX_STRING_LEN {
            return Err(EnvironmentConfigError::TooLong {
                field: format!("env[{k}]"),
                len: v.len(),
                max: MAX_STRING_LEN,
            });
        }
    }
    Ok(())
}

fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ---- lifecycle ----------------------------------------------------------

/// A lifecycle / verification / setup command.
///
/// Shape matches the `LifecycleCommand` enum in
/// `server/crates/djinn-agent-worker/src/lifecycle.rs`. In P5, that module's
/// local enum is replaced with this canonical definition so the on-disk
/// config JSON round-trips through both sides without a translation layer.
///
/// The three forms follow the devcontainer spec that originally inspired them:
/// a shell string passed to `/bin/sh -c`, an argv array exec'd directly, or
/// a named map run in parallel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum HookCommand {
    Shell(String),
    Exec(Vec<String>),
    Parallel(BTreeMap<String, HookCommand>),
}

impl HookCommand {
    fn validate(&self, field: &str) -> EnvResult<()> {
        match self {
            HookCommand::Shell(s) => {
                if s.len() > MAX_HOOK_SHELL_LEN {
                    return Err(EnvironmentConfigError::TooLong {
                        field: field.into(),
                        len: s.len(),
                        max: MAX_HOOK_SHELL_LEN,
                    });
                }
            }
            HookCommand::Exec(argv) => {
                if argv.is_empty() {
                    return Err(EnvironmentConfigError::EmptyValue {
                        field: field.into(),
                    });
                }
                if argv.len() > MAX_LANGUAGE_LIST {
                    return Err(EnvironmentConfigError::ListTooLong {
                        field: field.into(),
                        len: argv.len(),
                        max: MAX_LANGUAGE_LIST,
                    });
                }
                for arg in argv {
                    if arg.len() > MAX_STRING_LEN {
                        return Err(EnvironmentConfigError::TooLong {
                            field: field.into(),
                            len: arg.len(),
                            max: MAX_STRING_LEN,
                        });
                    }
                }
            }
            HookCommand::Parallel(map) => {
                if map.len() > MAX_HOOKS_PER_PHASE {
                    return Err(EnvironmentConfigError::ListTooLong {
                        field: field.into(),
                        len: map.len(),
                        max: MAX_HOOKS_PER_PHASE,
                    });
                }
                for (name, inner) in map {
                    let inner_field = format!("{field}[{name}]");
                    inner.validate(&inner_field)?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LifecycleHooks {
    /// `RUN` lines appended to the generated Dockerfile. Runs at image-build
    /// time, not at warm/task time.
    #[serde(default)]
    pub post_build: Vec<HookCommand>,
    /// Runs in the warm Pod before indexers kick off.
    #[serde(default)]
    pub pre_warm: Vec<HookCommand>,
    /// Runs in the task-run Pod before the supervisor starts.
    #[serde(default)]
    pub pre_task: Vec<HookCommand>,
}

impl LifecycleHooks {
    fn validate(&self) -> EnvResult<()> {
        validate_hook_list("lifecycle.post_build", &self.post_build)?;
        validate_hook_list("lifecycle.pre_warm", &self.pre_warm)?;
        validate_hook_list("lifecycle.pre_task", &self.pre_task)?;
        Ok(())
    }
}

fn validate_hook_list(field: &str, hooks: &[HookCommand]) -> EnvResult<()> {
    if hooks.len() > MAX_HOOKS_PER_PHASE {
        return Err(EnvironmentConfigError::ListTooLong {
            field: field.into(),
            len: hooks.len(),
            max: MAX_HOOKS_PER_PHASE,
        });
    }
    for (i, hook) in hooks.iter().enumerate() {
        hook.validate(&format!("{field}[{i}]"))?;
    }
    Ok(())
}

// ---- verification -------------------------------------------------------

/// One verification rule.
///
/// Field names and semantics match the pre-cut-over
/// `djinn_db::repositories::project::VerificationRule`, so the P5 boot hook
/// can copy `projects.verification_rules` straight into
/// `environment_config.verification.rules` without a translation step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationRule {
    pub match_pattern: String,
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Verification {
    /// Commands that set the workspace up before verification runs. Moved
    /// here from the pre-cut-over `.djinn/settings.json`'s `setup` field.
    #[serde(default)]
    pub setup: Vec<HookCommand>,
    /// Rules moved verbatim from `projects.verification_rules` by the P5
    /// boot reseed hook.
    #[serde(default)]
    pub rules: Vec<VerificationRule>,
}

impl Verification {
    fn validate(&self) -> EnvResult<()> {
        validate_hook_list("verification.setup", &self.setup)?;
        if self.rules.len() > MAX_VERIFICATION_RULES {
            return Err(EnvironmentConfigError::ListTooLong {
                field: "verification.rules".into(),
                len: self.rules.len(),
                max: MAX_VERIFICATION_RULES,
            });
        }
        for (i, rule) in self.rules.iter().enumerate() {
            if rule.match_pattern.len() > MAX_STRING_LEN {
                return Err(EnvironmentConfigError::TooLong {
                    field: format!("verification.rules[{i}].match_pattern"),
                    len: rule.match_pattern.len(),
                    max: MAX_STRING_LEN,
                });
            }
            if rule.commands.is_empty() {
                return Err(EnvironmentConfigError::EmptyVerificationCommands {
                    field: format!("verification.rules[{i}].commands"),
                });
            }
            if rule.commands.len() > MAX_HOOKS_PER_PHASE {
                return Err(EnvironmentConfigError::ListTooLong {
                    field: format!("verification.rules[{i}].commands"),
                    len: rule.commands.len(),
                    max: MAX_HOOKS_PER_PHASE,
                });
            }
            for (j, cmd) in rule.commands.iter().enumerate() {
                if cmd.len() > MAX_HOOK_SHELL_LEN {
                    return Err(EnvironmentConfigError::TooLong {
                        field: format!("verification.rules[{i}].commands[{j}]"),
                        len: cmd.len(),
                        max: MAX_HOOK_SHELL_LEN,
                    });
                }
            }
        }
        Ok(())
    }
}

// ---- string validators --------------------------------------------------

/// Accept `[A-Za-z0-9._-]+` — the character set that's safe in a `RUN`
/// `FOO="$value"` position. No whitespace, no quoting, no shell metachars.
fn validate_identifier(field: &str, value: &str) -> EnvResult<()> {
    if value.is_empty() {
        return Err(EnvironmentConfigError::EmptyValue {
            field: field.into(),
        });
    }
    if value.len() > MAX_STRING_LEN {
        return Err(EnvironmentConfigError::TooLong {
            field: field.into(),
            len: value.len(),
            max: MAX_STRING_LEN,
        });
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(EnvironmentConfigError::UnsafeIdentifier {
            field: field.into(),
            value: value.into(),
        });
    }
    Ok(())
}

fn validate_identifier_list(field: &str, values: &[String]) -> EnvResult<()> {
    if values.len() > MAX_LANGUAGE_LIST {
        return Err(EnvironmentConfigError::ListTooLong {
            field: field.into(),
            len: values.len(),
            max: MAX_LANGUAGE_LIST,
        });
    }
    for value in values {
        validate_identifier(field, value)?;
    }
    Ok(())
}

/// Accept a repo-relative path: same alphabet as `validate_identifier` plus
/// `/`. Rejects absolute paths and `..` segments to keep the reseed output
/// a pure repo-local slug.
fn validate_path(field: &str, value: &str) -> EnvResult<()> {
    if value.is_empty() {
        // Root workspace — allowed, represented as "" or "."; normalize in
        // a later pass if needed.
        return Ok(());
    }
    if value.len() > MAX_STRING_LEN {
        return Err(EnvironmentConfigError::TooLong {
            field: field.into(),
            len: value.len(),
            max: MAX_STRING_LEN,
        });
    }
    if value.starts_with('/') || value.split('/').any(|seg| seg == "..") {
        return Err(EnvironmentConfigError::UnsafeIdentifier {
            field: field.into(),
            value: value.into(),
        });
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
    {
        return Err(EnvironmentConfigError::UnsafeIdentifier {
            field: field.into(),
            value: value.into(),
        });
    }
    Ok(())
}

// ---- tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_minimal() -> EnvironmentConfig {
        EnvironmentConfig::empty()
    }

    #[test]
    fn empty_config_validates() {
        assert!(valid_minimal().validate().is_ok());
    }

    #[test]
    fn column_default_parses_to_empty_with_schema_version_zero() {
        // The Dolt column defaults to `'{}'`. That's NOT the same as
        // `EnvironmentConfig::empty()` — the former has `schema_version: 0`
        // on deserialize, which is the signal the P5 reseed hook uses to
        // spot un-reseeded projects.
        let parsed: EnvironmentConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.schema_version, 0);
        assert_eq!(parsed.source, ConfigSource::AutoDetected);
        assert!(parsed.workspaces.is_empty());
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let err = serde_json::from_value::<EnvironmentConfig>(json!({
            "schema_version": 1,
            "unknown_field": "x"
        }))
        .unwrap_err();
        assert!(err.to_string().contains("unknown_field"), "got: {err}");
    }

    #[test]
    fn rejects_schema_version_too_high() {
        let mut cfg = valid_minimal();
        cfg.schema_version = SCHEMA_VERSION + 1;
        let err = cfg.validate().unwrap_err();
        assert!(matches!(
            err,
            EnvironmentConfigError::UnsupportedSchemaVersion { .. }
        ));
    }

    #[test]
    fn rejects_shell_injection_in_toolchain() {
        let mut cfg = valid_minimal();
        cfg.languages.rust = Some(RustLanguage {
            default_toolchain: "stable; rm -rf /".to_owned(),
            components: vec![],
            targets: vec![],
        });
        let err = cfg.validate().unwrap_err();
        assert!(matches!(
            err,
            EnvironmentConfigError::UnsafeIdentifier { .. }
        ));
    }

    #[test]
    fn rejects_shell_injection_in_distro_variant() {
        let mut cfg = valid_minimal();
        cfg.base.variant = "bookworm$(whoami)".to_owned();
        let err = cfg.validate().unwrap_err();
        assert!(matches!(
            err,
            EnvironmentConfigError::UnsafeIdentifier { .. }
        ));
    }

    #[test]
    fn accepts_nightly_dated_toolchain() {
        let mut cfg = valid_minimal();
        cfg.languages.rust = Some(RustLanguage {
            default_toolchain: "nightly-2026-04-01".to_owned(),
            components: vec!["rust-analyzer".to_owned()],
            targets: vec![],
        });
        cfg.schema_version = SCHEMA_VERSION;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_duplicate_workspace_slugs() {
        let mut cfg = valid_minimal();
        cfg.workspaces = vec![
            Workspace {
                slug: "server".to_owned(),
                root: "server".to_owned(),
                language: "rust".to_owned(),
                toolchain: Some("stable".to_owned()),
                version: None,
                package_manager: None,
            },
            Workspace {
                slug: "server".to_owned(),
                root: "server2".to_owned(),
                language: "rust".to_owned(),
                toolchain: None,
                version: None,
                package_manager: None,
            },
        ];
        let err = cfg.validate().unwrap_err();
        assert!(matches!(
            err,
            EnvironmentConfigError::DuplicateWorkspaceSlug { .. }
        ));
    }

    #[test]
    fn rejects_absolute_workspace_root() {
        let mut cfg = valid_minimal();
        cfg.workspaces = vec![Workspace {
            slug: "abs".to_owned(),
            root: "/etc".to_owned(),
            language: "rust".to_owned(),
            toolchain: None,
            version: None,
            package_manager: None,
        }];
        let err = cfg.validate().unwrap_err();
        assert!(matches!(
            err,
            EnvironmentConfigError::UnsafeIdentifier { .. }
        ));
    }

    #[test]
    fn rejects_dotdot_workspace_root() {
        let mut cfg = valid_minimal();
        cfg.workspaces = vec![Workspace {
            slug: "esc".to_owned(),
            root: "../outside".to_owned(),
            language: "rust".to_owned(),
            toolchain: None,
            version: None,
            package_manager: None,
        }];
        let err = cfg.validate().unwrap_err();
        assert!(matches!(
            err,
            EnvironmentConfigError::UnsafeIdentifier { .. }
        ));
    }

    #[test]
    fn accepts_nested_workspace_root() {
        let mut cfg = valid_minimal();
        cfg.workspaces = vec![Workspace {
            slug: "tools-codegen".to_owned(),
            root: "tools/codegen".to_owned(),
            language: "rust".to_owned(),
            toolchain: Some("1.85.0".to_owned()),
            version: None,
            package_manager: None,
        }];
        cfg.schema_version = SCHEMA_VERSION;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_workspace_list_over_cap() {
        let mut cfg = valid_minimal();
        cfg.workspaces = (0..(MAX_WORKSPACES + 1))
            .map(|i| Workspace {
                slug: format!("ws-{i}"),
                root: format!("dir{i}"),
                language: "rust".to_owned(),
                toolchain: None,
                version: None,
                package_manager: None,
            })
            .collect();
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, EnvironmentConfigError::ListTooLong { .. }));
    }

    #[test]
    fn rejects_bad_env_key() {
        let mut cfg = valid_minimal();
        cfg.env.insert("3BAD".to_owned(), "v".to_owned());
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, EnvironmentConfigError::InvalidEnvKey { .. }));
    }

    #[test]
    fn rejects_newline_env_value() {
        let mut cfg = valid_minimal();
        cfg.env.insert("GOOD".to_owned(), "a\nb".to_owned());
        let err = cfg.validate().unwrap_err();
        assert!(matches!(
            err,
            EnvironmentConfigError::InvalidEnvValue { .. }
        ));
    }

    #[test]
    fn accepts_package_plus_sign() {
        let mut cfg = valid_minimal();
        cfg.system_packages.apt = vec!["libstdc++-dev".to_owned()];
        cfg.schema_version = SCHEMA_VERSION;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_package_shell_meta() {
        let mut cfg = valid_minimal();
        cfg.system_packages.apt = vec!["bash;evil".to_owned()];
        let err = cfg.validate().unwrap_err();
        assert!(matches!(
            err,
            EnvironmentConfigError::UnsafeIdentifier { .. }
        ));
    }

    #[test]
    fn verification_rules_round_trip_existing_shape() {
        // Matches the JSON shape the pre-cut-over djinn-db VerificationRule
        // emits via serde — P5's reseed hook copies that blob verbatim.
        let raw = r#"[
            {"match_pattern": "src/**/*.rs", "commands": ["cargo test"]},
            {"match_pattern": "**", "commands": ["cargo check", "cargo fmt --check"]}
        ]"#;
        let rules: Vec<VerificationRule> = serde_json::from_str(raw).unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].match_pattern, "src/**/*.rs");
        assert_eq!(rules[1].commands.len(), 2);
    }

    #[test]
    fn verification_rule_with_empty_commands_rejected() {
        let mut cfg = valid_minimal();
        cfg.verification.rules = vec![VerificationRule {
            match_pattern: "**".to_owned(),
            commands: vec![],
        }];
        let err = cfg.validate().unwrap_err();
        assert!(matches!(
            err,
            EnvironmentConfigError::EmptyVerificationCommands { .. }
        ));
    }

    #[test]
    fn hook_command_all_three_shapes_round_trip() {
        let raw = r#"{
            "schema_version": 1,
            "lifecycle": {
                "post_build": ["echo build-time"],
                "pre_warm": [["bash", "-lc", "echo ready"]],
                "pre_task": [{"index": "scip-python", "deps": "pip install -e ."}]
            }
        }"#;
        let cfg: EnvironmentConfig = serde_json::from_str(raw).unwrap();
        assert!(matches!(
            cfg.lifecycle.post_build[0],
            HookCommand::Shell(_)
        ));
        assert!(matches!(cfg.lifecycle.pre_warm[0], HookCommand::Exec(_)));
        assert!(matches!(
            cfg.lifecycle.pre_task[0],
            HookCommand::Parallel(_)
        ));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn from_stack_seeds_rust_when_only_workspace_detected() {
        // Bare Cargo.toml without a rust-toolchain.toml → no
        // runtimes.rust, but a workspace entry with no toolchain. We
        // still populate languages.rust so the image has cargo
        // + rust-analyzer.
        let mut stack = crate::schema::Stack::empty();
        stack.workspaces = vec![crate::schema::StackWorkspace {
            slug: "root".into(),
            root: "".into(),
            language: "rust".into(),
            toolchain: None,
            package_manager: None,
        }];
        let cfg = EnvironmentConfig::from_stack(&stack);
        assert_eq!(cfg.schema_version, SCHEMA_VERSION);
        assert_eq!(cfg.source, ConfigSource::AutoDetected);
        let rust = cfg.languages.rust.as_ref().expect("rust block");
        assert_eq!(rust.default_toolchain, "stable");
        assert!(rust.components.contains(&"rust-analyzer".to_string()));
        assert_eq!(cfg.workspaces.len(), 1);
        assert!(cfg.workspaces[0].toolchain.is_none());
        assert!(cfg.workspaces[0].version.is_none());
    }

    #[test]
    fn from_stack_routes_rust_toolchain_and_node_version_distinctly() {
        let mut stack = crate::schema::Stack::empty();
        stack.runtimes.rust = Some("1.84".into());
        stack.runtimes.node = Some("22".into());
        stack.package_managers = vec!["pnpm".into(), "cargo".into()];
        stack.workspaces = vec![
            crate::schema::StackWorkspace {
                slug: "server".into(),
                root: "server".into(),
                language: "rust".into(),
                toolchain: Some("stable".into()),
                package_manager: None,
            },
            crate::schema::StackWorkspace {
                slug: "ui".into(),
                root: "ui".into(),
                language: "node".into(),
                toolchain: Some("20".into()),
                package_manager: Some("pnpm".into()),
            },
        ];
        let cfg = EnvironmentConfig::from_stack(&stack);
        // Rust workspace uses `toolchain`, not `version`.
        let rust_ws = cfg.workspaces.iter().find(|w| w.slug == "server").unwrap();
        assert_eq!(rust_ws.toolchain.as_deref(), Some("stable"));
        assert!(rust_ws.version.is_none());
        // Node workspace uses `version`, not `toolchain`.
        let node_ws = cfg.workspaces.iter().find(|w| w.slug == "ui").unwrap();
        assert!(node_ws.toolchain.is_none());
        assert_eq!(node_ws.version.as_deref(), Some("20"));
        assert_eq!(node_ws.package_manager.as_deref(), Some("pnpm"));
        // Language defaults flow through verbatim.
        assert_eq!(
            cfg.languages.rust.as_ref().unwrap().default_toolchain,
            "1.84"
        );
        assert_eq!(
            cfg.languages.node.as_ref().unwrap().default_version,
            "22"
        );
        // The first Node-capable package manager wins for the language default.
        assert_eq!(
            cfg.languages
                .node
                .as_ref()
                .unwrap()
                .default_package_manager
                .as_deref(),
            Some("pnpm")
        );
    }

    #[test]
    fn from_stack_omits_languages_with_no_signal() {
        // Empty stack → empty language blocks. The Dockerfile generator
        // skips empty blocks, so the resulting image is base + worker
        // only.
        let stack = crate::schema::Stack::empty();
        let cfg = EnvironmentConfig::from_stack(&stack);
        assert!(cfg.languages.rust.is_none());
        assert!(cfg.languages.node.is_none());
        assert!(cfg.languages.python.is_none());
        assert!(cfg.languages.go.is_none());
        assert!(cfg.workspaces.is_empty());
    }

    #[test]
    fn from_stack_produces_config_that_validates() {
        let mut stack = crate::schema::Stack::empty();
        stack.runtimes.rust = Some("1.84".into());
        stack.workspaces = vec![crate::schema::StackWorkspace {
            slug: "server".into(),
            root: "server".into(),
            language: "rust".into(),
            toolchain: Some("stable".into()),
            package_manager: None,
        }];
        let cfg = EnvironmentConfig::from_stack(&stack);
        cfg.validate().unwrap();
    }

    #[test]
    fn plan_example_config_validates() {
        // The JSON example from the plan, lightly condensed — this is the
        // shape P3's image-builder golden tests will feed into the
        // Dockerfile generator.
        let raw = r#"{
            "schema_version": 1,
            "source": "auto-detected",
            "base": {"distro": "debian", "variant": "bookworm-slim"},
            "languages": {
                "rust":   {"default_toolchain": "stable", "components": ["rust-analyzer"], "targets": []},
                "node":   {"default_version": "22", "default_package_manager": "pnpm", "scip_indexer": "scip-typescript"},
                "python": {"default_version": "3.12", "scip_indexer": "scip-python"},
                "go":     {"default_version": "1.22", "scip_indexer": "scip-go"}
            },
            "workspaces": [
                {"slug": "server", "root": "server", "language": "rust", "toolchain": "stable"},
                {"slug": "tools-codegen", "root": "tools/codegen", "language": "rust", "toolchain": "1.85.0"},
                {"slug": "ui", "root": "ui", "language": "node", "version": "20", "package_manager": "pnpm"}
            ],
            "system_packages": {"apt": ["postgresql-client"], "apk": []},
            "env": {"RUST_LOG": "info"},
            "lifecycle": {"post_build": [], "pre_warm": [], "pre_task": []},
            "verification": {"setup": [], "rules": []}
        }"#;
        let cfg: EnvironmentConfig = serde_json::from_str(raw).unwrap();
        cfg.validate().unwrap();
    }
}
