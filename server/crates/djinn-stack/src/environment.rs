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
    #[error("duplicate workspace {root:?} ({language})")]
    DuplicateWorkspace { root: String, language: String },
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
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ConfigSource {
    #[default]
    AutoDetected,
    UserEdited,
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
    pub languages: Languages,
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    /// apt packages installed in the image. Alpine was dropped in the
    /// 2026-04-22 cleanup — every image is `debian:bookworm-slim` now.
    #[serde(default)]
    pub system_packages: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub lifecycle: LifecycleHooks,
    #[serde(default)]
    pub verification: Verification,
    /// Per-agent-role MCP server defaults. Moved here from the pre-cut-over
    /// `.djinn/settings.json`'s `agent_mcp_defaults` field. The key is a role
    /// name (e.g. `"worker"`, `"chat"`) or `"*"` for the fallback applied to
    /// any role with no explicit entry. The value is the list of MCP server
    /// names (from root `mcp.json`) that sessions for that role should
    /// connect to by default. Specialist role assignments override these.
    #[serde(default)]
    pub agent_mcp_defaults: BTreeMap<String, Vec<String>>,
    /// Skills injected into every agent prompt regardless of role. Moved here
    /// from the pre-cut-over `.djinn/settings.json`'s `global_skills` field.
    /// Each entry is a skill file stem (resolved against `.djinn/skills/`).
    #[serde(default)]
    pub global_skills: Vec<String>,
}

impl EnvironmentConfig {
    /// Minimal valid config — what the column's default `'{}'` parses into
    /// once the P5 reseed hook tags the source.
    pub fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            source: ConfigSource::AutoDetected,
            languages: Languages::default(),
            workspaces: Vec::new(),
            system_packages: Vec::new(),
            env: BTreeMap::new(),
            lifecycle: LifecycleHooks::default(),
            verification: Verification::default(),
            agent_mcp_defaults: BTreeMap::new(),
            global_skills: Vec::new(),
        }
    }

    /// Seed a fresh config from a freshly-detected [`crate::schema::Stack`].
    /// Called by the P5 boot reseed hook for every project whose
    /// `environment_config` column is still `'{}'`.
    ///
    /// Populates:
    /// * `schema_version`, `source = AutoDetected`
    /// * `languages.*` — one entry per language detected in the stack.
    /// * `workspaces` — one entry per `StackWorkspace`, with
    ///   toolchain/version routed to the right field per language.
    /// * `verification.rules` — safe default rules for Rust / Go workspaces.
    /// * `env`, `system_packages`, `lifecycle` — empty.
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
            });
        }
        if has_python {
            cfg.languages.python = Some(PythonLanguage {
                default_version: stack
                    .runtimes
                    .python
                    .clone()
                    .unwrap_or_else(|| "3.12".to_string()),
            });
        }
        if has_go {
            cfg.languages.go = Some(GoLanguage {
                default_version: stack
                    .runtimes
                    .go
                    .clone()
                    .unwrap_or_else(|| "1.22".to_string()),
            });
        }

        // Workspace entries — route StackWorkspace.toolchain to the
        // right field per language: Rust uses `toolchain`, others use
        // `version`.
        // Populate the toolchain/version fields so the UI shows a concrete
        // value for every workspace. Priority: (1) the workspace's own pin
        // from its manifest, (2) the project-wide detected runtime, (3) the
        // language's hard default. Means a Rust workspace without its own
        // `rust-toolchain.toml` still displays `stable` rather than an empty
        // placeholder, and the user can edit it from there.
        cfg.workspaces = stack
            .workspaces
            .iter()
            .map(|ws| {
                let (toolchain, version) = match ws.language.as_str() {
                    "rust" => {
                        let tc = ws
                            .toolchain
                            .clone()
                            .or_else(|| stack.runtimes.rust.clone())
                            .or_else(|| Some("stable".to_string()));
                        (tc, None)
                    }
                    "node" => (
                        None,
                        ws.toolchain
                            .clone()
                            .or_else(|| stack.runtimes.node.clone())
                            .or_else(|| Some("22".to_string())),
                    ),
                    "python" => (
                        None,
                        ws.toolchain
                            .clone()
                            .or_else(|| stack.runtimes.python.clone())
                            .or_else(|| Some("3.12".to_string())),
                    ),
                    "go" => (
                        None,
                        ws.toolchain
                            .clone()
                            .or_else(|| stack.runtimes.go.clone())
                            .or_else(|| Some("1.22".to_string())),
                    ),
                    _ => (None, ws.toolchain.clone()),
                };
                Workspace {
                    root: ws.root.clone(),
                    language: ws.language.clone(),
                    toolchain,
                    version,
                    package_manager: ws.package_manager.clone(),
                }
            })
            .collect();

        // Auto-populate verification rules per detected workspace using
        // language-primitive commands that succeed without project-
        // specific configuration. Node/Python/Java/Ruby/Dotnet/Clang
        // are skipped because their verification surface depends on
        // package.json scripts / pyproject extras / pom targets that
        // the user has to wire explicitly. Rust + Go have a safe default.
        cfg.verification.rules = stack
            .workspaces
            .iter()
            .filter_map(|ws| default_verification_rule(&ws.language, &ws.root))
            .collect();

        cfg
    }

    /// Validate the config. Called from the MCP `_set` tool before any Dolt write.
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
        self.languages.validate()?;
        validate_workspaces(&self.workspaces)?;
        validate_package_list("system_packages", &self.system_packages)?;
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

// Per-language knobs were pared down in the 2026-04-22 cleanup: the SCIP
// indexer, Rust `components`, and Rust `targets` are now image-builder
// concerns (hard-coded per language there). We tolerate unknown fields
// on read so old rows that still carry those keys deserialize cleanly.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RustLanguage {
    pub default_toolchain: String,
}

impl RustLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_identifier("languages.rust.default_toolchain", &self.default_toolchain)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NodeLanguage {
    pub default_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_package_manager: Option<String>,
}

impl NodeLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_identifier("languages.node.default_version", &self.default_version)?;
        if let Some(pm) = &self.default_package_manager {
            validate_identifier("languages.node.default_package_manager", pm)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PythonLanguage {
    pub default_version: String,
}

impl PythonLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_identifier("languages.python.default_version", &self.default_version)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GoLanguage {
    pub default_version: String,
}

impl GoLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_identifier("languages.go.default_version", &self.default_version)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct JavaLanguage {
    pub default_version: String,
}

impl JavaLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_identifier("languages.java.default_version", &self.default_version)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RubyLanguage {
    pub default_version: String,
}

impl RubyLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_identifier("languages.ruby.default_version", &self.default_version)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DotnetLanguage {
    pub default_version: String,
}

impl DotnetLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_identifier("languages.dotnet.default_version", &self.default_version)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClangLanguage {
    pub default_version: String,
}

impl ClangLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_identifier("languages.clang.default_version", &self.default_version)
    }
}

// ---- workspaces ---------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Workspace {
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
    let mut seen: HashSet<(&str, &str)> = HashSet::with_capacity(workspaces.len());
    for ws in workspaces {
        if !seen.insert((ws.root.as_str(), ws.language.as_str())) {
            return Err(EnvironmentConfigError::DuplicateWorkspace {
                root: ws.root.clone(),
                language: ws.language.clone(),
            });
        }
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
// Not `deny_unknown_fields`: the 2026-04-22 rename `pre_warm` → `pre_anything`
// means older rows carry a `pre_warm` key that we need to tolerate on read.
// The serde `alias` below routes that legacy key into `pre_anything`.
pub struct LifecycleHooks {
    /// `RUN` lines appended to the generated Dockerfile. Bundle anything
    /// you want baked into the image here (apt packages are the easy path;
    /// curl-installs like `protoc` go here).
    #[serde(default)]
    pub post_build: Vec<HookCommand>,
    /// Runs in every Pod djinn starts (warm AND task-run), before any
    /// djinn work. The pre-2026-04-22 `pre_warm` field routes here via
    /// the serde alias.
    #[serde(default, alias = "pre_warm")]
    pub pre_anything: Vec<HookCommand>,
    /// Runs in the task-run Pod before the supervisor starts.
    #[serde(default)]
    pub pre_task: Vec<HookCommand>,
    /// Runs once in the task-run Pod, before any verification rule fires.
    /// Typically `pnpm install` / `cargo build` / similar — commands that
    /// prepare the workspace so the `verification.rules` commands succeed.
    /// Previously lived as `verification.setup`.
    #[serde(default)]
    pub pre_verification: Vec<HookCommand>,
}

impl LifecycleHooks {
    fn validate(&self) -> EnvResult<()> {
        validate_hook_list("lifecycle.post_build", &self.post_build)?;
        validate_hook_list("lifecycle.pre_anything", &self.pre_anything)?;
        validate_hook_list("lifecycle.pre_task", &self.pre_task)?;
        validate_hook_list("lifecycle.pre_verification", &self.pre_verification)?;
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
    /// Rules moved verbatim from `projects.verification_rules` by the P5
    /// boot reseed hook. The former `setup` field lives at
    /// `lifecycle.pre_verification` now.
    #[serde(default)]
    pub rules: Vec<VerificationRule>,
}

impl Verification {
    fn validate(&self) -> EnvResult<()> {
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

/// Auto-detected verification rule for a workspace, if its language has
/// a safe language-primitive command set. Returns `None` for languages
/// whose verification surface depends on project-specific configuration
/// (Node/Python/Java/Ruby/Dotnet/Clang) — those rules are author-only.
///
/// `root` is the repo-relative workspace root (empty string for a
/// root-level workspace). Commands run from the repo root so we embed
/// the manifest path; the glob likewise anchors under `root`.
fn default_verification_rule(language: &str, root: &str) -> Option<VerificationRule> {
    // Build the glob prefix: empty root → match repo root; otherwise
    // `root/**` style match. Normalize trailing `/` so we don't end up
    // with `server//**`.
    let root_trim = root.trim_end_matches('/');
    let glob_prefix = if root_trim.is_empty() {
        String::new()
    } else {
        format!("{root_trim}/")
    };
    match language {
        "rust" => {
            let manifest = if root_trim.is_empty() {
                "Cargo.toml".to_string()
            } else {
                format!("{root_trim}/Cargo.toml")
            };
            // `cargo clippy -- -D warnings` is a strict superset of
            // `cargo check` (it runs the type checker AND clippy lints),
            // so we skip the redundant `cargo check` invocation.
            Some(VerificationRule {
                match_pattern: format!("{glob_prefix}**/*.rs"),
                commands: vec![
                    format!("cargo clippy --manifest-path {manifest} -- -D warnings"),
                    format!("cargo test --manifest-path {manifest}"),
                ],
            })
        }
        "go" => {
            let scope = if root_trim.is_empty() {
                "./...".to_string()
            } else {
                format!("./{root_trim}/...")
            };
            Some(VerificationRule {
                match_pattern: format!("{glob_prefix}**/*.go"),
                commands: vec![format!("go vet {scope}"), format!("go test {scope}")],
            })
        }
        _ => None,
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
        });
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
        });
        cfg.schema_version = SCHEMA_VERSION;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_duplicate_workspaces() {
        let mut cfg = valid_minimal();
        cfg.workspaces = vec![
            Workspace {
                root: "server".to_owned(),
                language: "rust".to_owned(),
                toolchain: Some("stable".to_owned()),
                version: None,
                package_manager: None,
            },
            Workspace {
                root: "server".to_owned(),
                language: "rust".to_owned(),
                toolchain: None,
                version: None,
                package_manager: None,
            },
        ];
        let err = cfg.validate().unwrap_err();
        assert!(matches!(
            err,
            EnvironmentConfigError::DuplicateWorkspace { .. }
        ));
    }

    #[test]
    fn accepts_same_root_different_language() {
        // The motivating case for dropping slugs: a polyglot repo with
        // multiple manifests at the root (e.g. go.mod + package.json).
        let mut cfg = valid_minimal();
        cfg.workspaces = vec![
            Workspace {
                root: "".to_owned(),
                language: "go".to_owned(),
                toolchain: None,
                version: Some("1.22".to_owned()),
                package_manager: None,
            },
            Workspace {
                root: "".to_owned(),
                language: "node".to_owned(),
                toolchain: None,
                version: Some("22".to_owned()),
                package_manager: Some("pnpm".to_owned()),
            },
        ];
        cfg.schema_version = SCHEMA_VERSION;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_absolute_workspace_root() {
        let mut cfg = valid_minimal();
        cfg.workspaces = vec![Workspace {
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
        cfg.system_packages = vec!["libstdc++-dev".to_owned()];
        cfg.schema_version = SCHEMA_VERSION;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_package_shell_meta() {
        let mut cfg = valid_minimal();
        cfg.system_packages = vec!["bash;evil".to_owned()];
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
                "pre_anything": [["bash", "-lc", "echo ready"]],
                "pre_task": [{"index": "scip-python", "deps": "pip install -e ."}]
            }
        }"#;
        let cfg: EnvironmentConfig = serde_json::from_str(raw).unwrap();
        assert!(matches!(
            cfg.lifecycle.post_build[0],
            HookCommand::Shell(_)
        ));
        assert!(matches!(cfg.lifecycle.pre_anything[0], HookCommand::Exec(_)));
        assert!(matches!(
            cfg.lifecycle.pre_task[0],
            HookCommand::Parallel(_)
        ));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn legacy_pre_warm_alias_routes_to_pre_anything() {
        // Older rows still carry `pre_warm` — the serde alias should
        // keep them loadable post-rename.
        let raw = r#"{
            "schema_version": 1,
            "lifecycle": {
                "pre_warm": ["echo legacy"]
            }
        }"#;
        let cfg: EnvironmentConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(cfg.lifecycle.pre_anything.len(), 1);
        assert!(matches!(
            cfg.lifecycle.pre_anything[0],
            HookCommand::Shell(_)
        ));
    }

    #[test]
    fn from_stack_seeds_rust_when_only_workspace_detected() {
        // Bare Cargo.toml without a rust-toolchain.toml → no
        // runtimes.rust, but a workspace entry with no toolchain. We
        // still populate languages.rust so the image has cargo
        // + rust-analyzer.
        let mut stack = crate::schema::Stack::empty();
        stack.workspaces = vec![crate::schema::StackWorkspace {
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
        assert_eq!(cfg.workspaces.len(), 1);
        // Unpinned workspace now falls back to the language default so
        // the UI can show a concrete toolchain.
        assert_eq!(cfg.workspaces[0].toolchain.as_deref(), Some("stable"));
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
                root: "server".into(),
                language: "rust".into(),
                toolchain: Some("stable".into()),
                package_manager: None,
            },
            crate::schema::StackWorkspace {
                root: "ui".into(),
                language: "node".into(),
                toolchain: Some("20".into()),
                package_manager: Some("pnpm".into()),
            },
        ];
        let cfg = EnvironmentConfig::from_stack(&stack);
        // Rust workspace uses `toolchain`, not `version`.
        let rust_ws = cfg.workspaces.iter().find(|w| w.root == "server").unwrap();
        assert_eq!(rust_ws.toolchain.as_deref(), Some("stable"));
        assert!(rust_ws.version.is_none());
        // Node workspace uses `version`, not `toolchain`.
        let node_ws = cfg.workspaces.iter().find(|w| w.root == "ui").unwrap();
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
        // Canonical post-2026-04-22-cleanup shape, matching what the
        // image-builder golden tests feed into the Dockerfile generator.
        let raw = r#"{
            "schema_version": 1,
            "source": "auto-detected",
            "languages": {
                "rust":   {"default_toolchain": "stable"},
                "node":   {"default_version": "22", "default_package_manager": "pnpm"},
                "python": {"default_version": "3.12"},
                "go":     {"default_version": "1.22"}
            },
            "workspaces": [
                {"slug": "server", "root": "server", "language": "rust", "toolchain": "stable"},
                {"slug": "tools-codegen", "root": "tools/codegen", "language": "rust", "toolchain": "1.85.0"},
                {"slug": "ui", "root": "ui", "language": "node", "version": "20", "package_manager": "pnpm"}
            ],
            "system_packages": ["postgresql-client"],
            "env": {"RUST_LOG": "info"},
            "lifecycle": {"post_build": [], "pre_anything": [], "pre_task": [], "pre_verification": []},
            "verification": {"rules": []}
        }"#;
        let cfg: EnvironmentConfig = serde_json::from_str(raw).unwrap();
        cfg.validate().unwrap();
    }
}
