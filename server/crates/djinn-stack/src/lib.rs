//! `djinn-stack` — Phase 3 PR 1.
//!
//! Given the path to a project mirror (bare git repo or plain working
//! tree), produce a [`Stack`] describing the languages, package
//! managers, frameworks, test runners, and devcontainer-presence
//! signals present in the repo. The resulting JSON is what gets
//! persisted to `projects.stack` and what the UI devcontainer banner +
//! future role-prompt injection consume.
//!
//! Downstream wiring (mirror-fetcher hook, MCP tool, DB column) lands
//! in PR 2; this crate is standalone and pure-function.
//!
//! # The language-knowledge seam
//!
//! `djinn-stack` is the platform's single declared language-knowledge crate.
//! Language, package-manager, framework, test-runner, and manifest knowledge
//! belongs here — in [`languages`], [`frameworks`], [`test_runners`], and
//! [`manifests`] — and anything that varies per project belongs in that
//! project's [`environment::EnvironmentConfig`].
//!
//! Generic platform crates — `djinn-agent`, `djinn-mcp-extension`,
//! `djinn-coordinator`, `djinn-agent-worker`, `djinn-telemetry`, `djinn-k8s` —
//! MUST NOT acquire their own language branches. When one of them needs to
//! behave differently for a language, toolchain, or build tool, the
//! discriminator is declared here or resolved from per-project configuration,
//! and the generic crate consumes the already-resolved value.
//!
//! This is a review contract, not a lint, and deliberately so. A keyword or
//! confinement check over the generic crates was prototyped and measured
//! against ~1000 merged PRs: it fired on 42 of them and found a single true
//! positive that ordinary design review had already removed, while `main`
//! legitimately carries approved toolchain branching inside those same crates.
//! Enforcement therefore lives in review and in this seam, not in a guard with
//! a standing exclusion list. See ri23 AC6 for the measurement and the record.

pub mod detect;
pub mod environment;
pub mod frameworks;
pub mod heuristics;
pub mod languages;
pub mod manifests;
pub mod resources;
pub mod schema;
pub mod slug;
pub mod test_runners;

pub use detect::{detect, detect_blocking};
pub use environment::{
    CargoCachePolicy, CargoCachePolicyOverride, CargoWarmCommand, ClangLanguage, ConfigSource,
    DotnetLanguage, EnvironmentConfig, EnvironmentConfigError, GoLanguage, HookCommand, JavaLanguage,
    Languages, LifecycleHooks, NodeLanguage, PreTaskCommand, PreTaskFailurePolicy, PythonLanguage,
    RubyLanguage, RustLanguage, SCHEMA_VERSION, Workspace,
};
pub use schema::{LanguageStat, ManifestSignals, Runtimes, Stack};
pub use slug::workspace_slug;
