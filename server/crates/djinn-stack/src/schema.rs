//! `Stack` JSON schema mirroring §4.2 of the Phase 3 plan.
//!
//! This struct is what gets persisted to `projects.stack` and returned
//! from the `get_project_stack` MCP tool. Field order / naming is
//! load-bearing — downstream consumers (UI banner, role-prompt injection)
//! parse this shape.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Top-level stack descriptor — one per project, recomputed on every
/// mirror fetch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Stack {
    /// Wall-clock timestamp of the detection pass.
    #[schemars(with = "String")]
    pub detected_at: DateTime<Utc>,

    /// Languages detected by extension, sorted descending by byte share.
    pub languages: Vec<LanguageStat>,

    /// The single language with the largest byte share (first entry of
    /// `languages` by construction), or `None` for an empty repo.
    pub primary_language: Option<String>,

    /// Canonical slugs: `npm`, `pnpm`, `yarn`, `bun`, `cargo`, `uv`,
    /// `poetry`, `pdm`, `pip`, `go-mod`, `gradle`, `maven`, `bundler`.
    pub package_managers: Vec<String>,

    /// Slugs: `pnpm-workspaces`, `yarn-workspaces`, `npm-workspaces`,
    /// `turbo`, `nx`, `lerna`, `cargo-workspace`, `go-workspace`.
    pub monorepo_tools: Vec<String>,

    /// Derived: `monorepo_tools` is non-empty.
    pub is_monorepo: bool,

    /// Slugs: `vitest`, `jest`, `mocha`, `playwright`, `nextest`,
    /// `pytest`, `go-test`, `junit`, `rspec`.
    pub test_runners: Vec<String>,

    /// Slugs: `react`, `next`, `vue`, `svelte`, `axum`, `actix`,
    /// `rocket`, `fastapi`, `flask`, `django`, `rails`, `spring`.
    pub frameworks: Vec<String>,

    /// Declared runtime versions (e.g. `{"node": "22"}`). `None` when
    /// the manifest exists but the field is absent.
    ///
    /// This is "*a* version" for each language — convenient for the UI
    /// banner and Phase-3 image hashing. Multi-toolchain callers
    /// (post-env-config P5) read [`Stack::workspaces`] instead, since a
    /// single string can't express "two Rust crates pinned to different
    /// toolchains".
    pub runtimes: Runtimes,

    /// Boolean presence flags for the manifests / top-level files the
    /// downstream consumers care about. Drives the UI devcontainer
    /// banner.
    pub manifest_signals: ManifestSignals,

    /// Per-workspace toolchain detail — one entry per manifest dir that
    /// the detector considered a distinct workspace root. Populated by
    /// [`detect`]; empty when no manifests are present.
    ///
    /// Used by [`crate::environment::EnvironmentConfig::from_stack`] to
    /// seed `environment_config.workspaces` during the P5 boot reseed
    /// hook.
    #[serde(default)]
    pub workspaces: Vec<StackWorkspace>,
}

/// One workspace the detector spotted in a mirror. A "workspace" here
/// means "a directory that owns its own package manifest and deserves
/// its own toolchain scope" — not every member crate in a Cargo
/// workspace, but the Cargo-workspace root itself (or a lone Cargo
/// crate without a workspace table); plus every `package.json`,
/// `pyproject.toml`, `go.mod`, `Gemfile`, `pom.xml`, and root-level
/// build.gradle(.kts) found at any depth.
///
/// The detector de-duplicates by "shallowest ancestor per language":
/// if `repo/server/Cargo.toml` is already a workspace, a member
/// `repo/server/crates/foo/Cargo.toml` does *not* also get emitted.
/// That keeps the toolchain list bounded for typical monorepos while
/// still emitting two Rust entries when two unrelated Cargo workspaces
/// sit next to each other (the motivating case for the env-config
/// refactor).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StackWorkspace {
    /// Repo-relative path to the workspace directory (forward-slash,
    /// never absolute, no `..`). The empty string represents the repo
    /// root.
    pub root: String,
    /// Canonical language slug: `rust` | `node` | `python` | `go` |
    /// `java` | `ruby` (future: `dotnet` | `clang`).
    pub language: String,
    /// Toolchain / version the manifest pins, in its raw form.
    /// * rust: channel from `rust-toolchain.toml` or
    ///   `package.rust-version` (e.g. `"stable"`, `"1.85.0"`,
    ///   `"nightly-2026-04-01"`).
    /// * node: major from `engines.node` (e.g. `"22"`).
    /// * python: major.minor from `requires-python` (e.g. `"3.12"`).
    /// * go: `go` directive (e.g. `"1.22"`).
    /// * java/ruby: currently unused — left `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<String>,
    /// Package-manager slug pinned by the manifest, when it carries one.
    /// Currently populated for `node` (`packageManager` or lockfile
    /// fallback) and `python` (`uv` / `poetry` / `pdm` / `pip`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_manager: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LanguageStat {
    pub name: String,
    #[schemars(with = "i64")]
    pub bytes: u64,
    pub pct: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Runtimes {
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub rust: Option<String>,
    #[serde(default)]
    pub python: Option<String>,
    #[serde(default)]
    pub go: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ManifestSignals {
    pub has_package_json: bool,
    pub has_cargo_toml: bool,
    pub has_pyproject_toml: bool,
    pub has_go_mod: bool,
    pub has_pnpm_workspace: bool,
    pub has_turbo_json: bool,
}

impl Stack {
    /// Empty stack used as the DB default before the first detection
    /// pass completes.
    pub fn empty() -> Self {
        Self {
            detected_at: Utc::now(),
            languages: Vec::new(),
            primary_language: None,
            package_managers: Vec::new(),
            monorepo_tools: Vec::new(),
            is_monorepo: false,
            test_runners: Vec::new(),
            frameworks: Vec::new(),
            runtimes: Runtimes::default(),
            manifest_signals: ManifestSignals::default(),
            workspaces: Vec::new(),
        }
    }
}
