// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
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

use crate::workspace_slug;

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
const MAX_WORKSPACE_TAGS: usize = 32;
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
    TooLong {
        field: String,
        len: usize,
        max: usize,
    },
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
    #[schemars(with = "i64")]
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
    /// Project-level override for Cargo target-cache warming/running policy.
    ///
    /// The default is [`CargoCachePolicy::AutoDetected`]: djinn detects the
    /// cache strategy from the repository it is about to run, including the
    /// Cargo workspace layout, `.cargo/config.toml` settings such as
    /// `rustc-wrapper`, and configured setup/verification command shapes. That
    /// detection is deliberately read-only. djinn may read `.cargo/config.toml`
    /// to keep warm-job and worker behavior consistent with the project, but it
    /// never creates, edits, or rewrites the project's `.cargo/config.toml`.
    ///
    /// Set an explicit policy only when project authors need to override the
    /// detected feature/sccache/incremental choices at the environment-config
    /// level. `None` is accepted for legacy rows and is treated the same as the
    /// default auto-detected policy by consumers.
    #[serde(default = "default_cargo_cache_policy")]
    pub cargo_cache_policy: Option<CargoCachePolicy>,
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
            cargo_cache_policy: default_cargo_cache_policy(),
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
    /// * `env`, `system_packages`, `lifecycle` — empty.
    ///
    /// Verification rules are no longer part of this config — they live in the
    /// `project_verifications` table now. Seed them via
    /// [`default_verification_rules`].
    pub fn from_stack(stack: &crate::schema::Stack) -> Self {
        let mut cfg = Self::empty();
        cfg.source = ConfigSource::AutoDetected;

        // Detect which languages appear in workspaces or runtimes so we
        // only populate `languages.*` blocks that the image will actually
        // install.
        let has_rust =
            stack.runtimes.rust.is_some() || stack.workspaces.iter().any(|w| w.language == "rust");
        let has_node =
            stack.runtimes.node.is_some() || stack.workspaces.iter().any(|w| w.language == "node");
        let has_python = stack.runtimes.python.is_some()
            || stack.workspaces.iter().any(|w| w.language == "python");
        let has_go =
            stack.runtimes.go.is_some() || stack.workspaces.iter().any(|w| w.language == "go");

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
            let default_version = stack
                .runtimes
                .node
                .clone()
                .unwrap_or_else(|| "22".to_string());
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
                    slug: Some(workspace_slug(std::path::Path::new(&ws.root))),
                    name: None,
                    tags: Vec::new(),
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
        if let Some(policy) = &self.cargo_cache_policy {
            policy.validate()?;
        }
        Ok(())
    }
}

fn default_cargo_cache_policy() -> Option<CargoCachePolicy> {
    Some(CargoCachePolicy::AutoDetected)
}

// ---- cargo cache policy --------------------------------------------------

/// Per-project Cargo target-cache strategy override.
///
/// The default [`AutoDetected`](Self::AutoDetected) mode is detection-driven:
/// consumers resolve the policy by reading the project shape (Cargo workspace
/// layout, `.cargo/config.toml`, and configured setup/verification command
/// patterns) instead of hardcoding one universal compile set. That resolver is
/// intentionally non-mutating. It may observe a project's `.cargo/config.toml`,
/// including `rustc-wrapper = "sccache"` and feature-related settings, but it
/// must never create or modify `.cargo/config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case", tag = "mode", content = "policy")]
pub enum CargoCachePolicy {
    /// Resolve the policy from the project itself at warm/worker runtime.
    #[default]
    AutoDetected,
    /// Use this explicit, project-authored override instead of detection.
    Explicit(CargoCachePolicyOverride),
}

impl CargoCachePolicy {
    fn validate(&self) -> EnvResult<()> {
        match self {
            CargoCachePolicy::AutoDetected => Ok(()),
            CargoCachePolicy::Explicit(policy) => policy.validate(),
        }
    }
}

/// Explicit Cargo target-cache policy used when auto-detection is overridden.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CargoCachePolicyOverride {
    /// Whether the Cargo root is a workspace rather than a single package.
    #[serde(default)]
    pub workspace: bool,
    /// Feature names to pass consistently to warm and worker cargo commands.
    /// Empty means default features only. Use `all_features` for
    /// `--all-features`; do not include `all-features` in this list.
    #[serde(default)]
    pub features: Vec<String>,
    /// Whether warm and worker cargo commands should pass `--all-features`.
    #[serde(default)]
    pub all_features: bool,
    /// Whether the project should use sccache for rustc invocations.
    #[serde(default)]
    pub sccache: bool,
    /// Whether to enable Cargo incremental compilation.
    #[serde(default = "default_cargo_incremental")]
    pub incremental: bool,
    /// Warm-base cargo commands derived from or overridden for this project.
    #[serde(default)]
    pub warm_commands: Vec<CargoWarmCommand>,
}

impl CargoCachePolicyOverride {
    fn validate(&self) -> EnvResult<()> {
        validate_feature_list("cargo_cache_policy.policy.features", &self.features)?;
        if self.all_features && !self.features.is_empty() {
            return Err(EnvironmentConfigError::UnsafeIdentifier {
                field: "cargo_cache_policy.policy.features".into(),
                value: "features cannot be combined with all_features".into(),
            });
        }
        if self.sccache && self.incremental {
            return Err(EnvironmentConfigError::UnsafeIdentifier {
                field: "cargo_cache_policy.policy.incremental".into(),
                value: "incremental must be false when sccache is true".into(),
            });
        }
        if self.warm_commands.len() > MAX_HOOKS_PER_PHASE {
            return Err(EnvironmentConfigError::ListTooLong {
                field: "cargo_cache_policy.policy.warm_commands".into(),
                len: self.warm_commands.len(),
                max: MAX_HOOKS_PER_PHASE,
            });
        }
        for (i, command) in self.warm_commands.iter().enumerate() {
            command.validate(&format!("cargo_cache_policy.policy.warm_commands[{i}]"))?;
        }
        Ok(())
    }
}

fn default_cargo_incremental() -> bool {
    true
}

/// One Cargo command used to warm a project's target cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CargoWarmCommand {
    /// Human-readable label for logs and metrics.
    pub label: String,
    /// Cargo subcommand and arguments, excluding the leading `cargo` binary.
    pub args: Vec<String>,
}

impl CargoWarmCommand {
    fn validate(&self, field: &str) -> EnvResult<()> {
        validate_plain_string(&format!("{field}.label"), &self.label, MAX_STRING_LEN)?;
        if self.args.is_empty() {
            return Err(EnvironmentConfigError::EmptyValue {
                field: format!("{field}.args"),
            });
        }
        if self.args.len() > MAX_LANGUAGE_LIST {
            return Err(EnvironmentConfigError::ListTooLong {
                field: format!("{field}.args"),
                len: self.args.len(),
                max: MAX_LANGUAGE_LIST,
            });
        }
        for (i, arg) in self.args.iter().enumerate() {
            validate_plain_string(&format!("{field}.args[{i}]"), arg, MAX_STRING_LEN)?;
        }
        Ok(())
    }
}

impl Default for EnvironmentConfig {
    fn default() -> Self {
        Self::empty()
    }
}

// ---- languages ----------------------------------------------------------

// NOTE: `skip_serializing_if = "Option::is_none"` is intentionally absent on
// the language fields below. The attribute is JSON-only and bincode-fatal:
// bincode is positional and reads garbage from the next field's slot when a
// serializer skips a slot the deserializer expects. `EnvironmentConfig` no
// longer crosses bincode directly (`ServiceRpcResponse::GetEnvironmentConfig`
// ships an opaque JSON-encoded `String` after the 2026-05-19 wire fix —
// `HookCommand`'s `#[serde(untagged)]` representation can't survive bincode
// either, so the whole config travels as JSON), but keeping the explicit
// `Option`-discriminant serialization preserves bincode-safety for any
// future direct embedding (and matches the SerializableDjinnEvent pattern).
// `#[serde(default)]` is preserved so JSON deserialisation still tolerates
// missing keys in older Dolt rows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Languages {
    #[serde(default)]
    pub rust: Option<RustLanguage>,
    #[serde(default)]
    pub node: Option<NodeLanguage>,
    #[serde(default)]
    pub python: Option<PythonLanguage>,
    #[serde(default)]
    pub go: Option<GoLanguage>,
    #[serde(default)]
    pub java: Option<JavaLanguage>,
    #[serde(default)]
    pub ruby: Option<RubyLanguage>,
    #[serde(default)]
    pub dotnet: Option<DotnetLanguage>,
    #[serde(default)]
    pub clang: Option<ClangLanguage>,
}

impl Languages {
    /// True when at least one language toolchain is configured. Used to decide
    /// whether a project has indexable code at all — a project with none (e.g.
    /// a docs / memory-only repo) has nothing for the canonical-graph warmer
    /// (a CODE graph) to index, so warming it is wasted work.
    pub fn has_any(&self) -> bool {
        self.rust.is_some()
            || self.node.is_some()
            || self.python.is_some()
            || self.go.is_some()
            || self.java.is_some()
            || self.ruby.is_some()
            || self.dotnet.is_some()
            || self.clang.is_some()
    }

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
    // `skip_serializing_if` dropped — `NodeLanguage` rides the bincode wire
    // inside `EnvironmentConfig`, and JSON-only skip attributes break the
    // positional codec. See the comment on `Languages`.
    #[serde(default)]
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
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub root: String,
    pub language: String,
    // `skip_serializing_if` dropped on all three Options — `Workspace` rides
    // the bincode wire inside `EnvironmentConfig`. See the comment on
    // `Languages` above for the full rationale.
    #[serde(default)]
    pub toolchain: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
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
        if let Some(slug) = &ws.slug {
            validate_identifier("workspaces[*].slug", slug)?;
        }
        if let Some(name) = &ws.name {
            validate_workspace_name("workspaces[*].name", name)?;
        }
        validate_workspace_tags("workspaces[*].tags", &ws.tags)?;
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

fn validate_workspace_name(field: &str, value: &str) -> EnvResult<()> {
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
    if value.chars().any(|c| matches!(c, '\n' | '\r' | '\0')) {
        return Err(EnvironmentConfigError::UnsafeIdentifier {
            field: field.into(),
            value: value.into(),
        });
    }
    Ok(())
}

fn validate_workspace_tags(field: &str, tags: &[String]) -> EnvResult<()> {
    if tags.len() > MAX_WORKSPACE_TAGS {
        return Err(EnvironmentConfigError::ListTooLong {
            field: field.into(),
            len: tags.len(),
            max: MAX_WORKSPACE_TAGS,
        });
    }
    for tag in tags {
        validate_identifier(field, tag)?;
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
    pub fn validate(&self) -> EnvResult<()> {
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

/// Safe default verification rules for a freshly-detected stack — one rule per
/// workspace whose language has a language-primitive command set (Rust, Go).
/// Extracted from `from_stack` when verification moved out of
/// `EnvironmentConfig` into its own `project_verifications` store; the boot
/// reseed hook and the on-demand reset path seed the verification table with
/// the output.
pub fn default_verification_rules(stack: &crate::schema::Stack) -> Vec<VerificationRule> {
    stack
        .workspaces
        .iter()
        .filter_map(|ws| default_verification_rule(&ws.language, &ws.root))
        .collect()
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

fn validate_feature_list(field: &str, features: &[String]) -> EnvResult<()> {
    if features.len() > MAX_LANGUAGE_LIST {
        return Err(EnvironmentConfigError::ListTooLong {
            field: field.into(),
            len: features.len(),
            max: MAX_LANGUAGE_LIST,
        });
    }
    for feature in features {
        validate_identifier(field, feature)?;
    }
    Ok(())
}

fn validate_plain_string(field: &str, value: &str, max: usize) -> EnvResult<()> {
    if value.is_empty() {
        return Err(EnvironmentConfigError::EmptyValue {
            field: field.into(),
        });
    }
    if value.len() > max {
        return Err(EnvironmentConfigError::TooLong {
            field: field.into(),
            len: value.len(),
            max,
        });
    }
    if value.chars().any(|c| matches!(c, '\n' | '\r' | '\0')) {
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

    #[test]
    fn languages_has_any_reflects_configured_toolchains() {
        assert!(
            !Languages::default().has_any(),
            "empty (docs-only) → no code"
        );
        let with_rust = Languages {
            rust: Some(RustLanguage {
                default_toolchain: "stable".into(),
            }),
            ..Default::default()
        };
        assert!(with_rust.has_any());
    }

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
    fn schema_version_stays_one_for_reseed_gate_compatibility() {
        // Keep this in sync with the boot reseed gate: existing declared
        // configs with schema_version >= 1 must not be forced through reseed
        // merely because workspace naming metadata was added.
        assert_eq!(SCHEMA_VERSION, 1);
        assert_eq!(EnvironmentConfig::empty().schema_version, 1);
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
                slug: None,
                name: None,
                tags: Vec::new(),
                root: "server".to_owned(),
                language: "rust".to_owned(),
                toolchain: Some("stable".to_owned()),
                version: None,
                package_manager: None,
            },
            Workspace {
                slug: None,
                name: None,
                tags: Vec::new(),
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
                slug: Some("root-go".to_owned()),
                name: Some("Go root".to_owned()),
                tags: vec!["backend".to_owned()],
                root: "".to_owned(),
                language: "go".to_owned(),
                toolchain: None,
                version: Some("1.22".to_owned()),
                package_manager: None,
            },
            Workspace {
                slug: Some("root-node".to_owned()),
                name: Some("Node root".to_owned()),
                tags: vec!["frontend".to_owned()],
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
    fn accepts_duplicate_workspace_slugs() {
        let mut cfg = valid_minimal();
        cfg.workspaces = vec![
            Workspace {
                slug: Some("shared".to_owned()),
                name: None,
                tags: Vec::new(),
                root: "server".to_owned(),
                language: "rust".to_owned(),
                toolchain: None,
                version: None,
                package_manager: None,
            },
            Workspace {
                slug: Some("shared".to_owned()),
                name: None,
                tags: Vec::new(),
                root: "ui".to_owned(),
                language: "node".to_owned(),
                toolchain: None,
                version: Some("22".to_owned()),
                package_manager: Some("pnpm".to_owned()),
            },
        ];
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_invalid_workspace_slug() {
        let mut cfg = valid_minimal();
        cfg.workspaces = vec![Workspace {
            slug: Some("bad slug".to_owned()),
            name: None,
            tags: Vec::new(),
            root: "server".to_owned(),
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
    fn accepts_human_readable_workspace_name_and_safe_tags() {
        let mut cfg = valid_minimal();
        cfg.workspaces = vec![Workspace {
            slug: Some("api".to_owned()),
            name: Some("API Server (production)".to_owned()),
            tags: vec![
                "backend".to_owned(),
                "rust_2024".to_owned(),
                "tier.1".to_owned(),
            ],
            root: "server".to_owned(),
            language: "rust".to_owned(),
            toolchain: Some("stable".to_owned()),
            version: None,
            package_manager: None,
        }];

        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_empty_workspace_name() {
        let mut cfg = valid_minimal();
        cfg.workspaces = vec![Workspace {
            slug: None,
            name: Some(String::new()),
            tags: Vec::new(),
            root: "server".to_owned(),
            language: "rust".to_owned(),
            toolchain: None,
            version: None,
            package_manager: None,
        }];

        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, EnvironmentConfigError::EmptyValue { .. }));
    }

    #[test]
    fn rejects_workspace_name_with_newline() {
        let mut cfg = valid_minimal();
        cfg.workspaces = vec![Workspace {
            slug: None,
            name: Some("API\nServer".to_owned()),
            tags: Vec::new(),
            root: "server".to_owned(),
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
    fn rejects_invalid_workspace_tag() {
        let mut cfg = valid_minimal();
        cfg.workspaces = vec![Workspace {
            slug: None,
            name: None,
            tags: vec!["bad tag".to_owned()],
            root: "server".to_owned(),
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
    fn rejects_too_many_workspace_tags() {
        let mut cfg = valid_minimal();
        cfg.workspaces = vec![Workspace {
            slug: None,
            name: None,
            tags: (0..=MAX_WORKSPACE_TAGS)
                .map(|i| format!("tag{i}"))
                .collect(),
            root: "server".to_owned(),
            language: "rust".to_owned(),
            toolchain: None,
            version: None,
            package_manager: None,
        }];

        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, EnvironmentConfigError::ListTooLong { .. }));
    }

    #[test]
    fn legacy_workspace_defaults_slug_name_and_tags() {
        let raw = r#"{
            "schema_version": 1,
            "workspaces": [
                {"root": "server", "language": "rust", "toolchain": "stable"}
            ]
        }"#;
        let cfg: EnvironmentConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(cfg.workspaces[0].slug, None);
        assert_eq!(cfg.workspaces[0].name, None);
        assert!(cfg.workspaces[0].tags.is_empty());
        cfg.validate().unwrap();
    }

    #[test]
    fn cargo_cache_policy_defaults_to_auto_detected() {
        let cfg = EnvironmentConfig::empty();
        assert_eq!(cfg.cargo_cache_policy, Some(CargoCachePolicy::AutoDetected));

        let parsed: EnvironmentConfig = serde_json::from_str(r#"{"schema_version":1}"#).unwrap();
        assert_eq!(
            parsed.cargo_cache_policy,
            Some(CargoCachePolicy::AutoDetected)
        );
        parsed.validate().unwrap();
    }

    #[test]
    fn cargo_cache_policy_explicit_round_trips_and_validates() {
        let mut cfg = EnvironmentConfig::empty();
        cfg.cargo_cache_policy = Some(CargoCachePolicy::Explicit(CargoCachePolicyOverride {
            workspace: true,
            features: vec!["ci".to_owned(), "postgres".to_owned()],
            all_features: false,
            sccache: true,
            incremental: false,
            warm_commands: vec![CargoWarmCommand {
                label: "clippy".to_owned(),
                args: vec![
                    "clippy".to_owned(),
                    "--workspace".to_owned(),
                    "--all-targets".to_owned(),
                ],
            }],
        }));

        cfg.validate().unwrap();
        let serialized = serde_json::to_string(&cfg).unwrap();
        let value: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(value["cargo_cache_policy"]["mode"], json!("explicit"));
        assert_eq!(
            value["cargo_cache_policy"]["policy"]["features"],
            json!(["ci", "postgres"])
        );

        let round_tripped: EnvironmentConfig = serde_json::from_str(&serialized).unwrap();
        assert_eq!(round_tripped, cfg);
    }

    #[test]
    fn cargo_cache_policy_rejects_malformed_override() {
        let mut cfg = EnvironmentConfig::empty();
        cfg.cargo_cache_policy = Some(CargoCachePolicy::Explicit(CargoCachePolicyOverride {
            all_features: true,
            features: vec!["ci".to_owned()],
            ..Default::default()
        }));
        assert!(matches!(
            cfg.validate().unwrap_err(),
            EnvironmentConfigError::UnsafeIdentifier { .. }
        ));

        cfg.cargo_cache_policy = Some(CargoCachePolicy::Explicit(CargoCachePolicyOverride {
            sccache: true,
            incremental: true,
            ..Default::default()
        }));
        assert!(matches!(
            cfg.validate().unwrap_err(),
            EnvironmentConfigError::UnsafeIdentifier { .. }
        ));
    }

    #[test]
    fn workspace_slug_name_and_tags_round_trip_through_json() {
        let mut cfg = EnvironmentConfig::empty();
        cfg.workspaces = vec![
            Workspace {
                slug: Some("server".to_owned()),
                name: Some("Server".to_owned()),
                tags: vec!["backend".to_owned(), "rust".to_owned()],
                root: "server".to_owned(),
                language: "rust".to_owned(),
                toolchain: Some("stable".to_owned()),
                version: None,
                package_manager: None,
            },
            Workspace {
                slug: Some("ui".to_owned()),
                name: Some("User interface".to_owned()),
                tags: vec!["frontend".to_owned(), "node".to_owned()],
                root: "ui".to_owned(),
                language: "node".to_owned(),
                toolchain: None,
                version: Some("22".to_owned()),
                package_manager: Some("pnpm".to_owned()),
            },
        ];

        let serialized = serde_json::to_string(&cfg).unwrap();
        let value: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(value["schema_version"], json!(1));
        assert_eq!(value["workspaces"][0]["slug"], json!("server"));
        assert_eq!(value["workspaces"][0]["name"], json!("Server"));
        assert_eq!(value["workspaces"][0]["tags"], json!(["backend", "rust"]));

        let round_tripped: EnvironmentConfig = serde_json::from_str(&serialized).unwrap();
        assert_eq!(round_tripped, cfg);
        round_tripped.validate().unwrap();
    }

    #[test]
    fn public_schema_exposes_workspace_metadata() {
        let schema = schemars::schema_for!(EnvironmentConfig);
        let value = serde_json::to_value(&schema).unwrap();
        let workspace = &value["$defs"]["Workspace"];

        assert_eq!(
            workspace["properties"]["slug"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(workspace["properties"]["slug"]["default"], json!(null));

        assert_eq!(
            workspace["properties"]["name"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(workspace["properties"]["name"]["default"], json!(null));

        assert_eq!(workspace["properties"]["tags"]["type"], json!("array"));
        assert_eq!(workspace["properties"]["tags"]["default"], json!([]));
        assert_eq!(
            workspace["properties"]["tags"]["items"]["type"],
            json!("string")
        );
    }

    #[test]
    fn rejects_absolute_workspace_root() {
        let mut cfg = valid_minimal();
        cfg.workspaces = vec![Workspace {
            slug: None,
            name: None,
            tags: Vec::new(),
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
            slug: None,
            name: None,
            tags: Vec::new(),
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
            slug: None,
            name: None,
            tags: Vec::new(),
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
                slug: None,
                name: None,
                tags: Vec::new(),
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
        // Verification now validates standalone (it left EnvironmentConfig).
        let v = Verification {
            rules: vec![VerificationRule {
                match_pattern: "**".to_owned(),
                commands: vec![],
            }],
        };
        let err = v.validate().unwrap_err();
        assert!(matches!(
            err,
            EnvironmentConfigError::EmptyVerificationCommands { .. }
        ));
    }

    #[test]
    fn config_no_longer_serializes_a_verification_key() {
        let json = serde_json::to_string(&EnvironmentConfig::empty()).unwrap();
        // Match the top-level key specifically — `lifecycle.pre_verification`
        // legitimately contains the substring "verification".
        assert!(
            !json.contains("\"verification\""),
            "verification moved out of EnvironmentConfig; got: {json}"
        );
    }

    #[test]
    fn default_verification_rules_seeds_rust_and_go_only() {
        let mut stack = crate::schema::Stack::empty();
        stack.workspaces = vec![
            crate::schema::StackWorkspace {
                root: "".into(),
                language: "rust".into(),
                toolchain: None,
                package_manager: None,
            },
            crate::schema::StackWorkspace {
                root: "svc".into(),
                language: "go".into(),
                toolchain: None,
                package_manager: None,
            },
            crate::schema::StackWorkspace {
                root: "web".into(),
                language: "node".into(),
                toolchain: None,
                package_manager: Some("pnpm".into()),
            },
        ];
        // Rust + Go get a safe default; Node does not.
        assert_eq!(default_verification_rules(&stack).len(), 2);
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
        assert!(matches!(cfg.lifecycle.post_build[0], HookCommand::Shell(_)));
        assert!(matches!(
            cfg.lifecycle.pre_anything[0],
            HookCommand::Exec(_)
        ));
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
        assert_eq!(
            cfg.workspaces[0].slug.as_deref(),
            Some(workspace_slug(std::path::Path::new("")).as_str())
        );
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
        assert_eq!(
            rust_ws.slug.as_deref(),
            Some(workspace_slug(std::path::Path::new("server")).as_str())
        );
        assert_eq!(rust_ws.toolchain.as_deref(), Some("stable"));
        assert!(rust_ws.version.is_none());
        // Node workspace uses `version`, not `toolchain`.
        let node_ws = cfg.workspaces.iter().find(|w| w.root == "ui").unwrap();
        assert_eq!(
            node_ws.slug.as_deref(),
            Some(workspace_slug(std::path::Path::new("ui")).as_str())
        );
        assert!(node_ws.toolchain.is_none());
        assert_eq!(node_ws.version.as_deref(), Some("20"));
        assert_eq!(node_ws.package_manager.as_deref(), Some("pnpm"));
        // Language defaults flow through verbatim.
        assert_eq!(
            cfg.languages.rust.as_ref().unwrap().default_toolchain,
            "1.84"
        );
        assert_eq!(cfg.languages.node.as_ref().unwrap().default_version, "22");
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
    fn from_stack_uses_collision_safe_workspace_slugs() {
        let mut stack = crate::schema::Stack::empty();
        stack.workspaces = vec![
            crate::schema::StackWorkspace {
                root: "packages/api".into(),
                language: "node".into(),
                toolchain: None,
                package_manager: Some("pnpm".into()),
            },
            crate::schema::StackWorkspace {
                root: "packages-api".into(),
                language: "node".into(),
                toolchain: None,
                package_manager: Some("pnpm".into()),
            },
            crate::schema::StackWorkspace {
                root: "packages api".into(),
                language: "node".into(),
                toolchain: None,
                package_manager: Some("pnpm".into()),
            },
        ];

        let cfg = EnvironmentConfig::from_stack(&stack);
        let slugs: Vec<_> = cfg
            .workspaces
            .iter()
            .map(|workspace| workspace.slug.as_deref().unwrap())
            .collect();

        assert_eq!(
            slugs,
            vec![
                workspace_slug(std::path::Path::new("packages/api")),
                workspace_slug(std::path::Path::new("packages-api")),
                workspace_slug(std::path::Path::new("packages api")),
            ]
        );
        assert_eq!(slugs[0], "packages-api-f59bf297");
        assert_eq!(slugs[1], "packages-api");
        assert!(slugs[2].starts_with("packages-api-"));
        assert_ne!(slugs[0], slugs[1]);
        assert_ne!(slugs[0], slugs[2]);
        assert_ne!(slugs[1], slugs[2]);
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
                {"slug": "server", "name": "Server", "tags": ["backend"], "root": "server", "language": "rust", "toolchain": "stable"},
                {"slug": "tools-codegen", "name": "Codegen", "tags": ["tools"], "root": "tools/codegen", "language": "rust", "toolchain": "1.85.0"},
                {"slug": "ui", "name": "UI", "tags": ["frontend"], "root": "ui", "language": "node", "version": "20", "package_manager": "pnpm"}
            ],
            "system_packages": ["postgresql-client"],
            "env": {"RUST_LOG": "info"},
            "lifecycle": {"post_build": [], "pre_anything": [], "pre_task": [], "pre_verification": []}
        }"#;
        let cfg: EnvironmentConfig = serde_json::from_str(raw).unwrap();
        cfg.validate().unwrap();
    }
}
