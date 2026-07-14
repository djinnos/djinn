//! Data passed into and returned from final prompt-context assembly.

use std::collections::BTreeMap;
use std::path::Path;

use djinn_core::models::Task;
use djinn_core::models::task_attempt::TaskAttemptPromptSummary;

use crate::actors::slot::MergeConflictMetadata;
use crate::actors::slot::lifecycle::memory_intent_planner::PlannedQuery;
use crate::context::AgentContext;
use crate::roles::AgentRole;
use crate::skills::ResolvedSkill;

/// Fully assembled prompt context for a single role session.
#[allow(dead_code)]
pub(crate) struct PromptContext {
    pub conflict_files: Option<String>,
    pub activity_text: Option<String>,
    pub worker_summary: Option<String>,
    pub worker_concerns: Option<String>,
    pub epic_context: Option<String>,
    pub knowledge_context: Option<String>,
    pub code_graph_context: Option<String>,
    pub reviewer_diff_context: Option<String>,
    pub ci_blocking_directive: Option<String>,
    pub prior_attempts: Option<Vec<TaskAttemptPromptSummary>>,
    pub completed_dependency_parents: Option<Vec<djinn_db::CompletedParentSummary>>,
    pub worker_resume_note: Option<String>,
    pub arbiter_directive: Option<String>,
    pub base_system_prompt: String,
    pub system_prompt_with_extensions: String,
    /// Exact final provider prompt, after all rendering and capping.
    pub system_prompt: String,
    /// Hash of the exact UTF-8 bytes in `system_prompt`.
    pub system_prompt_hash: String,
    pub prompt_setup_commands: Option<String>,
}

/// Sibling project flagged as relevant (read-only multi-repo, no eager checkout).
#[derive(Debug, Clone)]
pub(crate) struct ReadSourceInfo {
    pub slug: String,
    pub name: String,
}

/// Real dispatch identities used by knowledge retrieval and trace persistence.
#[derive(Debug, Clone, Copy)]
pub(crate) struct KnowledgeContextIdentity<'a> {
    pub session_id: &'a str,
    pub task_run_id: &'a str,
    pub created_by_user_id: Option<&'a str>,
    /// This remains untruncated; it is not the worker-facing resume note.
    pub resume_progress_summary: Option<&'a str>,
}

/// Inputs for final prompt-context assembly.
#[allow(clippy::too_many_arguments)]
pub(crate) struct PromptContextInputs<'a> {
    pub task: &'a Task,
    pub runtime_role: &'a dyn AgentRole,
    pub role_for_epic_check: &'a dyn AgentRole,
    pub project_path: &'a str,
    pub worktree_path: &'a Path,
    pub conflict_ctx: Option<&'a MergeConflictMetadata>,
    pub merge_validation_ctx: Option<String>,
    pub prompt_setup_commands: Option<String>,
    pub system_prompt_extensions: &'a str,
    pub resolved_skills: &'a [ResolvedSkill],
    pub app_state: &'a AgentContext,
    pub knowledge_identity: Option<KnowledgeContextIdentity<'a>>,
    /// Validated planner queries from the attributed host call. `None` keeps
    /// the legacy scope-only rendering path byte-for-byte unchanged.
    pub planned_queries: Option<&'a [PlannedQuery]>,
    pub read_sources: &'a [ReadSourceInfo],
    pub worker_resume_note: Option<&'a str>,
    pub arbiter_directive: Option<&'a str>,
    pub mcp_server_instructions: &'a BTreeMap<String, String>,
}
