use std::path::{Path, PathBuf};

use djinn_orchestration_types::coordinator::PR_REVIEW_FEEDBACK_EVENT;
use crate::context::AgentContext;
use djinn_core::models::Task;
use djinn_db::ActivityQuery;
use djinn_db::ProjectRepository;
use djinn_db::TaskRepository;
use djinn_provider::repos::CredentialRepository;

use super::*;

// ─── Constants ────────────────────────────────────────────────────────────────

/// Max characters for verification output included in user messages.
/// Keeps the user-message payload reasonable (clippy stderr can be huge).
const MAX_VERIFICATION_CHARS: usize = 3000;

/// Max characters for a single inline PR review comment included in the prompt.
const MAX_PR_COMMENT_CHARS: usize = 500;

/// PR E2 feature flag: comma-separated list of role names (matching
/// `RoleConfig::name`) that opt-in to auto-injected `code_graph context`
/// summaries. Empty / unset → no roles get auto-injection.
///
/// Example: `DJINN_AUTO_CODE_CONTEXT_ROLES=worker,reviewer`.
const AUTO_CODE_CONTEXT_ROLES_ENV: &str = "DJINN_AUTO_CODE_CONTEXT_ROLES";

/// Char budget for the auto-injected `code_graph context` block. Mirrors
/// the existing 2000-char knowledge-context cap and is enforced via
/// `truncate::smart_truncate` so we keep both head + tail of the block.
const AUTO_CODE_CONTEXT_BUDGET_CHARS: usize = 2000;

/// Cap on the number of high-PageRank symbols we pull from `ranked` before
/// filtering by scope-path overlap. Bounds the worst-case `context()`
/// fan-out per dispatch.
const AUTO_CODE_CONTEXT_RANKED_POOL: usize = 60;

/// Per-file cap on auto-included symbols. The plan calls for "top 3 by
/// PageRank in F".
const AUTO_CODE_CONTEXT_PER_FILE: usize = 3;

/// Outer cap on emitted bullets — prevents runaway expansion when many
/// scope-path files match. Soft cap; the char budget is the hard cap.
const AUTO_CODE_CONTEXT_MAX_BULLETS: usize = 9;

/// PR E3: char budget for the auto-injected `code_graph detect_changes`
/// reviewer block. Mirrors the E2 cap so the two slots never collectively
/// blow past 4k chars in a reviewer prompt.
const REVIEWER_DIFF_CONTEXT_BUDGET_CHARS: usize = 2000;

/// PR E3: outer cap on emitted touched-symbol bullets in the reviewer
/// diff context. Soft cap; the char budget is the hard cap.
const REVIEWER_DIFF_CONTEXT_MAX_BULLETS: usize = 30;

/// PR E3: BFS depth for the per-symbol `impact` lookup that drives risk
/// classification. Matches the `code_graph impact` default
/// (`graph_tools.rs:1346`).
const REVIEWER_DIFF_IMPACT_DEPTH: usize = 3;

mod code_context;
mod feedback;
mod provider_resolution;
mod reviewer_diff;

// Tests hold `AUTO_CODE_CONTEXT_ENV_LOCK` across `.await` on purpose: the lock
// serializes env-var mutation (set/remove) for the duration of each async test
// so concurrent tests cannot race the shared process env. Deliberate test-only
// guard, not a production async-lock concern.
#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests;

#[allow(unused_imports)]
pub(crate) use code_context::{
    build_role_code_graph_context, derive_task_scope_paths, format_knowledge_notes,
    is_role_auto_code_context_enabled,
};
#[cfg(test)]
pub(crate) use feedback::log_snippet;
#[allow(unused_imports)]
pub(crate) use feedback::{
    COMBINED_BRIEF_SECTION_FLOOR_CHARS, COMBINED_BRIEF_TOTAL_CHARS, budget_combined_sections,
    conflict_context_for_dispatch, default_target_branch, extract_worker_context,
    format_command_details, initial_user_message_for_task, load_task, parse_conflict_metadata,
    pr_review_feedback_context, raw_ci_feedback_in_cycle, recent_feedback, runtime_env_diagnostics,
    runtime_fs_diagnostics,
};
pub use provider_resolution::{
    OAuthAuthMethodWire, OAuthCapabilitiesWire, OAuthConfigWire, OAuthFormatFamilyWire,
    ProviderCredential, auth_method_for_provider, capabilities_for_provider, default_base_url,
    format_family_for_provider, load_provider_credential, parse_model_id,
};
#[allow(unused_imports)]
pub(crate) use provider_resolution::{
    build_provider_from_resolved, build_telemetry_meta, build_telemetry_meta_with_attribution,
    resolved_needs_base_url,
};
#[allow(unused_imports)]
pub(crate) use reviewer_diff::build_reviewer_diff_context;
