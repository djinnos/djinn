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
    DotnetLanguage, EnvironmentConfig, EnvironmentConfigError, ExternalInputDeclaration,
    FinalVerificationCommand, FinalVerificationCommandGroup, FinalVerificationPlan,
    FinalVerificationSelectionRule, GoLanguage, HermeticityDeclaration, HookCommand, JavaLanguage,
    Languages, LifecycleHooks, NodeLanguage, PreTaskCommand, PreTaskFailurePolicy, PythonLanguage,
    RubyLanguage, RustLanguage, SCHEMA_VERSION, VerificationInputManifest, Workspace,
};
pub use schema::{LanguageStat, ManifestSignals, Runtimes, Stack};
pub use slug::workspace_slug;
