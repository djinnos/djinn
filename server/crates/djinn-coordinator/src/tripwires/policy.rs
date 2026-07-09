//! Tripwire policy schema.
//!
//! Defines [`TripwirePolicy`] — the pure-data, additive configuration schema
//! consumed by the deterministic rule engine and the enforcement epic.
//!
//! The schema covers all seven proposal rule families:
//!
//! 1. [`MigrationRuleConfig`] — `migration_change`
//! 2. [`DependencyIdentityRuleConfig`] — `dependency_identity_change`
//! 3. [`NetworkEgressRuleConfig`] — `network_egress_change`
//! 4. [`UnsafeCodeRuleConfig`] — `unsafe_code_change`
//! 5. [`BoundaryPathRuleConfig`] (see also [`TripwirePolicy::allowlist`] /
//!    [`BoundaryAllowlistRef`]) — `boundary_path_change`
//! 6. [`LargeDeleteRewriteRuleConfig`] — `large_delete_or_rewrite`
//! 7. [`CIWorkflowRuleConfig`] — `ci_workflow_change`
//!
//! Per-rule controls are intentionally uniform: every rule exposes `enabled`
//! and `report_only` (enforcement gate vs. advisory log) plus rule-specific
//! tuning. [`TripwirePolicy::default`] returns a safe, enforcement-on posture
//! for every rule — the failure mode of a missing policy is conservative
//! (block risky changes) rather than permissive.
//!
//! Policy revision tracking is two-pronged:
//! - [`TripwirePolicy::policy_revision`] identifies the org policy revision
//!   evaluated against the diff. Defaults to `"org-policy:default"`.
//! - [`TripwirePolicy::allowlist`] carries the boundary allowlist revision
//!   sourced from `scripts/capability-boundary-allowlist.toml`. If the
//!   allowlist is unavailable the boundary rule enters degraded mode (see
//!   [`TripwirePolicy::boundary_path`]) and downstream callers must emit a
//!   warning event.
//!
//! The schema is intentionally deserializable so future enforcement layers
//! (`org_policy_get/set`) can persist overrides without changing the shape
//! consumed by the engine.

// Contract types are intentionally additive — sibling tasks (`gl0p`, `imb4`,
// `na0w`, `nptj`, `r3qg`, `zuir`, `cdb6`) consume them. Suppress dead_code
// warnings at the module level so consumers can land incrementally without
// this module churning.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

// ─── Constants ──────────────────────────────────────────────────────────────

/// Default revision label used when no org policy has been published.
///
/// The revision value is opaque to the engine — it propagates into activity
/// payloads and idempotency keys. The literal `"default"` is reserved for
/// fallback / unconfigured deployments and MUST NOT be reused by persisted
/// revisions (which adopt the `org-policy:<n>` shape).
pub const DEFAULT_POLICY_REVISION: &str = "org-policy:default";

/// Default revision label used when the capability-boundary allowlist has not
/// been parsed. The boundary rule family enters degraded mode at this
/// revision (see [`TripwirePolicy::boundary_path`]).
pub const MISSING_ALLOWLIST_REVISION: &str = "capability-boundary-allowlist:missing";

/// Source path (relative to repo root) of the boundary allowlist TOML the
/// boundary-path rule consults at evaluation time. Engine code reads this
/// file lazily; the policy struct only carries the revision + source.
pub const DEFAULT_ALLOWLIST_SOURCE: &str = "scripts/capability-boundary-allowlist.toml";

// ─── Policy source label ────────────────────────────────────────────────────

/// Provenance of a [`TripwirePolicy`] instance.
///
/// `Default` indicates a built-in safe-default posture that has not been
/// overridden by an org policy. Downstream enforcement MUST log a
/// `policy_source=default` activity entry whenever a `Default` policy
/// produces a blocking decision, so operators can audit "who's running on
/// fallback policy".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicySource {
    /// Built-in safe default. Record on every blocking decision.
    #[default]
    Default,
    /// Persisted org policy revision (`org-policy:<n>`).
    Org,
}

// ─── Adjudication mode ─────────────────────────────────────────────────────

/// Who resolves an enforcement hold produced by a rule.
///
/// The operator directive is **NO human-review holds**: every tripwire hold
/// is adjudicated by the ARBITER (lead role) autonomously. [`Arbiter`] is the
/// default for every rule. [`Human`] is a config-only escape hatch — only
/// when an operator explicitly publishes `adjudication = human` for a rule via
/// org policy does the legacy human-review remediation path run for holds
/// that rule triggers.
///
/// [`Arbiter`]: Adjudication::Arbiter
/// [`Human`]: Adjudication::Human
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Adjudication {
    /// Autonomous arbiter (lead role) adjudication. The default.
    #[default]
    Arbiter,
    /// Legacy human-review hold. Only reachable when an operator explicitly
    /// opts a rule out of autonomous adjudication.
    Human,
}

impl Adjudication {
    /// Stable wire literal.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Arbiter => "arbiter",
            Self::Human => "human",
        }
    }

    /// Whether this rule's holds must be routed to a human reviewer.
    pub const fn is_human(&self) -> bool {
        matches!(self, Self::Human)
    }
}

// ─── Per-rule configuration structs ────────────────────────────────────────

/// Migration change detection: files created, changed, or deleted under the
/// repository's migration directories (e.g. `migrations/`, `db/migrations/`,
/// `prisma/migrations/`, `migrations_postgres/`, database crate SQL paths).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MigrationRuleConfig {
    /// Master switch. `false` → rule is skipped entirely.
    pub enabled: bool,
    /// When `true`, findings are recorded as report-only (advisory) and do
    /// not block the gate.
    pub report_only: bool,
    /// Who adjudicates a hold this rule triggers. Defaults to
    /// [`Adjudication::Arbiter`]; an operator may set [`Adjudication::Human`]
    /// via org policy to route this rule's holds to a human reviewer.
    #[serde(default)]
    pub adjudication: Adjudication,
    /// Glob patterns identifying migration files (e.g. `["migrations/**",
    /// "db/migrations/**", "migrations_postgres/**", "crates/*/migrations/**"]`).
    pub path_globs: Vec<String>,
}

impl Default for MigrationRuleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            report_only: false,
            adjudication: Adjudication::Arbiter,
            path_globs: vec![
                // Standard migration directories
                "migrations/**".to_owned(),
                "db/migrations/**".to_owned(),
                "prisma/migrations/**".to_owned(),
                // Postgres-specific migration directories
                "migrations_postgres/**".to_owned(),
                "**/migrations_postgres/**".to_owned(),
                // SQL files under database-related directories
                "**/db/**/*.sql".to_owned(),
                "**/database/**/*.sql".to_owned(),
                // Database crate migrations (e.g., crates/db-core/migrations/**)
                "crates/*/migrations/**".to_owned(),
                "crates/*/migrations/**/*.sql".to_owned(),
            ],
        }
    }
}

/// Dependency identity change: external package identity added, removed,
/// renamed, source-switched, or with a major-version bump that crosses a
/// configured threshold.
///
/// Tuning knobs:
/// - `major_version_bump_blocks`: `true` → any major-version bump in a
///   declared manifest is enforced; `false` → advisory only.
/// - `lockfile_change_blocks`: `true` → lockfile-only changes (no manifest
///   diff) still trip the rule. `false` → lockfile changes alone are silent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DependencyIdentityRuleConfig {
    pub enabled: bool,
    pub report_only: bool,
    /// Who adjudicates a hold this rule triggers (defaults to arbiter).
    #[serde(default)]
    pub adjudication: Adjudication,
    pub major_version_bump_blocks: bool,
    pub lockfile_change_blocks: bool,
    /// Manifest paths watched (e.g. `Cargo.toml`, `package.json`,
    /// `pyproject.toml`, `go.mod`).
    pub manifest_paths: Vec<String>,
    /// Lockfile paths watched.
    pub lockfile_paths: Vec<String>,
}

impl Default for DependencyIdentityRuleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            report_only: false,
            adjudication: Adjudication::Arbiter,
            major_version_bump_blocks: true,
            lockfile_change_blocks: false,
            manifest_paths: vec![
                "**/Cargo.toml".to_owned(),
                "**/package.json".to_owned(),
                "**/pyproject.toml".to_owned(),
                "**/go.mod".to_owned(),
            ],
            lockfile_paths: vec![
                "**/Cargo.lock".to_owned(),
                "**/package-lock.json".to_owned(),
                "**/pnpm-lock.yaml".to_owned(),
                "**/yarn.lock".to_owned(),
                "**/uv.lock".to_owned(),
                "**/poetry.lock".to_owned(),
                "**/go.sum".to_owned(),
            ],
        }
    }
}

/// Network egress change: new outbound host, protocol, SDK, webhook target,
/// or network client introduced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetworkEgressRuleConfig {
    pub enabled: bool,
    pub report_only: bool,
    /// Who adjudicates a hold this rule triggers (defaults to arbiter).
    #[serde(default)]
    pub adjudication: Adjudication,
    /// Substring matchers that flag a file as a network-egress touchpoint
    /// (e.g. `reqwest::`, `curl_easy`, `Webhook`, `http::Request`).
    pub matcher_substrings: Vec<String>,
    /// Path globs excluded from this rule (overlaps with global
    /// [`GeneratedExclusions`] / [`VendorExclusions`] but is rule-local so
    /// the engine can short-circuit before global filters run).
    pub path_globs: Vec<String>,
}

impl Default for NetworkEgressRuleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            report_only: false,
            adjudication: Adjudication::Arbiter,
            matcher_substrings: vec![
                "reqwest::".to_owned(),
                "ureq::".to_owned(),
                "curl::".to_owned(),
                "hyper::".to_owned(),
                "tokio_tungstenite".to_owned(),
                "Webhook".to_owned(),
                "fetch(".to_owned(),
                "axios.".to_owned(),
            ],
            path_globs: Vec::new(),
        }
    }
}

/// Unsafe code change: `unsafe` block or FFI/escape hatch added or
/// materially changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnsafeCodeRuleConfig {
    pub enabled: bool,
    pub report_only: bool,
    /// Who adjudicates a hold this rule triggers (defaults to arbiter).
    #[serde(default)]
    pub adjudication: Adjudication,
    /// File extensions the rule scans for `unsafe` markers. Files outside
    /// this list are ignored.
    pub extensions: Vec<String>,
    /// Substring matchers that flag a block as an unsafe/FFI touchpoint
    /// (Rust: `unsafe {`, `extern "C"`; TS/JS: `eval(`; Python: `ctypes.`,
    /// `cffi.`; Go: `//go:linkname`).
    pub matcher_substrings: Vec<String>,
}

impl Default for UnsafeCodeRuleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            report_only: false,
            adjudication: Adjudication::Arbiter,
            extensions: vec![
                "rs".to_owned(),
                "ts".to_owned(),
                "tsx".to_owned(),
                "js".to_owned(),
                "jsx".to_owned(),
                "mjs".to_owned(),
                "cjs".to_owned(),
                "py".to_owned(),
                "go".to_owned(),
            ],
            matcher_substrings: vec![
                "unsafe {".to_owned(),
                "unsafe fn".to_owned(),
                "extern \"C\"".to_owned(),
                "extern \"system\"".to_owned(),
                "eval(".to_owned(),
                "new Function(".to_owned(),
                "ctypes.".to_owned(),
                "cffi.".to_owned(),
                "//go:linkname".to_owned(),
                "//go:nosplit".to_owned(),
            ],
        }
    }
}

/// Boundary-path change: changed path matches auth, permission, secrets,
/// deployment, billing, or capability-boundary allowlist patterns.
///
/// Tuning knobs:
/// - `match_outside_allowlist`: `true` → any changed file matching the
///   boundary category patterns that is NOT in the allowlist trips the rule;
///   `false` → only new boundary-category files trip it.
/// - `category_patterns`: glob patterns identifying boundary-sensitive paths
///   (e.g. `scripts/capability-boundary-allowlist.toml`, auth/permission
///   files, secrets, deployment configs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoundaryPathRuleConfig {
    /// Master switch. `false` → rule is skipped entirely.
    pub enabled: bool,
    /// When `true`, findings are recorded as report-only (advisory) and do
    /// not block the gate.
    pub report_only: bool,
    /// Who adjudicates a hold this rule triggers (defaults to arbiter).
    #[serde(default)]
    pub adjudication: Adjudication,
    /// `true` → changed files matching boundary category patterns that are
    /// absent from the allowlist trip the rule. `false` → only newly
    /// introduced boundary-category files trigger findings.
    pub match_outside_allowlist: bool,
    /// Glob patterns identifying boundary-sensitive paths (auth, permission,
    /// secrets, deployment, billing, capability-boundary allowlist).
    pub category_patterns: Vec<String>,
}

impl Default for BoundaryPathRuleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            report_only: false,
            adjudication: Adjudication::Arbiter,
            match_outside_allowlist: true,
            category_patterns: vec![
                "scripts/capability-boundary-allowlist.toml".to_owned(),
                "**/auth/**".to_owned(),
                "**/authz/**".to_owned(),
                "**/permissions/**".to_owned(),
                "**/secrets/**".to_owned(),
                "**/deploy/**".to_owned(),
                "**/deployment/**".to_owned(),
                "**/billing/**".to_owned(),
                "**/.env*".to_owned(),
                "**/iam/**".to_owned(),
                "**/rbac/**".to_owned(),
            ],
        }
    }
}

/// Boundary allowlist reference carried inside [`TripwirePolicy`].
///
/// The boundary-path rule uses the allowlist as its positive list for
/// "expected" boundary-adjacent paths; deviations (new files matching the
/// boundary categories, or changed files outside the allowlist) become
/// findings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryAllowlistRef {
    /// Opaque revision label (e.g. `capability-boundary-allowlist:9`).
    /// When unset / the literal
    /// [`MISSING_ALLOWLIST_REVISION`], the boundary rule enters degraded
    /// mode and downstream code MUST emit a warning activity entry.
    pub revision: String,
    /// Source artifact path (e.g.
    /// [`DEFAULT_ALLOWLIST_SOURCE`]). The engine reads this lazily.
    pub source: String,
    /// Optional SHA256 of the allowlist file at evaluation time; lets the
    /// idempotency key vary when the allowlist content changes between
    /// evaluations even if the revision label is unchanged.
    pub content_sha256: Option<String>,
}

impl Default for BoundaryAllowlistRef {
    fn default() -> Self {
        Self {
            revision: MISSING_ALLOWLIST_REVISION.to_owned(),
            source: DEFAULT_ALLOWLIST_SOURCE.to_owned(),
            content_sha256: None,
        }
    }
}

/// Large deletion or rewrite: deletion/rewrite exceeds configured line,
/// file, or percentage thresholds after generated/vendor exclusions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LargeDeleteRewriteRuleConfig {
    pub enabled: bool,
    pub report_only: bool,
    /// Who adjudicates a hold this rule triggers (defaults to arbiter).
    #[serde(default)]
    pub adjudication: Adjudication,
    /// Maximum lines deletable in a single file before the rule trips.
    pub per_file_line_threshold: u32,
    /// Maximum net lines deleted across the PR before the rule trips.
    pub aggregate_line_threshold: u32,
    /// Maximum percentage (0-100) of a file that can be rewritten before
    /// the rule trips.
    pub file_rewrite_percentage_threshold: u32,
    /// Minimum file count of substantive changes before aggregate thresholds
    /// apply. Lower than this → only per-file thresholds are checked.
    pub aggregate_min_files: u32,
}

impl Default for LargeDeleteRewriteRuleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            report_only: false,
            adjudication: Adjudication::Arbiter,
            per_file_line_threshold: 400,
            aggregate_line_threshold: 1_500,
            file_rewrite_percentage_threshold: 60,
            aggregate_min_files: 5,
        }
    }
}

/// CI/workflow change: workflow, CI, release, deploy, or automation policy
/// files change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CIWorkflowRuleConfig {
    pub enabled: bool,
    pub report_only: bool,
    /// Who adjudicates a hold this rule triggers (defaults to arbiter).
    #[serde(default)]
    pub adjudication: Adjudication,
    /// Path globs identifying CI / workflow / release / deploy / automation
    /// files.
    pub path_globs: Vec<String>,
}

impl Default for CIWorkflowRuleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            report_only: false,
            adjudication: Adjudication::Arbiter,
            path_globs: vec![
                ".github/workflows/**".to_owned(),
                ".github/actions/**".to_owned(),
                ".gitlab-ci.yml".to_owned(),
                ".circleci/**".to_owned(),
                "deploy/**".to_owned(),
                "release/**".to_owned(),
                "Tiltfile".to_owned(),
                "Makefile".to_owned(),
                "scripts/check-*-boundary.sh".to_owned(),
            ],
        }
    }
}

// ─── Global exclusions ─────────────────────────────────────────────────────

/// Glob patterns whose contents the tripwire engine treats as generated
/// code (lockfile fragments, build artifacts, codegen output) and skips
/// across all rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedExclusions {
    pub path_globs: Vec<String>,
}

impl Default for GeneratedExclusions {
    fn default() -> Self {
        Self {
            path_globs: vec![
                "**/target/**".to_owned(),
                "**/node_modules/**".to_owned(),
                "**/dist/**".to_owned(),
                "**/build/**".to_owned(),
                "**/.next/**".to_owned(),
                "**/coverage/**".to_owned(),
                "**/__generated__/**".to_owned(),
                "**/*.generated.*".to_owned(),
                "**/*.gen.*".to_owned(),
                "**/Cargo.lock".to_owned(),
                "**/package-lock.json".to_owned(),
                "**/pnpm-lock.yaml".to_owned(),
                "**/yarn.lock".to_owned(),
                "**/uv.lock".to_owned(),
                "**/poetry.lock".to_owned(),
                "**/go.sum".to_owned(),
            ],
        }
    }
}

/// Glob patterns whose contents are vendored third-party code (vendored
/// dependencies, fixtures, snapshots) and skipped across all rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorExclusions {
    pub path_globs: Vec<String>,
}

impl Default for VendorExclusions {
    fn default() -> Self {
        Self {
            path_globs: vec![
                "**/vendor/**".to_owned(),
                "**/third_party/**".to_owned(),
                "**/fixtures/**".to_owned(),
                "**/snapshots/**".to_owned(),
                "**/__snapshots__/**".to_owned(),
                "**/testdata/**".to_owned(),
            ],
        }
    }
}

// ─── Top-level policy struct ───────────────────────────────────────────────

/// All tripwire policy knobs in a single additive struct.
///
/// Constructed by the enforcement epic from `org_policy_get` (or
/// [`TripwirePolicy::default`] when none has been published), and consumed
/// read-only by the deterministic rule engine.
///
/// Every field is public so the struct round-trips cleanly through serde
/// and the engine can pattern-match without method plumbing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TripwirePolicy {
    /// Opaque revision label identifying which org policy snapshot was
    /// evaluated. Propagates into every gate activity payload and the
    /// idempotency key. Defaults to [`DEFAULT_POLICY_REVISION`].
    pub policy_revision: String,
    /// Provenance: built-in [`PolicySource::Default`] or persisted
    /// [`PolicySource::Org`]. Enforcement MUST log a warning when a
    /// `Default` policy produces a blocking decision.
    pub policy_source: PolicySource,
    /// Allowlist revision + source metadata for the boundary-path rule.
    pub allowlist: BoundaryAllowlistRef,
    /// Global generated-path exclusions applied to every rule.
    pub generated_exclusions: GeneratedExclusions,
    /// Global vendored-path exclusions applied to every rule.
    pub vendor_exclusions: VendorExclusions,
    /// Migration change rule.
    pub migration: MigrationRuleConfig,
    /// Dependency identity change rule.
    pub dependency_identity: DependencyIdentityRuleConfig,
    /// Network egress change rule.
    pub network_egress: NetworkEgressRuleConfig,
    /// Unsafe code change rule.
    pub unsafe_code: UnsafeCodeRuleConfig,
    /// Boundary-path change rule with per-rule enabled/report_only/tuning
    /// controls. The [`allowlist`](TripwirePolicy::allowlist) field carries
    /// revision/source metadata; this field controls whether the rule fires
    /// and how findings are surfaced.
    pub boundary_path: BoundaryPathRuleConfig,
    /// Large deletion/rewrite rule.
    pub large_delete_rewrite: LargeDeleteRewriteRuleConfig,
    /// CI / workflow / release / deploy automation rule.
    pub ci_workflow: CIWorkflowRuleConfig,
    /// Optional human-readable rationale attached to the policy snapshot
    /// (e.g. commit message or operator note that introduced it). Carried
    /// into the `tripwire.policy.changed` payload but not into the
    /// idempotency key.
    pub rationale: Option<String>,
}

impl Default for TripwirePolicy {
    /// Safe defaults: every rule is `enabled=true` and `report_only=false`
    /// (enforcement-on). The boundary allowlist revision defaults to
    /// [`MISSING_ALLOWLIST_REVISION`] which puts the boundary rule into
    /// degraded mode by design — operators must publish an allowlist
    /// revision for boundary findings to be enforced.
    fn default() -> Self {
        Self {
            policy_revision: DEFAULT_POLICY_REVISION.to_owned(),
            policy_source: PolicySource::Default,
            allowlist: BoundaryAllowlistRef::default(),
            generated_exclusions: GeneratedExclusions::default(),
            vendor_exclusions: VendorExclusions::default(),
            migration: MigrationRuleConfig::default(),
            dependency_identity: DependencyIdentityRuleConfig::default(),
            network_egress: NetworkEgressRuleConfig::default(),
            unsafe_code: UnsafeCodeRuleConfig::default(),
            boundary_path: BoundaryPathRuleConfig::default(),
            large_delete_rewrite: LargeDeleteRewriteRuleConfig::default(),
            ci_workflow: CIWorkflowRuleConfig::default(),
            rationale: None,
        }
    }
}

impl TripwirePolicy {
    /// Returns `true` when the boundary rule is in degraded mode — i.e.
    /// the allowlist has not been published. Callers MUST emit a warning
    /// activity entry when this returns `true` and the boundary rule is
    /// otherwise enabled.
    pub fn boundary_allowlist_is_degraded(&self) -> bool {
        self.allowlist.revision == MISSING_ALLOWLIST_REVISION || self.allowlist.revision.is_empty()
    }

    /// Returns the reason code for a given rule id (forwarding to
    /// [`super::reason_codes::reason_code_for_rule`]) so policy consumers
    /// do not have to know about the reason-code module path.
    pub fn reason_code_for(&self, rule: super::reason_codes::TripwireRuleId) -> &'static str {
        super::reason_codes::reason_code_for_rule(rule)
    }

    /// Return a clone of this policy with every rule set to `report_only =
    /// true`.  The returned policy preserves all other tuning knobs
    /// (enabled, path globs, thresholds, etc.) so the engine still
    /// produces findings — they just land as advisory rather than
    /// enforcement.
    ///
    /// This is used for **report-only backfill** of existing open PRs:
    /// findings are emitted and logged for visibility, but no
    /// `human-review-hold` label is applied and the PR is not blocked.
    pub fn make_report_only(&self) -> Self {
        let mut clone = self.clone();
        clone.migration.report_only = true;
        clone.dependency_identity.report_only = true;
        clone.network_egress.report_only = true;
        clone.unsafe_code.report_only = true;
        clone.boundary_path.report_only = true;
        clone.large_delete_rewrite.report_only = true;
        clone.ci_workflow.report_only = true;
        clone
    }

    /// The [`Adjudication`] mode configured for a given rule family.
    ///
    /// Defaults to [`Adjudication::Arbiter`] for every rule — the operator
    /// directive is NO human-review holds. Only an explicit org-policy
    /// override flips a rule to [`Adjudication::Human`].
    pub fn adjudication_for(&self, rule: super::reason_codes::TripwireRuleId) -> Adjudication {
        use super::reason_codes::TripwireRuleId;
        match rule {
            TripwireRuleId::MigrationChange => self.migration.adjudication,
            TripwireRuleId::DependencyIdentityChange => self.dependency_identity.adjudication,
            TripwireRuleId::NetworkEgressChange => self.network_egress.adjudication,
            TripwireRuleId::UnsafeCodeChange => self.unsafe_code.adjudication,
            TripwireRuleId::BoundaryPathChange => self.boundary_path.adjudication,
            TripwireRuleId::LargeDeleteOrRewrite => self.large_delete_rewrite.adjudication,
            TripwireRuleId::CIWorkflowChange => self.ci_workflow.adjudication,
        }
    }

    /// Whether any of the given rule families is configured for human
    /// adjudication. A hold whose enforcement findings include at least one
    /// human-adjudicated rule takes the legacy human-review path; otherwise
    /// the arbiter adjudicates autonomously.
    pub fn any_rule_requires_human<I>(&self, rules: I) -> bool
    where
        I: IntoIterator<Item = super::reason_codes::TripwireRuleId>,
    {
        rules
            .into_iter()
            .any(|r| self.adjudication_for(r).is_human())
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tripwires::reason_codes::{
        REASON_CI_WORKFLOW_CHANGE, REASON_DEPENDENCY_IDENTITY_CHANGED,
        REASON_LARGE_DELETE_OR_REWRITE, REASON_MIGRATION_CHANGE, REASON_NETWORK_EGRESS_CHANGE,
        REASON_PATH_BOUNDARY_CHANGE, REASON_UNSAFE_CODE_CHANGE, TripwireRuleId,
    };

    /// Default policy must cover all seven rule families with sensible
    /// enforcement-on defaults. The default must also report `Default`
    /// source and the canonical default revision so downstream code can
    /// distinguish "fallback posture" from "explicitly published policy".
    #[test]
    fn default_policy_covers_all_seven_rule_families() {
        let p = TripwirePolicy::default();

        assert_eq!(p.policy_revision, DEFAULT_POLICY_REVISION);
        assert_eq!(p.policy_source, PolicySource::Default);
        assert!(p.boundary_allowlist_is_degraded());

        // Every rule family must be present, enabled, and enforcement-on
        // (report_only=false) by default.
        for (name, enabled, report_only) in [
            ("migration", p.migration.enabled, p.migration.report_only),
            (
                "dependency_identity",
                p.dependency_identity.enabled,
                p.dependency_identity.report_only,
            ),
            (
                "network_egress",
                p.network_egress.enabled,
                p.network_egress.report_only,
            ),
            (
                "unsafe_code",
                p.unsafe_code.enabled,
                p.unsafe_code.report_only,
            ),
            (
                "boundary_path",
                p.boundary_path.enabled,
                p.boundary_path.report_only,
            ),
            (
                "large_delete_rewrite",
                p.large_delete_rewrite.enabled,
                p.large_delete_rewrite.report_only,
            ),
            (
                "ci_workflow",
                p.ci_workflow.enabled,
                p.ci_workflow.report_only,
            ),
        ] {
            assert!(enabled, "{name} rule must be enabled by default");
            assert!(
                !report_only,
                "{name} rule must default to enforcement-on (report_only=false)"
            );
        }

        // Boundary allowlist degraded-mode flag must reflect the default
        // (allowlist has not been published).
        assert!(p.boundary_allowlist_is_degraded());
    }

    /// Every default rule family must produce at least one matched path /
    /// matcher so the engine has something to scan against.
    #[test]
    fn default_policy_has_non_empty_scan_inputs() {
        let p = TripwirePolicy::default();
        assert!(
            !p.migration.path_globs.is_empty(),
            "migration globs must be populated"
        );
        assert!(
            !p.dependency_identity.manifest_paths.is_empty(),
            "manifest paths must be populated"
        );
        assert!(
            !p.dependency_identity.lockfile_paths.is_empty(),
            "lockfile paths must be populated"
        );
        assert!(
            !p.network_egress.matcher_substrings.is_empty(),
            "egress matchers must be populated"
        );
        assert!(
            !p.unsafe_code.matcher_substrings.is_empty(),
            "unsafe matchers must be populated"
        );
        assert!(
            !p.boundary_path.category_patterns.is_empty(),
            "boundary category_patterns must be populated"
        );
        assert!(
            p.large_delete_rewrite.per_file_line_threshold > 0
                && p.large_delete_rewrite.aggregate_line_threshold > 0,
            "large_delete thresholds must be > 0"
        );
        assert!(
            p.large_delete_rewrite.file_rewrite_percentage_threshold > 0
                && p.large_delete_rewrite.file_rewrite_percentage_threshold <= 100,
            "large_delete percentage threshold must be in (0, 100]"
        );
        assert!(
            !p.ci_workflow.path_globs.is_empty(),
            "ci_workflow globs must be populated"
        );
        assert!(
            !p.generated_exclusions.path_globs.is_empty(),
            "generated exclusions must be populated"
        );
        assert!(
            !p.vendor_exclusions.path_globs.is_empty(),
            "vendor exclusions must be populated"
        );
    }

    /// Default policy's `reason_code_for` mapping must cover every rule
    /// family. This locks down the rule-id ↔ reason-code contract at the
    /// policy layer too, so a future refactor that drops one family is
    /// caught at unit-test time.
    #[test]
    fn default_policy_reason_code_mapping_is_total() {
        let p = TripwirePolicy::default();
        assert_eq!(
            p.reason_code_for(TripwireRuleId::MigrationChange),
            REASON_MIGRATION_CHANGE
        );
        assert_eq!(
            p.reason_code_for(TripwireRuleId::DependencyIdentityChange),
            REASON_DEPENDENCY_IDENTITY_CHANGED
        );
        assert_eq!(
            p.reason_code_for(TripwireRuleId::NetworkEgressChange),
            REASON_NETWORK_EGRESS_CHANGE
        );
        assert_eq!(
            p.reason_code_for(TripwireRuleId::UnsafeCodeChange),
            REASON_UNSAFE_CODE_CHANGE
        );
        assert_eq!(
            p.reason_code_for(TripwireRuleId::BoundaryPathChange),
            REASON_PATH_BOUNDARY_CHANGE
        );
        assert_eq!(
            p.reason_code_for(TripwireRuleId::LargeDeleteOrRewrite),
            REASON_LARGE_DELETE_OR_REWRITE
        );
        assert_eq!(
            p.reason_code_for(TripwireRuleId::CIWorkflowChange),
            REASON_CI_WORKFLOW_CHANGE
        );
    }

    /// Boundary allowlist degraded-mode flag must distinguish "missing"
    /// (literal `MISSING_ALLOWLIST_REVISION`) from "explicitly published".
    #[test]
    fn boundary_allowlist_degraded_flag_handles_explicit_revision() {
        let mut p = TripwirePolicy::default();
        assert!(p.boundary_allowlist_is_degraded());
        p.allowlist.revision = "capability-boundary-allowlist:9".to_owned();
        assert!(!p.boundary_allowlist_is_degraded());
        p.allowlist.revision = String::new();
        assert!(p.boundary_allowlist_is_degraded());
    }

    /// Default policy must serialize / deserialize round-trip identically.
    /// This locks the JSON shape so downstream enforcement + UI + audit
    /// consumers can persist overrides safely.
    #[test]
    fn default_policy_serde_round_trip() {
        let p = TripwirePolicy::default();
        let json = serde_json::to_string(&p).expect("serialize default policy");
        let back: TripwirePolicy = serde_json::from_str(&json).expect("deserialize default policy");
        assert_eq!(p, back);
    }

    /// Per-rule config structs must also round-trip through serde so a
    /// future enforcement layer can override individual knobs without
    /// re-publishing the whole policy document.
    #[test]
    fn per_rule_configs_serde_round_trip() {
        let cases: Vec<(&'static str, String)> = vec![
            (
                "migration",
                serde_json::to_string(&MigrationRuleConfig::default()).unwrap(),
            ),
            (
                "dependency_identity",
                serde_json::to_string(&DependencyIdentityRuleConfig::default()).unwrap(),
            ),
            (
                "network_egress",
                serde_json::to_string(&NetworkEgressRuleConfig::default()).unwrap(),
            ),
            (
                "unsafe_code",
                serde_json::to_string(&UnsafeCodeRuleConfig::default()).unwrap(),
            ),
            (
                "boundary_path",
                serde_json::to_string(&BoundaryPathRuleConfig::default()).unwrap(),
            ),
            (
                "large_delete_rewrite",
                serde_json::to_string(&LargeDeleteRewriteRuleConfig::default()).unwrap(),
            ),
            (
                "ci_workflow",
                serde_json::to_string(&CIWorkflowRuleConfig::default()).unwrap(),
            ),
            (
                "allowlist",
                serde_json::to_string(&BoundaryAllowlistRef::default()).unwrap(),
            ),
            (
                "generated_exclusions",
                serde_json::to_string(&GeneratedExclusions::default()).unwrap(),
            ),
            (
                "vendor_exclusions",
                serde_json::to_string(&VendorExclusions::default()).unwrap(),
            ),
        ];
        for (name, json) in cases {
            let value: serde_json::Value =
                serde_json::from_str(&json).unwrap_or_else(|e| panic!("{name}: invalid json: {e}"));
            assert!(
                value.is_object(),
                "{name}: serialized value must be an object, got {value}"
            );
        }
    }

    /// Every rule defaults to arbiter adjudication (NO human-review holds),
    /// and `any_rule_requires_human` reflects an explicit per-rule override.
    #[test]
    fn adjudication_defaults_to_arbiter_and_honours_human_override() {
        let mut p = TripwirePolicy::default();
        for rule in [
            TripwireRuleId::MigrationChange,
            TripwireRuleId::DependencyIdentityChange,
            TripwireRuleId::NetworkEgressChange,
            TripwireRuleId::UnsafeCodeChange,
            TripwireRuleId::BoundaryPathChange,
            TripwireRuleId::LargeDeleteOrRewrite,
            TripwireRuleId::CIWorkflowChange,
        ] {
            assert_eq!(
                p.adjudication_for(rule),
                Adjudication::Arbiter,
                "{rule:?} must default to arbiter adjudication"
            );
        }
        // No rule requires human by default.
        assert!(!p.any_rule_requires_human([TripwireRuleId::MigrationChange]));

        // Explicit operator override flips exactly one rule to human.
        p.migration.adjudication = Adjudication::Human;
        assert_eq!(
            p.adjudication_for(TripwireRuleId::MigrationChange),
            Adjudication::Human
        );
        assert!(p.any_rule_requires_human([
            TripwireRuleId::MigrationChange,
            TripwireRuleId::UnsafeCodeChange,
        ]));
        // A hold whose findings do not include the human-adjudicated rule
        // stays on the arbiter path.
        assert!(!p.any_rule_requires_human([TripwireRuleId::UnsafeCodeChange]));
    }

    /// Adjudication mode must survive a policy serde round-trip and decode
    /// from a pre-adjudication policy document (serde default = arbiter).
    #[test]
    fn adjudication_round_trips_and_defaults_when_absent() {
        let mut p = TripwirePolicy::default();
        p.unsafe_code.adjudication = Adjudication::Human;
        let json = serde_json::to_string(&p).unwrap();
        let back: TripwirePolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back.unsafe_code.adjudication, Adjudication::Human);
        assert_eq!(back.migration.adjudication, Adjudication::Arbiter);

        // A migration config JSON that predates the field must decode as
        // arbiter (serde default).
        let legacy = r#"{"enabled":true,"report_only":false,"path_globs":["migrations/**"]}"#;
        let cfg: MigrationRuleConfig = serde_json::from_str(legacy).unwrap();
        assert_eq!(cfg.adjudication, Adjudication::Arbiter);
    }

    /// Boundary allowlist content SHA256 is optional but must round-trip
    /// when present so idempotency-key construction downstream sees the
    /// change.
    #[test]
    fn allowlist_content_sha256_round_trips() {
        let a = BoundaryAllowlistRef {
            content_sha256: Some("a".repeat(64)),
            ..BoundaryAllowlistRef::default()
        };
        let json = serde_json::to_string(&a).unwrap();
        let back: BoundaryAllowlistRef = serde_json::from_str(&json).unwrap();
        assert_eq!(a, back);
    }
}
