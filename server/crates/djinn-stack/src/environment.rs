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
const MAX_LANGUAGE_LIST: usize = 64;
const MAX_STRING_LEN: usize = 512;
const MAX_WORKSPACE_TAGS: usize = 32;
const MAX_HOOK_SHELL_LEN: usize = 16 * 1024;
const MAX_PRE_TASK_COMMANDS: usize = 20;
const MAX_PRE_TASK_COMMAND_LEN: usize = 4096;
const PRE_TASK_TIMEOUT_DEFAULT: u64 = 300;
const PRE_TASK_TIMEOUT_MIN: u64 = 1;
const PRE_TASK_TIMEOUT_MAX: u64 = 1800;
const FINAL_VERIFICATION_VERSION: u32 = 1;
const MAX_FINAL_VERIFICATION_COMMANDS: usize = 64;
const MAX_FINAL_VERIFICATION_COMMAND_GROUPS: usize = 64;
const MAX_FINAL_VERIFICATION_SELECTION_RULES: usize = 128;
const MAX_FINAL_VERIFICATION_INPUTS: usize = 128;
const MAX_FINAL_VERIFICATION_OUTPUTS: usize = 128;
const FINAL_VERIFICATION_TIMEOUT_MIN: u64 = 1;
const FINAL_VERIFICATION_TIMEOUT_MAX: u64 = 3600;

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
    #[error("{field}: value {value} out of range [{min}, {max}]")]
    OutOfRange {
        field: String,
        value: u64,
        min: u64,
        max: u64,
    },
    #[error("{field}: duplicate name after normalization: {name:?}")]
    DuplicateName { field: String, name: String },
    #[error("{field}: value {value:?} is not a valid Kubernetes resource quantity")]
    InvalidQuantity { field: String, value: String },
    #[error("{field}: request {request:?} exceeds limit {limit:?}")]
    RequestExceedsLimit {
        field: String,
        request: String,
        limit: String,
    },
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
    /// `rustc-wrapper`, and configured setup command shapes. That
    /// detection is deliberately read-only. djinn may read `.cargo/config.toml`
    /// to keep warm-job and worker behavior consistent with the project, but it
    /// never creates, edits, or rewrites the project's `.cargo/config.toml`.
    ///
    /// Set an explicit policy only when project authors need to override the
    /// detected cargo feature set at the environment-config level. `None` is
    /// accepted for legacy rows and is treated the same as the
    /// default auto-detected policy by consumers.
    #[serde(default = "default_cargo_cache_policy")]
    pub cargo_cache_policy: Option<CargoCachePolicy>,
    /// Per-agent-role MCP server defaults. The key is a role
    /// name (e.g. `"worker"`, `"chat"`) or `"*"` for the fallback applied to
    /// any role with no explicit entry. The value is the list of MCP server
    /// names (from root `mcp.json`) that sessions for that role should
    /// connect to by default. Specialist role assignments override these.
    #[serde(default)]
    pub agent_mcp_defaults: BTreeMap<String, Vec<String>>,
    /// Skills injected into every agent prompt regardless of role. Each entry
    /// is a registered skill identifier.
    #[serde(default)]
    pub global_skills: Vec<String>,
    /// Optional per-project CPU/memory overrides for the task-run and warm
    /// Pods, layered over the deployment-wide defaults. `None` (the default)
    /// keeps every kind on its deployment default. Task and warm blocks resolve
    /// independently; the resolved quantities are rendered into the k8s Job/Pod
    /// specs by `djinn-k8s`. See [`crate::resources::BuildResources`].
    #[serde(default)]
    pub build_resources: Option<crate::resources::BuildResources>,
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
            build_resources: None,
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
        if let Some(build_resources) = &self.build_resources {
            build_resources.validate()?;
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
/// layout, `.cargo/config.toml`, and configured setup command
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
///
/// NOT `deny_unknown_fields`: the dead `sccache`/`incremental` knobs were
/// removed once the platform began forcing `CARGO_INCREMENTAL=1` +
/// `RUSTC_WRAPPER=""` on every warm/verify/worker pod (PR #874). Stored rows
/// may still carry those keys; serde ignores them on read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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

    /// Validate each language block, in canonical order: rust, node, python,
    /// go, java, ruby, dotnet, clang.  The first invalid language block
    /// determines the returned error.
    fn validate(&self) -> EnvResult<()> {
        // Order matches the field declaration order and the historical
        // validation sequence.  Changing this order would change which error
        // is returned first for a config with multiple invalid language blocks.
        validate_optional(&self.rust)?;
        validate_optional(&self.node)?;
        validate_optional(&self.python)?;
        validate_optional(&self.go)?;
        validate_optional(&self.java)?;
        validate_optional(&self.ruby)?;
        validate_optional(&self.dotnet)?;
        validate_optional(&self.clang)?;
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NodeLanguage {
    pub default_version: String,
    // `skip_serializing_if` dropped — `NodeLanguage` rides the bincode wire
    // inside `EnvironmentConfig`, and JSON-only skip attributes break the
    // positional codec. See the comment on `Languages`.
    #[serde(default)]
    pub default_package_manager: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PythonLanguage {
    pub default_version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct GoLanguage {
    pub default_version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct JavaLanguage {
    pub default_version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RubyLanguage {
    pub default_version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DotnetLanguage {
    pub default_version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClangLanguage {
    pub default_version: String,
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
        validate_workspace_fields(ws)?;
    }
    Ok(())
}

/// Validate all fields of a single workspace, preserving the exact
/// field-path strings and early-return order established by the
/// original inline loop.
fn validate_workspace_fields(ws: &Workspace) -> EnvResult<()> {
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
        validate_env_entry(k, v)?;
    }
    Ok(())
}

/// Validate a single env var entry, preserving exact error variants
/// and early-return order.
fn validate_env_entry(key: &str, value: &str) -> EnvResult<()> {
    if !is_valid_env_key(key) {
        return Err(EnvironmentConfigError::InvalidEnvKey { key: key.into() });
    }
    if value.contains('\n') || value.contains('\r') || value.contains('\0') {
        return Err(EnvironmentConfigError::InvalidEnvValue { key: key.into() });
    }
    if value.len() > MAX_STRING_LEN {
        return Err(EnvironmentConfigError::TooLong {
            field: format!("env[{key}]"),
            len: value.len(),
            max: MAX_STRING_LEN,
        });
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

/// A lifecycle / setup command.
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

/// Failure policy for a pre-task command.
///
/// * `blocking` (default) — the task run fails if the command fails.
/// * `best_effort` — failures are logged but do not abort the task run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PreTaskFailurePolicy {
    #[default]
    Blocking,
    BestEffort,
}

/// A named pre-task command declared in the project environment config.
///
/// Pre-task commands run in the task-run Pod before the supervisor starts.
/// Each command carries an optional name (auto-generated as `pre_task_N`
/// when omitted), a shell command string, a timeout, and a failure policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PreTaskCommand {
    /// Optional display/identity name. When `None`, resolved to
    /// `pre_task_1`, `pre_task_2`, etc. at validation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Shell command passed to `/bin/sh -c`.
    pub command: String,
    /// Maximum wall-clock seconds the command may run. Default 300 (5 min).
    #[serde(default = "default_pre_task_timeout")]
    #[schemars(with = "i64")]
    pub timeout_seconds: u64,
    /// What to do when the command exits non-zero.
    #[serde(default)]
    pub failure_policy: PreTaskFailurePolicy,
}

fn default_pre_task_timeout() -> u64 {
    PRE_TASK_TIMEOUT_DEFAULT
}

impl PreTaskCommand {
    /// Return the effective name: supplied or auto-generated from the
    /// 1-based index.
    pub fn resolved_name(&self, index: usize) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("pre_task_{}", index + 1))
    }

    fn validate(&self, field: &str) -> EnvResult<()> {
        // Name — only validate when explicitly supplied.
        if let Some(name) = &self.name {
            validate_identifier(&format!("{field}.name"), name)?;
        }

        // Command — non-empty and capped.
        if self.command.is_empty() {
            return Err(EnvironmentConfigError::EmptyValue {
                field: format!("{field}.command"),
            });
        }
        if self.command.len() > MAX_PRE_TASK_COMMAND_LEN {
            return Err(EnvironmentConfigError::TooLong {
                field: format!("{field}.command"),
                len: self.command.len(),
                max: MAX_PRE_TASK_COMMAND_LEN,
            });
        }

        // Timeout — inclusive range.
        if self.timeout_seconds < PRE_TASK_TIMEOUT_MIN
            || self.timeout_seconds > PRE_TASK_TIMEOUT_MAX
        {
            return Err(EnvironmentConfigError::OutOfRange {
                field: format!("{field}.timeout_seconds"),
                value: self.timeout_seconds,
                min: PRE_TASK_TIMEOUT_MIN,
                max: PRE_TASK_TIMEOUT_MAX,
            });
        }

        Ok(())
    }
}

/// Validate a list of [`PreTaskCommand`]s: cap the list length, validate
/// each command, then check that resolved names are unique.
fn validate_pre_task_commands(field: &str, commands: &[PreTaskCommand]) -> EnvResult<()> {
    if commands.len() > MAX_PRE_TASK_COMMANDS {
        return Err(EnvironmentConfigError::ListTooLong {
            field: field.into(),
            len: commands.len(),
            max: MAX_PRE_TASK_COMMANDS,
        });
    }

    let mut seen_names = HashSet::new();
    for (i, cmd) in commands.iter().enumerate() {
        let cmd_field = format!("{field}[{i}]");
        cmd.validate(&cmd_field)?;

        let resolved = cmd.resolved_name(i);
        if !seen_names.insert(resolved.clone()) {
            return Err(EnvironmentConfigError::DuplicateName {
                field: field.into(),
                name: resolved,
            });
        }
    }
    Ok(())
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
    pub pre_task: Vec<PreTaskCommand>,
    /// Workspace setup hook that runs once in the task-run Pod before the
    /// supervisor starts. Typically `pnpm install` / `cargo build` / similar
    /// — commands that prepare the workspace for the agent session.
    #[serde(default)]
    pub pre_verification: Vec<HookCommand>,
    /// Authoritative post-authoring plan, distinct from setup-time hooks.
    #[serde(default)]
    pub final_verification: FinalVerificationPlan,
}

impl LifecycleHooks {
    fn validate(&self) -> EnvResult<()> {
        validate_lifecycle_phase("lifecycle.post_build", &self.post_build)?;
        validate_lifecycle_phase("lifecycle.pre_anything", &self.pre_anything)?;
        validate_pre_task_commands("lifecycle.pre_task", &self.pre_task)?;
        validate_lifecycle_phase("lifecycle.pre_verification", &self.pre_verification)?;
        self.final_verification.validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FinalVerificationPlan {
    #[serde(default = "final_verification_version")]
    #[schemars(with = "i64")]
    pub version: u32,
    #[serde(default)]
    pub profile_id: String,
    #[serde(default)]
    #[schemars(with = "i64")]
    pub profile_revision: u32,
    /// Legacy flat declaration. New configurations must use `command_groups`
    /// and `selection_rules`; this remains readable so persisted rows can be
    /// migrated without a flag day.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<FinalVerificationCommand>,
    /// Ordered command groups used by path selection in new declarations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command_groups: Vec<FinalVerificationCommandGroup>,
    /// Ordered path rules. A matching rule selects its named command groups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selection_rules: Vec<FinalVerificationSelectionRule>,
    #[serde(default)]
    pub required_checks: Vec<String>,
    #[serde(default)]
    pub input_manifest: VerificationInputManifest,
    #[serde(default)]
    pub read_only_external_inputs: Vec<ExternalInputDeclaration>,
    #[serde(default)]
    pub output_only_globs: Vec<String>,
    #[serde(default)]
    pub hermeticity: HermeticityDeclaration,
}

/// Manual `Default` so that `version` matches the serde default (1) rather
/// than the zero the derive would produce. This is the sole unconfigured
/// declaration accepted by validation; partial declarations must be complete.
impl Default for FinalVerificationPlan {
    fn default() -> Self {
        FinalVerificationPlan {
            version: FINAL_VERIFICATION_VERSION,
            profile_id: String::new(),
            profile_revision: 0,
            commands: Vec::new(),
            command_groups: Vec::new(),
            selection_rules: Vec::new(),
            required_checks: Vec::new(),
            input_manifest: VerificationInputManifest::default(),
            read_only_external_inputs: Vec::new(),
            output_only_globs: Vec::new(),
            hermeticity: HermeticityDeclaration::default(),
        }
    }
}

fn final_verification_version() -> u32 {
    FINAL_VERIFICATION_VERSION
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FinalVerificationCommand {
    pub check_id: String,
    pub executable: String,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub working_directory: String,
    #[serde(default)]
    pub environment_names: Vec<String>,
    #[schemars(with = "i64")]
    pub timeout_seconds: u64,
    #[serde(default = "final_verification_version")]
    #[schemars(with = "i64")]
    pub descriptor_revision: u32,
}

/// A named, ordered subset of a final-verification plan.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FinalVerificationCommandGroup {
    pub name: String,
    #[serde(default)]
    pub commands: Vec<FinalVerificationCommand>,
}

/// Ordered path selection for one or more command groups.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FinalVerificationSelectionRule {
    /// Repository-relative globs. `**` is the explicit fail-safe catch-all.
    #[serde(default, rename = "match")]
    pub match_globs: Vec<String>,
    /// Names of command groups selected by this rule.
    #[serde(default)]
    pub command_groups: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationInputManifest {
    #[serde(default = "final_verification_version")]
    #[schemars(with = "i64")]
    pub version: u32,
    #[serde(default)]
    pub repo_paths: Vec<String>,
    #[serde(default)]
    pub environment_names: Vec<String>,
}

/// Manual `Default` so that `version` matches the serde default (1).
impl Default for VerificationInputManifest {
    fn default() -> Self {
        VerificationInputManifest {
            version: FINAL_VERIFICATION_VERSION,
            repo_paths: Vec::new(),
            environment_names: Vec::new(),
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExternalInputDeclaration {
    pub id: String,
    pub locator: String,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HermeticityDeclaration {
    #[serde(default)]
    pub hermetic: bool,
    #[serde(default)]
    pub reusable: bool,
    #[serde(default)]
    pub network_access: bool,
}
impl FinalVerificationPlan {
    /// Normalize both persisted plan representations into ordered commands.
    ///
    /// `commands` is retained exclusively for legacy persisted rows. Valid new
    /// plans use ordered command groups, so consumers can use this method and
    /// never need to execute two different plan representations.
    pub fn normalized_commands(&self) -> Vec<FinalVerificationCommand> {
        if self.command_groups.is_empty() {
            self.commands.clone()
        } else {
            self.command_groups
                .iter()
                .flat_map(|group| group.commands.iter().cloned())
                .collect()
        }
    }

    fn validate(&self) -> EnvResult<()> {
        if self.version != 1 {
            return Err(EnvironmentConfigError::OutOfRange {
                field: "lifecycle.final_verification.version".into(),
                value: self.version as u64,
                min: 1,
                max: 1,
            });
        };
        // Preserve the serde default as an unconfigured lifecycle. Do not
        // treat any partial declaration as empty: independent declarations
        // such as a manifest, external inputs, outputs, or hermeticity must
        // still be validated rather than bypassing the contract.
        if self == &Self::default() {
            return Ok(());
        }
        validate_identifier("lifecycle.final_verification.profile_id", &self.profile_id)?;
        if self.profile_revision == 0 {
            return Err(EnvironmentConfigError::EmptyValue {
                field: "lifecycle.final_verification.profile_revision".into(),
            });
        }
        if !self.commands.is_empty()
            && (!self.command_groups.is_empty() || !self.selection_rules.is_empty())
        {
            return Err(EnvironmentConfigError::UnsafeIdentifier {
                field: "lifecycle.final_verification.commands".into(),
                value: "legacy commands cannot be combined with command_groups or selection_rules"
                    .into(),
            });
        }
        let mut ids = HashSet::new();
        if self.command_groups.is_empty() {
            validate_final_verification_commands(
                &self.commands,
                "lifecycle.final_verification.commands",
                &mut ids,
            )?;
        } else {
            if self.command_groups.len() > MAX_FINAL_VERIFICATION_COMMAND_GROUPS {
                return Err(EnvironmentConfigError::ListTooLong {
                    field: "lifecycle.final_verification.command_groups".into(),
                    len: self.command_groups.len(),
                    max: MAX_FINAL_VERIFICATION_COMMAND_GROUPS,
                });
            }
            let mut group_names = HashSet::new();
            let mut command_count = 0;
            for (index, group) in self.command_groups.iter().enumerate() {
                let group_field = format!("lifecycle.final_verification.command_groups[{index}]");
                validate_identifier(&format!("{group_field}.name"), &group.name)?;
                if !group_names.insert(group.name.as_str()) {
                    return Err(EnvironmentConfigError::DuplicateName {
                        field: "lifecycle.final_verification.command_groups".into(),
                        name: group.name.clone(),
                    });
                }
                if group.commands.is_empty() {
                    return Err(EnvironmentConfigError::EmptyValue {
                        field: format!("{group_field}.commands"),
                    });
                }
                command_count += group.commands.len();
                validate_final_verification_commands(
                    &group.commands,
                    &format!("{group_field}.commands"),
                    &mut ids,
                )?;
            }
            if command_count > MAX_FINAL_VERIFICATION_COMMANDS {
                return Err(EnvironmentConfigError::ListTooLong {
                    field: "lifecycle.final_verification.command_groups".into(),
                    len: command_count,
                    max: MAX_FINAL_VERIFICATION_COMMANDS,
                });
            }
            validate_selection_rules(&self.selection_rules, &group_names)?;
        }
        if ids.is_empty() {
            return Err(EnvironmentConfigError::EmptyValue {
                field: "lifecycle.final_verification.commands".into(),
            });
        }
        if self.required_checks.len() > MAX_FINAL_VERIFICATION_COMMANDS {
            return Err(EnvironmentConfigError::ListTooLong {
                field: "lifecycle.final_verification.required_checks".into(),
                len: self.required_checks.len(),
                max: MAX_FINAL_VERIFICATION_COMMANDS,
            });
        }
        let mut required_ids = HashSet::new();
        for x in &self.required_checks {
            validate_identifier("lifecycle.final_verification.required_checks", x)?;
            if !required_ids.insert(x.as_str()) {
                return Err(EnvironmentConfigError::DuplicateName {
                    field: "lifecycle.final_verification.required_checks".into(),
                    name: x.clone(),
                });
            }
            if !ids.contains(x.as_str()) {
                return Err(EnvironmentConfigError::UnsafeIdentifier {
                    field: "lifecycle.final_verification.required_checks".into(),
                    value: x.clone(),
                });
            }
        }
        if !self.command_groups.is_empty() {
            validate_selected_required_checks(
                &self.command_groups,
                &self.selection_rules,
                &self.required_checks,
            )?;
        }
        if self.input_manifest.version != 1 {
            return Err(EnvironmentConfigError::OutOfRange {
                field: "lifecycle.final_verification.input_manifest.version".into(),
                value: self.input_manifest.version as u64,
                min: 1,
                max: 1,
            });
        };
        // Bounded manifest paths/environment_names lists.
        if self.input_manifest.repo_paths.len() > MAX_FINAL_VERIFICATION_INPUTS {
            return Err(EnvironmentConfigError::ListTooLong {
                field: "lifecycle.final_verification.input_manifest.repo_paths".into(),
                len: self.input_manifest.repo_paths.len(),
                max: MAX_FINAL_VERIFICATION_INPUTS,
            });
        };
        for p in &self.input_manifest.repo_paths {
            validate_path("lifecycle.final_verification.input_manifest.repo_paths", p)?;
        }
        if self.input_manifest.environment_names.len() > MAX_ENV_ENTRIES {
            return Err(EnvironmentConfigError::ListTooLong {
                field: "lifecycle.final_verification.input_manifest.environment_names".into(),
                len: self.input_manifest.environment_names.len(),
                max: MAX_ENV_ENTRIES,
            });
        };
        for env_name in &self.input_manifest.environment_names {
            validate_identifier(
                "lifecycle.final_verification.input_manifest.environment_names",
                env_name,
            )?;
        }
        // Bounded external inputs list with uniqueness checks.
        if self.read_only_external_inputs.len() > MAX_FINAL_VERIFICATION_INPUTS {
            return Err(EnvironmentConfigError::ListTooLong {
                field: "lifecycle.final_verification.read_only_external_inputs".into(),
                len: self.read_only_external_inputs.len(),
                max: MAX_FINAL_VERIFICATION_INPUTS,
            });
        };
        let mut input_ids = HashSet::new();
        for input in &self.read_only_external_inputs {
            validate_plain_string(
                "lifecycle.final_verification.read_only_external_inputs.id",
                &input.id,
                MAX_STRING_LEN,
            )?;
            if !input_ids.insert(input.id.as_str()) {
                return Err(EnvironmentConfigError::DuplicateName {
                    field: "lifecycle.final_verification.read_only_external_inputs".into(),
                    name: input.id.clone(),
                });
            };
            validate_plain_string(
                "lifecycle.final_verification.read_only_external_inputs.locator",
                &input.locator,
                MAX_STRING_LEN,
            )?;
        }
        // Bounded output-only globs list with path safety.
        if self.output_only_globs.len() > MAX_FINAL_VERIFICATION_OUTPUTS {
            return Err(EnvironmentConfigError::ListTooLong {
                field: "lifecycle.final_verification.output_only_globs".into(),
                len: self.output_only_globs.len(),
                max: MAX_FINAL_VERIFICATION_OUTPUTS,
            });
        };
        for glob in &self.output_only_globs {
            validate_glob("lifecycle.final_verification.output_only_globs", glob)?;
        }
        if self.hermeticity.reusable
            && (!self.hermeticity.hermetic || self.hermeticity.network_access)
        {
            return Err(EnvironmentConfigError::UnsafeIdentifier {
                field: "lifecycle.final_verification.hermeticity.reusable".into(),
                value: "reusable plans must be hermetic and deny network access".into(),
            });
        };
        Ok(())
    }
}
fn validate_final_verification_commands(
    commands: &[FinalVerificationCommand],
    field: &str,
    ids: &mut HashSet<String>,
) -> EnvResult<()> {
    if commands.len() > MAX_FINAL_VERIFICATION_COMMANDS {
        return Err(EnvironmentConfigError::ListTooLong {
            field: field.into(),
            len: commands.len(),
            max: MAX_FINAL_VERIFICATION_COMMANDS,
        });
    }
    for command in commands {
        validate_identifier(&format!("{field}.check_id"), &command.check_id)?;
        if !ids.insert(command.check_id.clone()) {
            return Err(EnvironmentConfigError::DuplicateName {
                field: field.into(),
                name: command.check_id.clone(),
            });
        }
        validate_plain_string(
            &format!("{field}.executable"),
            &command.executable,
            MAX_STRING_LEN,
        )?;
        if command.argv.len() > MAX_STRING_LEN {
            return Err(EnvironmentConfigError::ListTooLong {
                field: format!("{field}.argv"),
                len: command.argv.len(),
                max: MAX_STRING_LEN,
            });
        }
        for arg in &command.argv {
            validate_plain_string(&format!("{field}.argv"), arg, MAX_STRING_LEN)?;
        }
        validate_path(
            &format!("{field}.working_directory"),
            &command.working_directory,
        )?;
        if command.environment_names.len() > MAX_ENV_ENTRIES {
            return Err(EnvironmentConfigError::ListTooLong {
                field: format!("{field}.environment_names"),
                len: command.environment_names.len(),
                max: MAX_ENV_ENTRIES,
            });
        }
        for env_name in &command.environment_names {
            validate_identifier(&format!("{field}.environment_names"), env_name)?;
        }
        if command.timeout_seconds < FINAL_VERIFICATION_TIMEOUT_MIN
            || command.timeout_seconds > FINAL_VERIFICATION_TIMEOUT_MAX
        {
            return Err(EnvironmentConfigError::OutOfRange {
                field: format!("{field}.timeout_seconds"),
                value: command.timeout_seconds,
                min: FINAL_VERIFICATION_TIMEOUT_MIN,
                max: FINAL_VERIFICATION_TIMEOUT_MAX,
            });
        }
        if command.descriptor_revision == 0 {
            return Err(EnvironmentConfigError::EmptyValue {
                field: format!("{field}.descriptor_revision"),
            });
        }
    }
    Ok(())
}

fn validate_selection_rules(
    rules: &[FinalVerificationSelectionRule],
    group_names: &HashSet<&str>,
) -> EnvResult<()> {
    if rules.is_empty() {
        return Err(EnvironmentConfigError::EmptyValue {
            field: "lifecycle.final_verification.selection_rules".into(),
        });
    }
    if rules.len() > MAX_FINAL_VERIFICATION_SELECTION_RULES {
        return Err(EnvironmentConfigError::ListTooLong {
            field: "lifecycle.final_verification.selection_rules".into(),
            len: rules.len(),
            max: MAX_FINAL_VERIFICATION_SELECTION_RULES,
        });
    }
    let mut has_fail_safe = false;
    for (index, rule) in rules.iter().enumerate() {
        let field = format!("lifecycle.final_verification.selection_rules[{index}]");
        if rule.match_globs.is_empty() {
            return Err(EnvironmentConfigError::EmptyValue {
                field: format!("{field}.match"),
            });
        }
        if rule.match_globs.len() > MAX_FINAL_VERIFICATION_INPUTS {
            return Err(EnvironmentConfigError::ListTooLong {
                field: format!("{field}.match"),
                len: rule.match_globs.len(),
                max: MAX_FINAL_VERIFICATION_INPUTS,
            });
        }
        for glob in &rule.match_globs {
            validate_glob(&format!("{field}.match"), glob)?;
        }
        if rule.command_groups.is_empty() {
            return Err(EnvironmentConfigError::EmptyValue {
                field: format!("{field}.command_groups"),
            });
        }
        let mut referenced = HashSet::new();
        for name in &rule.command_groups {
            validate_identifier(&format!("{field}.command_groups"), name)?;
            if !referenced.insert(name.as_str()) {
                return Err(EnvironmentConfigError::DuplicateName {
                    field: format!("{field}.command_groups"),
                    name: name.clone(),
                });
            }
            if !group_names.contains(name.as_str()) {
                return Err(EnvironmentConfigError::UnsafeIdentifier {
                    field: format!("{field}.command_groups"),
                    value: name.clone(),
                });
            }
        }
        if rule.match_globs.iter().any(|glob| glob == "**") {
            if referenced.len() != group_names.len() {
                return Err(EnvironmentConfigError::UnsafeIdentifier {
                    field: format!("{field}.command_groups"),
                    value: "catch-all rule must reference every command group".into(),
                });
            }
            has_fail_safe = true;
        }
    }
    if !has_fail_safe {
        return Err(EnvironmentConfigError::EmptyValue {
            field: "lifecycle.final_verification.selection_rules".into(),
        });
    }
    Ok(())
}

fn validate_selected_required_checks(
    groups: &[FinalVerificationCommandGroup],
    rules: &[FinalVerificationSelectionRule],
    required_checks: &[String],
) -> EnvResult<()> {
    for (index, rule) in rules.iter().enumerate() {
        let selected: HashSet<&str> = rule
            .command_groups
            .iter()
            .flat_map(|name| {
                groups
                    .iter()
                    .find(|group| group.name == *name)
                    .into_iter()
                    .flat_map(|group| {
                        group
                            .commands
                            .iter()
                            .map(|command| command.check_id.as_str())
                    })
            })
            .collect();
        for required in required_checks {
            if !selected.contains(required.as_str()) {
                return Err(EnvironmentConfigError::UnsafeIdentifier {
                    field: format!(
                        "lifecycle.final_verification.selection_rules[{index}].command_groups"
                    ),
                    value: required.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Validate one lifecycle phase: cap the list length, then validate each hook
/// with its indexed field path.  Preserves exact field strings and early-return
/// order.
fn validate_lifecycle_phase(field: &str, hooks: &[HookCommand]) -> EnvResult<()> {
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

// ---- validation helpers -------------------------------------------------

/// Call `validate` on the inner value when the option is `Some`.
/// Preserves early-return semantics: the first validation failure wins.
fn validate_optional<T: Validatable>(opt: &Option<T>) -> EnvResult<()> {
    if let Some(inner) = opt {
        inner.validate()?;
    }
    Ok(())
}

/// Trait for types that carry a `validate(&self) -> EnvResult<()>` method.
/// Used by [`validate_optional`] to dispatch validation generically on
/// per-language `Option<T>` fields.
trait Validatable {
    fn validate(&self) -> EnvResult<()>;
}

impl Validatable for RustLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_identifier("languages.rust.default_toolchain", &self.default_toolchain)
    }
}

impl Validatable for NodeLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_identifier("languages.node.default_version", &self.default_version)?;
        if let Some(pm) = &self.default_package_manager {
            validate_identifier("languages.node.default_package_manager", pm)?;
        }
        Ok(())
    }
}

impl Validatable for PythonLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_default_version("python", &self.default_version)
    }
}

impl Validatable for GoLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_default_version("go", &self.default_version)
    }
}

impl Validatable for JavaLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_default_version("java", &self.default_version)
    }
}

impl Validatable for RubyLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_default_version("ruby", &self.default_version)
    }
}

impl Validatable for DotnetLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_default_version("dotnet", &self.default_version)
    }
}

impl Validatable for ClangLanguage {
    fn validate(&self) -> EnvResult<()> {
        validate_default_version("clang", &self.default_version)
    }
}

/// Validate a language's `default_version` field as an identifier.
/// Centralizes the repeated `validate_identifier("languages.<lang>.default_version", ...)`
/// call shared by six of the eight per-language validators.
fn validate_default_version(lang: &str, value: &str) -> EnvResult<()> {
    validate_identifier(&format!("languages.{lang}.default_version"), value)
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

/// Validate an output-only glob: like a repo-relative path but allows `*`
/// and `**` wildcard segments. Rejects absolute paths, `..` segments, and
/// unsafe characters. Globs must stay repo-relative.
fn validate_glob(field: &str, value: &str) -> EnvResult<()> {
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
    if value.starts_with('/') || value.split('/').any(|seg| seg == "..") {
        return Err(EnvironmentConfigError::UnsafeIdentifier {
            field: field.into(),
            value: value.into(),
        });
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '*' | '?'))
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
    }

    #[test]
    fn cargo_cache_policy_override_ignores_legacy_sccache_and_incremental_keys() {
        // Backward-compat: stored env-configs written before the dead
        // `sccache`/`incremental` knobs were removed still carry those keys.
        // `CargoCachePolicyOverride` is NOT `deny_unknown_fields`, so the old
        // JSON must still deserialize cleanly (the extra keys are dropped).
        let raw = r#"{
            "schema_version": 1,
            "cargo_cache_policy": {
                "mode": "explicit",
                "policy": {
                    "workspace": true,
                    "features": ["ci"],
                    "all_features": false,
                    "sccache": true,
                    "incremental": false,
                    "warm_commands": []
                }
            }
        }"#;
        let cfg: EnvironmentConfig = serde_json::from_str(raw).expect("legacy keys must parse");
        match cfg.cargo_cache_policy {
            Some(CargoCachePolicy::Explicit(ref policy)) => {
                assert!(policy.workspace);
                assert_eq!(policy.features, vec!["ci".to_owned()]);
                assert!(!policy.all_features);
            }
            other => panic!("expected explicit policy, got {other:?}"),
        }
        cfg.validate().unwrap();
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
    fn hook_command_all_three_shapes_round_trip() {
        let raw = r#"{
            "schema_version": 1,
            "lifecycle": {
                "post_build": ["echo build-time"],
                "pre_anything": [["bash", "-lc", "echo ready"]],
                "pre_task": [{"command": "pip install -e .", "name": "install-deps"}]
            }
        }"#;
        let cfg: EnvironmentConfig = serde_json::from_str(raw).unwrap();
        assert!(matches!(cfg.lifecycle.post_build[0], HookCommand::Shell(_)));
        assert!(matches!(
            cfg.lifecycle.pre_anything[0],
            HookCommand::Exec(_)
        ));
        assert_eq!(cfg.lifecycle.pre_task[0].command, "pip install -e .");
        assert_eq!(
            cfg.lifecycle.pre_task[0].name.as_deref(),
            Some("install-deps")
        );
        assert!(cfg.validate().is_ok());
    }

    fn final_verification_fixture() -> serde_json::Value {
        json!({
            "version": 1,
            "profile_id": "default",
            "profile_revision": 1,
            "commands": [{
                "check_id": "cargo-test",
                "executable": "cargo",
                "argv": ["test", "--locked"],
                "working_directory": "server",
                "environment_names": ["RUST_LOG"],
                "timeout_seconds": 600,
                "descriptor_revision": 1
            }],
            "required_checks": ["cargo-test"],
            "input_manifest": {
                "version": 1,
                "repo_paths": ["server/Cargo.lock"],
                "environment_names": ["RUST_LOG"]
            },
            "read_only_external_inputs": [{"id": "rust-toolchain", "locator": "rustup:stable"}],
            "output_only_globs": ["server/target/**"],
            "hermeticity": {"hermetic": true, "reusable": true, "network_access": false}
        })
    }

    #[test]
    fn final_verification_round_trips_separately_from_pre_verification() {
        let raw = json!({
            "schema_version": 1,
            "lifecycle": {
                "pre_verification": ["pnpm install --frozen-lockfile"],
                "final_verification": final_verification_fixture()
            }
        });
        let config: EnvironmentConfig = serde_json::from_value(raw).unwrap();
        assert!(matches!(
            config.lifecycle.pre_verification.as_slice(),
            [HookCommand::Shell(command)] if command == "pnpm install --frozen-lockfile"
        ));
        assert_eq!(
            config.lifecycle.final_verification.commands[0].check_id,
            "cargo-test"
        );
        config.validate().unwrap();

        let json_round_trip: EnvironmentConfig =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        let yaml_round_trip: EnvironmentConfig =
            serde_yaml::from_str(&serde_yaml::to_string(&config).unwrap()).unwrap();
        assert_eq!(json_round_trip, config);
        assert_eq!(yaml_round_trip, config);
    }

    #[test]
    fn pre_verification_hook_list_cannot_deserialize_as_final_verification_plan() {
        let hook_list = json!(["cargo test"]);

        let pre_verification: Vec<HookCommand> = serde_json::from_value(hook_list.clone()).unwrap();
        assert!(matches!(
            pre_verification.as_slice(),
            [HookCommand::Shell(command)] if command == "cargo test"
        ));
        assert!(
            serde_json::from_value::<FinalVerificationPlan>(hook_list).is_err(),
            "a pre-verification hook list must not deserialize as a final-verification plan"
        );
    }

    #[test]
    fn partial_final_verification_declarations_do_not_bypass_validation() {
        let raw = json!({
            "schema_version": 1,
            "lifecycle": {
                "final_verification": {
                    "input_manifest": {"version": 2},
                    "output_only_globs": ["../outside/**"],
                    "read_only_external_inputs": [{"id": "same", "locator": "one"}, {"id": "same", "locator": "two"}],
                    "hermeticity": {"reusable": true}
                }
            }
        });
        let config: EnvironmentConfig = serde_json::from_value(raw).unwrap();
        let error = config.validate().unwrap_err();
        assert!(
            matches!(error, EnvironmentConfigError::EmptyValue { ref field } if field == "lifecycle.final_verification.profile_id"),
            "partial final-verification declarations must not be accepted: {error}"
        );
    }

    #[test]
    fn final_verification_rejects_duplicate_required_check_ids() {
        let mut config = EnvironmentConfig::empty();
        config.lifecycle.final_verification =
            serde_json::from_value(final_verification_fixture()).unwrap();
        config
            .lifecycle
            .final_verification
            .required_checks
            .push("cargo-test".into());
        let error = config.validate().unwrap_err();
        assert!(
            matches!(error, EnvironmentConfigError::DuplicateName { ref field, .. } if field == "lifecycle.final_verification.required_checks"),
            "expected duplicate required-check error, got: {error}"
        );
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

    #[test]
    fn validate_reports_schema_version_zero_before_language_errors() {
        // Top-level ordering: schema_version == 0 is checked before
        // language validation, so even an invalid toolchain is invisible
        // when the schema version is the reseed sentinel.
        let mut cfg = EnvironmentConfig {
            schema_version: 0,
            ..EnvironmentConfig::empty()
        };
        cfg.languages.rust = Some(RustLanguage {
            default_toolchain: "bad;injection".into(),
        });
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::EmptyValue { ref field } if field == "schema_version"),
            "expected EmptyValue for schema_version, got: {err:?}"
        );
    }

    #[test]
    fn validate_reports_unsupported_schema_version_before_language_errors() {
        let mut cfg = EnvironmentConfig {
            schema_version: SCHEMA_VERSION + 1,
            ..EnvironmentConfig::empty()
        };
        cfg.languages.python = Some(PythonLanguage {
            default_version: "bad;injection".into(),
        });
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::UnsupportedSchemaVersion { .. }),
            "expected UnsupportedSchemaVersion, got: {err:?}"
        );
    }

    #[test]
    fn language_validate_rust_before_python_ordering() {
        // Language ordering: rust is validated before python, so when both
        // have invalid values the rust error wins.
        let mut cfg = EnvironmentConfig::empty();
        cfg.languages.rust = Some(RustLanguage {
            default_toolchain: "bad;injection".into(),
        });
        cfg.languages.python = Some(PythonLanguage {
            default_version: "also;bad".into(),
        });
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::UnsafeIdentifier { ref field, .. } if field == "languages.rust.default_toolchain"),
            "expected rust error first, got: {err:?}"
        );
    }

    #[test]
    fn language_validate_node_before_dotnet_ordering() {
        // node (index 1) is validated before dotnet (index 6).
        let mut cfg = EnvironmentConfig::empty();
        cfg.languages.node = Some(NodeLanguage {
            default_version: "bad;node".into(),
            default_package_manager: None,
        });
        cfg.languages.dotnet = Some(DotnetLanguage {
            default_version: "bad;dotnet".into(),
        });
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::UnsafeIdentifier { ref field, .. } if field == "languages.node.default_version"),
            "expected node error first, got: {err:?}"
        );
    }

    #[test]
    fn workspace_first_error_is_root_before_slug() {
        // When a workspace has both an invalid root and an invalid slug,
        // the root error (UnsafeIdentifier) must be returned first because
        // validate_workspace_fields checks root before slug.
        let mut cfg = valid_minimal();
        cfg.workspaces = vec![Workspace {
            slug: Some("bad slug".to_owned()),
            name: None,
            tags: Vec::new(),
            root: "/etc".to_owned(),
            language: "rust".to_owned(),
            toolchain: None,
            version: None,
            package_manager: None,
        }];
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::UnsafeIdentifier { ref field, .. } if field == "workspaces[*].root"),
            "expected root error first, got: {err:?}"
        );
    }

    #[test]
    fn env_var_value_too_long_reports_correct_field() {
        let mut cfg = valid_minimal();
        let long_value = "x".repeat(MAX_STRING_LEN + 1);
        cfg.env.insert("KEY".to_owned(), long_value);
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::TooLong { ref field, .. } if field == "env[KEY]"),
            "expected env[KEY] TooLong, got: {err:?}"
        );
    }

    #[test]
    fn lifecycle_hook_field_path_preserved_for_exec_argv() {
        // A HookCommand::Exec with an empty argv list must report the
        // indexed field path so callers know which phase and hook failed.
        let mut cfg = valid_minimal();
        cfg.lifecycle.post_build = vec![HookCommand::Exec(vec![])];
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::EmptyValue { ref field } if field == "lifecycle.post_build[0]"),
            "expected lifecycle.post_build[0] EmptyValue, got: {err:?}"
        );
    }

    #[test]
    fn lifecycle_phase_order_preserved() {
        // post_build is validated before pre_anything, so an error in
        // post_build wins even when pre_anything is also invalid.
        let mut cfg = valid_minimal();
        cfg.lifecycle.post_build = vec![HookCommand::Exec(vec![])];
        cfg.lifecycle.pre_anything = vec![HookCommand::Shell("x".repeat(MAX_HOOK_SHELL_LEN + 1))];
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::EmptyValue { ref field } if field == "lifecycle.post_build[0]"),
            "expected post_build error first, got: {err:?}"
        );
    }

    #[test]
    fn cargo_cache_policy_warm_command_validation_preserved() {
        // Cargo-cache-policy section ordering: a malformed warm command is
        // caught after languages, workspaces, system_packages, env, and
        // lifecycle all pass.  This test guards the warm-command field-path
        // string in the error variant.
        let mut cfg = EnvironmentConfig::empty();
        cfg.cargo_cache_policy = Some(CargoCachePolicy::Explicit(CargoCachePolicyOverride {
            workspace: true,
            features: Vec::new(),
            all_features: false,
            warm_commands: vec![CargoWarmCommand {
                label: String::new(), // triggers EmptyValue
                args: vec!["build".into()],
            }],
        }));
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::EmptyValue { ref field } if field == "cargo_cache_policy.policy.warm_commands[0].label"),
            "expected warm_commands[0].label EmptyValue, got: {err:?}"
        );
    }

    #[test]
    fn cargo_cache_policy_all_features_incompatibility_preserved() {
        // features + all_features conflict is caught by
        // CargoCachePolicyOverride::validate.
        let mut cfg = EnvironmentConfig::empty();
        cfg.cargo_cache_policy = Some(CargoCachePolicy::Explicit(CargoCachePolicyOverride {
            workspace: false,
            features: vec!["ci".into()],
            all_features: true,
            warm_commands: Vec::new(),
        }));
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::UnsafeIdentifier { ref field, .. } if field == "cargo_cache_policy.policy.features"),
            "expected features conflict, got: {err:?}"
        );
    }

    // ---- PreTaskCommand / PreTaskFailurePolicy tests --------------------

    fn make_pre_task_cmd(command: &str) -> PreTaskCommand {
        PreTaskCommand {
            name: None,
            command: command.into(),
            timeout_seconds: PRE_TASK_TIMEOUT_DEFAULT,
            failure_policy: PreTaskFailurePolicy::default(),
        }
    }

    #[test]
    fn pretask_default_serde_roundtrip() {
        // Minimal JSON: only `command` is required; everything else defaults.
        let raw = r#"{"command": "echo hi"}"#;
        let cmd: PreTaskCommand = serde_json::from_str(raw).unwrap();
        assert_eq!(cmd.command, "echo hi");
        assert!(cmd.name.is_none());
        assert_eq!(cmd.timeout_seconds, 300);
        assert_eq!(cmd.failure_policy, PreTaskFailurePolicy::Blocking);
        // Roundtrip: serialize and re-parse.
        let json = serde_json::to_string(&cmd).unwrap();
        let back: PreTaskCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn pretask_failure_policy_best_effort_parses() {
        let raw = r#"{"command": "echo ok", "failure_policy": "best_effort"}"#;
        let cmd: PreTaskCommand = serde_json::from_str(raw).unwrap();
        assert_eq!(cmd.failure_policy, PreTaskFailurePolicy::BestEffort);
    }

    #[test]
    fn pretask_failure_policy_blocking_parses() {
        let raw = r#"{"command": "echo ok", "failure_policy": "blocking"}"#;
        let cmd: PreTaskCommand = serde_json::from_str(raw).unwrap();
        assert_eq!(cmd.failure_policy, PreTaskFailurePolicy::Blocking);
    }

    #[test]
    fn pretask_failure_policy_invalid_value_rejected() {
        let raw = r#"{"command": "echo ok", "failure_policy": "ignore"}"#;
        let err = serde_json::from_str::<PreTaskCommand>(raw).unwrap_err();
        assert!(
            err.to_string().contains("ignore"),
            "expected parse error mentioning invalid value, got: {err}"
        );
    }

    #[test]
    fn pretask_name_supplied_roundtrip() {
        let raw = r#"{"command": "echo hi", "name": "my-step"}"#;
        let cmd: PreTaskCommand = serde_json::from_str(raw).unwrap();
        assert_eq!(cmd.name.as_deref(), Some("my-step"));
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"name\""), "name should be serialized");
        let back: PreTaskCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name.as_deref(), Some("my-step"));
    }

    #[test]
    fn pretask_name_none_not_serialized() {
        let cmd = make_pre_task_cmd("echo hi");
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(
            !json.contains("\"name\""),
            "name: None should be skipped in serialization, got: {json}"
        );
    }

    #[test]
    fn pretask_resolved_name_auto_generated() {
        let cmd = make_pre_task_cmd("echo hi");
        assert_eq!(cmd.resolved_name(0), "pre_task_1");
        assert_eq!(cmd.resolved_name(4), "pre_task_5");
    }

    #[test]
    fn pretask_resolved_name_uses_supplied() {
        let cmd = PreTaskCommand {
            name: Some("custom".into()),
            command: "echo hi".into(),
            timeout_seconds: 100,
            failure_policy: PreTaskFailurePolicy::BestEffort,
        };
        assert_eq!(cmd.resolved_name(0), "custom");
        assert_eq!(cmd.resolved_name(99), "custom");
    }

    #[test]
    fn pretask_valid_command_passes() {
        let cmd = make_pre_task_cmd("pip install -e .");
        assert!(cmd.validate("test").is_ok());
    }

    #[test]
    fn pretask_empty_command_rejected() {
        let mut cmd = make_pre_task_cmd("echo ok");
        cmd.command = String::new();
        let err = cmd.validate("test").unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::EmptyValue { ref field } if field == "test.command"),
            "expected EmptyValue for command, got: {err:?}"
        );
    }

    #[test]
    fn pretask_command_too_long_rejected() {
        let mut cmd = make_pre_task_cmd("echo ok");
        cmd.command = "x".repeat(MAX_PRE_TASK_COMMAND_LEN + 1);
        let err = cmd.validate("test").unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::TooLong { ref field, max, .. } if field == "test.command" && max == MAX_PRE_TASK_COMMAND_LEN),
            "expected TooLong for command, got: {err:?}"
        );
    }

    #[test]
    fn pretask_command_at_max_len_accepted() {
        let mut cmd = make_pre_task_cmd("echo ok");
        cmd.command = "x".repeat(MAX_PRE_TASK_COMMAND_LEN);
        assert!(cmd.validate("test").is_ok());
    }

    #[test]
    fn pretask_timeout_zero_rejected() {
        let mut cmd = make_pre_task_cmd("echo ok");
        cmd.timeout_seconds = 0;
        let err = cmd.validate("test").unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::OutOfRange { ref field, value, min, max }
                if field == "test.timeout_seconds" && value == 0 && min == 1 && max == 1800),
            "expected OutOfRange for timeout=0, got: {err:?}"
        );
    }

    #[test]
    fn pretask_timeout_too_high_rejected() {
        let mut cmd = make_pre_task_cmd("echo ok");
        cmd.timeout_seconds = PRE_TASK_TIMEOUT_MAX + 1;
        let err = cmd.validate("test").unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::OutOfRange { ref field, value, max, .. }
                if field == "test.timeout_seconds" && value == PRE_TASK_TIMEOUT_MAX + 1 && max == PRE_TASK_TIMEOUT_MAX),
            "expected OutOfRange for timeout too high, got: {err:?}"
        );
    }

    #[test]
    fn pretask_timeout_at_boundary_accepted() {
        let mut cmd = make_pre_task_cmd("echo ok");
        cmd.timeout_seconds = PRE_TASK_TIMEOUT_MIN;
        assert!(cmd.validate("test").is_ok());
        cmd.timeout_seconds = PRE_TASK_TIMEOUT_MAX;
        assert!(cmd.validate("test").is_ok());
    }

    #[test]
    fn pretask_unsafe_name_rejected() {
        let mut cmd = make_pre_task_cmd("echo ok");
        cmd.name = Some("bad name!".into());
        let err = cmd.validate("test").unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::UnsafeIdentifier { ref field, .. } if field == "test.name"),
            "expected UnsafeIdentifier for name, got: {err:?}"
        );
    }

    #[test]
    fn pretask_empty_name_rejected() {
        let mut cmd = make_pre_task_cmd("echo ok");
        cmd.name = Some(String::new());
        let err = cmd.validate("test").unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::EmptyValue { ref field } if field == "test.name"),
            "expected EmptyValue for empty name, got: {err:?}"
        );
    }

    #[test]
    fn pretask_max_commands_accepted() {
        let mut cfg = valid_minimal();
        cfg.lifecycle.pre_task = (0..MAX_PRE_TASK_COMMANDS)
            .map(|i| PreTaskCommand {
                name: Some(format!("step_{i}")),
                command: "echo ok".into(),
                timeout_seconds: 10,
                failure_policy: PreTaskFailurePolicy::BestEffort,
            })
            .collect();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn pretask_too_many_commands_rejected() {
        let mut cfg = valid_minimal();
        cfg.lifecycle.pre_task = (0..MAX_PRE_TASK_COMMANDS + 1)
            .map(|i| make_pre_task_cmd(&format!("echo {i}")))
            .collect();
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::ListTooLong { ref field, len, max }
                if field == "lifecycle.pre_task" && len == MAX_PRE_TASK_COMMANDS + 1 && max == MAX_PRE_TASK_COMMANDS),
            "expected ListTooLong for pre_task, got: {err:?}"
        );
    }

    #[test]
    fn pretask_duplicate_supplied_names_rejected() {
        let mut cfg = valid_minimal();
        cfg.lifecycle.pre_task = vec![
            PreTaskCommand {
                name: Some("same".into()),
                command: "echo a".into(),
                ..make_pre_task_cmd("echo a")
            },
            PreTaskCommand {
                name: Some("same".into()),
                command: "echo b".into(),
                ..make_pre_task_cmd("echo b")
            },
        ];
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::DuplicateName { ref field, ref name }
                if field == "lifecycle.pre_task" && name == "same"),
            "expected DuplicateName for \"same\", got: {err:?}"
        );
    }

    #[test]
    fn pretask_auto_generated_name_conflict_rejected() {
        // First command has no name → auto-generated as "pre_task_1".
        // Second command explicitly named "pre_task_1" → conflict.
        let mut cfg = valid_minimal();
        cfg.lifecycle.pre_task = vec![
            make_pre_task_cmd("echo a"),
            PreTaskCommand {
                name: Some("pre_task_1".into()),
                command: "echo b".into(),
                ..make_pre_task_cmd("echo b")
            },
        ];
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::DuplicateName { ref field, ref name }
                if field == "lifecycle.pre_task" && name == "pre_task_1"),
            "expected DuplicateName for auto-generated conflict, got: {err:?}"
        );
    }

    #[test]
    fn pretask_empty_list_still_valid() {
        let mut cfg = valid_minimal();
        cfg.lifecycle.pre_task = vec![];
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn pretask_absent_lifecycle_defaults_to_empty() {
        // `{ "schema_version": 1 }` with no `lifecycle` key.
        let raw = r#"{"schema_version": 1}"#;
        let cfg: EnvironmentConfig = serde_json::from_str(raw).unwrap();
        assert!(cfg.lifecycle.pre_task.is_empty());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn pretask_absent_pre_task_defaults_to_empty() {
        // `{ "lifecycle": {} }` — no `pre_task` key.
        let raw = r#"{"schema_version": 1, "lifecycle": {}}"#;
        let cfg: EnvironmentConfig = serde_json::from_str(raw).unwrap();
        assert!(cfg.lifecycle.pre_task.is_empty());
    }

    #[test]
    fn pretask_empty_json_object_defaults_to_empty() {
        // The Dolt column default: `'{}'`
        let cfg: EnvironmentConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.lifecycle.pre_task.is_empty());
    }

    #[test]
    fn pretask_empty_array_valid() {
        let raw = r#"{"schema_version": 1, "lifecycle": {"pre_task": []}}"#;
        let cfg: EnvironmentConfig = serde_json::from_str(raw).unwrap();
        assert!(cfg.lifecycle.pre_task.is_empty());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn pretask_full_command_roundtrip() {
        let raw = r#"{
            "schema_version": 1,
            "lifecycle": {
                "pre_task": [{
                    "name": "setup-db",
                    "command": "pg_isready || pg_ctlcluster 16 main start",
                    "timeout_seconds": 60,
                    "failure_policy": "best_effort"
                }]
            }
        }"#;
        let cfg: EnvironmentConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(cfg.lifecycle.pre_task.len(), 1);
        let cmd = &cfg.lifecycle.pre_task[0];
        assert_eq!(cmd.name.as_deref(), Some("setup-db"));
        assert_eq!(cmd.command, "pg_isready || pg_ctlcluster 16 main start");
        assert_eq!(cmd.timeout_seconds, 60);
        assert_eq!(cmd.failure_policy, PreTaskFailurePolicy::BestEffort);
        assert!(cfg.validate().is_ok());
        // Full serde roundtrip.
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let back: EnvironmentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn pretask_json_schema_contains_definition_names() {
        let schema = schemars::schema_for!(EnvironmentConfig);
        let schema_str = serde_json::to_string(&schema).unwrap();
        assert!(
            schema_str.contains("PreTaskCommand"),
            "schema should contain PreTaskCommand definition"
        );
        assert!(
            schema_str.contains("PreTaskFailurePolicy"),
            "schema should contain PreTaskFailurePolicy definition"
        );
    }

    // ---- final_verification / pre_verification distinction tests -----------

    /// Build a valid minimal `FinalVerificationPlan` for round-trip tests.
    fn valid_final_verification_plan() -> FinalVerificationPlan {
        FinalVerificationPlan {
            version: 1,
            profile_id: "ci-default".into(),
            profile_revision: 1,
            commands: vec![FinalVerificationCommand {
                check_id: "cargo-test".into(),
                executable: "cargo".into(),
                argv: vec!["test".into(), "--workspace".into()],
                working_directory: "server".into(),
                environment_names: vec!["CI".into()],
                timeout_seconds: 300,
                descriptor_revision: 1,
            }],
            command_groups: vec![],
            selection_rules: vec![],
            required_checks: vec!["cargo-test".into()],
            input_manifest: VerificationInputManifest {
                version: 1,
                repo_paths: vec!["server".into()],
                environment_names: vec!["CI".into()],
            },
            read_only_external_inputs: vec![ExternalInputDeclaration {
                id: "base-image".into(),
                locator: "docker://ghcr.io/djinn/base:latest".into(),
            }],
            output_only_globs: vec!["target/**/*.d".into()],
            hermeticity: HermeticityDeclaration {
                hermetic: true,
                reusable: false,
                network_access: false,
            },
        }
    }

    fn valid_grouped_final_verification_plan() -> FinalVerificationPlan {
        let mut plan = valid_final_verification_plan();
        let cargo = plan.commands.remove(0);
        plan.command_groups = vec![
            FinalVerificationCommandGroup {
                name: "rust".into(),
                commands: vec![cargo],
            },
            FinalVerificationCommandGroup {
                name: "web".into(),
                commands: vec![FinalVerificationCommand {
                    check_id: "web-test".into(),
                    executable: "pnpm".into(),
                    argv: vec!["test".into()],
                    working_directory: "ui".into(),
                    environment_names: vec![],
                    timeout_seconds: 300,
                    descriptor_revision: 1,
                }],
            },
        ];
        plan.selection_rules = vec![
            FinalVerificationSelectionRule {
                match_globs: vec!["server/**".into()],
                command_groups: vec!["rust".into()],
            },
            FinalVerificationSelectionRule {
                match_globs: vec!["**".into()],
                command_groups: vec!["rust".into(), "web".into()],
            },
        ];
        plan
    }

    #[test]
    fn final_verification_grouped_plan_round_trips_and_normalizes_in_order() {
        let plan = valid_grouped_final_verification_plan();
        plan.validate().unwrap();
        let serialized = serde_json::to_string(&plan).unwrap();
        assert!(serialized.contains("command_groups"));
        assert!(serialized.contains("selection_rules"));
        let value: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert!(value.get("commands").is_none());
        let back: FinalVerificationPlan = serde_json::from_str(&serialized).unwrap();
        assert_eq!(back, plan);
        assert_eq!(
            back.normalized_commands()
                .iter()
                .map(|command| command.check_id.as_str())
                .collect::<Vec<_>>(),
            ["cargo-test", "web-test"]
        );
    }

    #[test]
    fn final_verification_legacy_plan_normalizes_without_empty_commands() {
        let plan = valid_final_verification_plan();
        plan.validate().unwrap();
        assert_eq!(plan.normalized_commands(), plan.commands);
        assert!(!plan.normalized_commands().is_empty());
    }

    #[test]
    fn final_verification_grouped_validation_rejects_invalid_shapes() {
        let mut plan = valid_grouped_final_verification_plan();
        plan.selection_rules[0].match_globs.clear();
        let error = plan.validate().unwrap_err();
        assert!(
            matches!(error, EnvironmentConfigError::EmptyValue { ref field }
            if field == "lifecycle.final_verification.selection_rules[0].match")
        );

        let mut plan = valid_grouped_final_verification_plan();
        plan.selection_rules[0].command_groups = vec!["missing".into()];
        let error = plan.validate().unwrap_err();
        assert!(
            matches!(error, EnvironmentConfigError::UnsafeIdentifier { ref field, ref value }
            if field == "lifecycle.final_verification.selection_rules[0].command_groups" && value == "missing")
        );

        let mut plan = valid_grouped_final_verification_plan();
        plan.command_groups[1].commands[0].check_id = "cargo-test".into();
        let error = plan.validate().unwrap_err();
        assert!(
            matches!(error, EnvironmentConfigError::DuplicateName { ref field, ref name }
            if field == "lifecycle.final_verification.command_groups[1].commands" && name == "cargo-test")
        );

        let mut plan = valid_grouped_final_verification_plan();
        plan.selection_rules.pop();
        let error = plan.validate().unwrap_err();
        assert!(
            matches!(error, EnvironmentConfigError::EmptyValue { ref field }
            if field == "lifecycle.final_verification.selection_rules")
        );

        let mut plan = valid_grouped_final_verification_plan();
        plan.selection_rules[1].match_globs = vec!["ui/**".into()];
        let error = plan.validate().unwrap_err();
        assert!(
            matches!(error, EnvironmentConfigError::EmptyValue { ref field }
            if field == "lifecycle.final_verification.selection_rules")
        );

        let mut plan = valid_grouped_final_verification_plan();
        plan.command_groups.push(plan.command_groups[0].clone());
        let error = plan.validate().unwrap_err();
        assert!(
            matches!(error, EnvironmentConfigError::DuplicateName { ref field, ref name }
            if field == "lifecycle.final_verification.command_groups" && name == "rust")
        );

        let mut plan = valid_grouped_final_verification_plan();
        plan.selection_rules[0].command_groups.push("rust".into());
        let error = plan.validate().unwrap_err();
        assert!(
            matches!(error, EnvironmentConfigError::DuplicateName { ref field, ref name }
            if field == "lifecycle.final_verification.selection_rules[0].command_groups" && name == "rust")
        );

        let mut plan = valid_grouped_final_verification_plan();
        plan.commands
            .push(plan.command_groups[0].commands[0].clone());
        let error = plan.validate().unwrap_err();
        assert!(
            matches!(error, EnvironmentConfigError::UnsafeIdentifier { ref field, .. }
            if field == "lifecycle.final_verification.commands")
        );

        let mut plan = valid_grouped_final_verification_plan();
        plan.required_checks.push("web-test".into());
        let error = plan.validate().unwrap_err();
        assert!(
            matches!(error, EnvironmentConfigError::UnsafeIdentifier { ref field, ref value }
            if field == "lifecycle.final_verification.selection_rules[0].command_groups" && value == "web-test")
        );
    }

    #[test]
    fn final_verification_valid_plan_round_trips_through_json() {
        let plan = valid_final_verification_plan();
        let serialized = serde_json::to_string(&plan).unwrap();
        let back: FinalVerificationPlan = serde_json::from_str(&serialized).unwrap();
        assert_eq!(back, plan);
    }

    #[test]
    fn final_verification_validates_through_full_config() {
        let mut cfg = EnvironmentConfig::empty();
        cfg.lifecycle.final_verification = valid_final_verification_plan();
        cfg.validate().unwrap();
    }

    #[test]
    fn final_verification_defaults_to_empty_plan() {
        let plan = FinalVerificationPlan::default();
        // Manual Default sets version=1 (matches serde default function).
        assert_eq!(plan.version, 1);
        assert!(plan.commands.is_empty());
        assert!(plan.profile_id.is_empty());
        // An empty plan validates — it means "no final verification configured."
        assert!(plan.validate().is_ok());
    }

    #[test]
    fn final_verification_absent_key_defaults_to_empty() {
        let raw = r#"{"schema_version": 1, "lifecycle": {}}"#;
        let cfg: EnvironmentConfig = serde_json::from_str(raw).unwrap();
        // serde default of final_verification_version() is 1, but when
        // deserializing with #[serde(default = "...")] the version picks
        // up the function default (1) when the key is absent.
        assert!(cfg.lifecycle.final_verification.commands.is_empty());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn final_verification_unsupported_version_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.version = 2;
        let err = plan.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::OutOfRange { ref field, .. }
                if field == "lifecycle.final_verification.version"),
            "expected OutOfRange for version, got: {err:?}"
        );
    }

    #[test]
    fn final_verification_manifest_unsupported_version_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.input_manifest.version = 2;
        let err = plan.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::OutOfRange { ref field, .. }
                if field == "lifecycle.final_verification.input_manifest.version"),
            "expected OutOfRange for manifest version, got: {err:?}"
        );
    }

    #[test]
    fn final_verification_duplicate_check_ids_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.commands.push(plan.commands[0].clone());
        let err = plan.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::DuplicateName { ref field, ref name }
                if field == "lifecycle.final_verification.commands" && name == "cargo-test"),
            "expected DuplicateName for check_id, got: {err:?}"
        );
    }

    #[test]
    fn final_verification_empty_check_id_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.commands[0].check_id.clear();
        let err = plan.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::EmptyValue { ref field }
                if field == "lifecycle.final_verification.commands.check_id"),
            "expected EmptyValue for check_id, got: {err:?}"
        );
    }

    #[test]
    fn final_verification_required_check_not_in_commands_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.required_checks.push("nonexistent-check".into());
        let err = plan.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::UnsafeIdentifier { ref field, ref value }
                if field == "lifecycle.final_verification.required_checks"
                    && value == "nonexistent-check"),
            "expected required_checks coverage error, got: {err:?}"
        );
    }

    #[test]
    fn final_verification_timeout_too_high_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.commands[0].timeout_seconds = FINAL_VERIFICATION_TIMEOUT_MAX + 1;
        let err = plan.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::OutOfRange { ref field, value, max, .. }
                if field == "lifecycle.final_verification.commands.timeout_seconds"
                    && value == FINAL_VERIFICATION_TIMEOUT_MAX + 1
                    && max == FINAL_VERIFICATION_TIMEOUT_MAX),
            "expected OutOfRange for timeout too high, got: {err:?}"
        );
    }

    #[test]
    fn final_verification_descriptor_revision_zero_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.commands[0].descriptor_revision = 0;
        let err = plan.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::EmptyValue { ref field }
                if field == "lifecycle.final_verification.commands.descriptor_revision"),
            "expected EmptyValue for descriptor_revision=0, got: {err:?}"
        );
    }

    #[test]
    fn final_verification_too_many_commands_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.commands = (0..MAX_FINAL_VERIFICATION_COMMANDS + 1)
            .map(|i| FinalVerificationCommand {
                check_id: format!("check-{i}"),
                executable: "cargo".into(),
                argv: vec!["test".into()],
                working_directory: String::new(),
                environment_names: vec![],
                timeout_seconds: 60,
                descriptor_revision: 1,
            })
            .collect();
        let err = plan.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::ListTooLong { ref field, len, max }
                if field == "lifecycle.final_verification.commands"
                    && len == MAX_FINAL_VERIFICATION_COMMANDS + 1
                    && max == MAX_FINAL_VERIFICATION_COMMANDS),
            "expected ListTooLong for commands, got: {err:?}"
        );
    }

    #[test]
    fn final_verification_unsafe_working_directory_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.commands[0].working_directory = "/etc/passwd".into();
        let err = plan.validate().unwrap_err();
        assert!(matches!(
            err,
            EnvironmentConfigError::UnsafeIdentifier { .. }
        ));
    }

    #[test]
    fn final_verification_dotdot_working_directory_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.commands[0].working_directory = "../outside".into();
        let err = plan.validate().unwrap_err();
        assert!(matches!(
            err,
            EnvironmentConfigError::UnsafeIdentifier { .. }
        ));
    }

    #[test]
    fn final_verification_empty_executable_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.commands[0].executable.clear();
        let err = plan.validate().unwrap_err();
        assert!(matches!(err, EnvironmentConfigError::EmptyValue { .. }));
    }

    #[test]
    fn final_verification_duplicate_external_input_ids_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.read_only_external_inputs
            .push(ExternalInputDeclaration {
                id: "base-image".into(), // duplicate
                locator: "docker://other".into(),
            });
        let err = plan.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::DuplicateName { ref field, ref name }
                if field == "lifecycle.final_verification.read_only_external_inputs"
                    && name == "base-image"),
            "expected DuplicateName for external input id, got: {err:?}"
        );
    }

    #[test]
    fn final_verification_empty_external_input_id_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.read_only_external_inputs[0].id.clear();
        let err = plan.validate().unwrap_err();
        assert!(matches!(err, EnvironmentConfigError::EmptyValue { .. }));
    }

    #[test]
    fn final_verification_too_many_external_inputs_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.read_only_external_inputs = (0..MAX_FINAL_VERIFICATION_INPUTS + 1)
            .map(|i| ExternalInputDeclaration {
                id: format!("input-{i}"),
                locator: "loc".into(),
            })
            .collect();
        let err = plan.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::ListTooLong { ref field, .. }
                if field == "lifecycle.final_verification.read_only_external_inputs"),
            "expected ListTooLong for external inputs, got: {err:?}"
        );
    }

    #[test]
    fn final_verification_unsafe_glob_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.output_only_globs = vec!["../escape/*".into()];
        let err = plan.validate().unwrap_err();
        assert!(matches!(
            err,
            EnvironmentConfigError::UnsafeIdentifier { .. }
        ));
    }

    #[test]
    fn final_verification_absolute_glob_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.output_only_globs = vec!["/etc/*".into()];
        let err = plan.validate().unwrap_err();
        assert!(matches!(
            err,
            EnvironmentConfigError::UnsafeIdentifier { .. }
        ));
    }

    #[test]
    fn final_verification_empty_glob_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.output_only_globs = vec![String::new()];
        let err = plan.validate().unwrap_err();
        assert!(matches!(err, EnvironmentConfigError::EmptyValue { .. }));
    }

    #[test]
    fn final_verification_too_many_output_globs_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.output_only_globs = (0..MAX_FINAL_VERIFICATION_OUTPUTS + 1)
            .map(|i| format!("out-{i}/*"))
            .collect();
        let err = plan.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::ListTooLong { ref field, .. }
                if field == "lifecycle.final_verification.output_only_globs"),
            "expected ListTooLong for output globs, got: {err:?}"
        );
    }

    #[test]
    fn final_verification_valid_glob_accepted() {
        let mut plan = valid_final_verification_plan();
        plan.output_only_globs = vec!["target/**/*.d".into(), "dist/*.js".into()];
        assert!(plan.validate().is_ok());
    }

    #[test]
    fn final_verification_non_hermetic_reusable_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.hermeticity = HermeticityDeclaration {
            hermetic: false,
            reusable: true,
            network_access: false,
        };
        let err = plan.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::UnsafeIdentifier { ref field, .. }
                if field == "lifecycle.final_verification.hermeticity.reusable"),
            "expected reusable-without-hermetic error, got: {err:?}"
        );
    }

    #[test]
    fn final_verification_network_reusable_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.hermeticity = HermeticityDeclaration {
            hermetic: true,
            reusable: true,
            network_access: true,
        };
        let err = plan.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::UnsafeIdentifier { ref field, .. }
                if field == "lifecycle.final_verification.hermeticity.reusable"),
            "expected reusable-with-network error, got: {err:?}"
        );
    }

    #[test]
    fn final_verification_profile_revision_zero_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.profile_revision = 0;
        let err = plan.validate().unwrap_err();
        assert!(
            matches!(err, EnvironmentConfigError::EmptyValue { ref field }
                if field == "lifecycle.final_verification.profile_revision"),
            "expected EmptyValue for profile_revision, got: {err:?}"
        );
    }

    #[test]
    fn final_verification_empty_plan_validates() {
        // A FinalVerificationPlan with serde defaults (version=1 from the
        // default function, empty commands, empty profile_id) is valid —
        // it means "no final verification configured."
        let raw = r#"{"version": 1}"#;
        let plan: FinalVerificationPlan = serde_json::from_str(raw).unwrap();
        assert!(plan.validate().is_ok());
    }

    #[test]
    fn final_verification_unsafe_environment_name_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.commands[0].environment_names = vec!["bad name!".into()];
        let err = plan.validate().unwrap_err();
        assert!(matches!(
            err,
            EnvironmentConfigError::UnsafeIdentifier { .. }
        ));
    }

    #[test]
    fn final_verification_unsafe_manifest_path_rejected() {
        let mut plan = valid_final_verification_plan();
        plan.input_manifest.repo_paths = vec!["../escape".into()];
        let err = plan.validate().unwrap_err();
        assert!(matches!(
            err,
            EnvironmentConfigError::UnsafeIdentifier { .. }
        ));
    }

    // ---- pre_verification vs final_verification distinction tests -----------

    #[test]
    fn pre_verification_is_vec_of_hook_commands() {
        // pre_verification must remain the unchanged setup-only Vec<HookCommand>.
        let mut cfg = EnvironmentConfig::empty();
        cfg.lifecycle.pre_verification = vec![
            HookCommand::Shell("echo setup".into()),
            HookCommand::Exec(vec!["bash".into(), "-lc".into(), "make build".into()]),
        ];
        // It validates as a standard lifecycle phase, not as a
        // FinalVerificationPlan.
        assert!(cfg.validate().is_ok());
        // Confirm the type is Vec<HookCommand>, not FinalVerificationPlan.
        let _hooks: &Vec<HookCommand> = &cfg.lifecycle.pre_verification;
    }

    #[test]
    fn pre_verification_cannot_be_deserialized_as_final_verification() {
        // A JSON shape that's valid for pre_verification (array of shell
        // strings) cannot be deserialized into final_verification (which
        // expects an object with specific fields).
        let raw = r#"{
            "schema_version": 1,
            "lifecycle": {
                "pre_verification": ["echo setup", "make build"],
                "final_verification": "echo not-a-plan"
            }
        }"#;
        // Deserialization must fail — a string is not a valid
        // FinalVerificationPlan object.
        assert!(
            serde_json::from_str::<EnvironmentConfig>(raw).is_err(),
            "string shape must not deserialize into final_verification plan object"
        );
    }

    #[test]
    fn pre_verification_shell_hook_validates_but_final_verification_requires_plan() {
        // pre_verification accepts arbitrary shell strings as HookCommand::Shell.
        let raw_pre = r#"{
            "schema_version": 1,
            "lifecycle": {
                "pre_verification": ["echo hi"]
            }
        }"#;
        let cfg: EnvironmentConfig = serde_json::from_str(raw_pre).unwrap();
        assert_eq!(cfg.lifecycle.pre_verification.len(), 1);
        assert!(matches!(
            cfg.lifecycle.pre_verification[0],
            HookCommand::Shell(_)
        ));
        assert!(cfg.validate().is_ok());

        // final_verification with the same shell-string shape fails to
        // deserialize — it must be a structured plan object, not an array
        // of hook commands. The deserialization error proves that
        // final_verification and pre_verification have distinct types.
        let raw_fv = r#"{
            "schema_version": 1,
            "lifecycle": {
                "final_verification": ["echo hi"]
            }
        }"#;
        assert!(
            serde_json::from_str::<EnvironmentConfig>(raw_fv).is_err(),
            "array shape must not deserialize into final_verification plan object"
        );
    }

    #[test]
    fn final_verification_json_schema_exposes_all_types() {
        let schema = schemars::schema_for!(EnvironmentConfig);
        let schema_str = serde_json::to_string(&schema).unwrap();
        assert!(
            schema_str.contains("FinalVerificationPlan"),
            "schema should contain FinalVerificationPlan definition"
        );
        assert!(
            schema_str.contains("FinalVerificationCommand"),
            "schema should contain FinalVerificationCommand definition"
        );
        assert!(
            schema_str.contains("FinalVerificationCommandGroup"),
            "schema should contain FinalVerificationCommandGroup definition"
        );
        assert!(
            schema_str.contains("FinalVerificationSelectionRule"),
            "schema should contain FinalVerificationSelectionRule definition"
        );
        assert!(
            schema_str.contains("HermeticityDeclaration"),
            "schema should contain HermeticityDeclaration definition"
        );
        assert!(
            schema_str.contains("ExternalInputDeclaration"),
            "schema should contain ExternalInputDeclaration definition"
        );
        assert!(
            schema_str.contains("VerificationInputManifest"),
            "schema should contain VerificationInputManifest definition"
        );
    }

    #[test]
    fn final_verification_round_trips_through_full_environment_config() {
        let mut cfg = EnvironmentConfig::empty();
        cfg.lifecycle.final_verification = valid_final_verification_plan();
        let serialized = serde_json::to_string_pretty(&cfg).unwrap();
        let back: EnvironmentConfig = serde_json::from_str(&serialized).unwrap();
        assert_eq!(back, cfg);
        assert_eq!(back.lifecycle.final_verification.profile_id, "ci-default");
        assert_eq!(
            back.lifecycle.final_verification.commands[0].check_id,
            "cargo-test"
        );
    }
}
