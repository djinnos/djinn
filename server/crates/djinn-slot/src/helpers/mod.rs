//! Slot helper functions.
//!
//! These were extracted from `djinn-agent::actors::slot::helpers` and adapted
//! to use [`SlotContext`] instead of `AgentContext`.

use std::path::{Path, PathBuf};

use crate::host::SlotContext;
use djinn_core::models::{ReopenLedgerEntry, Task};
use djinn_db::ActivityQuery;
use djinn_db::ProjectRepository;
use djinn_db::TaskRepository;
use djinn_orchestration_types::coordinator::PR_REVIEW_FEEDBACK_EVENT;
// use djinn_provider::repos::CredentialRepository; // unused — re-enable when credential helpers are wired

/// Max characters for verification output included in user messages.
/// Keeps the user-message payload reasonable (clippy stderr can be huge).
const MAX_VERIFICATION_CHARS: usize = 3000;

/// Max characters for a single inline PR review comment included in the prompt.
const MAX_PR_COMMENT_CHARS: usize = 500;

/// PR E2 feature flag: comma-separated list of role names (matching
/// `RoleConfig::name`) that opt-in to auto-injected `code_graph context`
/// summaries. Empty / unset → no roles get auto-injection.
const AUTO_CODE_CONTEXT_ROLES_ENV: &str = "DJINN_AUTO_CODE_CONTEXT_ROLES";

/// Char budget for the auto-injected `code_graph context` block.
const AUTO_CODE_CONTEXT_BUDGET_CHARS: usize = 2000;

/// Cap on the number of high-PageRank symbols we pull from `ranked`.
const AUTO_CODE_CONTEXT_RANKED_POOL: usize = 60;

/// Per-file cap on auto-included symbols.
const AUTO_CODE_CONTEXT_PER_FILE: usize = 3;

/// Outer cap on emitted bullets.
const AUTO_CODE_CONTEXT_MAX_BULLETS: usize = 9;

/// Char budget for reviewer diff context.
const REVIEWER_DIFF_CONTEXT_BUDGET_CHARS: usize = 2000;

/// Outer cap on emitted touched-symbol bullets in the reviewer diff context.
const REVIEWER_DIFF_CONTEXT_MAX_BULLETS: usize = 30;

/// BFS depth for the per-symbol `impact` lookup.
const REVIEWER_DIFF_IMPACT_DEPTH: usize = 3;

mod code_context;
mod feedback;
pub mod provider_resolution;
mod reviewer_diff;

// Tests hold `AUTO_CODE_CONTEXT_ENV_LOCK` across `.await` on purpose.
#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests;

pub use code_context::{
    FormatKnowledgeNotesTrace, NotePackingOutcome, build_role_code_graph_context,
    derive_task_scope_paths, format_knowledge_notes, format_knowledge_notes_with_trace,
    is_role_auto_code_context_enabled,
};
#[cfg(test)]
pub(crate) use feedback::log_snippet;
#[allow(unused_imports)]
pub use feedback::{
    COMBINED_BRIEF_SECTION_FLOOR_CHARS, COMBINED_BRIEF_TOTAL_CHARS, LEDGER_BUDGET_CHARS,
    budget_combined_sections, conflict_context_for_dispatch, default_target_branch,
    extract_worker_context, format_attempt_history, format_command_details, format_reopen_ledger,
    initial_user_message_for_task, load_task, parse_conflict_metadata, pr_review_feedback_context,
    raw_ci_feedback_in_cycle, recent_feedback, runtime_env_diagnostics, runtime_fs_diagnostics,
};
pub use provider_resolution::{
    ProviderCredential, auth_method_for_provider, capabilities_for_provider, default_base_url,
    format_family_for_provider, load_provider_credential, parse_model_id,
};
#[allow(unused_imports)]
pub(crate) use provider_resolution::{
    build_provider_from_resolved, build_telemetry_meta, build_telemetry_meta_with_attribution,
    resolved_needs_base_url,
};
pub use reviewer_diff::build_reviewer_diff_context;
