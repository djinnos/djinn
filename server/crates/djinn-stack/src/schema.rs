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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stack {
    /// Wall-clock timestamp of the detection pass.
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
    pub runtimes: Runtimes,

    /// Boolean presence flags for the manifests / top-level files the
    /// downstream consumers care about. Drives the UI devcontainer
    /// banner.
    pub manifest_signals: ManifestSignals,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LanguageStat {
    pub name: String,
    pub bytes: u64,
    pub pct: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ManifestSignals {
    pub has_package_json: bool,
    pub has_cargo_toml: bool,
    pub has_pyproject_toml: bool,
    pub has_go_mod: bool,
    pub has_pnpm_workspace: bool,
    pub has_turbo_json: bool,
    pub has_devcontainer: bool,
    pub has_devcontainer_lock: bool,
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
        }
    }
}
