use std::path::{Path, PathBuf};

use crate::context::AgentContext;
use djinn_core::models::Task;
use djinn_db::ActivityQuery;
use djinn_db::ProjectRepository;
use djinn_db::TaskRepository;
use djinn_orchestration_types::coordinator::PR_REVIEW_FEEDBACK_EVENT;
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
/// Retained here for agent-side tests that toggle this env var; the canonical
/// implementation (and its own copy of this constant) lives in `djinn-slot`.
///
/// Example: `DJINN_AUTO_CODE_CONTEXT_ROLES=worker,reviewer`.
#[cfg(test)]
pub(crate) const AUTO_CODE_CONTEXT_ROLES_ENV: &str = "DJINN_AUTO_CODE_CONTEXT_ROLES";

mod code_context;
mod feedback;
pub mod provider_resolution;
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
#[allow(unused_imports)]
// retained for agent facade compatibility tests; canonical home is djinn-slot
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
    refresh_oauth_credential_after_401,
};
#[allow(unused_imports)]
pub(crate) use provider_resolution::{
    build_provider_from_resolved, build_telemetry_meta, build_telemetry_meta_with_attribution,
    resolved_needs_base_url,
};
#[allow(unused_imports)]
pub(crate) use reviewer_diff::build_reviewer_diff_context;
